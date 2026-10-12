//! 添加供应商的预设卡片表（提案 10 第三节）：分国内 / 海外 / 中转 / 推理平台 / 自己填，带搜索。
//! 加过的打勾，再点是「再加一家」（同一家的另一个 Key）。选一张就交给编辑器，只要填 Key。

use confdeck_core::preset::{self, Preset};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::data::{self, Logo, Store};
use crate::theme::Theme;
use crate::ui;

pub enum AddEvent {
    /// 选了一张预设卡；`None` 是自定义。
    Chosen(Option<&'static Preset>),
    Closed,
}

/// 预设分组。每个预设必须在其中一组里（下面有测试），新加预设时在这里补一行。
const DOMESTIC: &[&str] = &[
    "deepseek",
    "kimi",
    "kimi-coding",
    "zhipu",
    "zhipu-coding",
    "minimax",
    "qianwen",
    "bailian-token-plan",
    "volcengine",
    "volcengine-coding",
    "xiaomi",
    "xiaomi-token-plan",
    "ant-ling",
    "siliconflow",
];
const OVERSEAS: &[&str] = &[
    "anthropic",
    "openai",
    "google",
    "xai",
    "mistral",
    "meta",
    "kimi-intl",
    "kimi-coding-intl",
    "zai",
    "zai-coding",
    "minimax-intl",
    "qwencloud",
    "bailian-token-plan-intl",
    "byteplus-coding",
    "xiaomi-token-plan-sgp",
    "xiaomi-token-plan-ams",
    "siliconflow-intl",
];
const RELAY: &[&str] = &[
    "openrouter",
    "vercel",
    "huggingface",
    "opencode-zen",
    "opencode-go",
];
/// 托管开源模型的推理平台。
const HOSTING: &[&str] = &[
    "together",
    "fireworks",
    "groq",
    "cerebras",
    "baseten",
    "nvidia",
];

pub const WIDTH: f32 = 656.;

pub struct AddProvider {
    store: Entity<Store>,
    search: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<AddEvent> for AddProvider {}

impl AddProvider {
    pub fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let t = Theme::window(window.appearance());
        crate::theme::sync_component_theme(&t, cx);
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(t!("add.search")));
        search.update(cx, |search, cx| search.focus(window, cx));

