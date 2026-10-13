//! 选供应商的浮层，照 magpie 的 model picker：顶部是过滤框，下面是选项列表。
//!
//! - 单选 agent：「默认」在最前，选一项就关。
//! - 多选 agent：每行前面有勾选框，勾一下写一次、浮层不关，可以连着勾（提案 01）。
//!   打开时勾着的排在「写进 X 的配置」一组，其余在「可添加」；之后勾、取消只改勾选框，不重新排，行不会在鼠标底下跳。
//! - 接口对不上的供应商排在最后、置灰、写明缺哪种接口，选不了（提案 01）。
//!
//! - 配置不是 confdeck 写的（「自定义 · 主机名」「Key 不一致」，预览 09）：最上面多一组「现在 · 不是 confdeck 写的」，
//!   第一行写文件连到哪，下面挂着动作行：自定义时「把当前配置存成供应商…」，Key 不一致时「用 confdeck 里的 Key 重写」
//!   和「把文件里的 Key 存进这家」。有过滤词时这组不显示，让人专心找供应商。
//!
//! - Codex、Grok Build 存过官方账号时（`accounts.rs`），「默认」换成一个账号一行，选一个就切回默认、换上这个账号的登录。
//!
//! 打开就能直接打字过滤；上下键移动高亮、回车选定（多选时是勾 / 取消）、Esc 关闭；鼠标悬停也会移动高亮。
//! 多选时过滤框是空的，空格也能勾 / 取消；有过滤词时空格照常输入（供应商名里有空格）。

use std::time::Duration;

use confdeck_core::agent::{Connection, Credential, Mode};
use confdeck_core::provider::Provider;
use confdeck_core::status::AgentStatus;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::data::{self, Logo, Store, Unusable};
use crate::theme::Theme;
use crate::ui;

pub enum PickerEvent {
    /// 单选 agent 选了一项：`None` 是「默认」，交还给 agent 自带的登录。
    Picked(Option<usize>),
    /// 多选 agent 勾上或取消了一家，浮层不关。
    Toggled(usize),
    /// 「Key 不一致」：把配置文件里的 Key 存进这家（供应商下标）。
    AdoptKey {
        provider: usize,
        key: String,
    },
    /// 「自定义」：把配置文件里的连接存成一家新供应商，打开预先填好的新建表单。
    SaveAs(Provider),
    /// 选了一个存下的官方账号（账号 ID）。
    Account(String),
    Dismissed,
}

/// 「现在」那组里的动作行。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    /// 用 confdeck 里这家的 Key 重写，就是选这家。
    Rewrite(usize),
    /// 把文件里的 Key 存进这家。
    AdoptKey(usize),
    /// 把当前配置存成供应商。
    SaveAs,
}

impl Action {
    fn icon(self) -> &'static str {
        match self {
            Action::Rewrite(_) => "rotate-ccw",
            Action::AdoptKey(_) => "key-round",
            Action::SaveAs => "plus",
        }
    }
}

/// 选得了的一行：一个选项，一个官方账号（`Picker::accounts` 里第几个），或一个动作。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pick {
    Option(Option<usize>),
    Account(usize),
    Action(Action),
}

/// 「默认」展开后的一个官方账号。
struct Choice {
    /// 存下的账号的 ID；登录文件里还没存的账号是 `None`（选它就是「默认」）。
    id: Option<String>,
    /// 用户起的名字；没起是 `None`，只写邮箱。
    name: Option<SharedString>,
    email: SharedString,
    plan: Option<SharedString>,
    /// 在用（[`crate::official::Entry::in_use`]）：打勾、打开时高亮。
    in_use: bool,
}

/// 列表里的一行。
enum Row {
    Header(SharedString),
    /// 「现在」那组的第一行：文件连到哪。只是看的，选不了。
    Now,
    Option {
        /// `None` 是「默认」。
        option: Option<usize>,
        /// 接口对不上的原因；有原因的置灰、选不了。
        reason: Option<SharedString>,
    },
    Account(usize),
    Action(Action),
}

impl Row {
    fn selectable(&self) -> Option<Pick> {
        match self {
            Row::Option {
                option,
                reason: None,
            } => Some(Pick::Option(*option)),
            Row::Account(i) => Some(Pick::Account(*i)),
            Row::Action(action) => Some(Pick::Action(*action)),
            _ => None,
        }
    }

