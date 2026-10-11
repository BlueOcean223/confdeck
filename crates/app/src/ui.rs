//! 按 magpie 的样式画的通用部件。尺寸取自 magpie 的 app.css，注释里标了对应的类名。

use std::rc::Rc;
use std::time::Duration;

use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use gpui_kit::component::input::{Input, InputState};

use crate::data::Logo;
use crate::icons;
use crate::theme::Theme;

pub fn logo(logo: Logo, size: f32, t: &Theme, window: &Window, cx: &App) -> AnyElement {
    match logo {
        Logo::Color(path) => {
            // 按实际像素栅格化（见 icons.rs），画不了就让 GPUI 自己画。
            let source: ImageSource = match icons::raster(path, px(size), window, cx) {
                Some(image) => image.into(),
                None => path.into(),
            };
            img(source).size(px(size)).flex_none().into_any_element()
        }
        Logo::Mono(path) => svg()
            .path(path)
            .size(px(size))
            .flex_none()
            .text_color(t.fg)
            .into_any_element(),
        // 还没有图标的供应商：首字装在小方块里。
        Logo::Letter(ch) => div()
            .flex_none()
            .size(px(size))
            .rounded(px((size * 0.27).round()))
            .bg(t.pill)
            .border_1()
            .border_color(t.line)
            .flex()
            .items_center()
            .justify_center()
            .text_size(px((size * 0.5).round()))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(t.fg_2)
            .child(ch.to_string())
            .into_any_element(),
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
        row.min_h(px(40.))
            .gap(px(9.))
            .pl(px(11.))
            .pr(px(9.))
            .py(px(5.))
    } else {
        row.min_h(px(48.))
            .gap(px(12.))
            .pl(px(14.))
            .pr(px(12.))
            .py(px(7.))
    };
    if first {
        row
    } else {
        row.border_t_1().border_color(t.line_2)
    }
}

/// 下拉框（magpie 的 `.field`）：图标、文字、向下的箭头。打开时描一圈强调色。
/// 面板里的更像系统的弹出按钮：白底、细描边、一点阴影。
/// `logos` 是前面的图标：一个就直接画，几个就叠起来（多选 agent 勾了哪几家），见 [`logo_stack`]。
// 图标和文字一起传进来，拆成结构体也还是这些东西。
#[allow(clippy::too_many_arguments)]
pub fn select_pill(
    id: impl Into<ElementId>,
    t: &Theme,
    panel: bool,
    logos: &[Logo],
    label: impl Into<SharedString>,
    open: bool,
    dashed: bool,
    window: &Window,
    cx: &App,
) -> Stateful<Div> {
    let hover = t.pill_hover;
    let edge = if open {
        t.accent
    } else if panel || dashed {
        t.pill_line
    } else {
        gpui_kit::transparent_black()
    };
    // 「自定义」：虚线边框、不垫底色，一眼看出不是 confdeck 写的（预览 09 的 `.pill.custom`）。
    let bg = if open {
        t.pill_hover
    } else if dashed {
        gpui_kit::transparent_black()
    } else {
        t.pill
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
        .when(dashed, |pill| pill.border_dashed())
        .bg(bg)
        .when(panel, |pill| pill.shadow(t.pill_shadow()))
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .child(match logos {
            [logo] => self::logo(*logo, 14., t, window, cx),
            logos => logo_stack(logos, t, window, cx).into_any_element(),
        })
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

/// 叠起来的图标（多选 agent 的胶囊）：最多三个，每个装在一个小圆角方块里，往左叠四分之一，
/// 后面的压在前面的上面。几家都写进了配置，看图标就知道个大概，全名在胶囊的文字里。
/// 容器定宽、方块绝对定位：用负边距叠的话 Taffy 会把容器算成零宽，文字压到图标上。
pub fn logo_stack(logos: &[Logo], t: &Theme, window: &Window, cx: &App) -> Div {
    const CHIP: f32 = 18.;
    const STEP: f32 = 13.;
    let shown = &logos[..logos.len().min(3)];
    let width = CHIP + STEP * shown.len().saturating_sub(1) as f32;
    div()
        .relative()
        .flex_none()
        .w(px(width))
        .h(px(CHIP))
        .children(shown.iter().enumerate().map(|(i, &logo)| {
            div()
                .absolute()
                .top_0()
                .left(px(STEP * i as f32))
                .size(px(CHIP))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.))
                .bg(t.card)
                .border_1()
                .border_color(t.line)
                .child(self::logo(logo, 12., t, window, cx))
        }))
}

