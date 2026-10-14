//! 会话页：各 agent 的本地会话，最近活动的在前；能搜、按 agent 筛，点开看对话，复制恢复命令。
//! 会话列表和用量一起扫（`Store::refresh_usage`，`Loaded::sessions`），点开一个会话时才在后台读那个文件（core 的 `session::messages`）。
//! 返回列表、点开别的会话时，没读完的那次读取就停掉，读回来的结果也只给读的那个会话。
//! 搜索：每份会话列表把要搜的几样转好小写存一份，筛出来的结果按搜索词和 agent 留着，画的时候不再一个个转。
//! 列表只排版、只画看得见的那十几行（`visible_rows`）：一行要排两段文字，以前一次画 50 行，
//! 会话多的时候切到这页、搜索时每敲一个键都要十几到三十几毫秒。

use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{Datelike as _, Local, TimeZone as _};
use confdeck_core::agent::AgentKind;
use confdeck_core::session::{self, Message, Role, Session};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::data::{Logo, Notice, NoticeKind, Store};
use crate::errors::Describe as _;
use crate::theme::Theme;
use crate::ui;
use crate::usage::Loaded;
use crate::visible_rows::visible_rows;

/// 列表一行多高：上下边距 9、两行字 41.5、上边线 1。
const ROW: Pixels = px(60.5);
/// 对话一次多画多少条（从最后往前）。
const MESSAGES_PAGE: usize = 100;

pub struct SessionsPage {
    store: Entity<Store>,
    search: Entity<InputState>,
    /// 只看哪个 agent；`None` 是全部。
    agent: Option<AgentKind>,
    agent_from: Option<(usize, u64)>,
    clicks: u64,
    open: Option<Opened>,
    search_cache: Option<Searched>,
    _subscriptions: Vec<Subscription>,
}

/// 一份会话列表的搜索：每个会话要搜的几样（[`haystack`]），和上一次按什么（搜索词、agent）筛出来的下标。
struct Searched {
    loaded: Arc<Loaded>,
    haystacks: Vec<String>,
    key: Option<(String, Option<AgentKind>)>,
    hits: Vec<usize>,
}

/// 读回来的一条消息，画的时候不再拷贝文字。工具那行的家目录在读回来时就换成 `~`。
struct Line {
    role: Role,
    text: SharedString,
}

/// 点开的会话。返回列表、换一个会话时整个丢掉：在读的任务跟着丢，后台的读取看到 `stop` 就停。
struct Opened {
    session: Session,
    /// 还在读是 `None`。
    messages: Option<Result<Vec<Line>, String>>,
    /// 对话从倒数第几条画起。
    shown: usize,
    _reading: Task<()>,
    /// 读消息的循环每读一行看一眼（文件可能几十 MB，不看了就别读完）。
    stop: Arc<AtomicBool>,
}

