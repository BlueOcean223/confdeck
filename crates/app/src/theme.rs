//! 配色照搬 magpie（internal/gui/assets/app.css 里的 CSS 变量），字段名与变量名对应。
//! 主窗口和菜单栏面板各有一套：面板里的卡片、下拉框更亮，分隔线是半透明的。

use gpui_kit::*;

#[derive(Clone, Copy)]
pub struct Theme {
    pub dark: bool,
    pub bg: Hsla,
    pub card: Hsla,
    pub card_2: Hsla,
    pub pill: Hsla,
    pub pill_hover: Hsla,
    pub pill_line: Hsla,
    pub seg_thumb: Hsla,
    pub ctl_fg: Hsla,
    pub line: Hsla,
    pub line_2: Hsla,
    pub fg: Hsla,
    pub fg_2: Hsla,
    pub muted: Hsla,
    pub faint: Hsla,
    pub accent: Hsla,
    pub sel: Hsla,
    pub pop_bg: Hsla,
    /// 删除、出错。
    pub danger: Hsla,
    /// 保存成功的提示。
    pub success: Hsla,
    /// 弹层编辑器底下盖住列表的那层。
    pub backdrop: Hsla,
    /// 输入框的底色。
    pub field: Hsla,
}

fn hex(c: u32) -> Hsla {
    rgb(c).into()
}

/// `0xRRGGBBAA`
fn hexa(c: u32) -> Hsla {
    rgba(c).into()
}

fn is_dark(appearance: WindowAppearance) -> bool {
    matches!(
        appearance,
        WindowAppearance::Dark | WindowAppearance::VibrantDark
    )
}

impl Theme {
    pub fn window(appearance: WindowAppearance) -> Self {
        if is_dark(appearance) {
            Self::dark()
        } else {
            Self::light()
        }
    }

    pub fn panel(appearance: WindowAppearance) -> Self {
        if is_dark(appearance) {
            Self {
                bg: hex(0x1b1c20),
                card: hex(0x25262c),
                card_2: hex(0x212227),
                line: hexa(0xffffff12),
                line_2: hexa(0xffffff0f),
                pill: hex(0x2e2f36),
                pill_hover: hex(0x383942),
                pill_line: hexa(0xffffff1c),
                muted: hex(0x9c9ca6),
                faint: hex(0x7c7c87),
                ..Self::dark()
            }
        } else {
            Self {
                bg: hex(0xf2f2f5),
                card: hex(0xffffff),
                card_2: hex(0xf8f8fa),
                line: hexa(0x00000012),
                line_2: hexa(0x0000000f),
                pill: hex(0xffffff),
                pill_hover: hex(0xf4f4f7),
                pill_line: hexa(0x0000001f),
                faint: hex(0x9a9aa3),
                ..Self::light()
            }
        }
    }

    fn light() -> Self {
        Self {
            dark: false,
            bg: hex(0xf4f4f6),
            card: hex(0xffffff),
            card_2: hex(0xf9f9fb),
            pill: hex(0xf1f1f4),
            pill_hover: hex(0xe9e9ee),
            pill_line: hex(0xe3e3e8),
            seg_thumb: hex(0xffffff),
            ctl_fg: hex(0x63636b),
            line: hex(0xe3e3e8),
            line_2: hex(0xececf0),
            fg: hex(0x1c1c21),
            fg_2: hex(0x4b4b53),
            muted: hex(0x85858d),
            faint: hex(0xb9b9c2),
            accent: hex(0x4f46e5),
            sel: hex(0xecebfb),
            pop_bg: hexa(0xfffffff0),
            danger: hex(0xdc2626),
            success: hex(0x15803d),
            backdrop: hexa(0x1c1c2140),
            field: hex(0xffffff),
        }
    }

