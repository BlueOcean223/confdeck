//! 官方登录当成一家供应商（10-13 用户定的：「官方不也是 provider 吗」）。
//!
//! - 供应商页最上面一组：每个装了的、有官方登录的 agent 一张卡（「Anthropic 官方」「OpenAI 官方」「xAI 官方」），
//!   和别的供应商一样写额度。Agent 页、菜单栏的胶囊和选择器里原来的「默认」也写成这个名字、用厂商的图标。
//! - 点开是 [`OfficialCard`]：登着的账号、存下的账号（Codex、Grok Build 能存几个，原来在 agent 的配置页），各自的额度；
//!   加账号、切换、删掉。Claude Code 存不了多个账号，只列登着的那个。

use confdeck_core::account::{self, Live};
use confdeck_core::agent::AgentKind;
use confdeck_core::current::Current;
use gpui_kit::component::*;
use gpui_kit::*;
use rust_i18n::t;

use crate::data::{self, Logo, Selection, Store};
use crate::quota_view::{self, Reading};
use crate::theme::Theme;
use crate::ui::{self, Button};

/// 官方卡片的宽度：账号一行要放下邮箱、三格额度和按钮。
const WIDTH: f32 = 640.;

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

impl Store {
    /// 有官方卡片的 agent：装了的、有官方登录的。
    pub fn officials(&self) -> Vec<usize> {
        (0..self.agents.len())
            .filter(|&i| self.agents[i].installed && vendor(self.agents[i].kind).is_some())
            .collect()
    }

    /// 这个 agent 现在用的是官方登录。
    pub fn uses_official(&self, agent: usize) -> bool {
        let row = &self.agents[agent];
        matches!(row.selection, Selection::Single(None))
            && matches!(row.current, Some(Current::Default))
    }

    /// 登着的官方账号的邮箱：Codex、Grok Build 看登录文件，Claude Code 看 `~/.claude.json`。
    pub fn official_email(&self, agent: usize) -> Option<String> {
        let kind = self.agents[agent].kind;
        if kind == AgentKind::ClaudeCode {
            return self.accounts.claude.as_ref()?.email.clone();
        }
        match self.live_account(kind)? {
            Live::Known(id) => self
                .accounts_of(kind)
                .into_iter()
                .find(|account| &account.id == id)?
                .email
                .clone(),
            Live::Unknown(identity) => identity.email.clone(),
            Live::LoggedOut => None,
        }
    }

    /// 登着的官方账号的套餐：查到的额度里有就用它，没有用账号资料里的。
    pub fn official_plan(&self, agent: usize) -> Option<String> {
        let kind = self.agents[agent].kind;
        if let Some(Reading::Quota(found)) = self.official_reading(kind)
            && let Some(plan) = &found.plan
        {
            return Some(plan.clone());
        }
        if kind == AgentKind::ClaudeCode {
            return self.accounts.claude.as_ref()?.plan.clone();
        }
        match self.live_account(kind)? {
            Live::Known(id) => self
                .accounts_of(kind)
                .into_iter()
                .find(|account| &account.id == id)?
                .plan
                .clone(),
            Live::Unknown(identity) => identity.plan.clone(),
            Live::LoggedOut => None,
        }
    }
}

pub enum OfficialEvent {
    Closed,
}

/// 点开一张官方卡片：盖在供应商页上，和供应商的编辑器一样。
pub struct OfficialCard {
    store: Entity<Store>,
    agent: usize,
    /// 正在问「删掉吗」的账号。
    removing: Option<String>,
    _subscription: Subscription,
}

impl EventEmitter<OfficialEvent> for OfficialCard {}

impl OfficialCard {
    pub fn new(store: Entity<Store>, agent: usize, cx: &mut Context<Self>) -> Self {
        let _subscription = cx.observe(&store, |_, _, cx| cx.notify());
        Self {
            store,
            agent,
            removing: None,
            _subscription,
        }
    }

    /// 点外面、点叉：在问删不删时先收起那句，不然关掉。
    pub fn request_close(&mut self, cx: &mut Context<Self>) {
        if self.removing.take().is_some() {
            cx.notify();
        } else {
            cx.emit(OfficialEvent::Closed);
        }
    }