impl Drop for Opened {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl SessionsPage {
    pub fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(t!("sessions.search")));
        let _subscriptions = vec![
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.subscribe(&search, |_, _, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    cx.notify();
                }
            }),
        ];
        store.update(cx, |store, cx| store.refresh_usage(cx));
        Self {
            store,
            search,
            agent: None,
            agent_from: None,
            clicks: 0,
            open: None,
            search_cache: None,
            _subscriptions,
        }
    }

    fn back(&mut self, cx: &mut Context<Self>) {
        self.open = None;
        cx.notify();
    }

    fn set_agent(
        &mut self,
        agent: Option<AgentKind>,
        agents: &[Option<AgentKind>],
        cx: &mut Context<Self>,
    ) {
        if agent == self.agent {
            return;
        }
        let from = agents.iter().position(|a| *a == self.agent).unwrap_or(0);
        self.clicks += 1;
        self.agent_from = Some((from, self.clicks));
        self.agent = agent;
        cx.notify();
    }

    fn open(&mut self, session: Session, cx: &mut Context<Self>) {
        let stop = Arc::new(AtomicBool::new(false));
        let reading = session.clone();
        let flag = stop.clone();
        let home = self.store.read(cx).home.to_string_lossy().into_owned();
        let task = cx.background_executor().spawn(async move {
            session::messages(&reading, &flag)
                .map(|messages| lines(messages, &home))
                .map_err(|err| err.describe())
        });
        let (agent, id) = (session.agent, session.id.clone());
        let waiting = cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                // 读完时开着的还得是这一个。
                if let Some(open) = this
                    .open
                    .as_mut()
                    .filter(|open| open.session.agent == agent && open.session.id == id)
                {
                    open.messages = Some(result);
                    cx.notify();
                }
            })
            .ok();
        });
        self.open = Some(Opened {
            session,
            messages: None,
            shown: MESSAGES_PAGE,
            _reading: waiting,
            stop,
        });
        cx.notify();
    }

    fn copy(&self, session: &Session, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(session.resume_command()));
        self.store.update(cx, |_, cx| {
            cx.emit(Notice {
                kind: NoticeKind::Success,
                text: t!("sessions.copied").into(),
            })
        });
    }

    fn list(
        &mut self,
        t: &Theme,
        loaded: &Arc<Loaded>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let sessions = &loaded.sessions;
        let query = self.search.read(cx).value().trim().to_lowercase();
        let present: HashSet<AgentKind> = sessions.iter().map(|s| s.agent).collect();
        let mut agents: Vec<Option<AgentKind>> = vec![None];
        agents.extend(
            AgentKind::ALL
                .into_iter()
                .filter(|a| present.contains(a))
                .map(Some),
        );
        let names: Vec<(Option<AgentKind>, SharedString)> = agents
            .iter()
            .map(|agent| {
                let name: SharedString = match agent {
                    None => t!("sessions.all").into(),
                    Some(kind) => agent_name(&self.store, *kind, cx).into(),
                };
                (*agent, name)
            })
            .collect();
        let hits = Searched::hits(&mut self.search_cache, loaded, (query, self.agent)).to_vec();
        let total = hits.len();
        let agents_for_click = agents.clone();
        let head = div()
            .h_flex()
            .flex_none()
            .items_center()
            .gap(px(10.))
            .h(px(34.))
            .mt(px(4.))
            .mb(px(6.))
            .child(
                div()
                    .w(px(260.))
                    .h_flex()
                    .items_center()
                    .gap(px(6.))
                    .h(px(28.))
                    .px(px(9.))
                    .rounded(px(7.))
                    .bg(t.field)
                    .border_1()
                    .border_color(t.line)
                    .child(ui::icon("search", 13., t.faint))
                    .child(
                        div().flex_1().min_w_0().text_size(px(12.5)).child(
                            Input::new(&self.search)
                                .appearance(false)
                                .xsmall()
                                .text_size(px(12.5)),
                        ),
                    ),
            )
            .when(agents.len() > 2, |head| {
                head.child(div().id("agents").bg(t.pill).rounded(px(8.)).child(ui::seg(
                    t,
                    &names,
                    self.agent,
                    self.agent_from,
                    window,
                    cx.listener(move |this, agent: &Option<AgentKind>, _, cx| {
                        this.set_agent(*agent, &agents_for_click, cx)
                    }),
                )))
            })
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(t.muted)
                    .child(if total == 1 {
                        t!("sessions.count_one").to_string()
                    } else {
                        t!("sessions.count_other", count = total).to_string()
                    }),
            );
        let body = if hits.is_empty() {
            card(t)
                .py(px(18.))
                .px(px(16.))
                .flex()
                .justify_center()
                .text_color(t.muted)
                .child(if sessions.is_empty() {
                    t!("sessions.none")
                } else {
                    t!("sessions.no_match")
                })
        } else {
            let home = self.store.read(cx).home.to_string_lossy().into_owned();
            let now = Local::now().timestamp_millis();
            let loaded = loaded.clone();
            card(t).child(visible_rows(
                hits.len(),
                ROW,
                cx.processor(move |this, range: Range<usize>, window, cx| {
                    let t = Theme::window(window.appearance());
                    range
                        .map(|i| {
                            let session = &loaded.sessions[hits[i]];
                            this.row(&t, i, session, &home, now, window, cx)
                        })
                        .collect()
                }),
            ))
        };
        div().v_flex().child(head).child(body)
    }

    #[allow(clippy::too_many_arguments)]
    fn row(
        &self,
        t: &Theme,
        i: usize,
        s: &Session,
        home: &str,
        now: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let hover = t.pill.opacity(0.5);
        let open = s.clone();
        let copy = s.clone();
        let logo = agent_logo(&self.store, s.agent, cx);
        div()
            .id(ElementId::named_usize("session", i))
            .h_flex()
            .items_center()
            .gap(px(12.))
            .h(ROW)
            .px(px(14.))
            .py(px(9.))
            // 第一行的线透明：每行一样高，只排看得见的几行时才算得出每行在哪。
            .border_t_1()
            .border_color(if i > 0 { t.line_2 } else { transparent_black() })
            .cursor_pointer()
            .hover(move |row| row.bg(hover))
            .on_click(cx.listener(move |this, _, _, cx| this.open(open.clone(), cx)))
            .child(ui::logo(logo, 18., t, window, cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .v_flex()
                    .gap(px(2.))
                    .child(ui::one_line(s.label()))
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(t.muted)
                            .child(ui::one_line(folder_text(s.cwd.as_deref(), home))),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(11.5))
                    .text_color(t.muted)
                    .child(ago_text(s.active, now)),
            )
            .child(
                ui::icon_button(ElementId::named_usize("copy", i), t, "copy")
                    .tooltip(ui::tooltip(t!("sessions.copy").to_string()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.copy(&copy, cx);
                    })),
            )
            .into_any_element()
    }

    fn detail(&self, t: &Theme, open: &Opened, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let s = &open.session;
        let home = self.store.read(cx).home.to_string_lossy().into_owned();
        let now = Local::now().timestamp_millis();
        let mut meta = vec![folder_text(s.cwd.as_deref(), &home)];
        if let Some(started) = s.started {
            meta.push(t!("sessions.started", when = time_text(started)).to_string());
        }
        meta.push(t!("sessions.active", when = ago_text(s.active, now)).to_string());
        let copy = s.clone();
        let logo = agent_logo(&self.store, s.agent, cx);
        let header = card(t)
            .v_flex()
            .gap(px(10.))
            .pt(px(14.))
            .pb(px(12.))
            .px(px(16.))
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap(px(10.))
                    .child(ui::logo(logo, 20., t, window, cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(15.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(s.label()),
                    ),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(t.muted)
                    .child(meta.join(" · ")),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_flex()
                            .items_center()
                            .gap(px(8.))
                            .h(px(30.))
                            .px(px(10.))
                            .rounded(px(7.))
                            .bg(t.pill)
                            .child(ui::icon("terminal", 13., t.muted))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .font_family("Menlo")
                                    .text_size(px(11.5))
                                    .child(ui::one_line(s.resume_command())),
                            ),
                    )
                    .child(
                        ui::button("copy-resume", t, ui::Button::Secondary)
                            .child(ui::icon("copy", 13., t.fg))
                            .child(t!("sessions.copy_short").to_string())
                            .on_click(cx.listener(move |this, _, _, cx| this.copy(&copy, cx))),
                    ),
            );
        let body: AnyElement = match &open.messages {
            None => div()
                .mt(px(14.))
                .text_color(t.muted)
                .child(t!("sessions.reading").to_string())
                .into_any_element(),
            Some(Err(err)) => div()
                .mt(px(14.))
                .text_color(t.danger)
                .child(t!("sessions.read_error", error = err).to_string())
                .into_any_element(),
            Some(Ok(messages)) if messages.is_empty() => div()
                .mt(px(14.))
                .text_color(t.muted)
                .child(t!("sessions.empty").to_string())
                .into_any_element(),
            Some(Ok(messages)) => {
                let skip = messages.len().saturating_sub(open.shown);
                let earlier = (skip > 0).then(|| {
                    div().flex().justify_center().child(
                        ui::button("earlier", t, ui::Button::Secondary)
                            .child(
                                t!("sessions.earlier", count = skip.min(MESSAGES_PAGE)).to_string(),
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(open) = this.open.as_mut() {
                                    open.shown += MESSAGES_PAGE;
                                    cx.notify();
                                }
                            })),
                    )
                });
                card(t)
                    .v_flex()
                    .gap(px(12.))
                    .mt(px(12.))
                    .mb(px(16.))
                    .px(px(16.))
                    .py(px(14.))
                    .children(earlier)
                    .children(conversation(t, &messages[skip..]))
                    .into_any_element()
            }
        };
        div()
            .v_flex()
            .child(
                div().h_flex().h(px(34.)).mt(px(4.)).mb(px(6.)).child(
                    ui::text_button("back", t)
                        .child(ui::icon("arrow-left", 14., t.ctl_fg))
                        .child(t!("sessions.back").to_string())
                        .on_click(cx.listener(|this, _, _, cx| this.back(cx))),
                ),
            )
            .child(header)
            .child(body)
    }
}

