//! 「额度查询」：一家供应商、一个官方账号的额度或余额查不查、多久自动查一次（10-13 用户要的：按供应商配置，
//! 参考 cc-switch 的「配置用量查询」，界面自己画）。供应商行、官方账号行的「⋯」里打开，和编辑器一样盖在页面上。
//!
//! - 「查询」开关：关了这一行那一格空着，点刷新也不查。
//! - 「自动查询」：不自动、1 / 5 / 15 / 30 分钟、1 小时。没设过的显示默认（在用的 5 分钟，不在用的不自动），
//!   没动它就还是默认，保存时不写。
//! - 「现在」一行是这家现在的读数，右边一个圆形箭头马上查一次（不用先保存），查着的时候转。查询关着时这一行变淡、箭头藏起来，
//!   卡片高矮不变（不然「保存」会挪位置）。

use confdeck_core::quota::Check;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::data::{Logo, Store};
use crate::quota_view;
use crate::theme::Theme;
use crate::ui::{self, Button};
use crate::usage::CheckOf;

/// 卡片的宽度：「自动查询」一行放得下六档。
const WIDTH: f32 = 540.;

/// 「自动查询」的几档（分钟），0 是不自动。
const EVERY: [u32; 6] = [0, 1, 5, 15, 30, 60];

pub enum CheckEvent {
    Closed,
}

pub struct CheckSheet {
    store: Entity<Store>,
    target: CheckOf,
    name: SharedString,
    logo: Logo,
    on: bool,
    every: u32,
    /// 动过「自动查询」：保存时写下来；没动过就留着原来的（没设过的还是默认）。
    touched: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<CheckEvent> for CheckSheet {}

impl CheckSheet {
    pub fn new(
        store: Entity<Store>,
        target: CheckOf,
        name: SharedString,
        logo: Logo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (check, in_use) = {
            let store = store.read(cx);
            (store.check_of(&target), store.in_use(&target))
        };
        // Esc 关掉。
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
                sheet.update(cx, |_, cx| cx.emit(CheckEvent::Closed));
                cx.stop_propagation();
            }
        });
        Self {
            _subscriptions: vec![cx.observe(&store, |_, _, cx| cx.notify()), intercept],
            store,
            target,
            name,
            logo,
            on: !check.off,
            every: check
                .every
                .unwrap_or(if in_use { Check::DEFAULT_EVERY } else { 0 }),
            touched: false,
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let old = self.store.read(cx).check_of(&self.target);
        let check = Check {
            off: !self.on,
            every: if self.touched {
                Some(self.every)
            } else {
                old.every
            },
        };
        let target = self.target.clone();
        self.store
            .update(cx, |store, cx| store.set_check(&target, check, cx));
        cx.emit(CheckEvent::Closed);
    }

    fn header(&self, t: &Theme, window: &Window, cx: &Context<Self>) -> Div {
        div()
            .h_flex()
            .flex_none()
            .items_center()
            .gap(px(10.))
            .px(px(20.))
            .h(px(60.))
            .border_b_1()
            .border_color(t.line_2)
            .child(ui::logo(self.logo, 24., t, window, cx))
            .child(
                div()
                    .v_flex()
                    .min_w_0()
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(t!("check.title")),
                    )
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(t.muted)
                            .truncate()
                            .child(self.name.clone()),
                    ),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("close-check")
                    .size(px(26.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .child(ui::icon("x", 15., t.muted))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(CheckEvent::Closed))),
            )
    }

    /// 一行：左边名字，右边控件。
    fn line(t: &Theme, label: SharedString, control: impl IntoElement) -> Div {
        div()
            .h_flex()
            .items_center()
            .gap(px(12.))
            .min_h(px(32.))
            .child(
                div()
                    .w(px(80.))
                    .flex_none()
                    .flex_shrink_0()
                    .text_size(px(13.))
                    .text_color(t.fg)
                    .child(label),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_flex()
                    .justify_end()
                    .child(control),
            )
    }
}

impl Render for CheckSheet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = Theme::window(window.appearance());
        let on = self.on;
        let items: Vec<(u32, SharedString)> = EVERY
            .iter()
            .map(|&every| {
                let label: SharedString = match every {
                    0 => t!("check.never").into(),
                    60 => t!("check.hour").into(),
                    n => t!("check.minutes", n = n).into(),
                };
                (every, label)
            })
            .collect();
        let entity = cx.entity().downgrade();
        let seg = ui::seg(&t, &items, self.every, None, window, move |every, _, cx| {
            let every = *every;
            if let Some(sheet) = entity.upgrade() {
                sheet.update(cx, |sheet, cx| {
                    sheet.every = every;
                    sheet.touched = true;
                    cx.notify();
                });
            }
        });

        // 没查过的那一格空着：右边的箭头就是查。
        let (cells, checking) = {
            let store = self.store.read(cx);
            let reading = match &self.target {
                CheckOf::Provider(id) => store
                    .providers
                    .iter()
                    .find(|p| &p.id == id)
                    .and_then(|p| store.provider_reading(p)),
                CheckOf::Account(kind, _) if store.in_use(&self.target) => {
                    store.official_reading(*kind)
                }
                CheckOf::Account(kind, id) => Some(store.account_reading(*kind, id)),
                CheckOf::Login(kind) => store.official_reading(*kind),
            };
            let cells = match &reading {
                None | Some(quota_view::Reading::Unchecked { .. }) => quota_view::blank(),
                Some(reading) => quota_view::cells(&t, "check-now", reading, &self.store),
            };
            (cells, store.checking_target(&self.target))
        };
        let now = div()
            .h_flex()
            .items_center()
            .gap(px(8.))
            .child(cells)
            .child(
                ui::icon_button_with(
                    "check-run",
                    &t,
                    ui::spinning(
                        ui::icon("rotate-cw", 15., t.ctl_fg),
                        "check-run-spin",
                        checking,
                    ),
                )
                .when(!on, |b| b.invisible())
                .tooltip(ui::tooltip(t!("check.run")))
                .on_click(cx.listener(|this, _, _, cx| {
                    let target = this.target.clone();
                    if !this.on || this.store.read(cx).checking_target(&target) {
                        return;
                    }
                    this.store
                        .update(cx, |store, cx| store.check_one(&target, cx));
                })),
            );

        div()
            .v_flex()
            .w(px(WIDTH))
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
                    .v_flex()
                    .gap(px(12.))
                    .px(px(20.))
                    .py(px(16.))
                    .child(Self::line(
                        &t,
                        t!("check.enabled").into(),
                        div()
                            .id("check-on")
                            .cursor_pointer()
                            .child(ui::toggle(&t, on))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.on = !this.on;
                                cx.notify();
                            })),
                    ))
                    .child(Self::line(
                        &t,
                        t!("check.every").into(),
                        div()
                            .rounded(px(8.))
                            .bg(t.pill)
                            .child(seg)
                            .when(!on, |d| d.opacity(0.4)),
                    ))
                    // 关掉时也留着这一行（变淡），开关来回切时卡片不变高矮。
                    .child(
                        Self::line(&t, t!("check.now").into(), now).when(!on, |d| d.opacity(0.4)),
                    ),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(20.))
                    .h(px(56.))
                    .border_t_1()
                    .border_color(t.line_2)
                    .child(div().flex_1())
                    .child(
                        ui::button("check-cancel", &t, Button::Secondary)
                            .child(t!("common.cancel"))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(CheckEvent::Closed))),
                    )
                    .child(
                        ui::button("check-save", &t, Button::Primary)
                            .child(t!("common.save"))
                            .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    ),
            )
    }
}
