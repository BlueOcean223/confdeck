//! 添加供应商的预设卡片表（提案 10 第三节）：最上面一张「自定义」，下面分账号登录 / 模型厂商 / 第三方平台，带搜索。
//! 加过的打勾，再点是「再加一家」（同一家的另一个 Key）。选一张预设就交给编辑器，只要填 Key；
//! 选账号登录就关掉卡片表、回到供应商页最上面官方那一组，不起登录（10-13 用户定的顺序和做法）。

use confdeck_core::preset::{self, Preset};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::data::{self, Logo, Store};
use crate::official;
use crate::theme::Theme;
use crate::ui;

pub enum AddEvent {
    /// 选了一张预设卡；`None` 是自定义。
    Chosen(Option<&'static Preset>),
    /// 选了账号登录的一张：回到官方那一组。
    Official,
    Closed,
}

/// 预设分组。每个预设必须在其中一组里（下面有测试），新加预设时在这里补一行。
/// 模型厂商：自己出模型的厂商的接口，国内的在前。
const VENDORS_CN: &[&str] = &[
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
];
const VENDORS_INTL: &[&str] = &[
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
];
/// 第三方平台：中转和托管开源模型的推理平台。
const PLATFORMS: &[&str] = &[
    "siliconflow",
    "siliconflow-intl",
    "openrouter",
    "vercel",
    "huggingface",
    "opencode-zen",
    "opencode-go",
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

    fn query(&self, cx: &App) -> String {
        self.search.read(cx).value().trim().to_lowercase()
    }

    fn matches(query: &str, preset: &Preset) -> bool {
        query.is_empty()
            || preset.name.to_lowercase().contains(query)
            || preset.id.contains(query)
            || host(preset).to_lowercase().contains(query)
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

    /// 一组预设卡片（搜索过滤后）；一张都不剩是 `None`。
    fn presets(
        &self,
        ids: &[&str],
        query: &str,
        t: &Theme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let presets: Vec<(&'static Preset, bool)> = {
            let store = self.store.read(cx);
            ids.iter()
                .filter_map(|id| preset::get(id))
                .filter(|preset| Self::matches(query, preset))
                .map(|preset| (preset, store.has_preset(&preset.id)))
                .collect()
        };
        if presets.is_empty() {
            return None;
        }
        let cards: Vec<Stateful<Div>> = presets
            .into_iter()
            .map(|(preset, added)| {
                let logo =
                    data::preset_logo(&preset.id).unwrap_or_else(|| Logo::letter(&preset.name));
                self.card(
                    ElementId::Name(format!("preset-{}", preset.id).into()),
                    logo,
                    preset.name.clone().into(),
                    host(preset).to_string().into(),
                    added,
                    t,
                    window,
                    cx,
                )
                .on_click(cx.listener(move |_, _, _, cx| cx.emit(AddEvent::Chosen(Some(preset)))))
            })
            .collect();
        Some(grid(cards))
    }

    /// 账号登录：装了的、有官方登录的 agent（Claude Code、Codex、Grok Build）各一张。点了回到供应商页官方那一组，
    /// 不起登录：账号跟着 agent 自己的登录走（10-13 用户说的：在官方应用里登录才放心）。
    fn logins(
        &self,
        query: &str,
        t: &Theme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let entries: Vec<(usize, Logo, SharedString, SharedString)> = {
            let store = self.store.read(cx);
            store
                .officials()
                .into_iter()
                .map(|i| {
                    let agent = &store.agents[i];
                    (
                        i,
                        official::logo(store, i),
                        official::name(agent.kind),
                        SharedString::from(agent.name),
                    )
                })
                .filter(|(_, _, name, sub)| {
                    query.is_empty()
                        || name.to_lowercase().contains(query)
                        || sub.to_lowercase().contains(query)
                })
                .collect()
        };
        if entries.is_empty() {
            return None;
        }
        let cards = entries
            .into_iter()
            .map(|(agent, logo, name, sub)| {
                self.card(
                    ElementId::named_usize("login", agent),
                    logo,
                    name,
                    sub,
                    false,
                    t,
                    window,
                    cx,
                )
                .on_click(cx.listener(move |_, _, _, cx| cx.emit(AddEvent::Official)))
            })
            .collect();
        Some(grid(cards))
    }
}

/// 一组：标题和下面的东西。
fn section(title: SharedString, t: &Theme) -> Div {
    div().v_flex().gap(px(8.)).child(
        div()
            .text_size(px(11.5))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(t.muted)
            .child(title),
    )
}

/// 组里再分的一小块（模型厂商的国内、海外）：小字标题。
fn part(title: SharedString, t: &Theme, body: Div) -> Div {
    div()
        .v_flex()
        .gap(px(6.))
        .child(div().text_size(px(10.5)).text_color(t.faint).child(title))
        .child(body)
}

/// 卡片第二行的主机名：第一个接口地址的。
fn host(preset: &Preset) -> &str {
    preset
        .endpoints
        .values()
        .next()
        .and_then(|url| confdeck_core::provider::host(url))
        .unwrap_or_default()
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
        let query = self.query(cx);
        let mut body = div()
            .id("add-body")
            .v_flex()
            .gap(px(16.))
            .px(px(20.))
            .py(px(16.))
            .flex_1()
            .min_h_0()
            .overflow_y_scroll();

        // 自定义单独一张，占满一行、虚线框，搜索时也在。
        body = body.child(
            self.card(
                "custom".into(),
                Logo::Letter('+'),
                t!("add.custom").into(),
                t!("add.custom_hint").into(),
                false,
                &t,
                window,
                cx,
            )
            .border_dashed()
            .border_color(t.faint.opacity(0.45))
            .on_click(cx.listener(|_, _, _, cx| cx.emit(AddEvent::Chosen(None)))),
        );

        let logins = self.logins(&query, &t, window, cx);
        let cn = self.presets(VENDORS_CN, &query, &t, window, cx);
        let intl = self.presets(VENDORS_INTL, &query, &t, window, cx);
        let platforms = self.presets(PLATFORMS, &query, &t, window, cx);
        let empty = logins.is_none() && cn.is_none() && intl.is_none() && platforms.is_none();
        if let Some(logins) = logins {
            body = body.child(section(t!("add.login").into(), &t).child(logins));
        }
        if cn.is_some() || intl.is_some() {
            body = body.child(
                section(t!("add.vendors").into(), &t)
                    .gap(px(10.))
                    .children(cn.map(|cn| part(t!("add.domestic").into(), &t, cn)))
                    .children(intl.map(|intl| part(t!("add.overseas").into(), &t, intl))),
            );
        }
        if let Some(platforms) = platforms {
            body = body.child(section(t!("add.platforms").into(), &t).child(platforms));
        }
        if empty {
            body = body.child(
                div()
                    .py(px(12.))
                    .text_size(px(12.5))
                    .text_color(t.muted)
                    .text_center()
                    .child(t!("add.empty")),
            );
        }

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

    use super::{PLATFORMS, VENDORS_CN, VENDORS_INTL};

    /// 每个预设都要分到一组，不然卡片表里看不到它。
    #[test]
    fn every_preset_is_in_exactly_one_group() {
        for preset in preset::all() {
            let groups = [VENDORS_CN, VENDORS_INTL, PLATFORMS]
                .iter()
                .filter(|group| group.contains(&preset.id.as_str()))
                .count();
            assert_eq!(groups, 1, "{} should be in exactly one group", preset.id);
        }
        for id in VENDORS_CN.iter().chain(VENDORS_INTL).chain(PLATFORMS) {
            assert!(preset::get(id).is_some(), "{id} is not a preset");
        }
    }
}
