//! 官方登录当成一家供应商。
//!
//! - 供应商页最上面一组（[`Officials`]）：每个装了的、有官方登录的 agent 一行（「Anthropic 官方」「OpenAI 官方」「xAI 官方」），
//!   和别的供应商一样写额度。Agent 页、菜单栏的胶囊和选择器里原来的「默认」也写成这个名字、用厂商的图标。
//! - 账号摊在这一组里：一个 agent 有两个以上账号时，它那行变成小标题，
//!   账号一行一个：名字、额度、「在用」或「使用」，行尾「⋯」里是编辑（名字和备注）、删除。只有一个账号时还是一行。
//!   Claude Code 的账号（只在 macOS）不查额度：行里没有额度，「⋯」里没有「额度查询」。

use confdeck_core::account::{self, Live};
use confdeck_core::agent::AgentKind;
use confdeck_core::quota::{CheckKey, QuotaError};
use confdeck_core::readings;
use confdeck_core::status::AgentStatus;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::data::{self, Logo, Store};
use crate::quota_view::{self, Reading, Shown};
use crate::readings::Readings;
use crate::theme::Theme;
use crate::ui::{self, Button};

/// 哪家的官方登录：预设 ID（图标）和名字。
pub fn vendor(kind: AgentKind) -> Option<(&'static str, &'static str)> {
    match kind {
        AgentKind::ClaudeCode => Some(("anthropic", "Anthropic")),
        AgentKind::Codex => Some(("openai", "OpenAI")),
        AgentKind::GrokBuild => Some(("xai", "xAI")),
        AgentKind::Pi | AgentKind::Opencode => None,
    }
}

/// 「Anthropic 官方」。
pub fn name(kind: AgentKind) -> SharedString {
    match vendor(kind) {
        Some((_, vendor)) => t!("official.name", vendor = vendor).into(),
        None => t!("common.default").into(),
    }
}

/// 厂商的图标；没有的用 agent 的。
pub fn logo(store: &Store, agent: usize) -> Logo {
    let row = &store.agents[agent];
    vendor(row.kind)
        .and_then(|(preset, _)| data::preset_logo(preset))
        .unwrap_or(row.logo)
}

/// 官方那一组里的一个账号。
pub struct Entry {
    /// 账号 ID：存下的，或登录文件里还没存的那个（起名字时存下）。Claude Code 的没有。
    pub id: Option<String>,
    /// 存下了（能删）。
    pub stored: bool,
    pub name: Option<String>,
    pub note: Option<String>,
    pub email: Option<String>,
    pub plan: Option<String>,
    /// 登录文件里就是它。
    pub live: bool,
    /// 在用：登录文件里是它，而且这个 agent 用着官方登录（文件连的是别处时不算）。打勾、高亮、「在用」都看它。
    pub in_use: bool,
    /// 读数记在哪（`readings.rs`）：登着的是 [`Store::live_target`]，存下的是这个账号。查不了的（Claude 官方）没有读数。
    pub check: CheckKey,
}

impl Entry {
    /// 写在界面上的：起了名字写名字，不然写邮箱。
    pub fn title(&self) -> String {
        self.name
            .clone()
            .or_else(|| self.email.clone())
            .or_else(|| self.id.clone())
            .unwrap_or_else(|| t!("official.this_login").to_string())
    }
}

impl Store {
    /// 有官方卡片的 agent：装了的、有官方登录的。
    pub fn officials(&self) -> Vec<usize> {
        (0..self.agents.len())
            .filter(|&i| self.agents[i].installed() && vendor(self.agents[i].kind).is_some())
            .collect()
    }

    /// 这个 agent 现在用的是官方登录。
    pub fn uses_official(&self, agent: usize) -> bool {
        self.agents[agent].status == AgentStatus::Official
    }

