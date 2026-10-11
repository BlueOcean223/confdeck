mod add_provider;
mod assets;
mod config_page;
mod data;
#[cfg(debug_assertions)]
mod drive;
mod editor;
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

use std::path::PathBuf;

use confdeck_core::data::DataDir;
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

            let (data, home) = locations();
            let store = cx.new(|_| Store::load(data, home));
            main_window::show(store.clone(), cx);
            // Codex 升级以后模型目录里的提示词要跟着换；每次启动在后台对一遍，内容一样就不写。
            store.update(cx, |store, cx| store.refresh_codex_catalog(cx));
            tray::install(store, cx).expect("failed to create tray icon");
            #[cfg(debug_assertions)]
            drive::start(cx);
        });
}

/// 数据目录和家目录。debug 构建里 `CONFDECK_DATA`、`CONFDECK_HOME` 可以指到别处，
/// 测试时不碰真实的 `~/.confdeck/` 和各 agent 的配置文件。
fn locations() -> (DataDir, PathBuf) {
    let home = std::env::home_dir().expect("home directory");
    let data = DataDir::default_location().expect("home directory");
    #[cfg(debug_assertions)]
    {
        let data = std::env::var_os("CONFDECK_DATA")
            .map(|dir| DataDir::new(PathBuf::from(dir)))
            .unwrap_or(data);
        let home = std::env::var_os("CONFDECK_HOME")
            .map(PathBuf::from)
            .unwrap_or(home);
        return (data, home);
    }
    #[allow(unreachable_code)]
    (data, home)
}
