//! 菜单栏图标。GPUI 没有托盘 API，这里用 tray-icon（muda 负责右键菜单）。
//!
//! 和 magpie 一样：左键弹出自己画的面板（见 `panel`），右键是系统菜单。
//! tray-icon 在主线程的回调里发事件，事件经 channel 转给 GPUI 的主线程任务处理，
//! 这样处理时能拿到 `&mut App`。

use std::time::Duration;

use gpui_kit::*;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{
    Icon, MouseButton as TrayButton, MouseButtonState, Rect, TrayIconBuilder, TrayIconEvent,
};

use crate::data::Store;
use crate::{main_window, panel};

enum Event {
    Menu(MenuId),
    Click(Rect),
    /// 不经过鼠标，模拟左键点一下图标（`--panel` 和 `drive` 脚本用）。
    ClickIcon,
}

struct TrayEvents(async_channel::Sender<Event>);

impl Global for TrayEvents {}

/// 模拟左键点一下菜单栏图标，效果和真的点一样。
pub fn click(cx: &App) {
    if let Some(events) = cx.try_global::<TrayEvents>() {
        events.0.try_send(Event::ClickIcon).ok();
    }
}

pub fn install(store: Entity<Store>, cx: &mut App) -> anyhow::Result<()> {
    let open = MenuItem::new("打开 confdeck", true, None);
    let quit = MenuItem::new("退出 confdeck", true, None);
    let menu = Menu::new();
    menu.append_items(&[&open, &PredefinedMenuItem::separator(), &quit])?;

    let builder = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip("confdeck");
    #[cfg(target_os = "macos")]
    let builder = builder.with_icon_templated(icon());
    #[cfg(not(target_os = "macos"))]
    let builder = builder.with_icon(icon());
    let tray = builder.build()?;

    let (tx, rx) = async_channel::unbounded();
    cx.set_global(TrayEvents(tx.clone()));
    let menu_tx = tx.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        menu_tx.try_send(Event::Menu(event.id)).ok();
    }));
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        if let TrayIconEvent::Click {
            rect,
            button: TrayButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } = event
        {
            tx.try_send(Event::Click(rect)).ok();
        }
    }));

    // `--panel`：启动后直接打开面板（对应 magpie 的 `magpie panel`）。
    let open_panel = std::env::args().any(|arg| arg == "--panel");

    cx.spawn(async move |cx| {
        // TrayIcon 被 drop 时图标会从菜单栏消失，所以由这个常驻任务持有。
        let tray = tray;
        if open_panel {
            // 图标刚挂上菜单栏时还没有位置，等它布局完。
            cx.background_executor().timer(Duration::from_millis(500)).await;
            cx.update(|cx| click(cx));
        }
        while let Ok(event) = rx.recv().await {
            cx.update(|cx| match event {
                Event::Menu(id) if id == *open.id() => main_window::show(store.clone(), cx),
                Event::Menu(id) if id == *quit.id() => cx.quit(),
                Event::Menu(_) => {}
                Event::Click(rect) => panel::toggle(to_points(rect), store.clone(), cx),
                Event::ClickIcon => {
                    if let Some(rect) = tray.rect() {
                        panel::toggle(to_points(rect), store.clone(), cx);
                    }
                }
            });
        }
    })
    .detach();

    Ok(())
}

/// tray-icon 给的是物理像素，GPUI 的窗口坐标用逻辑像素（点）。
fn to_points(rect: Rect) -> Bounds<Pixels> {
    let scale = screen_scale();
    Bounds::new(
        point(
            px((rect.position.x / scale) as f32),
            px((rect.position.y / scale) as f32),
        ),
        size(
            px((rect.size.width as f64 / scale) as f32),
            px((rect.size.height as f64 / scale) as f32),
        ),
    )
}

/// 主屏的缩放比例。TODO: 多显示器时应取托盘图标所在的那块屏。
#[cfg(target_os = "macos")]
fn screen_scale() -> f64 {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSScreen;

    let mtm = MainThreadMarker::new().expect("tray events are handled on the main thread");
    NSScreen::mainScreen(mtm).map_or(2.0, |screen| screen.backingScaleFactor())
}

#[cfg(not(target_os = "macos"))]
fn screen_scale() -> f64 {
    1.0
}

/// 18pt 菜单栏图标（36px，供 Retina 使用），三张叠起来的卡片。
/// 在 macOS 上作为 template 图标只用 alpha 通道，系统按菜单栏的深浅色自动着色。
fn icon() -> Icon {
    const SIZE: u32 = 36;
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    let mut fill = |x0: u32, y0: u32, x1: u32, y1: u32| {
        for y in y0..y1 {
            for x in x0..x1 {
                rgba[((y * SIZE + x) * 4 + 3) as usize] = 255;
            }
        }
    };
    fill(11, 6, 25, 9);
    fill(8, 11, 28, 14);
    fill(5, 16, 31, 30);
    Icon::from_rgba(rgba, SIZE, SIZE).expect("icon size matches buffer")
}
