//! GPUI 没提供、只能直接调 AppKit 的窗口操作。

use gpui_kit::{App, Window};
use objc2::msg_send;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSView, NSWindow};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

fn ns_window(window: &Window) -> Option<objc2::rc::Retained<NSWindow>> {
    let handle = HasWindowHandle::window_handle(window).ok()?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return None;
    };
    // SAFETY: GPUI 给的是它自己窗口的 content view，窗口存活期间一直有效。
    let view: &NSView = unsafe { handle.ns_view.cast().as_ref() };
    view.window()
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

/// 关窗口之前先交出第一响应者。面板里的输入框还是第一响应者、主窗口又刚变成 key 窗口时关面板，
/// AppKit 的 TouchBar 观察者会在下一次刷新里把 `nextResponder` 的观察解注册两次，抛 `NSRangeException`
/// 让整个应用崩溃（notes/panel-focus-crash.md）。
pub fn resign_first_responder(window: &Window) {
    if let Some(ns_window) = ns_window(window) {
        ns_window.makeFirstResponder(None);
    }
}

/// 改窗口高度，顶边不动、向下长。GPUI 的 `Window::resize` 固定的是左下角，
/// 面板会往上长、顶到菜单栏上面去。
///
/// 和 `Window::resize` 一样放到下一轮主线程任务里改：AppKit 在 `setFrame` 里会同步回调
/// GPUI 的 resize，而调用这个函数时 GPUI 正在处理事件、`App` 被借着，回调拿不到 `App`，
/// 这次尺寸变化就被丢掉了：窗口变高了，GPUI 却还按旧高度布局。
pub fn set_height_keep_top(window: &Window, height: f64, cx: &App) {
    let Some(ns_window) = ns_window(window) else {
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
