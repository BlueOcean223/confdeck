//! 额度和余额怎么画（10-13 从用量页挪出来，用户说它们属于供应商）：供应商页每一行（官方的和各家的）、官方卡片里的账号、
//! 菜单栏面板里每个 agent 选的那家。查和存在 `readings.rs`，这里只画它给的 [`Reading`]。
//! 供应商页上能查的每一行，读数后面一个圆形箭头：点了重查这一个，查着的时候转（10-13 用户：显示了的也要能手动刷新）。
//! 关掉查询的那一格空着、没有箭头；不自动查、还没查过的只有箭头。

use chrono::{Local, TimeZone as _};
use confdeck_core::balance::{Balance, BalanceError, Currency};
use confdeck_core::quota::{CheckKey, Quota, QuotaError, Span, Window};
use gpui_kit::component::*;
use gpui_kit::*;
use rust_i18n::t;

use crate::readings::Readings;
use crate::theme::Theme;
use crate::ui;

/// 一家的读数：查的是谁、在不在查、查到了什么。
pub struct Reading<'a> {
    pub of: CheckKey,
    /// 正在查：箭头转。
    pub checking: bool,
    pub shown: Shown<'a>,
}

/// 查到了什么。
pub enum Shown<'a> {
    /// 自动查的、还没有读数（到点就查，打开页面时也查）。
    Waiting,
    /// 关掉了查询：这一格空着。
    Off,
    /// 不自动查、还没查过：只有箭头。
    Unchecked,
    Quota(&'a Quota),
    Balance(&'a Balance),
    QuotaFailed(&'a QuotaError),
    BalanceFailed(&'a BalanceError),
}

/// 一行里最多画几个窗口。Claude 是 5 小时、每周、Fable 三个。
pub const MAX_WINDOWS: usize = 3;
/// 供应商页一行里一个窗口的宽度、窗口之间的空隙。
const CELL: f32 = 86.;
const CELL_GAP: f32 = 14.;
/// 三个窗口的宽度。
const METERS: f32 = CELL * MAX_WINDOWS as f32 + CELL_GAP * (MAX_WINDOWS as f32 - 1.);
/// 读数后面的圆形箭头占多宽、离读数多远。
const ARROW: f32 = 26.;
const ARROW_GAP: f32 = 6.;
/// 供应商页额度那一栏的宽度：三个窗口加箭头。
pub const COLUMN: f32 = METERS + ARROW_GAP + ARROW;

/// 供应商页一行右边的那一块：读数（[`meters`]），后面一个圆形箭头。宽度是 [`COLUMN`]，各行的格子、箭头上下对齐。
/// 箭头点了经 `readings` 重查这一个。关掉查询的整栏空着。
pub fn cells(t: &Theme, id: &str, reading: &Reading, readings: &Entity<Readings>) -> Div {
    let column = div()
        .flex_none()
        .w(px(COLUMN))
        .h_flex()
        .items_center()
        .gap(px(ARROW_GAP));
    if matches!(reading.shown, Shown::Off) {
        return column;
    }
    column
        .child(meters(t, id, &reading.shown))
        .child(refresh(t, id, reading, readings))
}

/// 读数：每个窗口一格（名字和百分比、细进度条，悬停说几点重置），或者「余额 ¥86.40」，或者一句状态。宽度是三个窗口。
pub fn meters(t: &Theme, id: &str, shown: &Shown) -> Div {
    let column = div().flex_none().w(px(METERS)).h_flex().items_center();
    let now = Local::now().timestamp_millis();
    match shown {
        Shown::Off | Shown::Unchecked => column,
        Shown::Waiting => column.gap(px(CELL_GAP)).children((0..2).map(|_| {
            div()
                .w(px(CELL))
                .v_flex()
                .gap(px(6.))
                .child(div().w(px(44.)).h(px(10.)).rounded(px(3.)).bg(t.pill))
                .child(div().h(px(3.)).rounded(px(2.)).bg(t.pill))
        })),
        Shown::Quota(found) if found.windows.is_empty() => {
            column.child(status(t, id, &QuotaError::NoPlan))
        }
        Shown::Quota(found) => column.gap(px(CELL_GAP)).children(
            found
                .windows
                .iter()
                .take(MAX_WINDOWS)
                .enumerate()
                .map(|(i, w)| cell(t, &format!("{id}-{i}"), w, now)),
        ),
        Shown::Balance(found) => column.child(money(t, found, 13.)),
        Shown::QuotaFailed(err) => column.child(status(t, id, err)),
        Shown::BalanceFailed(err) => column.child(balance_status(t, id, err)),
    }
}

/// 圆形箭头：点了重查这一个，查着的时候转。
fn refresh(t: &Theme, id: &str, reading: &Reading, readings: &Entity<Readings>) -> Stateful<Div> {
    let readings = readings.clone();
    let target = reading.of.clone();
    let checking = reading.checking;
    div()
        .id(ElementId::Name(format!("check-{id}").into()))
        .flex_none()
        .size(px(ARROW))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .cursor_pointer()
        .hover(|s| s.bg(t.pill))
        .tooltip(ui::tooltip(t!("quota.check_now")))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            if !checking {
                readings.update(cx, |readings, cx| readings.check_one(&target, cx));
            }
        })
        .child(ui::spinning(
            ui::icon("rotate-cw", 13., t.muted),
            ElementId::Name(format!("check-spin-{id}").into()),
            checking,
        ))
}

/// 没有额度可写的行：空出 [`COLUMN`] 那么宽，右边的东西和别的行对齐。
pub fn blank() -> Div {
    div().flex_none().w(px(COLUMN))
}

/// 菜单栏面板 agent 名字下面的一行：「5 小时 ▬ 23%  每周 ▬ 41%」或「余额 ¥86.40」，查不到的写一句。
pub fn line(t: &Theme, id: &str, reading: &Reading) -> Div {
    let row = div()
        .h_flex()
        .items_center()
        .gap(px(12.))
        .h(px(14.))
        .text_size(px(11.));
    let now = Local::now().timestamp_millis();
    match &reading.shown {
        Shown::Off | Shown::Unchecked => row,
        Shown::Waiting => row.child(div().w(px(120.)).h(px(8.)).rounded(px(3.)).bg(t.pill)),
        Shown::Quota(found) if found.windows.is_empty() => {
            row.child(status(t, id, &QuotaError::NoPlan))
        }
        Shown::Quota(found) => row.children(
            found
                .windows
                .iter()
                .take(MAX_WINDOWS)
                .enumerate()
                .map(|(i, w)| {
                    let (used, _) = shown(w, now);
                    let full = used >= 100.;
                    div()
                        .id(ElementId::Name(format!("{id}-{i}").into()))
                        .flex_none()
                        .h_flex()
                        .items_center()
                        .gap(px(5.))
                        .text_color(t.muted)
                        .child(span_label(&w.span, false))
                        .child(bar(t, used, 24.))
                        .child(
                            div()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(if full { t.danger } else { t.fg_2 })
                                .child(format!("{used:.0}%")),
                        )
                        .tooltip(ui::tooltip(tip(w, now)))
                }),
        ),
        Shown::Balance(found) => row.child(money(t, found, 11.5)),
        Shown::QuotaFailed(err) => row.child(status(t, id, err)),
        Shown::BalanceFailed(err) => row.child(balance_status(t, id, err)),
    }
}

/// 一个窗口的一格：上面名字和百分比，下面 3px 的进度条。到 100% 变红。
fn cell(t: &Theme, id: &str, w: &Window, now: i64) -> Stateful<Div> {
    let (used, _) = shown(w, now);
    let full = used >= 100.;
    div()
        .id(ElementId::Name(format!("quota-{id}").into()))
        .flex_none()
        .w(px(CELL))
        .v_flex()
        .gap(px(5.))
        .child(
            div()
                .h_flex()
                .items_baseline()
                .gap(px(4.))
                .text_size(px(11.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(t.muted)
                        .child(span_label(&w.span, false)),
                )
                .child(
                    div()
                        .flex_none()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(if full { t.danger } else { t.fg })
                        .child(format!("{used:.0}%")),
                ),
        )
        .child(bar(t, used, CELL))
        .tooltip(ui::tooltip(tip(w, now)))
}

/// 用了多少、还没过的重置时间。查过以后已经过了重置时间的算 0%（同 CCometixLine），不等下次查。
fn shown(w: &Window, now: i64) -> (f64, Option<i64>) {
    let reset = w.resets_at.filter(|at| *at > now);
    let used = match w.resets_at {
        Some(_) if reset.is_none() => 0.,
        _ => w.used.max(0.),
    };
    (used, reset)
}

fn bar(t: &Theme, used: f64, width: f32) -> Div {
    let full = used >= 100.;
    div()
        .flex_none()
        .w(px(width))
        .h(px(3.))
        .rounded(px(2.))
        .bg(t.pill)
        .child(
            div()
                .h_full()
                .w(relative((used / 100.).min(1.) as f32))
                .rounded(px(2.))
                .bg(if full { t.danger } else { t.accent }),
        )
}

/// 悬停：「每周 · 已用 41% · 10月20日 04:59 重置（6 天后）」。
fn tip(w: &Window, now: i64) -> String {
    let (used, reset) = shown(w, now);
    let mut tip = format!(
        "{} · {}",
        span_label(&w.span, true),
        t!("quota.used", percent = format!("{used:.0}"))
    );
    if let Some(at) = reset.and_then(|at| Local.timestamp_millis_opt(at).earliest()) {
        let when = if rust_i18n::locale().starts_with("zh") {
            at.format("%-m月%-d日 %H:%M").to_string()
        } else {
            at.format("%b %-d, %H:%M").to_string()
        };
        tip.push_str(" · ");
        tip.push_str(&t!("quota.resets_at", when = when));
        tip.push_str(&format!(" ({})", until_text(at.timestamp_millis() - now)));
    }
    tip
}

/// 「¥86.40」；OpenRouter 没设上限的 Key 只知道用了多少：「已用 $12.34」。用完了变红。
fn money(t: &Theme, found: &Balance, size: f32) -> Div {
    let (label, amount) = match found.left {
        Some(left) => (t!("quota.balance"), left),
        None => (t!("quota.spent"), found.used.unwrap_or(0.)),
    };
    let sign = match found.currency {
        Currency::Cny => "¥",
        Currency::Usd => "$",
        Currency::Plain => "",
    };
    div()
        .h_flex()
        .items_baseline()
        .gap(px(6.))
        .child(
            div()
                .text_size(px(size - 1.5))
                .text_color(t.muted)
                .child(label),
        )
        .child(
            div()
                .text_size(px(size))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(if found.left.is_some() && amount <= 0. {
                    t.danger
                } else {
                    t.fg
                })
                .child(format!("{sign}{amount:.2}")),
        )
}

/// 查额度出错时的一句：登录过期、Key 不对写红字，没有套餐、没登录写灰字，别的写「—」悬停说原因。
fn status(t: &Theme, id: &str, err: &QuotaError) -> AnyElement {
    let (text, color): (SharedString, Hsla) = match err {
        QuotaError::Expired => (t!("quota.expired").into(), t.danger),
        QuotaError::Auth => (t!("quota.key_rejected").into(), t.danger),
        QuotaError::NoPlan => (t!("quota.none").into(), t.muted),
        QuotaError::NotLoggedIn => (t!("quota.signed_out").into(), t.muted),
        QuotaError::Net(err) => return unknown(t, id, crate::net::describe(err)),
        QuotaError::Parse(_) => return unknown(t, id, t!("quota.unknown").into()),
    };
    div()
        .text_size(px(12.))
        .text_color(color)
        .child(text)
        .into_any_element()
}

fn balance_status(t: &Theme, id: &str, err: &BalanceError) -> AnyElement {
    match err {
        BalanceError::Auth => div()
            .text_size(px(12.))
            .text_color(t.danger)
            .child(t!("quota.key_rejected"))
            .into_any_element(),
        BalanceError::Net(err) => unknown(t, id, crate::net::describe(err)),
        _ => unknown(t, id, t!("quota.unknown").into()),
    }
}

/// 查不到：「—」，悬停说为什么。
fn unknown(t: &Theme, id: &str, why: SharedString) -> AnyElement {
    div()
        .id(ElementId::Name(format!("reading-error-{id}").into()))
        .text_size(px(13.))
        .text_color(t.faint)
        .tooltip(ui::tooltip(why))
        .child("—")
        .into_any_element()
}

/// 窗口的名字。某个模型的每周格子里只写模型名（「Fable」），`full` 时写全（「每周 · Fable」）。
fn span_label(span: &Span, full: bool) -> String {
    match span {
        Span::FiveHours => t!("quota.span_5h").to_string(),
        Span::Week => t!("quota.span_week").to_string(),
        Span::Month => t!("quota.span_month").to_string(),
        Span::WeekOf(model) if full => format!("{} · {model}", t!("quota.span_week")),
        Span::WeekOf(model) => model.clone(),
        Span::Minutes(minutes) if minutes % 1440 == 0 => {
            t!("quota.span_days", count = minutes / 1440).to_string()
        }
        Span::Minutes(minutes) => t!("quota.span_hours", count = minutes.div_ceil(60)).to_string(),
        Span::Period => t!("quota.span_period").to_string(),
    }
}

/// 「3 小时后」：一小时内按分钟，两天内按小时，再长按天。
fn until_text(millis: i64) -> String {
    let minutes = (millis / 60_000).max(1);
    if minutes < 60 {
        t!("quota.in_minutes", count = minutes).to_string()
    } else if minutes < 48 * 60 {
        t!("quota.in_hours", count = minutes / 60).to_string()
    } else {
        t!("quota.in_days", count = minutes / 1440).to_string()
    }
}