impl Render for SessionsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = Theme::window(window.appearance());
        let loaded = self.store.read(cx).usage.loaded.clone();
        if let Some(open) = self.open.take() {
            let page = self.detail(&t, &open, window, cx);
            self.open = Some(open);
            return page;
        }
        match loaded {
            Some(loaded) => self.list(&t, &loaded, window, cx),
            // 第一次扫完之前：灰条。
            None => div()
                .v_flex()
                .mt(px(44.))
                .child(card(&t).v_flex().children((0..6).map(|i| {
                    div()
                        .h_flex()
                        .gap(px(12.))
                        .px(px(14.))
                        .py(px(12.))
                        .when(i > 0, |row| row.border_t_1().border_color(t.line_2))
                        .child(div().size(px(18.)).rounded(px(4.)).bg(t.pill))
                        .child(div().w(relative(0.4)).h(px(12.)).rounded(px(3.)).bg(t.pill))
                }))),
        }
    }
}

fn card(t: &Theme) -> Div {
    div()
        .bg(t.card)
        .border_1()
        .border_color(t.line)
        .rounded(px(12.))
}

/// 读回来的消息转成画的时候用的：工具那行的家目录写成 `~`。
fn lines(messages: Vec<Message>, home: &str) -> Vec<Line> {
    messages
        .into_iter()
        .map(|m| Line {
            role: m.role,
            text: if m.role == Role::Tool && !home.is_empty() {
                m.text.replace(home, "~").into()
            } else {
                m.text.into()
            },
        })
        .collect()
}