    fn dark() -> Self {
        Self {
            dark: true,
            bg: hex(0x1a1a1e),
            card: hex(0x232327),
            card_2: hex(0x1f1f23),
            pill: hex(0x2d2d33),
            pill_hover: hex(0x36363d),
            pill_line: hex(0x333339),
            seg_thumb: hexa(0xffffff26),
            ctl_fg: hex(0xa1a1aa),
            line: hex(0x333339),
            line_2: hex(0x2c2c32),
            fg: hex(0xededf1),
            fg_2: hex(0xb9b9c1),
            muted: hex(0x8b8b94),
            faint: hex(0x56565f),
            accent: hex(0xa5b4fc),
            sel: hex(0x33345a),
            pop_bg: hexa(0x26262bf5),
            danger: hex(0xf87171),
            success: hex(0x4ade80),
            backdrop: hexa(0x00000080),
            field: hex(0x1c1c20),
        }
    }

    /// 分段标签里选中项的阴影（`--seg-edge`）。
    pub fn seg_edge(&self) -> Vec<BoxShadow> {
        if self.dark {
            vec![
                shadow(hexa(0xffffff14), 0., 0.5, 0., 0., true),
                shadow(hexa(0x0000004d), 0., 1., 2., 0., false),
            ]
        } else {
            vec![
                shadow(hexa(0x00000024), 0., 1., 2., 0., false),
                shadow(hexa(0x00000014), 0., 0., 0., 0.5, false),
            ]
        }
    }

    /// 面板里卡片的描边阴影（`--card-edge`）。
    pub fn card_edge(&self) -> Vec<BoxShadow> {
        if self.dark {
            vec![shadow(hexa(0xffffff0f), 0., 0., 0., 1., true)]
        } else {
            vec![
                shadow(hexa(0x0000000d), 0., 0., 0., 1., false),
                shadow(hexa(0x0000000a), 0., 1., 2., 0., false),
            ]
        }
    }

    /// 浮层（下拉菜单）的阴影（`--shadow`）。
    pub fn pop_shadow(&self) -> Vec<BoxShadow> {
        if self.dark {
            vec![
                shadow(hexa(0x0000008c), 0., 16., 44., 0., false),
                shadow(hexa(0x0000004d), 0., 2., 8., 0., false),
                shadow(hexa(0xffffff1a), 0., 0., 0., 0.5, false),
            ]
        } else {
            vec![
                shadow(hexa(0x14161e29), 0., 16., 44., 0., false),
                shadow(hexa(0x14161e0f), 0., 2., 8., 0., false),
                shadow(hexa(0x14161e1a), 0., 0., 0., 0.5, false),
            ]
        }
    }

    /// 下拉框的细阴影（`--pill-shadow`，只在面板里用）。
    pub fn pill_shadow(&self) -> Vec<BoxShadow> {
        if self.dark {
            vec![shadow(hexa(0x00000047), 0., 1., 2., 0., false)]
        } else {
            vec![shadow(hexa(0x0000000f), 0., 1., 1., 0., false)]
        }
    }
}

/// 过滤框用的是 gpui-component 的输入框，它的文字、占位符、光标和选区颜色取自
/// gpui-component 的全局主题，这里换成 magpie 的配色。
pub fn sync_component_theme(t: &Theme, cx: &mut App) {
    use gpui_kit::component::ThemeMode;
    // 代码编辑器（「原始文件」）的底色、行号栏、着色跟着 gpui-component 的明暗模式走，先切到和窗口一样的。
    let mode = if t.dark {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    };
    if gpui_kit::component::Theme::global(cx).mode != mode {
        gpui_kit::component::Theme::change(mode, None, cx);
    }
    let theme = gpui_kit::component::Theme::global_mut(cx);
    theme.foreground = t.fg;
    theme.muted_foreground = t.faint;
    theme.caret = t.fg;
    theme.selection = t.sel;
}

fn shadow(color: Hsla, x: f32, y: f32, blur: f32, spread: f32, inset: bool) -> BoxShadow {
    BoxShadow {
        color,
        offset: point(px(x), px(y)),
        blur_radius: px(blur),
        spread_radius: px(spread),
        inset,
    }
}