    fn height(&self) -> f32 {
        match self {
            Row::Header(_) => 24.,
            Row::Option { .. } | Row::Account(_) | Row::Action(_) => 34.,
            Row::Now => NOW_HEIGHT,
        }
    }
}

/// 配置不是 confdeck 写的时候，列表最上面「现在」那组的内容。打开时算一次。
struct Now {
    logos: Vec<Logo>,
    title: SharedString,
    sub: SharedString,
    actions: Vec<Action>,
    /// 文件里的 Key 本身，「把文件里的 Key 存进这家」要存的。
    key: Option<String>,
    /// 「存成供应商」交给表单的那一家。
    draft: Option<Provider>,
}

pub struct Picker {
    store: Entity<Store>,
    agent: usize,
    panel: bool,
    multi: bool,
    /// 多选 agent 打开时勾着的几家，决定分组。
    pinned: Vec<usize>,
    /// 配置不是 confdeck 写的时候才有。
    now: Option<Now>,
    /// 存过官方账号时「默认」展开成的几行。打开时算一次。
    accounts: Vec<Choice>,
    input: Entity<InputState>,
    /// 高亮的是 `rows()` 里第几行，只会落在选得了的行上。
    highlighted: usize,
    scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PickerEvent> for Picker {}

/// 没有匹配项时那行提示的高度。
const EMPTY_HEIGHT: f32 = 56.;
/// 「现在」那行的高度：两行，上面是胶囊上那几个字，下面是文件里的地址或 Key 差在哪（一行放不下，不截断说明）。
const NOW_HEIGHT: f32 = 46.;

impl Picker {
    pub fn new(
        store: Entity<Store>,
        agent: usize,
        panel: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let t = if panel {
            Theme::panel(window.appearance())
        } else {
            Theme::window(window.appearance())
        };
        crate::theme::sync_component_theme(&t, cx);
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(t!("picker.filter")));
        input.update(cx, |input, cx| input.focus(window, cx));

        let multi = store.read(cx).agents[agent].mode == Mode::Coexist;
        let pinned = store.read(cx).enabled(agent);
        let now = if multi {
            None
        } else {
            Self::now(store.read(cx), agent)
        };
        let accounts = if multi {
            Vec::new()
        } else {
            Self::accounts(store.read(cx), agent)
        };

        // 过滤框本身绑定了上下键、回车和 Esc，会先把这几个键吃掉，
        // 所以在按键绑定生效之前就拦下来。
        let picker = cx.entity().downgrade();
        let window_id = window.window_handle().window_id();
        let intercept = cx.intercept_keystrokes(move |event, window, cx| {
            if window.window_handle().window_id() != window_id
                || event.keystroke.modifiers.modified()
            {
                return;
            }
            let Some(picker) = picker.upgrade() else {
                return;
            };
            let key = event.keystroke.key.clone();
            if picker.update(cx, |picker, cx| picker.handle_key(&key, cx)) {
                cx.stop_propagation();
            }
        });

