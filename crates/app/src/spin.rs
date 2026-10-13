//! 转圈的箭头（在查额度、在刷新）画在盖在窗口上的一层（[`SpinLayer`]），不画在页面里（arch-perf R7）。
//!
//! GPUI 的动画每一帧通知它所在的 view，这个 view 和外面各层 view 都要重画，最外层的 view 每帧都重画；
//! 箭头画在供应商页里的话，查着的时候整页每帧都重画。所以：
//! - 页面是缓存着的 view（`main_window.rs` 的 `PageView`），没变就沿用上一帧；
//! - 要转的箭头在原处只占个位（[`marker`]），排版时记下位置和裁剪范围；
//! - [`SpinLayer`] 自己一个 view，按记下的位置画转着的箭头、要下一帧。动画只让它和窗口最外层重画。
//!
//! 各处按同一个起点算角度，一起转。

use std::collections::HashMap;
use std::f32::consts::TAU;
use std::time::{Duration, Instant};

use gpui_kit::*;

/// 转一圈多久（和 `ui::spinning` 一样）。
const PERIOD: Duration = Duration::from_millis(900);

/// 记下的箭头是谁画的：这个 view 重画时先清掉自己那组（[`clear`]），排版时再记。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Group {
    /// 窗口最外层（顶栏的刷新）。
    Window,
    /// 页面（供应商页各行的箭头）。
    Page,
}

#[derive(Clone)]
struct Spin {
    bounds: Bounds<Pixels>,
    /// 占位时的裁剪范围（页面滚动区）。
    clip: Bounds<Pixels>,
    path: SharedString,
    color: Hsla,
}

#[derive(Default)]
struct Spins {
    /// 角度从这时算起。
    since: Option<Instant>,
    marked: HashMap<(WindowId, Group), Vec<Spin>>,
}

impl Global for Spins {}

/// `group` 要重画了：清掉它记下的箭头。
pub fn clear(group: Group, window: &Window, cx: &mut App) {
    let key = (window.window_handle().window_id(), group);
    cx.default_global::<Spins>().marked.remove(&key);
}

/// 转着的箭头占的位：和 `ui::icon(name, size, color)` 一样大，自己不画，排版时把位置记给 [`SpinLayer`]。
/// 只能用在有 [`SpinLayer`] 的窗口里（现在只有主窗口）：别的窗口记下的没人清，也画不出来。
pub fn marker(group: Group, name: &str, size: f32, color: Hsla) -> impl IntoElement {
    let path: SharedString = format!("icons/{name}.svg").into();
    canvas(
        move |bounds, window, cx| {
            let key = (window.window_handle().window_id(), group);
            let spin = Spin {
                bounds,
                clip: window.content_mask().bounds,
                path,
                color,
            };
            cx.default_global::<Spins>()
                .marked
                .entry(key)
                .or_default()
                .push(spin);
        },
        |_, _, _, _| {},
    )
    .size(px(size))
    .flex_none()
}

/// 盖在窗口上的一层：画记下的箭头，有箭头就要下一帧。
pub struct SpinLayer;

impl Render for SpinLayer {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        canvas(
            |_, window, cx| {
                let id = window.window_handle().window_id();
                let spins = cx.default_global::<Spins>();
                let marked: Vec<Spin> = spins
                    .marked
                    .iter()
                    .filter(|((window, _), _)| *window == id)
                    .flat_map(|(_, marked)| marked.iter().cloned())
                    .collect();
                if marked.is_empty() {
                    return (marked, 0.);
                }
                let since = *spins.since.get_or_insert_with(Instant::now);
                let elapsed = since.elapsed().as_nanos() % PERIOD.as_nanos();
                window.request_animation_frame();
                (marked, elapsed as f32 / PERIOD.as_nanos() as f32)
            },
            |_, (marked, turn), window, cx| {
                let scale = window.scale_factor();
                for spin in marked {
                    let center = spin.bounds.center();
                    let matrix = TransformationMatrix::unit()
                        .translate(center.scale(scale))
                        .rotate(radians(turn * TAU))
                        .translate(center.scale(-scale));
                    window.with_content_mask(Some(ContentMask { bounds: spin.clip }), |window| {
                        window
                            .paint_svg(spin.bounds, spin.path, None, matrix, spin.color, cx)
                            .ok();
                    });
                }
            },
        )
        .absolute()
        .size_full()
    }
}