/// 勾选框（多选浮层每行前面的）：勾上是强调色底、白勾，没勾是细描边。
pub fn checkbox(t: &Theme, checked: bool) -> Div {
    let dark = t.dark;
    div()
        .flex_none()
        .size(px(14.))
        .rounded(px(4.))
        .flex()
        .items_center()
        .justify_center()
        .when(checked, |b| {
            b.bg(t.accent).child(icon(
                "check",
                10.,
                if dark { t.bg } else { gpui_kit::white() },
            ))
        })
        .when(!checked, |b| b.border_1().border_color(t.faint))
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
pub fn group_label(t: &Theme, label: impl Into<SharedString>) -> Div {
    div()
        .h(px(24.))
        .pt(px(8.))
        .px(px(8.))
        .text_size(px(10.5))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(t.faint)
        .child(label.into())
}

/// 顶栏中间的分段标签（`.seg`）：没有灰色轨道，只有选中项下面垫一块白色滑块。
/// 切换时滑块从上一个标签滑过来；`from` 是上一个标签的下标和这次切换的编号。
pub fn seg<T: Copy + PartialEq + 'static>(
    t: &Theme,
    items: &[(T, SharedString)],
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
                .shape_line(label.clone(), px(FONT), &[run], None);
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
        .children(items.iter().enumerate().map(|(i, (value, label))| {
            let value = *value;
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
                .child(label.clone())
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

/// 按钮的几种样子。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Button {
    /// 强调色底：保存、确认。
    Primary,
    /// 灰底：取消、次要操作。
    Secondary,
    /// 红字无底：删除。
    Danger,
}

/// 弹层里的按钮（magpie 的 `.btn`）：高 28，圆角 7。
pub fn button(id: impl Into<ElementId>, t: &Theme, kind: Button) -> Stateful<Div> {
    let on_accent = if t.dark { t.bg } else { gpui_kit::white() };
    let (bg, fg, hover_bg) = match kind {
        Button::Primary => (t.accent, on_accent, t.accent.opacity(0.85)),
        Button::Secondary => (t.pill, t.fg, t.pill_hover),
        Button::Danger => (
            gpui_kit::transparent_black(),
            t.danger,
            t.danger.opacity(0.1),
        ),
    };
    div()
        .id(id)
        .h_flex()
        .items_center()
        .justify_center()
        .gap(px(5.))
        .h(px(28.))
        .px(px(12.))
        .rounded(px(7.))
        .bg(bg)
        .text_size(px(12.5))
        .font_weight(FontWeight::MEDIUM)
        .text_color(fg)
        .cursor_pointer()
        .hover(move |s| s.bg(hover_bg))
}

/// 表单里的输入框：自己画的框（底色、细描边、聚焦时强调色），里面放 gpui-component 的 `Input`。
/// `suffix` 放在右边（Key 的「显示」按钮等）。
pub fn field(
    t: &Theme,
    state: &Entity<InputState>,
    mono: bool,
    suffix: Option<AnyElement>,
    window: &Window,
    cx: &App,
) -> Div {
    let focused = state.read(cx).focus_handle(cx).is_focused(window);
    div()
        .h_flex()
        .items_center()
        .gap(px(6.))
        .h(px(30.))
        .px(px(9.))
        .rounded(px(7.))
        .bg(t.field)
        .border_1()
        .border_color(if focused { t.accent } else { t.line })
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(12.5))
                .when(mono, |d| d.font_family("Menlo"))
                .child(
                    Input::new(state)
                        .appearance(false)
                        .xsmall()
                        .text_size(px(12.5)),
                ),
        )
        .children(suffix)
}

/// 悬停提示。用在 `.tooltip(ui::tooltip(text))`。
pub fn tooltip(text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyView {
    let text = text.into();
    move |window, cx| gpui_kit::component::tooltip::Tooltip::new(text.clone()).build(window, cx)
}

/// 小芯片（在用的 agent：图标 + 名字 + 模型名）。
pub fn chip(t: &Theme) -> Div {
    div()
        .h_flex()
        .items_center()
        .gap(px(5.))
        .h(px(24.))
        .pl(px(6.))
        .pr(px(8.))
        .rounded(px(6.))
        .bg(t.pill)
        .text_size(px(11.5))
}

/// 表单一行的标签（左边定宽）。
pub fn label(t: &Theme, text: impl Into<SharedString>) -> Div {
    // 和 `field` 一样高、文字居中，这样行里 items_start 时标签正对着输入框。
    div()
        .flex_none()
        .w(px(128.))
        .h(px(30.))
        .flex()
        .items_center()
        .text_size(px(12.5))
        .text_color(t.fg_2)
        .child(text.into())
}

/// 字段下面的小字说明。
pub fn hint(t: &Theme, text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(11.))
        .text_color(t.muted)
        .child(text.into())
}

/// 没有图标的下拉按钮（配置页里选模型）：文字和向下的箭头，打开时描一圈强调色。
pub fn menu_pill(
    id: impl Into<ElementId>,
    t: &Theme,
    label: impl Into<SharedString>,
    open: bool,
) -> Stateful<Div> {
    let hover = t.pill_hover;
    div()
        .id(id)
        .h_flex()
        .items_center()
        .gap(px(5.))
        .h(px(25.))
        .pl(px(8.))
        .pr(px(5.))
        .rounded(px(6.))
        .border_1()
        .border_color(if open {
            t.accent
        } else {
            gpui_kit::transparent_black()
        })
        .bg(if open { t.pill_hover } else { t.pill })
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .child(
            div()
                .min_w_0()
                .truncate()
                .font_family("Menlo")
                .text_size(px(12.))
                .child(label.into()),
        )
        .child(div().flex_1())
        .child(icon("chevron-down", 10., if open { t.muted } else { t.faint }).ml(px(1.)))
}

/// 单选点（默认模型）：选中是强调色圈加实心点，没选是细灰圈。
pub fn radio(t: &Theme, on: bool) -> Div {
    div()
        .flex_none()
        .size(px(14.))
        .rounded_full()
        .border_1()
        .border_color(if on { t.accent } else { t.faint })
        .flex()
        .items_center()
        .justify_center()
        .when(on, |r| {
            r.child(div().size(px(7.)).rounded_full().bg(t.accent))
        })
}

/// 卡片顶上的标题行（配置页：「模型」「原始文件」），右边可以放一个动作。
pub fn card_title(t: &Theme, title: impl Into<SharedString>) -> Div {
    div()
        .h_flex()
        .items_center()
        .gap(px(8.))
        .h(px(42.))
        .pl(px(14.))
        .pr(px(12.))
        .text_size(px(12.5))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(t.fg)
        .child(title.into())
}

/// 开关（设置页）：轨道 30×18，圆点 14，开了是强调色。
pub fn toggle(t: &Theme, on: bool) -> Div {
    let knob = if t.dark { t.fg } else { gpui_kit::white() };
    div()
        .flex_none()
        .w(px(30.))
        .h(px(18.))
        .rounded_full()
        .p(px(2.))
        .flex()
        .items_center()
        .bg(if on { t.accent } else { t.faint })
        .when(on, |track| track.justify_end())
        .child(div().size(px(14.)).rounded_full().bg(knob).shadow_sm())
}
