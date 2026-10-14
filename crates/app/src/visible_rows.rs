//! 一列等高的行，只排版、只画落在可见范围里的那几行。整列照总行数撑开高度，外面的页面照常滚动。
//!
//! GPUI 的 `uniform_list` 自己管滚动，放进会滚的页面里就得把页面拆成「上面不动、列表单独滚」；
//! 这里只看父元素裁出来的可见范围（`content_mask`），页面怎么滚、滚动条都不变。
//! 一行多高由调用的地方定死，行自己要 `.h(..)` 成同样的高度；宽度不用写，每行外面套了一层和整列一样宽的。

use std::ops::Range;

use gpui_kit::*;

/// 可见范围上下各多排几行：取整、边线差一点也不会在边上露出空白。
const OVERSCAN: usize = 2;

/// 给出一段下标（从 0 数）的行，按顺序。
type Render = dyn Fn(Range<usize>, &mut Window, &mut App) -> Vec<AnyElement>;

/// `count` 行、每行 `height` 高，行由 `render` 给出。
pub fn visible_rows(
    count: usize,
    height: Pixels,
    render: impl Fn(Range<usize>, &mut Window, &mut App) -> Vec<AnyElement> + 'static,
) -> VisibleRows {
    VisibleRows {
        count,
        height,
        render: Box::new(render),
    }
}

pub struct VisibleRows {
    count: usize,
    height: Pixels,
    render: Box<Render>,
}

impl VisibleRows {
    /// 和 `bounds` 里可见的那段相交的行（加上下各 [`OVERSCAN`] 行）。
    fn range(&self, bounds: Bounds<Pixels>, visible: Bounds<Pixels>) -> Range<usize> {
        let visible = bounds.intersect(&visible);
        if self.count == 0 || visible.size.height <= Pixels::ZERO {
            return 0..0;
        }
        let first = ((visible.top() - bounds.top()) / self.height).floor() as usize;
        let last = ((visible.bottom() - bounds.top()) / self.height).ceil() as usize;
        first.saturating_sub(OVERSCAN).min(self.count)..(last + OVERSCAN).min(self.count)
    }
}

impl Element for VisibleRows {
    type RequestLayoutState = ();
    type PrepaintState = Vec<AnyElement>;

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
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = (self.height * self.count).into();
        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Vec<AnyElement> {
        let range = self.range(bounds, window.content_mask().bounds);
        // 单独排版的行宽度按内容算，套一层定宽的竖排容器把它撑满。
        let mut rows: Vec<AnyElement> = (self.render)(range.clone(), window, cx)
            .into_iter()
            .map(|row| {
                div()
                    .flex()
                    .flex_col()
                    .w(bounds.size.width)
                    .child(row)
                    .into_any_element()
            })
            .collect();
        let space = size(
            AvailableSpace::Definite(bounds.size.width),
            AvailableSpace::Definite(self.height),
        );
        for (row, index) in rows.iter_mut().zip(range) {
            row.layout_as_root(space, window, cx);
            row.prepaint_at(
                point(bounds.left(), bounds.top() + self.height * index),
                window,
                cx,
            );
        }
        rows
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        rows: &mut Vec<AnyElement>,
        window: &mut Window,
        cx: &mut App,
    ) {
        for row in rows {
            row.paint(window, cx);
        }
    }
}

impl IntoElement for VisibleRows {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{Bounds, point, px, size};

    use super::visible_rows;

    fn bounds(top: f32, height: f32) -> Bounds<gpui_kit::Pixels> {
        Bounds::new(point(px(0.), px(top)), size(px(100.), px(height)))
    }

    #[test]
    fn only_rows_in_view_and_a_margin_are_drawn() {
        let rows = visible_rows(100, px(10.), |_, _, _| Vec::new());
        // 整列 1000 高、从 50 开始；看得见的是窗口里 0 到 200：第 0 到 15 行，再往下多 2 行。
        assert_eq!(rows.range(bounds(50., 1000.), bounds(0., 200.)), 0..17);
        // 往下滚了 300：列表顶在 -250，看得见的是第 25 到 45 行，上下各多 2 行。
        assert_eq!(rows.range(bounds(-250., 1000.), bounds(0., 200.)), 23..47);
        // 滚到底：最后一行到 100 为止。
        assert_eq!(rows.range(bounds(-900., 1000.), bounds(0., 200.)), 88..100);
        // 整列都在看不见的地方。
        assert_eq!(rows.range(bounds(300., 1000.), bounds(0., 200.)), 0..0);
        assert_eq!(
            visible_rows(0, px(10.), |_, _, _| Vec::new()).range(bounds(0., 0.), bounds(0., 200.)),
            0..0
        );
    }
}