    /// 这个 agent 的官方账号：存下的（先存的在前），再加登录文件里还没存的那个。
    /// 存不了多个账号的（不是 macOS 时的 Claude Code）只有登着的那个；Codex 的登录在钥匙串里时一个也没有（认不出）。
    pub fn official_entries(&self, agent: usize) -> Vec<Entry> {
        let kind = self.agents[agent].kind;
        let official = self.uses_official(agent);
        if self.account_blocked(kind) {
            return Vec::new();
        }
        let live = self.live_account(kind);
        let mut entries: Vec<Entry> = self
            .accounts_of(kind)
            .into_iter()
            .map(|account| {
                let is_live = matches!(live, Some(Live::Known(id)) if *id == account.id);
                Entry {
                    id: Some(account.id.clone()),
                    stored: true,
                    name: account.name.clone(),
                    note: account.note.clone(),
                    email: account.email.clone(),
                    plan: account.plan.clone(),
                    live: is_live,
                    in_use: is_live && official,
                    check: CheckKey::Account(kind, account.id.clone()),
                }
            })
            .collect();
        if let Some(Live::Unknown(identity) | Live::ViewOnly(identity)) = live {
            let storable = matches!(live, Some(Live::Unknown(_)));
            entries.push(Entry {
                id: storable.then(|| identity.id.clone()),
                stored: false,
                name: None,
                note: None,
                email: identity.email.clone(),
                plan: identity.plan.clone(),
                live: true,
                in_use: official,
                check: self.live_target(kind),
            });
        }
        entries
    }

    /// 登着的官方账号的邮箱。
    pub fn official_email(&self, agent: usize) -> Option<String> {
        let kind = self.agents[agent].kind;
        match self.live_account(kind)? {
            Live::Known(id) => self
                .accounts_of(kind)
                .into_iter()
                .find(|account| &account.id == id)?
                .email
                .clone(),
            Live::Unknown(identity) | Live::ViewOnly(identity) => identity.email.clone(),
            Live::LoggedOut => None,
        }
    }

    /// 登着的官方账号的套餐：查到的额度里有就用它，没有用账号资料里的。
    pub fn official_plan(&self, readings: &Readings, agent: usize) -> Option<String> {
        let kind = self.agents[agent].kind;
        if let Some(Reading {
            shown: Shown::Quota(found),
            ..
        }) = readings.of(&self.live_target(kind))
            && let Some(plan) = &found.plan
        {
            return Some(plan.clone());
        }
        match self.live_account(kind)? {
            Live::Known(id) => self
                .accounts_of(kind)
                .into_iter()
                .find(|account| &account.id == id)?
                .plan
                .clone(),
            Live::Unknown(identity) | Live::ViewOnly(identity) => identity.plan.clone(),
            Live::LoggedOut => None,
        }
    }
}

/// 一个账号：哪个 agent 的、ID。
type Key = (AgentKind, String);

/// 正在改名字和备注的账号。
struct Editing {
    key: Key,
    name: Entity<InputState>,
    note: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

pub enum OfficialsEvent {
    /// 「⋯」里点了「额度查询」。
    Check {
        target: CheckKey,
        name: SharedString,
        logo: Logo,
    },
}

/// 供应商页最上面官方那一组。
pub struct Officials {
    store: Entity<Store>,
    /// 打开了「⋯」的账号。
    menu: Option<Key>,
    /// 刚被「点外面」关掉的菜单：点的正好是它的「⋯」时，不要立刻再打开。
    just_closed: Option<Key>,
    /// 正在问「删掉吗」的账号。
    removing: Option<Key>,
    editing: Option<Editing>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<OfficialsEvent> for Officials {}

impl Officials {
    pub fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Esc：收起改名、菜单或「删掉吗」。输入框自己也绑了 Esc，在它之前拦下来。
        let this = cx.entity().downgrade();
        let window_id = window.window_handle().window_id();
        let intercept = cx.intercept_keystrokes(move |event, window, cx| {
            if window.window_handle().window_id() != window_id
                || event.keystroke.modifiers.modified()
                || event.keystroke.key != "escape"
            {
                return;
            }
            let Some(this) = this.upgrade() else {
                return;
            };
            let busy = {
                let this = this.read(cx);
                this.editing.is_some() || this.menu.is_some() || this.removing.is_some()
            };
            if busy {
                this.update(cx, |this, cx| this.dismiss(cx));
                cx.stop_propagation();
            }
        });
        let readings = store.read(cx).readings.clone();
        Self {
            _subscriptions: vec![
                cx.observe(&store, |_, _, cx| cx.notify()),
                cx.observe(&readings, |_, _, cx| cx.notify()),
                intercept,
            ],
            store,
            menu: None,
            just_closed: None,
            removing: None,
            editing: None,
        }
    }