    fn header(&self, t: &Theme, window: &Window, cx: &Context<Self>) -> Div {
        let store = self.store.read(cx);
        let agent = &store.agents[self.agent];
        div()
            .h_flex()
            .flex_none()
            .items_center()
            .gap(px(10.))
            .px(px(20.))
            .h(px(56.))
            .border_b_1()
            .border_color(t.line_2)
            .child(ui::logo(logo(store, self.agent), 24., t, window, cx))
            .child(
                div()
                    .text_size(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .truncate()
                    .child(name(agent.kind)),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap(px(5.))
                    .text_size(px(11.5))
                    .text_color(t.muted)
                    .child(ui::logo(agent.logo, 13., t, window, cx))
                    .child(agent.name),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("close")
                    .size(px(26.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .child(ui::icon("x", 15., t.muted))
                    .on_click(cx.listener(|this, _, _, cx| this.request_close(cx))),
            )
    }

    /// 账号一行一个：存下的、登录文件里还没存的那个（切走时自动存下）；登着又在用官方登录的写「在用」，别的有「使用」，
    /// 存下的能删。Codex、Grok Build 最后一行加账号，登录时换成「在浏览器里登录…」和「取消」。
    fn rows(&self, t: &Theme, cx: &Context<Self>) -> Vec<Div> {
        let store = self.store.read(cx);
        let agent_index = self.agent;
        let kind = store.agents[agent_index].kind;
        let official = store.uses_official(agent_index);
        let mut rows = Vec::new();

        if kind == AgentKind::ClaudeCode || !account::supported(kind) {
            let email = store.official_email(agent_index);
            let reading = store.official_reading(kind);
            rows.push(self.account_row(
                t,
                0,
                AccountRow {
                    id: None,
                    email,
                    plan: store.official_plan(agent_index),
                    live: true,
                    reading,
                },
                official,
                false,
                cx,
            ));
            return rows;
        }
        if store.account_blocked(kind) {
            rows.push(
                div()
                    .h_flex()
                    .min_h(px(48.))
                    .px(px(20.))
                    .text_size(px(12.5))
                    .text_color(t.muted)
                    .child(t!("accounts.keyring")),
            );
            return rows;
        }

        let live = store.live_account(kind);
        let mut entries: Vec<AccountRow> = store
            .accounts_of(kind)
            .into_iter()
            .map(|account| {
                let is_live = matches!(live, Some(Live::Known(id)) if *id == account.id);
                AccountRow {
                    id: Some(account.id.clone()),
                    email: account.email.clone().or_else(|| Some(account.id.clone())),
                    plan: account.plan.clone(),
                    live: is_live,
                    reading: if is_live {
                        store.official_reading(kind)
                    } else {
                        Some(store.account_reading(kind, &account.id))
                    },
                }
            })
            .collect();
        if let Some(Live::Unknown(identity)) = live {
            entries.push(AccountRow {
                id: None,
                email: identity.email.clone(),
                plan: identity.plan.clone(),
                live: true,
                reading: store.official_reading(kind),
            });
        }
        for (n, entry) in entries.into_iter().enumerate() {
            rows.push(self.account_row(t, n, entry, official, true, cx));
        }

        let last = match store.login_pending(kind) {
            None => ui::row(t, false, rows.is_empty()).px(px(20.)).child(
                ui::text_button("add-account", t)
                    .child(ui::icon("plus", 14., t.ctl_fg))
                    .child(t!("accounts.add"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.store.update(cx, |s, cx| s.add_account(kind, cx));
                    })),
            ),
            Some(pending) => ui::row(t, false, rows.is_empty())
                .px(px(20.))
                .child(
                    div()
                        .flex_1()
                        .text_size(px(12.5))
                        .text_color(t.muted)
                        .child(t!("accounts.logging_in")),
                )
                .children(pending.url.clone().map(|url| {
                    ui::text_button("open-login", t)
                        .child(ui::icon("external-link", 13., t.ctl_fg))
                        .child(t!("accounts.open_login"))
                        .on_click(move |_, _, cx| cx.open_url(&url))
                }))
                .child(
                    ui::text_button("cancel-login", t)
                        .child(t!("common.cancel"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.store.update(cx, |s, cx| s.cancel_login(kind, cx));
                        })),
                ),
        };
        rows.push(last);
        rows
    }

    /// 一个账号：邮箱和套餐、额度、「在用」或「使用」、删掉。`removable` 时最右边留删掉的位置（没存的空着，按钮一列对齐）。
    fn account_row(
        &self,
        t: &Theme,
        n: usize,
        entry: AccountRow,
        official: bool,
        removable: bool,
        cx: &Context<Self>,
    ) -> Div {
        let agent_index = self.agent;
        let kind = self.store.read(cx).agents[agent_index].kind;
        let removing = entry.id.is_some() && self.removing == entry.id;
        let email: SharedString = match &entry.email {
            Some(email) => email.clone().into(),
            None if entry.live => t!("official.this_login").into(),
            None => SharedString::default(),
        };
        // 右边一栏宽度固定：点了删除换成「取消 删除」时，左边的额度不跟着挪。
        let right_w = px(if removable { 110. } else { 54. });
        let right = if removing {
            let confirm = entry.id.clone().unwrap_or_default();
            div()
                .h_flex()
                .items_center()
                .justify_end()
                .gap(px(14.))
                .w(right_w)
                .child(
                    ui::text_button(ElementId::named_usize("keep-account", n), t)
                        .child(t!("common.cancel"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.removing = None;
                            cx.notify();
                        })),
                )
                .child(
                    ui::text_button(ElementId::named_usize("drop-account", n), t)
                        .text_color(t.danger)
                        .child(t!("accounts.remove"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.removing = None;
                            let id = confirm.clone();
                            this.store
                                .update(cx, |s, cx| s.remove_account(kind, &id, cx));
                        })),
                )
        } else {
            let use_id = entry.id.clone();
            let state: AnyElement = if entry.live && official {
                div()
                    .w(px(52.))
                    .text_size(px(12.))
                    .text_color(t.muted)
                    .child(t!("accounts.in_use"))
                    .into_any_element()
            } else {
                ui::button(
                    ElementId::named_usize("use-account", n),
                    t,
                    Button::Secondary,
                )
                .child(t!("accounts.use"))
                .on_click(cx.listener(move |this, _, _, cx| {
                    // 没存的就是登录文件里那个：切到官方登录就是用它。
                    let write = match use_id.clone() {
                        Some(id) => this
                            .store
                            .update(cx, |s, cx| s.use_account(agent_index, id, cx)),
                        None => this
                            .store
                            .update(cx, |s, cx| s.switch(agent_index, None, cx)),
                    };
                    write.detach();
                }))
                .into_any_element()
            };
            let remove: Option<AnyElement> = removable.then(|| match entry.id.clone() {
                Some(id) => {
                    ui::icon_button(ElementId::named_usize("remove-account", n), t, "trash")
                        .tooltip(ui::tooltip(t!("accounts.remove")))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.removing = Some(id.clone());
                            cx.notify();
                        }))
                        .into_any_element()
                }
                None => div().size(px(28.)).flex_none().into_any_element(),
            });
            div()
                .h_flex()
                .items_center()
                .justify_end()
                .gap(px(10.))
                .w(right_w)
                .child(state)
                .children(remove)
        };
        ui::row(t, false, n == 0)
            .px(px(20.))
            .gap(px(14.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_flex()
                    .items_baseline()
                    .gap(px(8.))
                    .child(div().min_w_0().truncate().child(email))
                    .children(entry.plan.map(|plan| {
                        div()
                            .flex_none()
                            .text_size(px(11.5))
                            .text_color(t.muted)
                            .child(ui::capitalize(&plan))
                    })),
            )
            .child(match &entry.reading {
                Some(reading) => quota_view::cells(t, &format!("account-{n}"), reading),
                None => quota_view::blank(),
            })
            .child(right)
    }
}

/// 官方卡片里的一个账号。
struct AccountRow<'a> {
    /// 存下的账号的 ID；登录文件里还没存的是 `None`。
    id: Option<String>,
    email: Option<String>,
    plan: Option<String>,
    /// 登录文件里就是它。
    live: bool,
    reading: Option<Reading<'a>>,
}

impl Render for OfficialCard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = Theme::window(window.appearance());
        let max_h = f32::from(window.viewport_size().height) - 72.;
        let rows = self.rows(&t, cx);
        div()
            .v_flex()
            .w(px(WIDTH))
            .max_h(px(max_h))
            .bg(t.card)
            .rounded(px(14.))
            .border_1()
            .border_color(t.line)
            .shadow(t.pop_shadow())
            .text_color(t.fg)
            .text_size(px(13.))
            .child(self.header(&t, window, cx))
            .child(
                div()
                    .id("official-rows")
                    .v_flex()
                    .overflow_y_scroll()
                    .pb(px(4.))
                    .children(rows),
            )
    }
}
