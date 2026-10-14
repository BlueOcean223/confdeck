//! GPUI 没提供、只能直接调 AppKit 的窗口操作。

use std::cell::RefCell;
use std::ptr::NonNull;
use std::rc::Rc;
use std::time::Duration;

use block2::RcBlock;
use gpui_kit::{App, BackgroundExecutor, ForegroundExecutor, Pixels, Point, Window};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObjectProtocol, ProtocolObject};
use objc2::{Message, msg_send};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationOptions, NSApplicationDidBecomeActiveNotification,
    NSApplicationDidResignActiveNotification, NSRunningApplication, NSScreen, NSView, NSWindow,
    NSWindowOrderingMode, NSWorkspace,
};
use objc2_foundation::{
    MainThreadMarker, NSArray, NSDictionary, NSNotification, NSNotificationCenter,
    NSNotificationName, NSNumber, NSPoint, NSRect, NSSize, NSString,
};
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
/// 让整个应用崩溃。
pub fn hide(window: &Window, cx: &App) {
    let Some(ns_window) = ns_window(window) else {
        return;
    };
    ns_window.setAlphaValue(0.);
    order_out(window, cx);
}

/// 收起窗口、不销毁：交出第一响应者，下一轮主线程任务再 `orderOut`。主窗口点关闭按钮时用它，
/// 理由和 `hide` 一样。之后 `activate_window` 就能再显示出来。
pub fn order_out(window: &Window, cx: &App) {
    let Some(ns_window) = ns_window(window) else {
        return;
    };
    ns_window.makeFirstResponder(None);
    cx.foreground_executor()
        .spawn(async move { ns_window.orderOut(None) })
        .detach();
}

/// 和点窗口左上角的关闭按钮一样（`performClose:`，会问 GPUI 的 `on_window_should_close`），给 drive 脚本用。
/// 放到下一轮主线程任务：调用时 `App` 正被借着，GPUI 的回调拿不到它，会当作「可以关」直接关掉。
#[cfg(debug_assertions)]
pub fn perform_close(window: &Window, cx: &App) {
    let Some(ns_window) = ns_window(window) else {
        return;
    };
    cx.foreground_executor()
        .spawn(async move { ns_window.performClose(None) })
        .detach();
}

/// 把 `hide` 藏起来的窗口挪到 `origin`（和 GPUI 开窗口时一样，以主屏左上角为原点）、
/// 内容高 `height`，显示出来并成为 key 窗口。
///
/// 窗口这时还是透明的，里面是上次藏起来时那一帧：`reveal` 为真时马上同步画一帧（走 GPUI 的 `displayLayer:`），
/// 画完再变回不透明。不等 GPUI 的下一帧：透明的窗口刚拿出来，系统要过一会儿才算它看得见，
/// 在那之前 GPUI 的 display link 不跑，面板有时要等很久才出来。
pub fn show_at(window: &Window, origin: Point<Pixels>, height: f64, reveal: bool, cx: &App) {
    let Some(view) = gpui_view(window) else {
        return;
    };
    let Some(ns_window) = view.window() else {
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
            #[cfg(debug_assertions)]
            eprintln!(
                "panel: {} ordered front, occluded={}",
                crate::panel::ms(),
                !ns_window
                    .occlusionState()
                    .contains(objc2_app_kit::NSWindowOcclusionState::Visible)
            );
            if reveal {
                view.setNeedsDisplay(true);
                view.displayIfNeeded();
                ns_window.setAlphaValue(1.);
                ns_window.invalidateShadow();
                #[cfg(debug_assertions)]
                eprintln!("panel: {} revealed", crate::panel::ms());
            }
        })
        .detach();
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    /// 返回的 CFArray 和 NSArray 免费桥接，调用方负责释放。
    fn CGWindowListCopyWindowInfo(
        option: u32,
        relative_to_window: u32,
    ) -> *mut NSArray<NSDictionary<NSString, AnyObject>>;
}

/// 等激活等多久：过了还没激活（系统没让）就不管了，免得以后哪次激活时把主窗口错放到后面。
const ACTIVATION_WAIT: Duration = Duration::from_secs(1);
/// 把前台还给别的应用以后，等 confdeck 退到后台最多等多久，到了照样收起面板。比 `panel::toggle` 里
/// 「刚关又点」的 300 毫秒短：再打开面板时，上一次延后的收起一定已经做完。
const HAND_BACK_WAIT: Duration = Duration::from_millis(250);

