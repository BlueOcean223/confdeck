mod assets;
mod data;
#[cfg(debug_assertions)]
mod drive;
mod i18n;
mod icons;
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

// 界面文案在 crates/app/locales/，各模块里 `use rust_i18n::t;` 后用 `t!("键")` 取。见 i18n.rs。
rust_i18n::i18n!("locales", fallback = "en");

fn main() {
    i18n::init();
    gpui_kit::application()
        .with_assets(assets::Assets)
        .run(|cx| {
            gpui_kit::init(cx);
            icons::init(cx);
            panel::init(cx);
            // 关掉窗口后继续留在菜单栏，只有「退出」会结束进程。
            cx.set_quit_mode(QuitMode::Explicit);

            #[cfg(all(debug_assertions, target_os = "macos"))]
            if let Ok(name) = std::env::var("CONFDECK_APPEARANCE") {
                mac::force_appearance(&name);
            }

            let store = cx.new(|_| Store::demo());
            main_window::show(store.clone(), cx);
            tray::install(store, cx).expect("failed to create tray icon");
            #[cfg(debug_assertions)]
            drive::start(cx);
        });
}