        let _subscriptions = vec![
            cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    this.highlight_first(cx);
                    cx.notify();
                }
            }),
            // 多选时勾了一家，勾选框要跟着变。
            cx.observe(&store, |_, _, cx| cx.notify()),
            intercept,
        ];

        let mut picker = Self {
            store,
            agent,
            panel,
            multi,
            pinned,
            now,
            accounts,
            input,
            highlighted: 0,
            scroll: ScrollHandle::new(),
            _subscriptions,
        };
        // 单选时高亮现在用的那家；没有在用的（默认、自定义、多选）高亮第一个选得了的行。
        let rows = picker.rows(cx);
        let store = picker.store.read(cx);
        let current = match &store.agents[agent].status {
            AgentStatus::Provider { .. } => store.picked(agent).map(|p| Pick::Option(Some(p))),
            AgentStatus::Official => picker
                .accounts
                .iter()
                .position(|choice| choice.in_use)
                .map(Pick::Account),
            _ => None,
        };
        picker.highlighted = rows
            .iter()
            .position(|row| current.is_some() && row.selectable() == current)
            .or_else(|| rows.iter().position(|row| row.selectable().is_some()))
            .unwrap_or(0);
        picker
    }

    /// 存过官方账号时，「默认」展开成存下的账号，加上登录文件里还没存的那个（和供应商页官方那一组一样）。
    fn accounts(store: &Store, agent: usize) -> Vec<Choice> {
        let kind = store.agents[agent].kind;
        if store.accounts_of(kind).is_empty() {
            return Vec::new();
        }
        store
            .official_entries(agent)
            .into_iter()
            .map(|entry| Choice {
                id: if entry.stored { entry.id.clone() } else { None },
                name: entry.name.clone().map(SharedString::from),
                email: entry
                    .email
                    .clone()
                    .or_else(|| entry.id.clone())
                    .unwrap_or_default()
                    .into(),
                plan: entry
                    .plan
                    .as_deref()
                    .map(|plan| SharedString::from(ui::capitalize(plan))),
                in_use: entry.in_use,
            })
            .collect()
    }

    /// 「现在」那组：文件连的不是 confdeck 列表里的哪家时才有。
    fn now(store: &Store, agent: usize) -> Option<Now> {
        let row = &store.agents[agent];
        match &row.status {
            AgentStatus::Custom {
                connection, draft, ..
            } => {
                let (logos, title) = store.pill(agent);
                // 小字是文件里的地址；Bedrock 这类没有地址的接法说一句是什么。
                let sub: SharedString = match connection {
                    Connection::Endpoint { base_url, .. } => base_url.clone().into(),
                    _ => t!("picker.other_setup").into(),
                };
                let actions = draft
                    .is_some()
                    .then_some(Action::SaveAs)
                    .into_iter()
                    .collect();
                Some(Now {
                    logos,
                    title,
                    sub,
                    actions,
                    key: None,
                    draft: draft.clone(),
                })
            }
            AgentStatus::KeyMismatch { id, credential } => {
                let (logos, title) = store.pill(agent);
                let p = store.providers.iter().position(|x| &x.id == id)?;
                // 只有文件里写的是 Key 本身才能存进供应商（core.md「现在用哪家」）。
                let (sub, key): (SharedString, Option<String>) = match credential {
                    Some(Credential::Key(key)) => {
                        (t!("picker.key_differs").into(), Some(key.clone()))
                    }
                    Some(Credential::Helper(_)) => (t!("picker.key_helper").into(), None),
                    Some(Credential::EnvVar(var)) => (t!("picker.key_env", var = var).into(), None),
                    None => (t!("picker.key_missing").into(), None),
                };
                let mut actions = vec![Action::Rewrite(p)];
                if key.is_some() {
                    actions.push(Action::AdoptKey(p));
                }
                Some(Now {
                    logos,
                    title,
                    sub,
                    actions,
                    key,
                    draft: None,
                })
            }
            _ => None,
        }
    }

    /// 过滤后的行。单选：「默认」、能用的供应商、接口对不上的；多选：勾着的、可添加的、接口对不上的。
    fn rows(&self, cx: &App) -> Vec<Row> {
        let store = self.store.read(cx);
        let agent = &store.agents[self.agent];
        let query = self.input.read(cx).value().trim().to_lowercase();
        let matches = |text: &str| query.is_empty() || text.to_lowercase().contains(&query);

        let mut rows = Vec::new();
        // 「现在」那组只在没有过滤词时显示。
        let now = self.now.as_ref().filter(|_| query.is_empty());
        if let Some(now) = now {
            rows.push(Row::Header(t!("picker.now").into()));
            rows.push(Row::Now);
            rows.extend(now.actions.iter().map(|&action| Row::Action(action)));
        }
        let default_keywords =
            matches(&t!("picker.default_keywords")) || matches(&crate::official::name(agent.kind));
        let default_rows: Vec<Row> = if self.multi {
            Vec::new()
        } else if self.accounts.is_empty() {
            default_keywords
                .then_some(Row::Option {
                    option: None,
                    reason: None,
                })
                .into_iter()
                .collect()
        } else {
            (0..self.accounts.len())
                .filter(|&i| {
                    let choice = &self.accounts[i];
                    default_keywords
                        || matches(&choice.email)
                        || choice.name.as_deref().is_some_and(&matches)
                })
                .map(Row::Account)
                .collect()
        };

        let shown: Vec<usize> = (0..store.providers.len())
            .filter(|&p| {
                let provider = &store.providers[p];
                matches(&provider.name) || data::host(provider).is_some_and(matches)
            })
            .collect();
        let (usable, unusable): (Vec<usize>, Vec<usize>) = shown
            .iter()
            .partition(|&&p| store.unusable(self.agent, p).is_none());

        let group = |rows: &mut Vec<Row>, label: SharedString, providers: &[usize]| {
            if providers.is_empty() {
                return;
            }
            rows.push(Row::Header(label));
            rows.extend(providers.iter().map(|&p| Row::Option {
                option: Some(p),
                reason: None,
            }));
        };
        if self.multi {
            let (pinned, rest): (Vec<usize>, Vec<usize>) =
                usable.iter().partition(|p| self.pinned.contains(p));
            group(
                &mut rows,
                t!("picker.in_config", agent = agent.name).into(),
                &pinned,
            );
            group(&mut rows, t!("picker.available").into(), &rest);
        } else if now.is_some() {
            // 有「现在」那组时「默认」也归到供应商那组底下，不然它会像是「现在」的一部分。
            rows.push(Row::Header(t!("picker.group_providers").into()));
            rows.extend(default_rows);
            rows.extend(usable.iter().map(|&p| Row::Option {
                option: Some(p),
                reason: None,
            }));
        } else {
            rows.extend(default_rows);
            group(&mut rows, t!("picker.group").into(), &usable);
        }
        if !unusable.is_empty() {
            let protocols = agent
                .protocols
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(&t!("common.list_separator"));
            let incompatible: SharedString =
                t!("picker.incompatible", protocols = protocols).into();
            let no_key: SharedString = t!("picker.no_key").into();
            if rows.is_empty() || self.multi {
                rows.push(Row::Header(t!("picker.incompatible_group").into()));
            }
            rows.extend(unusable.into_iter().map(|p| Row::Option {
                option: Some(p),
                reason: Some(match store.unusable(self.agent, p) {
                    Some(Unusable::NoKey) => no_key.clone(),
                    _ => incompatible.clone(),
                }),
            }));
        }
        rows
    }

    /// 整个选择器有多高（点），面板按它调整窗口高度。数字和 `render` 里的尺寸一一对应：
    /// 过滤框 39，列表上下各 5 的内边距、分组标题 24、每行 34、最高 300，外圈描边 2。
    pub fn height(&self, cx: &App) -> f32 {
        let rows = self.rows(cx);
        let list = if rows.is_empty() {
            EMPTY_HEIGHT
        } else {
            rows.iter().map(Row::height).sum()
        };
        39. + (list + 10.).min(300.) + 2.
    }

    fn highlight_first(&mut self, cx: &App) {
        self.highlighted = self
            .rows(cx)
            .iter()
            .position(|row| row.selectable().is_some())
            .unwrap_or(0);
    }

    /// 从 `from` 往 `step` 方向找下一个选得了的行，到头绕回去。
    fn step(rows: &[Row], from: usize, step: isize) -> usize {
        let count = rows.len() as isize;
        let mut at = from as isize;
        for _ in 0..count {
            at = (at + step).rem_euclid(count);
            if rows[at as usize].selectable().is_some() {
                return at as usize;
            }
        }
        from
    }

    fn activate(&mut self, pick: Pick, cx: &mut Context<Self>) {
        match pick {
            Pick::Option(option) => match (self.multi, option) {
                (true, Some(p)) => cx.emit(PickerEvent::Toggled(p)),
                (true, None) => {}
                (false, option) => cx.emit(PickerEvent::Picked(option)),
            },
            Pick::Account(i) => match &self.accounts[i].id {
                Some(id) => cx.emit(PickerEvent::Account(id.clone())),
                None => cx.emit(PickerEvent::Picked(None)),
            },
            Pick::Action(Action::Rewrite(p)) => cx.emit(PickerEvent::Picked(Some(p))),
            Pick::Action(Action::AdoptKey(provider)) => {
                if let Some(key) = self.now.as_ref().and_then(|now| now.key.clone()) {
                    cx.emit(PickerEvent::AdoptKey { provider, key });
                }
            }
            Pick::Action(Action::SaveAs) => {
                if let Some(draft) = self.now.as_ref().and_then(|now| now.draft.clone()) {
                    cx.emit(PickerEvent::SaveAs(draft));
                }
            }
        }
    }

    fn handle_key(&mut self, key: &str, cx: &mut Context<Self>) -> bool {
        let rows = self.rows(cx);
        match key {
            "up" if !rows.is_empty() => self.highlighted = Self::step(&rows, self.highlighted, -1),
            "down" if !rows.is_empty() => self.highlighted = Self::step(&rows, self.highlighted, 1),
            "enter" => {
                if let Some(option) = rows.get(self.highlighted).and_then(Row::selectable) {
                    self.activate(option, cx);
                }
                return true;
            }
            "space" if self.multi && self.input.read(cx).value().is_empty() => {
                if let Some(option) = rows.get(self.highlighted).and_then(Row::selectable) {
                    self.activate(option, cx);
                }
                return true;
            }
            "escape" => {
                cx.emit(PickerEvent::Dismissed);
                return true;
            }
            _ => return false,
        }
        self.scroll.scroll_to_item(self.highlighted);
        cx.notify();
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn option_row(
        &self,
        n: usize,
        option: Option<usize>,
        reason: Option<SharedString>,
        store: &Store,
        t: &Theme,
        window: &Window,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let agent = &store.agents[self.agent];
        let (logo, name, sub): (Logo, SharedString, SharedString) = match option {
            Some(p) => {
                let provider = &store.providers[p];
                let host = data::host(provider).unwrap_or_default();
                let sub = match (&reason, store.protocol_for(self.agent, p)) {
                    (Some(reason), _) => reason.clone(),
                    // 多协议的 agent 才需要说明走哪种协议（decisions「供应商和 agent 的关系」）。
                    (None, Some(protocol)) if agent.protocols.len() > 1 => {
                        format!("{host} · {protocol}").into()
                    }
                    (None, _) => host.to_string().into(),
                };
                (data::logo(provider), provider.name.clone().into(), sub)
            }
            None => (
                crate::official::logo(store, self.agent),
                crate::official::name(agent.kind),
                match store.official_email(self.agent) {
                    Some(email) => email.into(),
                    None => t!("picker.default_hint", agent = agent.name).into(),
                },
            ),
        };
        let disabled = reason.is_some();
        let checked = option.is_some_and(|p| store.is_enabled(self.agent, p));

        div()
            .id(ElementId::named_usize("option", n))
            .h_flex()
            .items_center()
            .gap(px(9.))
            .h(px(34.))
            .px(px(8.))
            .rounded(px(7.))
            .when(disabled, |row| row.opacity(0.5))
            .when(!disabled, |row| {
                row.cursor_pointer()
                    .when(n == self.highlighted, |row| row.bg(t.sel))
                    .on_mouse_move(cx.listener(move |this, _, _, cx| {
                        if this.highlighted != n {
                            this.highlighted = n;
                            cx.notify();
                        }
                    }))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.activate(Pick::Option(option), cx)),
                    )
            })
            .when(self.multi, |row| row.child(ui::checkbox(t, checked)))
            .child(ui::logo(logo, 16., t, window, cx))
            .child(
                div()
                    .h_flex()
                    .items_baseline()
                    .gap(px(9.))
                    .min_w_0()
                    .flex_1()
                    .child(div().flex_none().text_size(px(12.5)).child(name))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(10.5))
                            .text_color(t.muted)
                            .child(sub),
                    ),
            )
            .when(!self.multi && checked, |row| {
                row.child(ui::icon("check", 13., t.accent))
            })
    }

    /// 一个官方账号：agent 的图标、邮箱、订阅；现在登的、又在「默认」时打勾。
    fn account_row(
        &self,
        n: usize,
        i: usize,
        store: &Store,
        t: &Theme,
        window: &Window,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let choice = &self.accounts[i];
        let checked = choice.in_use;
        div()
            .id(ElementId::named_usize("account", n))
            .h_flex()
            .items_center()
            .gap(px(9.))
            .h(px(34.))
            .px(px(8.))
            .rounded(px(7.))
            .cursor_pointer()
            .when(n == self.highlighted, |row| row.bg(t.sel))
            .on_mouse_move(cx.listener(move |this, _, _, cx| {
                if this.highlighted != n {
                    this.highlighted = n;
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, _, _, cx| this.activate(Pick::Account(i), cx)))
            .child(ui::logo(
                crate::official::logo(store, self.agent),
                16.,
                t,
                window,
                cx,
            ))
            .child(
                div()
                    .h_flex()
                    .items_baseline()
                    .gap(px(9.))
                    .min_w_0()
                    .flex_1()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.5))
                            .child(choice.name.clone().unwrap_or_else(|| choice.email.clone())),
                    )
                    // 起了名字时邮箱写在后面的小字里。
                    .children(choice.name.as_ref().map(|_| {
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(10.5))
                            .text_color(t.muted)
                            .child(choice.email.clone())
                    }))
                    .children(choice.plan.clone().map(|plan| {
                        div()
                            .flex_none()
                            .text_size(px(10.5))
                            .text_color(t.muted)
                            .child(plan)
                    })),
            )
            .when(checked, |row| row.child(ui::icon("check", 13., t.accent)))
    }

    /// 「现在」那组的第一行：上面是胶囊上那几个字，下面一行小字是文件里的地址或 Key 差在哪。没有悬停、选不了。
    fn now_row(&self, now: &Now, t: &Theme, window: &Window, cx: &App) -> Div {
        div()
            .h_flex()
            .items_center()
            .gap(px(9.))
            .h(px(NOW_HEIGHT))
            .px(px(8.))
            .child(match now.logos.as_slice() {
                [logo] => ui::logo(*logo, 16., t, window, cx),
                logos => ui::logo_stack(logos, t, window, cx).into_any_element(),
            })
            .child(
                div()
                    .v_flex()
                    .min_w_0()
                    .flex_1()
                    .child(
                        div()
                            .truncate()
                            .text_size(px(12.5))
                            .child(now.title.clone()),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(10.5))
                            .text_color(t.muted)
                            .child(now.sub.clone()),
                    ),
            )
    }

    /// 动作行：图标加一句话，强调色（预览 09 的 `.item.add`）。
    fn action_row(
        &self,
        n: usize,
        action: Action,
        store: &Store,
        t: &Theme,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let name_of = |p: usize| store.providers[p].name.clone();
        let label: SharedString = match action {
            Action::Rewrite(p) => t!("picker.rewrite", name = name_of(p)).into(),
            Action::AdoptKey(p) => t!("picker.adopt_key", name = name_of(p)).into(),
            Action::SaveAs => t!("picker.save_as").into(),
        };
        div()
            .id(ElementId::named_usize("action", n))
            .h_flex()
            .items_center()
            .gap(px(9.))
            .h(px(34.))
            .px(px(8.))
            .rounded(px(7.))
            .cursor_pointer()
            .when(n == self.highlighted, |row| row.bg(t.sel))
            .on_mouse_move(cx.listener(move |this, _, _, cx| {
                if this.highlighted != n {
                    this.highlighted = n;
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, _, _, cx| this.activate(Pick::Action(action), cx)))
            .child(
                div()
                    .size(px(16.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(ui::icon(action.icon(), 13., t.accent)),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(12.5))
                    .text_color(t.accent)
                    .child(label),
            )
    }
}

impl Render for Picker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = if self.panel {
            Theme::panel(window.appearance())
        } else {
            Theme::window(window.appearance())
        };
        let rows = self.rows(cx);
        let store = self.store.read(cx);

        let mut list = div()
            .id("picker-list")
            .v_flex()
            .p(px(5.))
            .max_h(px(300.))
            .overflow_y_scroll()
            .track_scroll(&self.scroll);
        for (n, row) in rows.iter().enumerate() {
            list = match row {
                Row::Header(label) => list.child(ui::group_label(&t, label.clone())),
                Row::Now => match &self.now {
                    Some(now) => list.child(self.now_row(now, &t, window, cx)),
                    None => list,
                },
                Row::Option { option, reason } => {
                    list.child(self.option_row(n, *option, reason.clone(), store, &t, window, cx))
                }
                Row::Account(i) => list.child(self.account_row(n, *i, store, &t, window, cx)),
                Row::Action(action) => list.child(self.action_row(n, *action, store, &t, cx)),
            };
        }
        if rows.is_empty() {
            list = list.child(
                div()
                    .h(px(EMPTY_HEIGHT))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(12.5))
                    .text_color(t.muted)
                    .child(t!("picker.empty")),
            );
        }

        // magpie 的浮层是半透明加背景模糊；GPUI 的元素做不了背景模糊，半透明会让底下的行
        // 透出来，所以用不透明的底色。
        div()
            .v_flex()
            .w_full()
            .bg(Hsla { a: 1., ..t.pop_bg })
            .rounded(px(11.))
            .border_1()
            .border_color(t.line)
            .shadow(t.pop_shadow())
            .overflow_hidden()
            .occlude()
            .text_color(t.fg)
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap(px(8.))
                    .h(px(39.))
                    .px(px(12.))
                    .border_b_1()
                    .border_color(t.line_2)
                    .child(ui::icon("search", 14., t.faint))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(13.))
                            .child(Input::new(&self.input).appearance(false).text_size(px(13.))),
                    ),
            )
            .child(list)
            // 出现时淡入并从上方落下来（magpie 的 pop-fade + pop-in）。
            .with_animation(
                "picker-in",
                Animation::new(Duration::from_millis(320)).with_easing(ui::ease_out_quart),
                |el, delta| el.opacity((delta * 2.2).min(1.)).mt(px(-6. * (1. - delta))),
            )
    }
}