/// 对话：提问是灰底的块，回答是正文，连着的几次工具调用并成一组小字。
fn conversation(t: &Theme, messages: &[Line]) -> Vec<AnyElement> {
    let mut out = Vec::new();
    let mut tools: Vec<AnyElement> = Vec::new();
    for m in messages {
        if m.role == Role::Tool {
            tools.push(tool_line(t, m.text.clone()));
            continue;
        }
        if !tools.is_empty() {
            out.push(
                div()
                    .v_flex()
                    .gap(px(4.))
                    .children(tools.drain(..))
                    .into_any_element(),
            );
        }
        out.push(message(t, m));
    }
    if !tools.is_empty() {
        out.push(
            div()
                .v_flex()
                .gap(px(4.))
                .children(tools)
                .into_any_element(),
        );
    }
    out
}

fn tool_line(t: &Theme, text: SharedString) -> AnyElement {
    div()
        .h_flex()
        .items_center()
        .gap(px(6.))
        .px(px(2.))
        .text_size(px(11.5))
        .text_color(t.muted)
        .child(ui::icon("terminal", 12., t.faint))
        .child(
            div()
                .min_w_0()
                .font_family("Menlo")
                .child(ui::one_line(text)),
        )
        .into_any_element()
}

fn message(t: &Theme, m: &Line) -> AnyElement {
    match m.role {
        Role::User => div()
            .px(px(12.))
            .py(px(8.))
            .rounded(px(10.))
            .bg(t.pill)
            .text_size(px(13.))
            .child(m.text.clone())
            .into_any_element(),
        Role::Assistant => div()
            .px(px(2.))
            .text_size(px(13.))
            .line_height(px(20.))
            .child(m.text.clone())
            .into_any_element(),
        Role::Tool => tool_line(t, m.text.clone()),
    }
}

