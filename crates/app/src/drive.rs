//! 开发时用的界面驱动：照脚本直接往 GPUI 窗口里送鼠标、键盘事件，再按窗口截图。
//! 不动真实的鼠标键盘，所以跑测试时别的应用照常用，也不会截到别的窗口。
//!
//! 用法：`CONFDECK_DRIVE=脚本 cargo run -p confdeck-app`，截图存到 `$CONFDECK_SHOTS`
//! （默认 `/tmp/confdeck-shots`）。脚本一行一条命令，`#` 之后是注释：
//!
//! ```text
//! on main           之后的事件送到主窗口（默认）；`on panel` 送到菜单栏面板
//! click 402 25      在窗口坐标（单位是点）处左键单击
//! move 402 25       把鼠标移过去（触发悬停）
//! key escape        按一个键，写法同 GPUI 的快捷键：down、enter、cmd-a
//! type deep         逐字输入
//! shot tabs         当前窗口截图，存成 tabs.png
//! tray              模拟左键点一下菜单栏图标
//! wait 300          等 300 毫秒
//! quit              退出
//! ```

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use gpui_kit::*;

use crate::{main_window, panel, tray};

/// 每条命令之后等一会儿，让窗口把新的一帧画出来、命中区域更新好。
const SETTLE: u64 = 120;

#[derive(Clone, Copy, Debug)]
enum Target {
    Main,
    Panel,
}

/// 设了 `CONFDECK_DRIVE` 就在后台按脚本执行，没设什么都不做。
pub fn start(cx: &mut App) {
    let Ok(path) = std::env::var("CONFDECK_DRIVE") else {
        return;
    };
    let script = match std::fs::read_to_string(&path) {
        Ok(script) => script,
        Err(err) => {
            eprintln!("drive: cannot read {path}: {err}");
            return;
        }
    };
    let shots = PathBuf::from(
        std::env::var("CONFDECK_SHOTS").unwrap_or_else(|_| "/tmp/confdeck-shots".into()),
    );
    std::fs::create_dir_all(&shots).ok();

    cx.spawn(async move |cx| {
        let mut target = Target::Main;
        for line in script.lines() {
            let line = line.split('#').next().unwrap_or_default().trim();
            if line.is_empty() {
                continue;
            }
            eprintln!("drive: {line}");
            let (cmd, arg) = line.split_once(' ').unwrap_or((line, ""));
            let pause = match cmd {
                "wait" => arg.parse().unwrap_or(0),
                "on" if arg == "main" => {
                    target = Target::Main;
                    0
                }
                "on" if arg == "panel" => {
                    target = Target::Panel;
                    0
                }
                _ => {
                    if let Err(err) = cx.update(|cx| run(cmd, arg, target, &shots, cx)) {
                        eprintln!("drive: {line}: {err:#}");
                    }
                    SETTLE
                }
            };
            cx.background_executor()
                .timer(Duration::from_millis(pause))
                .await;
        }
    })
    .detach();
}

fn run(cmd: &str, arg: &str, target: Target, shots: &Path, cx: &mut App) -> anyhow::Result<()> {
    match cmd {
        "tray" => {
            tray::click(cx);
            return Ok(());
        }
        "quit" => {
            cx.quit();
            return Ok(());
        }
        _ => {}
    }
    let handle = match target {
        Target::Main => main_window::handle(cx),
        Target::Panel => panel::handle(cx),
    }
    .ok_or_else(|| anyhow!("{target:?} window is not open"))?;

    handle.update(cx, |_, window, cx| {
        match cmd {
            "move" => mouse_move(point_arg(arg)?, window, cx),
            "click" => click(point_arg(arg)?, window, cx),
            "key" => {
                window.dispatch_keystroke(Keystroke::parse(arg)?, cx);
            }
            "type" => {
                for ch in arg.chars() {
                    let key = if ch == ' ' { "space".into() } else { ch.to_string() };
                    window.dispatch_keystroke(Keystroke::parse(&key)?, cx);
                }
            }
            "shot" => shot(window, &shots.join(format!("{arg}.png")))?,
            _ => bail!("unknown command"),
        }
        Ok(())
    })?
}

fn point_arg(arg: &str) -> anyhow::Result<Point<Pixels>> {
    let (x, y) = arg.split_once(' ').context("expected `x y`")?;
    Ok(point(px(x.trim().parse()?), px(y.trim().parse()?)))
}

fn mouse_move(position: Point<Pixels>, window: &mut Window, cx: &mut App) {
    window.dispatch_event(
        PlatformInput::MouseMove(MouseMoveEvent {
            position,
            pressed_button: None,
            modifiers: Modifiers::default(),
        }),
        cx,
    );
}

/// 和真实点击一样：先移过去，再按下、抬起。
fn click(position: Point<Pixels>, window: &mut Window, cx: &mut App) {
    mouse_move(position, window, cx);
    window.dispatch_event(
        PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Left,
            position,
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        }),
        cx,
    );
    window.dispatch_event(
        PlatformInput::MouseUp(MouseUpEvent {
            button: MouseButton::Left,
            position,
            modifiers: Modifiers::default(),
            click_count: 1,
        }),
        cx,
    );
}

/// 按窗口编号截图：只截这个窗口，被别的窗口挡住也照样截得到。
#[cfg(target_os = "macos")]
fn shot(window: &Window, path: &Path) -> anyhow::Result<()> {
    let id = crate::mac::window_number(window).context("no native window")?;
    let status = std::process::Command::new("screencapture")
        .args(["-x", "-o", &format!("-l{id}")])
        .arg(path)
        .status()?;
    anyhow::ensure!(status.success(), "screencapture exited with {status}");
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn shot(_: &Window, _: &Path) -> anyhow::Result<()> {
    bail!("shot is only implemented on macOS")
}
