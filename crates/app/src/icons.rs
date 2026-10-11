//! 彩色品牌图标按显示尺寸精确栅格化，按（路径，像素）缓存。
//!
//! GPUI 的 `img()` 画 SVG 时按文件的固有尺寸乘 2 栅格化：LobeHub 的图标写的是 `1em`，算 12px，
//! 得到 24px 的位图，再拉到 Retina 上 22pt（44px）的格子里，边缘发虚。单色图标走 `svg()`，
//! 是按实际像素画的，所以清楚。这里给彩色图标也按实际像素画一次。PNG 同理：解码后用 Lanczos
//! 缩到目标尺寸；交给 GPU 的话它只做双线性采样，256px 缩到 44px 会把细线条采丢。

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use gpui_kit::*;

/// 键是（资源路径，边长像素），值是渲染结果；渲染失败也记下，不反复重试。
type Entry = Option<Arc<RenderImage>>;

#[derive(Default)]
struct Cache(RefCell<HashMap<(&'static str, u32), Entry>>);

impl Global for Cache {}

/// 启动时调一次。
pub fn init(cx: &mut App) {
    cx.set_global(Cache::default());
}

/// `path` 相对 assets/，和 `img()` 一样；`size` 是显示尺寸（点）。
/// 渲染不了就返回 None，调用方退回 `img(path)`。
pub fn raster(path: &'static str, size: Pixels, window: &Window, cx: &App) -> Entry {
    let device = (f32::from(size) * window.scale_factor()).round().max(1.) as u32;
    let cache = cx.global::<Cache>();
    if let Some(hit) = cache.0.borrow().get(&(path, device)) {
        return hit.clone();
    }
    let image = render(path, device, cx);
    cache.0.borrow_mut().insert((path, device), image.clone());
    image
}

fn render(path: &str, device: u32, cx: &App) -> Entry {
    let bytes = cx.asset_source().load(path).ok().flatten()?;
    if path.ends_with(".svg") {
        let renderer = cx.svg_renderer();
        let tree = renderer.parse_svg(&bytes).ok()?;
        let side = DevicePixels(device as i32);
        renderer
            .render_parsed(&tree, SvgSize::ExactSize(size(side, side)))
            .ok()
    } else {
        let decoded = image::load_from_memory(&bytes).ok()?.into_rgba8();
        let mut resized = image::imageops::resize(
            &decoded,
            device,
            device,
            image::imageops::FilterType::Lanczos3,
        );
        // GPUI 对解码出的位图只做 RGBA → BGRA，这里照它的来。
        for pixel in resized.pixels_mut() {
            pixel.0.swap(0, 2);
        }
        Some(Arc::new(RenderImage::new(vec![image::Frame::new(resized)])))
    }
}