impl Searched {
    /// 筛出来的会话（`loaded.sessions` 的下标）：会话列表、搜索词、agent 有一样变了才重筛，结果留在 `cache` 里。
    fn hits<'a>(
        cache: &'a mut Option<Searched>,
        loaded: &Arc<Loaded>,
        key: (String, Option<AgentKind>),
    ) -> &'a [usize] {
        let searched = match cache.take() {
            Some(searched) if Arc::ptr_eq(&searched.loaded, loaded) => searched,
            _ => Searched::new(loaded.clone()),
        };
        let searched = if searched.key.as_ref() == Some(&key) {
            searched
        } else {
            searched.filter(key)
        };
        &cache.insert(searched).hits
    }

    fn new(loaded: Arc<Loaded>) -> Self {
        Self {
            haystacks: loaded.sessions.iter().map(haystack).collect(),
            loaded,
            key: None,
            hits: Vec::new(),
        }
    }

    fn filter(mut self, key: (String, Option<AgentKind>)) -> Self {
        let (query, agent) = &key;
        let sessions = &self.loaded.sessions;
        self.hits = (0..sessions.len())
            .filter(|&n| agent.is_none_or(|agent| sessions[n].agent == agent))
            .filter(|&n| query.is_empty() || self.haystacks[n].contains(query.as_str()))
            .collect();
        self.key = Some(key);
        self
    }
}

/// 搜的是这几样：名字、第一句提问、目录、会话 ID（转成小写，一行一样，搜索词跨不过两样）。
fn haystack(s: &Session) -> String {
    [
        Some(s.label()),
        s.prompt.clone(),
        s.cwd.clone(),
        Some(s.id.clone()),
    ]
    .into_iter()
    .flatten()
    .map(|text| text.to_lowercase())
    .collect::<Vec<_>>()
    .join("\n")
}

fn agent_logo(store: &Entity<Store>, kind: AgentKind, cx: &App) -> Logo {
    store
        .read(cx)
        .agents
        .iter()
        .find(|a| a.kind == kind)
        .map_or(Logo::letter(kind.id()), |a| a.logo)
}

fn agent_name(store: &Entity<Store>, kind: AgentKind, cx: &App) -> &'static str {
    store
        .read(cx)
        .agents
        .iter()
        .find(|a| a.kind == kind)
        .map_or(kind.id(), |a| a.name)
}

/// 目录：家目录写成 `~`。
fn folder_text(cwd: Option<&str>, home: &str) -> String {
    match cwd {
        Some(cwd) if !home.is_empty() && cwd.starts_with(home) => {
            format!("~{}", &cwd[home.len()..])
        }
        Some(cwd) => cwd.to_string(),
        None => t!("sessions.no_folder").to_string(),
    }
}

/// 「3 小时前」：一分钟内是「刚刚」，一天内按分钟、小时，昨天，一周内按天，再早写日期。
fn ago_text(at: i64, now: i64) -> String {
    let minutes = (now - at).max(0) / 60_000;
    let today = Local::now().date_naive();
    let day = Local
        .timestamp_millis_opt(at)
        .earliest()
        .map(|d| d.date_naive());
    if minutes < 1 {
        t!("sessions.just_now").to_string()
    } else if minutes < 60 {
        t!("sessions.minutes_ago", count = minutes).to_string()
    } else if minutes < 24 * 60 && day == Some(today) {
        t!("sessions.hours_ago", count = minutes / 60).to_string()
    } else if day == today.pred_opt() {
        t!("sessions.yesterday").to_string()
    } else if minutes < 7 * 24 * 60 {
        t!("sessions.days_ago", count = (minutes / (24 * 60)).max(2)).to_string()
    } else {
        date_text(at)
    }
}

