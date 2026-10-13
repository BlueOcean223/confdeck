//! 会话页：各 agent 的本地会话，最近活动的在前；能搜、按 agent 筛，点开看对话，复制恢复命令。
//! 会话列表和用量一起扫（`Store::refresh_usage`，`Loaded::sessions`），点开一个会话时才在后台读那个文件（core 的 `session::messages`）。

use std::collections::HashSet;

use chrono::{Datelike as _, Local, TimeZone as _};
use confdeck_core::agent::AgentKind;
use confdeck_core::session::{self, Message, Role, Session};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::data::{Logo, Notice, NoticeKind, Store};
use crate::theme::Theme;
use crate::ui;

/// 列表一次多画多少条（「再显示」一次加这么多）。
const PAGE: usize = 50;
/// 对话一次多画多少条（从最后往前）。
const MESSAGES_PAGE: usize = 100;

pub struct SessionsPage {
    store: Entity<Store>,
    search: Entity<InputState>,
    /// 只看哪个 agent；`None` 是全部。
    agent: Option<AgentKind>,
    agent_from: Option<(usize, u64)>,
    clicks: u64,
    /// 列表画到第几条。
    shown: usize,
    open: Option<Opened>,
    _subscriptions: Vec<Subscription>,
}

/// 点开的会话。
struct Opened {
    session: Session,
    /// 还在读是 `None`。
    messages: Option<Result<Vec<Message>, String>>,
    /// 对话从倒数第几条画起。
    shown: usize,
}

impl SessionsPage {
    pub fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(t!("sessions.search")));
        let _subscriptions = vec![
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.subscribe(&search, |this, _, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    this.shown = PAGE;
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
            shown: PAGE,
            open: None,
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
        self.shown = PAGE;
        cx.notify();
    }

    fn open(&mut self, session: Session, cx: &mut Context<Self>) {
        let reading = session.clone();
        self.open = Some(Opened {
            session,
            messages: None,
            shown: MESSAGES_PAGE,
        });
        cx.notify();
        let task = cx
            .background_executor()
            .spawn(async move { session::messages(&reading).map_err(|err| err.to_string()) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if let Some(open) = this.open.as_mut() {
                    open.messages = Some(result);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
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
        sessions: &[Session],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
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
        let matches: Vec<&Session> = sessions
            .iter()
            .filter(|s| self.agent.is_none_or(|agent| s.agent == agent))
            .filter(|s| query.is_empty() || matches(s, &query))
            .collect();
        let total = matches.len();
        let home = self.store.read(cx).home.to_string_lossy().into_owned();
        let now = Local::now().timestamp_millis();
        let rows: Vec<AnyElement> = matches
            .iter()
            .take(self.shown)
            .enumerate()
            .map(|(i, s)| self.row(t, i, s, &home, now, window, cx))
            .collect();
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
        let body = if rows.is_empty() {
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
            card(t).v_flex().children(rows)
        };
        let more = (total > self.shown).then(|| {
            div().flex().justify_center().mt(px(10.)).child(
                ui::button("more", t, ui::Button::Secondary)
                    .child(t!("sessions.more", count = (total - self.shown).min(PAGE)).to_string())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.shown += PAGE;
                        cx.notify();
                    })),
            )
        });
        div().v_flex().child(head).child(body).children(more)
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
            .px(px(14.))
            .py(px(9.))
            .when(i > 0, |row| row.border_t_1().border_color(t.line_2))
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
                    .child(div().truncate().child(s.label()))
                    .child(
                        div()
                            .truncate()
                            .text_size(px(11.5))
                            .text_color(t.muted)
                            .child(folder_text(s.cwd.as_deref(), home)),
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
                                    .truncate()
                                    .font_family("Menlo")
                                    .text_size(px(11.5))
                                    .child(s.resume_command()),
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
                    .children(conversation(t, &messages[skip..], &home))
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
            Some(loaded) => self.list(&t, &loaded.sessions, window, cx),
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

/// 对话：提问是灰底的块，回答是正文，连着的几次工具调用并成一组小字。
fn conversation(t: &Theme, messages: &[Message], home: &str) -> Vec<AnyElement> {
    let mut out = Vec::new();
    let mut tools: Vec<AnyElement> = Vec::new();
    for m in messages {
        if m.role == Role::Tool {
            let text = if home.is_empty() {
                m.text.clone()
            } else {
                m.text.replace(home, "~")
            };
            tools.push(tool_line(t, text));
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

fn tool_line(t: &Theme, text: String) -> AnyElement {
    div()
        .h_flex()
        .items_center()
        .gap(px(6.))
        .px(px(2.))
        .text_size(px(11.5))
        .text_color(t.muted)
        .child(ui::icon("terminal", 12., t.faint))
        .child(div().min_w_0().truncate().font_family("Menlo").child(text))
        .into_any_element()
}

fn message(t: &Theme, m: &Message) -> AnyElement {
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

/// 搜：名字、第一句提问、目录、会话 ID 里有没有（不分大小写）。
fn matches(s: &Session, query: &str) -> bool {
    [
        Some(s.label()),
        s.prompt.clone(),
        s.cwd.clone(),
        Some(s.id.clone()),
    ]
    .into_iter()
    .flatten()
    .any(|text| text.to_lowercase().contains(query))
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
