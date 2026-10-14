//! 一行字，放不下时在末尾截掉、加「…」。用法：外面的 div 照常定字号、颜色，里面放 `ui::one_line(文字)`，不用再写 `.truncate()`。
//!
//! GPUI 的 `.truncate()` 把每个字单独排出来的宽度加起来，定截在哪；macOS 排整行时会在中文和英文、空格、标点之间多留一点间距，
//! 单个字量不出来。中文多的长句按加起来的宽度截完，实际排开还是更宽，省略号和最后几个字被裁在外面。
//! 这里按整行实际排出来的位置找截断点，截完再排一遍核对，放不下就往前退一个字。

use gpui_kit::*;

const ELLIPSIS: &str = "…";
/// 排出来比可用宽度宽不到这么多也算放得下：布局取整会差一点。
const SLACK: Pixels = px(0.5);

pub fn one_line(text: impl Into<SharedString>) -> OneLine {
    let text = text.into();
    // 换行、制表符排成一行时当空格。
    let text = if text.contains(['\n', '\r', '\t']) {
        text.replace(['\n', '\r', '\t'], " ").into()
    } else {
        text
    };
    OneLine { text }
}

pub struct OneLine {
    text: SharedString,
}

pub struct Measured {
    run: TextRun,
    font_size: Pixels,
    line_height: Pixels,
}

/// 排好的一行：截的时候要用的两样。
trait Shaped {
    fn width(&self) -> Pixels;
    /// 盖住横坐标 `x` 的那个字从第几个字节开始；`x` 超出整行时 `None`。
    fn index_for_x(&self, x: Pixels) -> Option<usize>;
}

impl Shaped for ShapedLine {
    fn width(&self) -> Pixels {
        self.width
    }

    fn index_for_x(&self, x: Pixels) -> Option<usize> {
        (**self).index_for_x(x)
    }
}

/// 放得下 `width` 的样子：整行放得下就原样，放不下就截掉末尾、加「…」。`shape` 把一段字排成一行。
fn fit<L: Shaped>(
    text: &SharedString,
    width: Pixels,
    mut shape: impl FnMut(SharedString) -> L,
) -> L {
    let full = shape(text.clone());
    if full.width() <= width + SLACK {
        return full;
    }
    let ellipsis = shape(ELLIPSIS.into()).width();
    let mut end = full.index_for_x(width - ellipsis).unwrap_or(0);
    loop {
        // 和 GPUI 一样，截断处的空白和英文标点不留。
        let kept =
            text[..end].trim_end_matches(|c: char| c.is_whitespace() || c.is_ascii_punctuation());
        let line = shape(format!("{kept}{ELLIPSIS}").into());
        if kept.is_empty() || line.width() <= width + SLACK {
            return line;
        }
        end = kept.char_indices().next_back().map_or(0, |(i, _)| i);
    }
}

impl Element for OneLine {
    type RequestLayoutState = Measured;
    type PrepaintState = ShapedLine;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        _: &mut App,
    ) -> (LayoutId, Measured) {
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.pixel_snap(
            style
                .line_height
                .to_pixels(font_size.into(), window.rem_size()),
        );
        let run = style.to_run(self.text.len());
        let text = self.text.clone();
        let full_run = run.clone();
        let layout_id =
            window.request_measured_layout(Style::default(), move |known, available, window, _| {
                let full = window
                    .text_system()
                    .shape_line(
                        text.clone(),
                        font_size,
                        std::slice::from_ref(&full_run),
                        None,
                    )
                    .width
                    .ceil();
                let width = known.width.unwrap_or(match available.width {
                    AvailableSpace::Definite(width) => full.min(width),
                    // 能截到一个字不剩，外面的 flex 可以把它压窄，不用另写 `min_w_0`。
                    AvailableSpace::MinContent => Pixels::ZERO,
                    AvailableSpace::MaxContent => full,
                });
                size(width, known.height.unwrap_or(line_height))
            });
        (
            layout_id,
            Measured {
                run,
                font_size,
                line_height,
            },
        )
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        measured: &mut Measured,
        window: &mut Window,
        _: &mut App,
    ) -> ShapedLine {
        fit(&self.text, bounds.size.width, |text| {
            let run = TextRun {
                len: text.len(),
                ..measured.run.clone()
            };
            window
                .text_system()
                .shape_line(text, measured.font_size, &[run], None)
        })
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        measured: &mut Measured,
        line: &mut ShapedLine,
        window: &mut Window,
        cx: &mut App,
    ) {
        // 和 GPUI 的文字元素一样画：先底色，再字。画不出来（字形栅格化失败）这一行就空着。
        let align = window.text_style().text_align;
        let width = Some(bounds.size.width);
        let _ = line.paint_background(
            bounds.origin,
            measured.line_height,
            align,
            width,
            window,
            cx,
        );
        let _ = line.paint(
            bounds.origin,
            measured.line_height,
            align,
            width,
            window,
            cx,
        );
    }
}

impl IntoElement for OneLine {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{Pixels, SharedString, px};

    use super::{Shaped, fit};

    /// 假的排版：每个字 10 宽，中文和非中文挨着的地方多 2（macOS 排整行时会这样，单个字量不出来）。
    struct Line {
        text: String,
        /// 每个字的开头在哪。
        starts: Vec<(usize, Pixels)>,
        width: Pixels,
    }

    fn shape(text: SharedString) -> Line {
        let wide = |c: char| c > '\u{2e80}';
        let mut x = px(0.);
        let mut starts = Vec::new();
        let mut prev: Option<char> = None;
        for (i, c) in text.char_indices() {
            if prev.is_some_and(|p| wide(p) != wide(c)) {
                x += px(2.);
            }
            starts.push((i, x));
            x += px(10.);
            prev = Some(c);
        }
        Line {
            text: text.to_string(),
            starts,
            width: x,
        }
    }

    impl Shaped for Line {
        fn width(&self) -> Pixels {
            self.width
        }

        fn index_for_x(&self, x: Pixels) -> Option<usize> {
            if x >= self.width {
                return None;
            }
            Some(
                self.starts
                    .iter()
                    .rev()
                    .find(|(_, start)| *start <= x)
                    .map_or(0, |(i, _)| *i),
            )
        }
    }

    fn cut(text: &str, width: f32) -> (String, Pixels) {
        let line = fit(&SharedString::from(text.to_string()), px(width), shape);
        (line.text, line.width)
    }

    #[test]
    fn text_that_fits_is_left_alone() {
        assert_eq!(cut("替换agent", 72.), ("替换agent".into(), px(72.)));
    }

    #[test]
    fn the_cut_text_with_its_ellipsis_fits() {
        // 逐字加是 120 宽，排开是 142。按逐字加的宽度截（GPUI 的做法）会留 8 个字加「…」，排开 104，放不下 100。
        assert_eq!(
            cut("中a中a中a中a中a中a", 100.),
            ("中a中a中a中…".into(), px(94.))
        );
    }

    #[test]
    fn steps_back_when_the_ellipsis_adds_spacing() {
        // 留 9 个字时「…」前面多出一处中英交界，排开 102，退一个字。
        assert_eq!(
            cut("中文中文中文中文中文中文", 100.),
            ("中文中文中文中文…".into(), px(92.))
        );
    }

    #[test]
    fn trailing_spaces_and_punctuation_are_dropped_at_the_cut() {
        assert_eq!(cut("ab, cdefgh", 50.).0, "ab…");
    }

    #[test]
    fn too_narrow_for_anything_leaves_the_ellipsis() {
        assert_eq!(cut("云沙箱调研", 5.).0, "…");
    }
}