fn date_text(at: i64) -> String {
    let Some(time) = Local.timestamp_millis_opt(at).earliest() else {
        return String::new();
    };
    let zh = rust_i18n::locale().starts_with("zh");
    let this_year = time.year() == Local::now().year();
    match (zh, this_year) {
        (true, true) => time.format("%-m月%-d日").to_string(),
        (true, false) => time.format("%Y年%-m月%-d日").to_string(),
        (false, true) => time.format("%b %-d").to_string(),
        (false, false) => time.format("%b %-d, %Y").to_string(),
    }
}

/// 日期加时分。
fn time_text(at: i64) -> String {
    let clock = Local
        .timestamp_millis_opt(at)
        .earliest()
        .map(|time| time.format("%H:%M").to_string())
        .unwrap_or_default();
    let today = Local::now().date_naive();
    let day = Local
        .timestamp_millis_opt(at)
        .earliest()
        .map(|d| d.date_naive());
    let date = if day == Some(today) {
        t!("sessions.today").to_string()
    } else if day == today.pred_opt() {
        t!("sessions.yesterday").to_string()
    } else {
        date_text(at)
    };
    format!("{date} {clock}")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use confdeck_core::agent::AgentKind;
    use confdeck_core::session::Session;

    use super::Searched;
    use crate::usage::Loaded;

    fn session(agent: AgentKind, id: &str, prompt: &str, cwd: &str) -> Session {
        Session {
            agent,
            id: id.into(),
            path: PathBuf::from(format!("/s/{id}.jsonl")),
            cwd: Some(cwd.into()),
            prompt: Some(prompt.into()),
            name: None,
            title: None,
            started: None,
            active: 0,
        }
    }

    fn hits(
        cache: &mut Option<Searched>,
        loaded: &Arc<Loaded>,
        query: &str,
        agent: Option<AgentKind>,
    ) -> Vec<usize> {
        Searched::hits(cache, loaded, (query.to_lowercase(), agent)).to_vec()
    }

    /// 名字、提问、目录、ID 都能搜，不分大小写；按 agent 筛；搜索词跨不过两样。
    #[test]
    fn sessions_are_found_by_any_of_their_texts() {
        let loaded = Arc::new(Loaded::of_sessions(vec![
            session(AgentKind::ClaudeCode, "a1", "Fix the Build", "/w/app"),
            session(AgentKind::Codex, "b2", "write docs", "/w/site"),
            session(AgentKind::ClaudeCode, "c3", "refactor", "/w/core"),
        ]));
        let cache = &mut None;
        assert_eq!(hits(cache, &loaded, "", None), [0, 1, 2]);
        assert_eq!(hits(cache, &loaded, "BUILD", None), [0]);
        assert_eq!(
            hits(cache, &loaded, "/w/", Some(AgentKind::ClaudeCode)),
            [0, 2]
        );
        assert_eq!(hits(cache, &loaded, "b2", None), [1]);
        // 提问的末尾和目录的开头连不成一个词。
        assert!(hits(cache, &loaded, "build/w", None).is_empty());
    }

    /// 留着的结果只在会话列表、搜索词、agent 都没变时用。
    #[test]
    fn the_search_is_redone_when_anything_changes() {
        let both = Arc::new(Loaded::of_sessions(vec![
            session(AgentKind::ClaudeCode, "a1", "fix", "/w/app"),
            session(AgentKind::Codex, "b2", "docs", "/w/site"),
        ]));
        let cache = &mut None;
        assert_eq!(hits(cache, &both, "", None), [0, 1]);
        assert_eq!(hits(cache, &both, "", None), [0, 1]);
        assert_eq!(hits(cache, &both, "", Some(AgentKind::Codex)), [1]);
        assert_eq!(
            hits(cache, &both, "fix", Some(AgentKind::Codex)),
            [] as [usize; 0]
        );
        // 换了一份会话列表，搜索词和 agent 一样也重筛。
        let codex = Arc::new(Loaded::of_sessions(vec![session(
            AgentKind::Codex,
            "b2",
            "docs",
            "/w/site",
        )]));
        assert_eq!(hits(cache, &both, "", Some(AgentKind::Codex)), [1]);
        assert_eq!(hits(cache, &codex, "", Some(AgentKind::Codex)), [0]);
    }
}
