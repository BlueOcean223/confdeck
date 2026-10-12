//! GPUI 没提供、只能直接调 AppKit 的窗口操作。

use gpui_kit::{App, Pixels, Point, Window};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{Message, msg_send};
use objc2_app_kit::{NSScreen, NSView, NSWindow};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

fn ns_window(window: &Window) -> Option<Retained<NSWindow>> {
    gpui_view(window)?.window()
}

/// GPUI 画内容的那个 view（`GPUIView`，在窗口 content view 里面）。
fn gpui_view(window: &Window) -> Option<Retained<NSView>> {
    let handle = HasWindowHandle::window_handle(window).ok()?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return None;
    };
    // SAFETY: GPUI 给的是它自己的 view，窗口存活期间一直有效；retain 一份，任务里拿着也安全。
    let view: &NSView = unsafe { handle.ns_view.cast().as_ref() };
    Some(view.retain())
}

/// 把窗口内容（GPUI 画的内容和它下面的毛玻璃）裁成圆角，并按新形状重算窗口阴影。
/// GPUI 的毛玻璃铺满整个矩形窗口，不裁的话四个角会露出直角。
pub fn round_corners(window: &Window, radius: f64) {
    let Some(ns_window) = ns_window(window) else {
        return;
    };
    let Some(content) = ns_window.contentView() else {
        return;
    };
    content.setWantsLayer(true);
    // SAFETY: wantsLayer 之后 content view 一定有 layer（CALayer），这两个都是它的标准属性。
    unsafe {
        let layer: *mut AnyObject = msg_send![&*content, layer];
        if !layer.is_null() {
            let _: () = msg_send![layer, setCornerRadius: radius];
            let _: () = msg_send![layer, setMasksToBounds: true];
        }
    }
    ns_window.invalidateShadow();
}

/// 藏起窗口：马上变透明，下一轮主线程任务再 `orderOut`（不在 AppKit 的回调里同步改 key 窗口）。
/// 面板关掉时不销毁窗口，只藏起来，下次 `show_at` 再用：销毁时 GPUI 会释放窗口的 content view，
/// AppKit 的 TouchBar 观察者要是还没挪走，下一次刷新就会对已经没了的 view 解注册，抛 `NSRangeException`
/// 让整个应用崩溃（notes/panel-focus-crash.md）。
pub fn hide(window: &Window, cx: &App) {
    let Some(ns_window) = ns_window(window) else {
        return;
    };
    ns_window.makeFirstResponder(None);
    ns_window.setAlphaValue(0.);
    cx.foreground_executor()
        .spawn(async move { ns_window.orderOut(None) })
        .detach();
}

/// 把 `hide` 藏起来的窗口挪到 `origin`（和 GPUI 开窗口时一样，以主屏左上角为原点）、
/// 内容高 `height`，显示出来并成为 key 窗口。窗口还是透明的：里面还是上次藏起来时那一帧，
/// 等 GPUI 按现在的状态画完一帧再 `reveal`。
pub fn show_at(window: &Window, origin: Point<Pixels>, height: f64, cx: &App) {
    let Some(ns_window) = ns_window(window) else {
        return;
    };
    cx.foreground_executor()
        .spawn(async move {
            let Some(mtm) = MainThreadMarker::new() else {
                return;
            };
            let Some(screen) = NSScreen::screens(mtm).firstObject() else {
                return;
            };
            let screen = screen.frame();
            let frame = ns_window.frame();
            let content = ns_window.contentRectForFrameRect(frame);
            let width = frame.size.width;
            let height = height + frame.size.height - content.size.height;
            let top = screen.origin.y + screen.size.height - f64::from(f32::from(origin.y));
            let next = NSRect::new(
                NSPoint::new(
                    screen.origin.x + f64::from(f32::from(origin.x)),
                    top - height,
                ),
                NSSize::new(width, height),
            );
            ns_window.setFrame_display(next, true);
            ns_window.makeKeyAndOrderFront(None);
        })
        .detach();
}

/// `show_at` 之后，GPUI 画好新的一帧了，窗口变回不透明。
pub fn reveal(window: &Window) {
    if let Some(ns_window) = ns_window(window) {
        ns_window.setAlphaValue(1.);
        ns_window.invalidateShadow();
    }
}

/// 改窗口高度，顶边不动、向下长。GPUI 的 `Window::resize` 固定的是左下角，
/// 面板会往上长、顶到菜单栏上面去。
///
/// 和 `Window::resize` 一样放到下一轮主线程任务里改：AppKit 在 `setFrame` 里会同步回调
/// GPUI 的 resize，而调用这个函数时 GPUI 正在处理事件、`App` 被借着，回调拿不到 `App`，
/// 这次尺寸变化就被丢掉了：窗口变高了，GPUI 却还按旧高度布局。
///
/// 改完马上让 GPUI 同步画一帧（`displayIfNeeded` 会走到 GPUI 的 `displayLayer:`，和窗口尺寸在同一个
/// CA 事务里提交）。不这样的话，中间有一帧窗口已经变高、里面还是旧画面，被系统缩放着铺满新尺寸，
/// 看着就是整个面板闪一下。
pub fn set_height_keep_top(window: &Window, height: f64, cx: &App) {
    let Some(view) = gpui_view(window) else {
        return;
    };
    let Some(ns_window) = view.window() else {
        return;
    };
    cx.foreground_executor()
        .spawn(async move {
            if !ns_window.isVisible() {
                return;
            }
            let frame = ns_window.frame();
            let content = ns_window.contentRectForFrameRect(frame);
            let new_height = height + frame.size.height - content.size.height;
            let top = frame.origin.y + frame.size.height;
            let mut next = frame;
            next.size.height = new_height;
            next.origin.y = top - new_height;
            ns_window.setFrame_display(next, true);
            view.setNeedsDisplay(true);
            view.displayIfNeeded();
            ns_window.invalidateShadow();
        })
        .detach();
}

/// 窗口在窗口服务器里的编号（`screencapture -l` 用的就是它）。
#[cfg(debug_assertions)]
#[cfg_attr(feature = "shots", allow(dead_code))]
pub fn window_number(window: &Window) -> Option<isize> {
    ns_window(window).map(|w| w.windowNumber())
}

/// 开发时强制整个应用用深色或浅色（`dark` / `light`），不改系统设置。
/// 跑 drive 脚本时用 `CONFDECK_APPEARANCE` 指定，两套外观各截一遍。
#[cfg(debug_assertions)]
pub fn force_appearance(name: &str) {
    use objc2_app_kit::{
        NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication,
    };
    use objc2_foundation::MainThreadMarker;

    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    // SAFETY: 两个名字是 AppKit 导出的只读常量字符串。
    let name = unsafe {
        match name {
            "dark" => NSAppearanceNameDarkAqua,
            "light" => NSAppearanceNameAqua,
            _ => return,
        }
    };
    let appearance = NSAppearance::appearanceNamed(name);
    NSApplication::sharedApplication(mtm).setAppearance(appearance.as_deref());
}
