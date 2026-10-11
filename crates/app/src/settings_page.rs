//! 设置页（顶栏的齿轮）：设一次就不动的开关，按 agent 分组，现在只有 Claude Code 一组：跳过首次确认、
//! VS Code 插件跟着切换、提交署名（decisions「配置页」：一次性的设置放设置页，署名先做成全局开关）。
//! 点一下写一次（`Store::set_claude_toggle`），不攒着等保存。行里的小字是文件和键名，查问题时用。

use confdeck_core::toggles::{CLAUDE_TOGGLES, ClaudeToggle};
use gpui_kit::component::*;
use gpui_kit::*;
use rust_i18n::t;

use crate::data::{Logo, Store};
use crate::theme::Theme;
use crate::ui;

pub struct SettingsPage {
    store: Entity<Store>,
    _subscription: Subscription,
}

impl SettingsPage {
    pub fn new(store: Entity<Store>, cx: &mut Context<Self>) -> Self {
        let _subscription = cx.observe(&store, |_, _, cx| cx.notify());
        Self {
            store,
            _subscription,
        }
    }

    fn flip(&mut self, toggle: ClaudeToggle, cx: &mut Context<Self>) {
        let on = self.store.read(cx).claude_toggle(toggle);
        self.store
            .update(cx, |store, cx| store.set_claude_toggle(toggle, !on, cx));
    }

    fn title(&self, t: &Theme) -> Div {
        div()
            .h_flex()
            .flex_none()
            .items_center()
            .h(px(28.))
            .px(px(4.))
            .text_size(px(15.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(t.fg)
            .child(t!("settings.title"))
    }

    fn claude_card(&self, t: &Theme, window: &Window, cx: &Context<Self>) -> Div {
        let store = self.store.read(cx);
        let logo = store
            .agents
            .iter()
            .find(|agent| agent.name == "Claude Code")
            .map(|agent| agent.logo)
            .unwrap_or(Logo::Letter('C'));
        let mut card = ui::list(t, false).child(
            div()
                .h_flex()
                .items_center()
                .gap(px(8.))
                .h(px(42.))
                .pl(px(14.))
                .pr(px(12.))
                .text_size(px(12.5))
                .font_weight(FontWeight::SEMIBOLD)
                .child(ui::logo(logo, 16., t, window, cx))
                .child("Claude Code"),
        );
        for (n, &toggle) in CLAUDE_TOGGLES.iter().enumerate() {
            let on = store.claude_toggle(toggle);
            let hover = t.card_2;
            card = card.child(
                div()
                    .id(ElementId::named_usize("toggle", n))
                    .h_flex()
                    .items_center()
                    .gap(px(12.))
                    .h(px(48.))
                    .pl(px(14.))
                    .pr(px(12.))
                    .border_t_1()
                    .border_color(t.line_2)
                    .cursor_pointer()
                    .hover(move |row| row.bg(hover))
                    .on_click(cx.listener(move |this, _, _, cx| this.flip(toggle, cx)))
                    .child(
                        div()
                            .v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_size(px(12.5)).child(label(toggle)))
                            .child(
                                div()
                                    .mt(px(2.))
                                    .truncate()
                                    .font_family("Menlo")
                                    .text_size(px(10.5))
                                    .text_color(t.faint)
                                    .child(format!(
                                        "{} · {}",
                                        store.claude_toggle_path(toggle),
                                        toggle.key()
                                    )),
                            ),
                    )
                    .child(ui::toggle(t, on)),
            );
        }
        card
    }
}

impl Render for SettingsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = Theme::window(window.appearance());
        div()
            .v_flex()
            .gap(px(10.))
            .child(self.title(&t))
            .child(self.claude_card(&t, window, cx))
    }
}

fn label(toggle: ClaudeToggle) -> SharedString {
    match toggle {
        ClaudeToggle::SkipOnboarding => t!("settings.skip_onboarding").into(),
        ClaudeToggle::VscodeFollows => t!("settings.vscode_follows").into(),
        ClaudeToggle::Attribution => t!("settings.attribution").into(),
    }
}