    /// 收起改名、菜单、「删掉吗」。
    fn dismiss(&mut self, cx: &mut Context<Self>) {
        self.editing = None;
        self.menu = None;
        self.removing = None;
        cx.notify();
    }

    fn toggle_menu(&mut self, key: Key, cx: &mut Context<Self>) {
        if self.just_closed.take().as_ref() == Some(&key) {
            return;
        }
        self.menu = if self.menu.as_ref() == Some(&key) {
            None
        } else {
            Some(key)
        };
        self.removing = None;
        cx.notify();
    }

    fn start_edit(&mut self, key: Key, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        self.removing = None;
        let (name, note, email) = {
            let store = self.store.read(cx);
            let found = store
                .accounts
                .read
                .list
                .iter()
                .find(|account| account.agent == key.0 && account.id == key.1);
            let email = found
                .and_then(|account| account.email.clone())
                .or_else(|| match store.live_account(key.0) {
                    Some(Live::Unknown(identity)) if identity.id == key.1 => identity.email.clone(),
                    _ => None,
                })
                .unwrap_or_default();
            (
                found.and_then(|a| a.name.clone()).unwrap_or_default(),
                found.and_then(|a| a.note.clone()).unwrap_or_default(),
                email,
            )
        };
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(if email.is_empty() {
                    t!("accounts.name_hint").to_string()
                } else {
                    email
                })
                .default_value(name)
        });
        let note = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("accounts.note_hint"))
                .default_value(note)
        });
        let enter =
            |this: &mut Self, _: Entity<InputState>, event: &InputEvent, cx: &mut Context<Self>| {
                if let InputEvent::PressEnter { .. } = event {
                    this.save_edit(cx);
                }
            };
        let _subscriptions = vec![cx.subscribe(&name, enter), cx.subscribe(&note, enter)];
        name.update(cx, |input, cx| input.focus(window, cx));
        self.editing = Some(Editing {
            key,
            name,
            note,
            _subscriptions,
        });
        cx.notify();
    }

    fn save_edit(&mut self, cx: &mut Context<Self>) {
        let Some(editing) = self.editing.take() else {
            return;
        };
        let name = editing.name.read(cx).value().to_string();
        let note = editing.note.read(cx).value().to_string();
        let (kind, id) = editing.key;
        self.store.update(cx, |store, cx| {
            store.label_account(kind, &id, &name, &note, cx)
        });
        cx.notify();
    }

    /// 一个 agent 的几行：一个账号以内是一行；两个以上（或存下的不是登着的那个）是小标题加账号一行一个。
    /// 正在登录时最后多一行「在浏览器里登录…」。
    fn agent_rows(
        &self,
        agent: usize,
        first: bool,
        t: &Theme,
        window: &Window,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        let store = self.store.read(cx);
        let row = &store.agents[agent];
        let kind = row.kind;
        let entries = store.official_entries(agent);
        let grouped = entries.len() > 1 || entries.first().is_some_and(|entry| !entry.live);
        let mut rows = Vec::new();

        if grouped {
            let sub: SharedString = format!(
                "{} · {}",
                row.name,
                t!("accounts.count", count = entries.len())
            )
            .into();
            rows.push(
                ui::row(t, false, first)
                    .child(ui::logo(logo(store, agent), 22., t, window, cx))
                    .child(title_column(
                        t,
                        format!("title-{agent}"),
                        name(kind),
                        sub,
                        None,
                    ))
                    .child(div().flex_1())
                    .children(self.add_button(agent, t, cx))
                    .child(more_slot())
                    .into_any_element(),
            );
            for (n, entry) in entries.iter().enumerate() {
                rows.push(self.account_row(agent, n, entry, t, window, cx));
            }
        } else {
            rows.push(self.single_row(agent, first, entries.first(), t, window, cx));
        }

        if let Some(pending) = store.login_pending(kind) {
            rows.push(
                ui::row(t, false, false)
                    .child(indent())
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(12.5))
                            .text_color(t.muted)
                            .child(t!("accounts.logging_in")),
                    )
                    .children(pending.url.clone().map(|url| {
                        ui::text_button(ElementId::named_usize("open-login", agent), t)
                            .child(ui::icon("external-link", 13., t.ctl_fg))
                            .child(t!("accounts.open_login"))
                            .on_click(move |_, _, cx| cx.open_url(&url))
                    }))
                    .child(
                        ui::text_button(ElementId::named_usize("cancel-login", agent), t)
                            .child(t!("common.cancel"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.store.update(cx, |s, cx| s.cancel_login(kind, cx));
                            })),
                    )
                    .into_any_element(),
            );
        }
        rows
    }

    /// 只有一个账号（或没登录）：厂商的图标、「xAI 官方」，下面是哪个 agent 和登着的账号；额度；在用就画 agent 的小图标；套餐。
    fn single_row(
        &self,
        agent: usize,
        first: bool,
        entry: Option<&Entry>,
        t: &Theme,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let store = self.store.read(cx);
        let row = &store.agents[agent];
        let kind = row.kind;
        let key = entry.and_then(|entry| Some((kind, entry.id.clone()?)));
        if let Some(editing) = self.editing_for(key.as_ref()) {
            return ui::row(t, false, first)
                .child(ui::logo(logo(store, agent), 22., t, window, cx))
                .child(self.edit_row(editing, t, window, cx))
                .into_any_element();
        }
        let readings = store.readings.read(cx);
        let reading = readings.of(&store.live_target(kind));
        let signed_out = matches!(
            reading.as_ref().map(|r| &r.shown),
            Some(Shown::QuotaFailed(QuotaError::NotLoggedIn))
        );
        let sub: SharedString = match entry {
            Some(entry) => format!("{} · {}", row.name, entry.title()).into(),
            None if signed_out => format!("{} · {}", row.name, t!("quota.signed_out")).into(),
            None => row.name.into(),
        };
        let note = entry.and_then(|entry| entry.note.clone());
        let blocked = store.account_blocked(kind);
        ui::row(t, false, first)
            .child(ui::logo(logo(store, agent), 22., t, window, cx))
            .child(title_column(
                t,
                format!("title-{agent}"),
                name(kind),
                sub,
                note,
            ))
            .child(match &reading {
                Some(reading) => {
                    quota_view::cells(t, &format!("official-{agent}"), reading, &store.readings)
                }
                None => quota_view::blank(),
            })
            .child(div().flex_1())
            .when(store.uses_official(agent), |r| {
                r.child(user_chip(
                    t,
                    format!("official-user-{agent}"),
                    row.logo,
                    row.name.to_string(),
                    window,
                    cx,
                ))
            })
            .children(store.official_plan(readings, agent).map(|plan| {
                div()
                    .flex_none()
                    .h(px(22.))
                    .px(px(8.))
                    .rounded(px(6.))
                    .flex()
                    .items_center()
                    .text_size(px(11.5))
                    .text_color(t.fg_2)
                    .bg(t.pill)
                    .child(ui::capitalize(&plan))
            }))
            .when(blocked, |r| {
                r.child(
                    div()
                        .id(ElementId::named_usize("keyring", agent))
                        .child(ui::icon("info", 14., t.faint))
                        .tooltip(ui::tooltip(t!("accounts.keyring"))),
                )
            })
            .children(self.add_button(agent, t, cx))
            .child(match key {
                Some(key) => {
                    let stored = entry.is_some_and(|entry| entry.stored);
                    self.row_end(agent, 0, key, stored, None, t, cx)
                }
                None => more_slot().into_any_element(),
            })
            .into_any_element()
    }

    /// 小标题下面的一个账号：名字（下面一行是邮箱、套餐）、额度、「在用」或「使用」、「⋯」。
    #[allow(clippy::too_many_arguments)]
    fn account_row(
        &self,
        agent: usize,
        n: usize,
        entry: &Entry,
        t: &Theme,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let store = self.store.read(cx);
        let readings = store.readings.read(cx);
        let kind = store.agents[agent].kind;
        let key = entry.id.clone().map(|id| (kind, id));
        if let Some(editing) = self.editing_for(key.as_ref()) {
            return ui::row(t, false, false)
                .min_h(px(42.))
                .py(px(4.))
                .child(indent())
                .child(self.edit_row(editing, t, window, cx))
                .into_any_element();
        }
        let sub: Vec<String> = [
            entry.name.as_ref().and(entry.email.clone()),
            entry.plan.as_deref().map(ui::capitalize),
        ]
        .into_iter()
        .flatten()
        .collect();
        let sub: Option<SharedString> = (!sub.is_empty()).then(|| sub.join(" · ").into());

        let use_id = entry.stored.then(|| entry.id.clone()).flatten();
        // 「在用」和「使用」占同一栏、居中，上下几行对齐。
        let state: AnyElement = if entry.in_use {
            div()
                .flex_none()
                .w(px(STATE_W))
                .flex()
                .justify_center()
                .text_size(px(12.))
                .text_color(t.muted)
                .child(t!("accounts.in_use"))
                .into_any_element()
        } else {
            ui::button(
                ElementId::Name(format!("use-account-{agent}-{n}").into()),
                t,
                Button::Secondary,
            )
            .w(px(STATE_W))
            .child(t!("accounts.use"))
            .on_click(cx.listener(move |this, _, _, cx| {
                // 没存的就是登录文件里那个：切到官方登录就是用它。
                match use_id.clone() {
                    Some(id) => this.store.update(cx, |s, cx| s.use_account(agent, id, cx)),
                    None => this.store.update(cx, |s, cx| s.switch(agent, None, cx)),
                };
            }))
            .into_any_element()
        };
        // 账号一行一个，比小标题矮：名字、后面小字是邮箱（起了名字时）和套餐。
        ui::row(t, false, false)
            .min_h(px(42.))
            .py(px(4.))
            .child(indent())
            .child(with_note(
                name_column(true).child(
                    div()
                        .h_flex()
                        .items_baseline()
                        .gap(px(8.))
                        .min_w_0()
                        .child(note_title(t, entry.title(), entry.note.is_some()))
                        .children(sub.map(|sub| {
                            div()
                                .min_w_0()
                                .text_size(px(11.5))
                                .text_color(t.muted)
                                .child(ui::one_line(sub))
                        })),
                ),
                format!("who-{agent}-{n}"),
                entry.note.clone(),
            ))
            .child(match &readings.of(&entry.check) {
                Some(reading) => {
                    quota_view::cells(t, &format!("account-{agent}-{n}"), reading, &store.readings)
                }
                None => quota_view::blank(),
            })
            .child(div().flex_1())
            .child(match key {
                Some(key) => self.row_end(agent, n, key, entry.stored, Some(state), t, cx),
                None => state,
            })
            .into_any_element()
    }

    /// 行尾：「在用」或「使用」，再是「⋯」（悬停才出来）；点了删除换成「取消 / 删除」。
    #[allow(clippy::too_many_arguments)]
    fn row_end(
        &self,
        agent: usize,
        n: usize,
        key: Key,
        stored: bool,
        state: Option<AnyElement>,
        t: &Theme,
        cx: &Context<Self>,
    ) -> AnyElement {
        let tag = format!("{agent}-{n}");
        if self.removing.as_ref() == Some(&key) {
            let (kind, id) = key;
            return div()
                .h_flex()
                .items_center()
                .gap(px(14.))
                .child(
                    ui::text_button(ElementId::Name(format!("keep-account-{tag}").into()), t)
                        .child(t!("common.cancel"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.removing = None;
                            cx.notify();
                        })),
                )
                .child(
                    ui::text_button(ElementId::Name(format!("drop-account-{tag}").into()), t)
                        .text_color(t.danger)
                        .child(t!("accounts.remove"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.removing = None;
                            this.store
                                .update(cx, |s, cx| s.remove_account(kind, &id, cx));
                        })),
                )
                .into_any_element();
        }
        let open = self.menu.as_ref() == Some(&key);
        let toggle = key.clone();
        let more = ui::more_button(ElementId::Name(format!("more-{tag}").into()), t, open)
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_menu(toggle.clone(), cx)));
        div()
            .h_flex()
            .items_center()
            .gap(px(8.))
            .children(state)
            .child(div().relative().child(more).when(open, |d| {
                d.child(ui::dropdown(self.menu_list(key, stored, t, cx)))
            }))
            .into_any_element()
    }

    /// 「⋯」里的菜单：编辑（名字和备注）、额度查询（Claude 的先不查）；存下的能删除。
    fn menu_list(&self, key: Key, stored: bool, t: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let edit = key.clone();
        let remove = key.clone();
        let closed = key.clone();
        let target = CheckKey::Account(key.0, key.1.clone());
        let checkable = readings::checkable(&target);
        ui::menu(t)
            .on_mouse_down_out(cx.listener(move |this, _, _, cx| {
                this.menu = None;
                this.just_closed = Some(closed.clone());
                cx.notify();
            }))
            .child(
                ui::menu_item("account-edit", t, t!("accounts.edit"), false).on_click(cx.listener(
                    move |this, _, window, cx| this.start_edit(edit.clone(), window, cx),
                )),
            )
            .when(checkable, |menu| {
                menu.child(
                    ui::menu_item("account-check", t, t!("check.title"), false).on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.menu = None;
                            this.open_check(target.clone(), cx);
                        }),
                    ),
                )
            })
            .when(stored, |menu| {
                menu.child(ui::menu_divider(t)).child(
                    ui::menu_item("account-remove", t, t!("accounts.remove"), true).on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.menu = None;
                            this.removing = Some(remove.clone());
                            cx.notify();
                        }),
                    ),
                )
            })
    }

    /// 「额度查询」：交给主窗口盖一层（[`OfficialsEvent::Check`]），名字写账号的名字或邮箱。
    fn open_check(&mut self, target: CheckKey, cx: &mut Context<Self>) {
        let (name, logo) = {
            let store = self.store.read(cx);
            let CheckKey::Account(kind, id) = &target else {
                return;
            };
            let Some(agent) = store.agents.iter().position(|a| a.kind == *kind) else {
                return;
            };
            let title = store
                .official_entries(agent)
                .into_iter()
                .find(|entry| entry.id.as_deref() == Some(id.as_str()))
                .map(|entry| entry.title())
                .unwrap_or_default();
            (
                SharedString::from(format!("{} · {title}", name(*kind))),
                logo(store, agent),
            )
        };
        cx.emit(OfficialsEvent::Check { target, name, logo });
        cx.notify();
    }

    fn editing_for(&self, key: Option<&Key>) -> Option<&Editing> {
        self.editing
            .as_ref()
            .filter(|editing| key == Some(&editing.key))
    }

    /// 改名字和备注：两个输入框，回车或「保存」存，Esc 或「取消」不存。
    fn edit_row(&self, editing: &Editing, t: &Theme, window: &Window, cx: &Context<Self>) -> Div {
        div()
            .flex_1()
            .min_w_0()
            .h_flex()
            .items_center()
            .gap(px(8.))
            .child(div().w(px(190.)).flex_none().child(ui::field(
                t,
                &editing.name,
                false,
                None,
                window,
                cx,
            )))
            .child(div().flex_1().min_w_0().child(ui::field(
                t,
                &editing.note,
                false,
                None,
                window,
                cx,
            )))
            .child(
                ui::button("cancel-edit", t, Button::Secondary)
                    .child(t!("common.cancel"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.editing = None;
                        cx.notify();
                    })),
            )
            .child(
                ui::button("save-edit", t, Button::Primary)
                    .child(t!("common.save"))
                    .on_click(cx.listener(|this, _, _, cx| this.save_edit(cx))),
            )
    }

    /// 「＋ 添加账号」：能存多个账号、没在登录的时候才有。
    fn add_button(&self, agent: usize, t: &Theme, cx: &Context<Self>) -> Option<Stateful<Div>> {
        let store = self.store.read(cx);
        let kind = store.agents[agent].kind;
        if !account::supported(kind)
            || store.account_blocked(kind)
            || store.login_pending(kind).is_some()
        {
            return None;
        }
        Some(
            ui::text_button(ElementId::named_usize("add-account", agent), t)
                .ml(px(4.))
                .child(ui::icon("plus", 13., t.ctl_fg))
                .child(t!("accounts.add"))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.store.update(cx, |s, cx| s.add_account(kind, cx));
                })),
        )
    }
}

