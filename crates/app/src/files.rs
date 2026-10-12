//! 系统的打开、存储对话框（导入导出用）。取消了是 `None`。
//!
//! debug 构建里 `drive` 脚本的 `path` 命令可以先把答案填好（[`answer_next`]），下一次要选文件时直接用它、不弹对话框：
//! 系统对话框是另一个进程画的，脚本点不到。

use std::path::{Path, PathBuf};

use gpui_kit::*;

/// 选一个要导入的文件。
pub fn pick_open(cx: &mut App) -> Task<Option<PathBuf>> {
    #[cfg(debug_assertions)]
    if let Some(path) = take_answer(cx) {
        return Task::ready(Some(path));
    }
    let picked = cx.prompt_for_paths(PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: None,
    });
    cx.spawn(async move |_| picked.await.ok()?.ok()??.into_iter().next())
}

/// 选导出到哪：从 `dir` 开始，文件名先填 `name`。
pub fn pick_save(dir: &Path, name: &str, cx: &mut App) -> Task<Option<PathBuf>> {
    #[cfg(debug_assertions)]
    if let Some(path) = take_answer(cx) {
        return Task::ready(Some(path));
    }
    let picked = cx.prompt_for_new_path(dir, Some(name));
    cx.spawn(async move |_| picked.await.ok()?.ok()?)
}

#[cfg(debug_assertions)]
#[derive(Default)]
struct NextAnswer(Option<PathBuf>);

#[cfg(debug_assertions)]
impl Global for NextAnswer {}

/// `drive` 的 `path` 命令：下一次选文件就选它。
#[cfg(debug_assertions)]
pub fn answer_next(path: PathBuf, cx: &mut App) {
    cx.set_global(NextAnswer(Some(path)));
}

#[cfg(debug_assertions)]
fn take_answer(cx: &mut App) -> Option<PathBuf> {
    cx.try_global::<NextAnswer>()?;
    cx.global_mut::<NextAnswer>().0.take()
}