/// 应用在后台时打开面板要先激活应用（不然 GPUI 会收走面板刚拿到的焦点），而激活会把开着的主窗口一起提到前面
/// （默认带上 main 和 key 窗口）。激活之前记下压在主窗口上面、离它最近的别的应用的窗口，
/// 在 `didBecomeActive` 通知里（和提窗口是同一轮事件）把主窗口放回它下面。
/// 应用已在前台、主窗口没显示或最小化、上面没有别的窗口时什么也不做。要在激活之前调。
pub fn stay_behind_on_activation(window: &Window, cx: &App) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    if NSApplication::sharedApplication(mtm).isActive() {
        return;
    }
    let Some(ns_window) = ns_window(window) else {
        return;
    };
    if !ns_window.isVisible() || ns_window.isMiniaturized() {
        return;
    }
    let Some(above) = window_above(ns_window.windowNumber()) else {
        return;
    };
    #[cfg(debug_assertions)]
    eprintln!("panel: {} main window stays below window {above}", crate::panel::ms());
    // SAFETY: AppKit 的通知名常量。
    let name = unsafe { NSApplicationDidBecomeActiveNotification };
    once(
        name,
        ACTIVATION_WAIT,
        false,
        cx.foreground_executor(),
        cx.background_executor(),
        move || ns_window.orderWindow_relativeTo(NSWindowOrderingMode::Below, above),
    );
}

/// confdeck 不在前台。
pub fn in_background() -> bool {
    MainThreadMarker::new().is_some_and(|mtm| !NSApplication::sharedApplication(mtm).isActive())
}

/// 应用激活完成以后（`didBecomeActive` 的下一轮）跑 `f`；[`ACTIVATION_WAIT`] 内没激活也跑。
/// 从后台打开面板时用：面板在激活之前拿到焦点的话，激活时系统把焦点还给主窗口，面板一失焦就关了。
pub fn after_activation(cx: &App, f: impl FnOnce(&mut App) + 'static) {
    let async_cx = cx.to_async();
    let later = cx.foreground_executor().clone();
    // SAFETY: AppKit 的通知名常量。
    let name = unsafe { NSApplicationDidBecomeActiveNotification };
    once(
        name,
        ACTIVATION_WAIT,
        true,
        cx.foreground_executor(),
        cx.background_executor(),
        move || {
            // 通知回调里不开窗口：下一轮再开。
            later.spawn(async move { async_cx.update(f) }).detach();
        },
    );
}

/// 打开面板之前在前台的那个应用，关面板时把前台还给它。
pub struct OtherApp(Retained<NSRunningApplication>);

/// confdeck 在后台时，现在在前台的应用；confdeck 自己在前台时是 `None`。
pub fn frontmost_other() -> Option<OtherApp> {
    let mtm = MainThreadMarker::new()?;
    if NSApplication::sharedApplication(mtm).isActive() {
        return None;
    }
    let app = NSWorkspace::sharedWorkspace().frontmostApplication()?;
    (app.processIdentifier() as u32 != std::process::id()).then_some(OtherApp(app))
}

/// 收起从后台打开的面板：马上变透明，下一轮看焦点去了哪。
/// - 还在 confdeck、又没有别的窗口接手（按了 Esc、点了托盘图标）：把前台还给 `back`，等 confdeck 退到后台
///   （`didResignActive`，最多 [`HAND_BACK_WAIT`]）再 `orderOut`。不等的话 AppKit 把焦点交给主窗口、把它提到前面。
/// - 用户点了主窗口或者别的应用：照常收起。
pub fn hide_and_hand_back(window: &Window, back: OtherApp, cx: &App) {
    let Some(ns_window) = ns_window(window) else {
        return;
    };
    ns_window.setAlphaValue(0.);
    let foreground = cx.foreground_executor().clone();
    let background = cx.background_executor().clone();
    cx.foreground_executor()
        .spawn(async move {
            let Some(mtm) = MainThreadMarker::new() else {
                return;
            };
            let app = NSApplication::sharedApplication(mtm);
            // 点托盘图标时 key 可能短暂落在状态栏的窗口上（也是 confdeck 进程的），那个不算接手。
            let taken = app.keyWindow().is_some_and(|key| {
                Retained::as_ptr(&key) != Retained::as_ptr(&ns_window) && key.canBecomeMainWindow()
            });
            #[cfg(debug_assertions)]
            eprintln!("panel: {} hand back? active={} taken={taken}", crate::panel::ms(), app.isActive());
            if !app.isActive() || taken {
                ns_window.makeFirstResponder(None);
                ns_window.orderOut(None);
                return;
            }
            let later = foreground.clone();
            // SAFETY: AppKit 的通知名常量。
            let name = unsafe { NSApplicationDidResignActiveNotification };
            once(
                name,
                HAND_BACK_WAIT,
                true,
                &foreground,
                &background,
                move || {
                    // 不在通知回调里同步改窗口（第一次崩溃的教训）：下一轮再收起。
                    later
                        .spawn(async move {
                            ns_window.makeFirstResponder(None);
                            ns_window.orderOut(None);
                        })
                        .detach();
                },
            );
            back.0
                .activateWithOptions(NSApplicationActivationOptions::empty());
        })
        .detach();
}

