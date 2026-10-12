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
//! scroll 480 300 -120   在窗口坐标处滚动：第三个数是纵向的像素差，负数往下看
//! key escape        按一个键，写法同 GPUI 的快捷键：down、enter、cmd-a
//! type deep         逐字输入
//! shot tabs         当前窗口截图，存成 tabs.png
//! tray              模拟左键点一下菜单栏图标
//! lang en           切换界面语言（en、zh-CN），和设置页以后做的事一样
//! path /tmp/x.json  下一次要选文件（导入、导出）时直接选它，不弹系统对话框
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
    #[cfg(target_os = "macos")]
    trap_objc_exceptions();

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
            // 等完也补画一帧：不然动画（比如编辑器落下来那 260ms）停在开头那帧上，
            // 下一次点击按旧帧的位置命中，差几个像素就点不到输入框。
            // 目标窗口已经关了（比如面板里选完就关）补不了画，不算错。
            #[cfg(feature = "shots")]
            cx.update(|cx| run("redraw", "", target, &shots, cx)).ok();
        }
    })
    .detach();
}

/// 跑脚本时 AppKit 抛出来的未捕获异常：打印原因就退出，不走 abort。abort 会弹系统的「意外退出」对话框，
/// 打扰正在用电脑的人；退出码 70 和日志里的 `drive: uncaught exception` 让 flows.sh 认出是崩溃。
#[cfg(target_os = "macos")]
fn trap_objc_exceptions() {
    use std::ffi::{CStr, c_char};

    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    unsafe extern "C" fn handler(exception: *mut AnyObject) {
        // SAFETY: AppKit 传进来的是 NSException，`reason` 是 NSString（可能为空）。
        let reason = unsafe {
            let reason: *mut AnyObject = msg_send![exception, reason];
            if reason.is_null() {
                String::from("?")
            } else {
                let utf8: *const c_char = msg_send![reason, UTF8String];
                CStr::from_ptr(utf8).to_string_lossy().into_owned()
            }
        };
        eprintln!("drive: uncaught exception: {reason}");
        std::process::exit(70);
    }

    #[link(name = "Foundation", kind = "framework")]
    unsafe extern "C" {
        fn NSSetUncaughtExceptionHandler(handler: Option<unsafe extern "C" fn(*mut AnyObject)>);
    }
    // SAFETY: 只在进程里装一次，处理函数不碰别的状态。
    unsafe { NSSetUncaughtExceptionHandler(Some(handler)) };
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
        "lang" => {
            crate::i18n::set(arg, cx);
            return Ok(());
        }
        "path" => {
            crate::files::answer_next(PathBuf::from(arg), cx);
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
            // 什么都不做，只走下面那句补画。
            "redraw" => {}
            "move" => mouse_move(point_arg(arg)?, window, cx),
            "scroll" => {
                let (at, dy) = arg.rsplit_once(' ').context("expected `x y dy`")?;
                let position = point_arg(at)?;
                window.dispatch_event(
                    PlatformInput::ScrollWheel(ScrollWheelEvent {
                        position,
                        delta: ScrollDelta::Pixels(point(px(0.), px(dy.trim().parse()?))),
                        modifiers: Modifiers::default(),
                        touch_phase: TouchPhase::Moved,
                    }),
                    cx,
                );
            }
            "click" => click(point_arg(arg)?, window, cx),
            "key" => {
                window.dispatch_keystroke(Keystroke::parse(arg)?, cx);
            }
            "type" => {
                for ch in arg.chars() {
                    let key = if ch == ' ' {
                        "space".into()
                    } else {
                        ch.to_string()
                    };
                    window.dispatch_keystroke(Keystroke::parse(&key)?, cx);
                }
            }
            "shot" => shot(window, cx, &shots.join(format!("{arg}.png")))?,
            // 点窗口左上角的关闭按钮。
            #[cfg(target_os = "macos")]
            "close" => crate::mac::perform_close(window, cx),
            _ => bail!("unknown command"),
        }
        // 屏幕锁着、窗口被挡住时 macOS 停掉了 display link，GPUI 不再画新帧，命中区域就停在旧的一帧上。
        // 开了 shots 特性就每条命令后自己画一帧，脚本在锁屏时也能跑。
        #[cfg(feature = "shots")]
        window.draw(cx).clear(cx);
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
/// 截图。开了 `shots` 特性就让 GPUI 把刚画好的那一帧渲染成图片（不碰屏幕，不要屏幕录制权限）；
/// 不然在 macOS 上用 `screencapture` 按窗口编号截。
#[cfg(feature = "shots")]
fn shot(window: &mut Window, cx: &mut App, path: &Path) -> anyhow::Result<()> {
    // 画两帧，中间等一下：淡入之类的动画从第一次画它开始计时，第二帧才是动画结束的样子。
    window.draw(cx).clear(cx);
    std::thread::sleep(Duration::from_millis(400));
    window.draw(cx).clear(cx);
    let image = window.render_to_image()?;
    image.save(path)?;
    Ok(())
}

#[cfg(all(not(feature = "shots"), target_os = "macos"))]
fn shot(window: &Window, _: &mut App, path: &Path) -> anyhow::Result<()> {
    let id = crate::mac::window_number(window).context("no native window")?;
    let status = std::process::Command::new("screencapture")
        .args(["-x", "-o", &format!("-l{id}")])
        .arg(path)
        .status()?;
    anyhow::ensure!(status.success(), "screencapture exited with {status}");
    Ok(())
}

#[cfg(all(not(feature = "shots"), not(target_os = "macos")))]
fn shot(_: &Window, _: &mut App, _: &Path) -> anyhow::Result<()> {
    bail!("shot is only implemented on macOS")
}