        let sheet = cx.entity().downgrade();
        let window_id = window.window_handle().window_id();
        let intercept = cx.intercept_keystrokes(move |event, window, cx| {
            if window.window_handle().window_id() != window_id
                || event.keystroke.modifiers.modified()
                || event.keystroke.key != "escape"
            {
                return;
            }
            if let Some(sheet) = sheet.upgrade() {
                sheet.update(cx, |_, cx| cx.emit(AddEvent::Closed));
                cx.stop_propagation();
            }
        });
        let _subscriptions = vec![
            cx.subscribe(&search, |_, _, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    cx.notify();
                }
            }),
            intercept,
        ];
        Self {
            store,
            search,
            _subscriptions,
        }
    }

    fn matches(&self, preset: &Preset, cx: &App) -> bool {
        let query = self.search.read(cx).value().trim().to_lowercase();
        if query.is_empty() {
            return true;
        }
        let host = preset
            .endpoints
            .values()
            .next()
            .and_then(|url| confdeck_core::provider::host(url))
            .unwrap_or_default();
        preset.name.to_lowercase().contains(&query)
            || preset.id.contains(&query)
            || host.to_lowercase().contains(&query)
    }

    // 一张卡要的东西就是这些，拆成结构体也还是这些。
    #[allow(clippy::too_many_arguments)]
    fn card(
        &self,
        id: ElementId,
        logo: Logo,
        name: SharedString,
        sub: SharedString,
        added: bool,
        t: &Theme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let hover_bg = t.card_2;
        let hover_line = t.pill_line;
        div()
            .id(id)
            .h_flex()
            .items_center()
            .gap(px(9.))
            .h(px(50.))
            .px(px(10.))
            .rounded(px(9.))
            .border_1()
            .border_color(t.line_2)
            .cursor_pointer()
            .hover(move |s| s.bg(hover_bg).border_color(hover_line))
            .child(ui::logo(logo, 20., t, window, cx))
            .child(
                div()
                    .v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_size(px(12.5))
                            .font_weight(FontWeight::MEDIUM)
                            .truncate()
                            .child(name),
                    )
                    .child(
                        // 「已加」放在第二行主机名旁边：放名字那行会把 Kimi For Coding 挤成省略号。
                        div()
                            .h_flex()
                            .items_center()
                            .gap(px(6.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(10.5))
                                    .text_color(t.muted)
                                    .truncate()
                                    .child(sub),
                            )
                            .when(added, |line| {
                                line.child(
                                    div()
                                        .flex_none()
                                        .h_flex()
                                        .items_center()
                                        .gap(px(2.))
                                        .text_size(px(10.5))
                                        .text_color(t.success)
                                        .child(ui::icon("check", 11., t.success))
                                        .child(t!("add.added")),
                                )
                            }),
                    ),
            )
    }

    fn group(
        &self,
        title: SharedString,
        ids: &[&str],
        t: &Theme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let presets: Vec<(&'static Preset, bool)> = {
            let store = self.store.read(cx);
            ids.iter()
                .filter_map(|id| preset::get(id))
                .filter(|preset| self.matches(preset, cx))
                .map(|preset| (preset, store.has_preset(&preset.id)))
                .collect()
        };
        if presets.is_empty() {
            return None;
        }
        let cards: Vec<Stateful<Div>> = presets
            .into_iter()
            .map(|(preset, added)| {
                let host = preset
                    .endpoints
                    .values()
                    .next()
                    .and_then(|url| confdeck_core::provider::host(url))
                    .unwrap_or_default()
                    .to_string();
                let logo =
                    data::preset_logo(&preset.id).unwrap_or_else(|| Logo::letter(&preset.name));
                self.card(
                    ElementId::Name(format!("preset-{}", preset.id).into()),
                    logo,
                    preset.name.clone().into(),
                    host.into(),
                    added,
                    t,
                    window,
                    cx,
                )
                .on_click(cx.listener(move |_, _, _, cx| cx.emit(AddEvent::Chosen(Some(preset)))))
            })
            .collect();
        Some(self.section(title, t).child(grid(cards)))
    }

    fn section(&self, title: SharedString, t: &Theme) -> Div {
        div().v_flex().gap(px(8.)).child(
            div()
                .text_size(px(11.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(t.muted)
                .child(title),
        )
    }
}

/// 三列的网格，最后一行不满也按三列分宽。
fn grid(cards: Vec<Stateful<Div>>) -> Div {
    let mut rows = div().v_flex().gap(px(8.));
    let mut cards = cards.into_iter().peekable();
    while cards.peek().is_some() {
        let mut row = div().h_flex().gap(px(8.));
        for _ in 0..3 {
            row = row.child(match cards.next() {
                Some(card) => div().flex_1().min_w_0().child(card),
                None => div().flex_1(),
            });
        }
        rows = rows.child(row);
    }
    rows
}

impl Render for AddProvider {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = Theme::window(window.appearance());
        let max_h = f32::from(window.viewport_size().height) - 72.;
        let groups = [
            (t!("add.domestic"), DOMESTIC),
            (t!("add.overseas"), OVERSEAS),
            (t!("add.relay"), RELAY),
            (t!("add.hosting"), HOSTING),
        ];
        let mut body = div()
            .id("add-body")
            .v_flex()
            .gap(px(16.))
            .px(px(20.))
            .py(px(16.))
            .flex_1()
            .min_h_0()
            .overflow_y_scroll();
        let mut any = false;
        for (title, ids) in groups {
            if let Some(group) = self.group(title.into(), ids, &t, window, cx) {
                any = true;
                body = body.child(group);
            }
        }
        if !any {
            body = body.child(
                div()
                    .py(px(12.))
                    .text_size(px(12.5))
                    .text_color(t.muted)
                    .text_center()
                    .child(t!("add.empty")),
            );
        }
        let custom = self
            .card(
                "custom".into(),
                Logo::Letter('+'),
                t!("add.custom").into(),
                t!("add.custom_hint").into(),
                false,
                &t,
                window,
                cx,
            )
            .on_click(cx.listener(|_, _, _, cx| cx.emit(AddEvent::Chosen(None))));
        body = body.child(
            self.section(t!("add.custom_group").into(), &t)
                .child(grid(vec![custom])),
        );

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
            .child(
                div()
                    .h_flex()
                    .flex_none()
                    .items_center()
                    .gap(px(12.))
                    .px(px(20.))
                    .h(px(56.))
                    .border_b_1()
                    .border_color(t.line_2)
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(15.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(t!("add.title")),
                    )
                    .child(
                        div()
                            .flex_1()
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
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(AddEvent::Closed))),
                    ),
            )
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：gpui 也导出一个 `test` 属性宏，会把 `#[test]` 抢走。
    use confdeck_core::preset;

    use super::{DOMESTIC, HOSTING, OVERSEAS, RELAY};

    /// 每个预设都要分到一组，不然卡片表里看不到它。
    #[test]
    fn every_preset_is_in_exactly_one_group() {
        for preset in preset::all() {
            let groups = [DOMESTIC, OVERSEAS, RELAY, HOSTING]
                .iter()
                .filter(|group| group.contains(&preset.id.as_str()))
                .count();
            assert_eq!(groups, 1, "{} should be in exactly one group", preset.id);
        }
        for id in DOMESTIC.iter().chain(OVERSEAS).chain(RELAY).chain(HOSTING) {
            assert!(preset::get(id).is_some(), "{id} is not a preset");
        }
    }
}