impl Render for Officials {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = Theme::window(window.appearance());
        let officials = self.store.read(cx).officials();
        let mut list = ui::list(&t, false)
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.just_closed = None),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.just_closed = None),
            );
        for (n, &agent) in officials.iter().enumerate() {
            list = list.children(self.agent_rows(agent, n == 0, &t, window, cx));
        }
        div().when(!officials.is_empty(), |d| d.child(list))
    }
}

/// 「在用」「使用」那一栏的宽度。
const STATE_W: f32 = 56.;

/// 没有「⋯」的行也空出它的位置：官方那一组每行的「添加账号」、套餐上下对齐。
fn more_slot() -> Div {
    div().flex_none().w(px(28.))
}

/// 小标题下面的账号往右缩一个图标的宽度，名字和上面「xAI 官方」对齐。
fn indent() -> Div {
    div().flex_none().w(px(22.))
}

/// 供应商页一行的名字那一列：有额度一列时定宽（窄窗口里可以收），没有时占满。
pub fn name_column(meters: bool) -> Div {
    let column = div().v_flex().min_w_0();
    if meters {
        column.w(px(220.)).flex_shrink(1.)
    } else {
        column.flex_1()
    }
}

/// 名字一行；有备注时后面跟一个小图标。
fn note_title(t: &Theme, title: impl Into<SharedString>, noted: bool) -> Div {
    div()
        .h_flex()
        .items_center()
        .gap(px(5.))
        .min_w_0()
        .child(div().min_w_0().child(ui::one_line(title.into())))
        .when(noted, |d| d.child(ui::icon("sticky-note", 11., t.faint)))
}