/// 下一次发 `name` 通知时跑一次 `f`（在主线程上、发通知的那一刻同步跑），然后撤掉观察者。
/// `wait` 内没发：`on_timeout` 为真时到点照样跑 `f`，否则不跑；观察者都撤掉。
fn once(
    name: &NSNotificationName,
    wait: Duration,
    on_timeout: bool,
    foreground: &ForegroundExecutor,
    background: &BackgroundExecutor,
    f: impl FnOnce() + 'static,
) {
    let f = Rc::new(RefCell::new(Some(f)));
    let observer: Rc<RefCell<Option<Observer>>> = Rc::default();
    let block = RcBlock::new({
        let (f, observer) = (f.clone(), observer.clone());
        move |_: NonNull<NSNotification>| {
            #[cfg(debug_assertions)]
            eprintln!("panel: {} notified", crate::panel::ms());
            remove_observer(&observer);
            if let Some(f) = f.borrow_mut().take() {
                f();
            }
        }
    });
    // SAFETY: 不给队列，回调在发通知的线程（主线程）上同步跑。
    let token = unsafe {
        NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
            Some(name),
            None,
            None,
            &block,
        )
    };
    *observer.borrow_mut() = Some(token);
    let timer = background.timer(wait);
    foreground
        .spawn(async move {
            timer.await;
            remove_observer(&observer);
            let f = f.borrow_mut().take();
            #[cfg(debug_assertions)]
            if f.is_some() {
                eprintln!("panel: {} timed out", crate::panel::ms());
            }
            if on_timeout && let Some(f) = f {
                f();
            }
        })
        .detach();
}

type Observer = Retained<ProtocolObject<dyn NSObjectProtocol>>;

fn remove_observer(observer: &RefCell<Option<Observer>>) {
    if let Some(token) = observer.borrow_mut().take() {
        // SAFETY: `token` 是 `addObserverForName…` 返回的那个。
        unsafe { NSNotificationCenter::defaultCenter().removeObserver(token.as_ref()) };
    }
}

/// 压在 `number` 这个窗口上面、离它最近的别的应用的普通窗口（层级 0）的编号。
fn window_above(number: isize) -> Option<isize> {
    const ON_SCREEN_ABOVE_WINDOW: u32 = 1 << 1;
    const EXCLUDE_DESKTOP_ELEMENTS: u32 = 1 << 4;
    let number = u32::try_from(number).ok()?;
    // SAFETY: Copy 规则，返回的数组归我们，交给 `Retained` 释放；拿不到是空指针。
    let list = unsafe {
        Retained::from_raw(CGWindowListCopyWindowInfo(
            ON_SCREEN_ABOVE_WINDOW | EXCLUDE_DESKTOP_ELEMENTS,
            number,
        ))
    }?;
    let me = std::process::id() as isize;
    let int = |entry: &NSDictionary<NSString, AnyObject>, key: &str| {
        let value = entry.objectForKey(&NSString::from_str(key))?;
        Some(value.downcast::<NSNumber>().ok()?.integerValue())
    };
    // 从前往后排，最后一个就是紧挨在它上面的。
    list.iter()
        .filter(|entry| int(entry, "kCGWindowLayer") == Some(0))
        .filter(|entry| int(entry, "kCGWindowOwnerPID") != Some(me))
        .filter_map(|entry| int(&entry, "kCGWindowNumber"))
        .last()
}

/// 跑 `drive` 脚本时的窗口（`crate::quiet`）：全透明、鼠标穿过去，不挡着正在用电脑的人。
pub fn hush(window: &Window) {
    if let Some(ns_window) = ns_window(window) {
        ns_window.setAlphaValue(0.);
        ns_window.setIgnoresMouseEvents(true);
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
            #[cfg(debug_assertions)]
            eprintln!("panel: {} resize to {height}", crate::panel::ms());
            ns_window.setFrame_display(next, true);
            view.setNeedsDisplay(true);
            view.displayIfNeeded();
            ns_window.invalidateShadow();
            #[cfg(debug_assertions)]
            eprintln!("panel: {} resized", crate::panel::ms());
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
