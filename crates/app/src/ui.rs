//! 按 magpie 的样式画的通用部件。尺寸取自 magpie 的 app.css，注释里标了对应的类名。

use std::rc::Rc;
use std::time::Duration;

use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::data::Logo;
use crate::theme::Theme;

pub fn logo(logo: Logo, size: f32, t: &Theme) -> AnyElement {
    if logo.colored {
        img(logo.path).size(px(size)).flex_none().into_any_element()
    } else {
        svg()
            .path(logo.path)
            .size(px(size))
            .flex_none()
            .text_color(t.fg)
            .into_any_element()
    }
}

/// Lucide 图标（gpui-kit 自带）。
pub fn icon(name: &str, size: f32, color: Hsla) -> Svg {
    svg()
        .path(format!("icons/{name}.svg"))
        .size(px(size))
        .flex_none()
        .text_color(color)
}

/// 圆角卡片，里面是一行行的条目（`.list`）。
pub fn list(t: &Theme, panel: bool) -> Div {
    let list = div().v_flex().w_full().flex_none().bg(t.card);
    if panel {
        list.rounded(px(10.)).shadow(t.card_edge())
    } else {
        list.rounded(px(12.)).border_1().border_color(t.line)
    }
}

/// 卡片里的一行（`.row`），第一行以外顶部画分隔线。
pub fn row(t: &Theme, panel: bool, first: bool) -> Div {
    let row = div().h_flex().items_center().w_full();
    let row = if panel {
        row.min_h(px(40.)).gap(px(9.)).pl(px(11.)).pr(px(9.)).py(px(5.))
    } else {
        row.min_h(px(48.)).gap(px(12.)).pl(px(14.)).pr(px(12.)).py(px(7.))
    };
    if first {
        row
    } else {
        row.border_t_1().border_color(t.line_2)
    }
}

/// 下拉框（magpie 的 `.field`）：图标、文字、向下的箭头。打开时描一圈强调色。
/// 面板里的更像系统的弹出按钮：白底、细描边、一点阴影。
pub fn select_pill(
    id: impl Into<ElementId>,
    t: &Theme,
    panel: bool,
    logo: Logo,
    label: impl Into<SharedString>,
    open: bool,
) -> Stateful<Div> {
    let hover = t.pill_hover;
    let edge = if open {
        t.accent
    } else if panel {
        t.pill_line
    } else {
        gpui_kit::transparent_black()
    };
    div()
        .id(id)
        .h_flex()
        .items_center()
        .gap(px(5.))
        .h(px(if panel { 24. } else { 25. }))
        .pl(px(8.))
        .pr(px(5.))
        .rounded(px(6.))
        .border_1()
        .border_color(edge)
        .bg(if open { t.pill_hover } else { t.pill })
        .when(panel, |pill| pill.shadow(t.pill_shadow()))
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .child(self::logo(logo, 14., t))
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_size(px(if panel { 12. } else { 12.5 }))
                .font_weight(FontWeight::MEDIUM)
                .child(label.into()),
        )
        .child(div().flex_1())
        .child(icon("chevron-down", 10., if open { t.muted } else { t.faint }).ml(px(1.)))
}

/// 开关（magpie 的 `.lib-switch`）：关时轨道是分隔线色，开时是文字色，圆钮是卡片色。
/// `animate` 为真时圆钮从另一头滑过来，只在刚被点过的那个开关上用。
pub fn switch(id: impl Into<ElementId>, t: &Theme, on: bool, animate: Option<u64>) -> Stateful<Div> {
    const TRAVEL: f32 = 13.;
    let knob = div()
        .absolute()
        .top(px(2.))
        .size(px(15.))
        .rounded_full()
        .bg(if on { t.card } else { gpui_kit::white() })
        .shadow(vec![BoxShadow {
            color: gpui_kit::black().opacity(0.25),
            offset: point(px(0.), px(1.)),
            blur_radius: px(2.),
            spread_radius: px(0.),
            inset: false,
        }]);
    let rest = if on { 2. + TRAVEL } else { 2. };
    let knob = match animate {
        Some(n) => knob
            .with_animation(
                ElementId::NamedInteger("knob".into(), n),
                Animation::new(Duration::from_millis(220)).with_easing(ease_out_quart),
                move |knob, delta| {
                    let from = if on { 2. } else { 2. + TRAVEL };
                    knob.left(px(from + (rest - from) * delta))
                },
            )
            .into_any_element(),
        None => knob.left(px(rest)).into_any_element(),
    };
    div()
        .id(id)
        .relative()
        .flex_none()
        .w(px(32.))
        .h(px(19.))
        .rounded_full()
        .bg(if on { t.fg } else { t.line })
        .cursor_pointer()
        .child(knob)
}