/// 名字那一列：上面「xAI 官方」，下面一行小字；有备注时悬停看。
fn title_column(
    t: &Theme,
    id: String,
    title: SharedString,
    sub: SharedString,
    note: Option<String>,
) -> Stateful<Div> {
    let has_note = note.is_some();
    with_note(
        name_column(true)
            .child(note_title(t, title, has_note).font_weight(FontWeight::MEDIUM))
            .child(
                div()
                    .mt(px(1.))
                    .text_size(px(11.5))
                    .text_color(t.muted)
                    .child(ui::one_line(sub)),
            ),
        id,
        note,
    )
}

/// 有备注的名字悬停时显示备注。
fn with_note(column: Div, id: String, note: Option<String>) -> Stateful<Div> {
    let column = column.id(ElementId::Name(id.into()));
    match note {
        Some(note) => column.tooltip(ui::tooltip(note)),
        None => column,
    }
}

/// 在用这家的 agent：一个带边框的小图标，悬停说是谁。
pub fn user_chip(
    t: &Theme,
    id: String,
    logo: data::Logo,
    tip: String,
    window: &Window,
    cx: &App,
) -> Stateful<Div> {
    div()
        .id(ElementId::Name(id.into()))
        .flex_none()
        .size(px(20.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(5.))
        .bg(t.card_2)
        .border_1()
        .border_color(t.line_2)
        .tooltip(ui::tooltip(tip))
        .child(ui::logo(logo, 13., t, window, cx))
}
