mod assets;
mod data;
#[cfg(debug_assertions)]
mod drive;
#[cfg(target_os = "macos")]
mod mac;
mod main_window;
mod panel;
mod picker;
mod theme;
mod tray;
mod ui;

use gpui_kit::*;

use crate::data::Store;

fn main() {
    gpui_kit::application()
        .with_assets(assets::Assets)
        .run(|cx| {
            gpui_kit::init(cx);
            panel::init(cx);
            // 关掉窗口后继续留在菜单栏，只有「退出」会结束进程。
            cx.set_quit_mode(QuitMode::Explicit);

            let store = cx.new(|_| Store::demo());
            main_window::show(store.clone(), cx);
            tray::install(store, cx).expect("failed to create tray icon");
            #[cfg(debug_assertions)]
            drive::start(cx);
        });
}