/// magpie 动画常用的减速曲线（cubic-bezier(.16, 1, .3, 1) 的近似）。
pub fn ease_out_quart(t: f32) -> f32 {
    1. - (1. - t).powi(4)
}

/// 把浮层挂在父元素正下方、右边对齐，画在其他内容之上。父元素要是 `relative()`。
pub fn dropdown(content: impl IntoElement) -> Div {
    div().absolute().top_full().right_0().child(
        deferred(
            anchored()
                .anchor(Anchor::TopRight)
                .snap_to_window_with_margin(px(8.))
                .child(div().pt(px(4.)).child(content)),
        )
        .with_priority(1),
    )
}

/// 浮层里的分组标题（`.pop li.group`）。
pub fn group_label(t: &Theme, label: &'static str) -> Div {
    div()
        .h(px(24.))
        .pt(px(8.))
        .px(px(8.))
        .text_size(px(10.5))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(t.faint)
        .child(label)
}

/// 顶栏中间的分段标签（`.seg`）：没有灰色轨道，只有选中项下面垫一块白色滑块。
/// 切换时滑块从上一个标签滑过来；`from` 是上一个标签的下标和这次切换的编号。
pub fn seg<T: Copy + PartialEq + 'static>(
    t: &Theme,
    items: &[(T, &'static str)],
    active: T,
    from: Option<(usize, u64)>,
    window: &mut Window,
    on_select: impl Fn(&T, &mut Window, &mut App) + 'static,
) -> Div {
    const FONT: f32 = 12.5;
    const PAD: f32 = 12.;
    // 先量出每个标签的宽度，才知道滑块该在哪、有多宽。
    let font = window.text_style().font();
    let widths: Vec<f32> = items
        .iter()
        .map(|(_, label)| {
            let run = TextRun {
                len: label.len(),
                font: font.clone(),
                color: t.fg,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let line = window
                .text_system()
                .shape_line((*label).into(), px(FONT), &[run], None);
            f32::from(line.width).ceil() + PAD * 2.
        })
        .collect();
    let left_of = |n: usize| 2. + widths[..n].iter().sum::<f32>();
    let active_ix = items.iter().position(|(v, _)| *v == active).unwrap_or(0);
    let (to_left, to_width) = (left_of(active_ix), widths[active_ix]);

    let thumb = div()
        .absolute()
        .top(px(2.))
        .h(px(24.))
        .rounded(px(6.))
        .bg(t.seg_thumb)
        .shadow(t.seg_edge());
    let thumb = match from {
        Some((from_ix, n)) if from_ix != active_ix => {
            let (from_left, from_width) = (left_of(from_ix), widths[from_ix]);
            thumb
                .with_animation(
                    ElementId::NamedInteger("seg-thumb".into(), n),
                    Animation::new(Duration::from_millis(260)).with_easing(ease_out_quart),
                    move |thumb, d| {
                        thumb
                            .left(px(from_left + (to_left - from_left) * d))
                            .w(px(from_width + (to_width - from_width) * d))
                    },
                )
                .into_any_element()
        }
        _ => thumb.left(px(to_left)).w(px(to_width)).into_any_element(),
    };

    let on_select = Rc::new(on_select);
    div()
        .relative()
        .h_flex()
        .p(px(2.))
        .rounded(px(8.))
        .child(thumb)
        .children(items.iter().enumerate().map(|(i, &(value, label))| {
            let on = value == active;
            let on_select = on_select.clone();
            let hover = t.fg_2;
            div()
                .id(ElementId::named_usize("seg", i))
                .w(px(widths[i]))
                .h(px(24.))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(FONT))
                .text_color(if on { t.fg } else { t.ctl_fg })
                .when(!on, |b| b.hover(move |s| s.text_color(hover)))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(move |_, window, cx| on_select(&value, window, cx))
                .child(label)
        }))
}

/// 没有底色的文字按钮，悬停时变深（`.text`）。
pub fn text_button(id: impl Into<ElementId>, t: &Theme) -> Stateful<Div> {
    let hover = t.fg;
    div()
        .id(id)
        .h_flex()
        .items_center()
        .gap(px(4.))
        .text_size(px(12.5))
        .text_color(t.ctl_fg)
        .cursor_pointer()
        .hover(move |s| s.text_color(hover))
}

/// 顶栏右边的图标按钮。
pub fn icon_button(id: impl Into<ElementId>, t: &Theme, name: &str) -> Stateful<Div> {
    let hover = t.pill;
    div()
        .id(id)
        .size(px(28.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(icon(name, 16., t.ctl_fg))
}
