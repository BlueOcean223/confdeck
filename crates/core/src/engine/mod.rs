//! 写入引擎：confdeck 改 agent 配置文件的唯一路径。
//!
//! 一次写入：
//!
//! 1. 拿锁（进程内一把，加上 `~/.confdeck/.lock` 的跨进程文件锁）。
//! 2. 跟着软链接找到真正的文件，读当前内容。
//! 3. 在内存里算出新内容（按键补丁；解析不了就停，不写）。内容没变就什么都不做。
//! 4. 按需备份当前内容（见下）。
//! 5. 新内容写进目标旁边的临时文件并 fsync。
//! 6. 再读一次目标：和第 2 步读到的不一样，说明这期间被别的程序改了，丢掉临时文件，
//!    以新内容为底从第 2 步重来（整份替换则报冲突）。
//! 7. 改名替换目标，记下新内容的 hash。
//!
//! **什么时候备份**（提案 08）：按键补丁只在当前内容不是 confdeck 上次写的
//! （第一次写、被外部改过）时备份；整份替换（「原始文件」页保存、从备份恢复）之前一定备份。
//! 连续切换供应商不产生备份。
//!
//! 第 6 步和改名之间仍有极小的窗口：外部程序恰好在这时写入，它的改动会被覆盖。
//! Claude Code 这些 agent 不参与 confdeck 的锁，这个窗口去不掉。

mod state;

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use chrono::Local;

use crate::backup::{self, Backup, Reason, Shelf};
use crate::data::DataDir;
use crate::fsx::{self, Digest, Mode};
use crate::net::Proxy;
use crate::patch::{self, Format, KeyPath, LivePatch, PatchError};
pub use state::Place;
use state::{Owned, State};

/// 第 6 步发现被外部改了以后最多重来几次。
const ATTEMPTS: usize = 3;

/// 一个由 confdeck 写入的配置文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveFile {
    /// 备份目录的第一层：agent 的 id。
    pub owner: &'static str,
    /// 配置里的路径，可以是软链接。
    pub path: PathBuf,
    pub mode: Mode,
    pub format: Format,
}

impl LiveFile {
    pub fn shelf(&self) -> Shelf {
        let name = self
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".to_string());
        Shelf::new(self.owner, &name)
    }
}

/// 读到的内容，整份替换时用来确认文件在这期间没被改过。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub bytes: Option<Vec<u8>>,
    pub digest: Option<Digest>,
}

impl Snapshot {
    fn of(bytes: Option<Vec<u8>>) -> Self {
        let digest = bytes.as_deref().map(Digest::of);
        Self { bytes, digest }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    /// 文件不存在。
    Missing,
    /// confdeck 没写过它（或记录丢了）。
    Untracked,
    /// 还是 confdeck 上次写的样子。
    Ours,
    /// confdeck 写过，之后被别的程序或人改了。
    ChangedOutside,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// 内容没变时为假，什么都没写。
    pub changed: bool,
    /// 写之前存的备份；没备份（不需要，或和上一份相同）时为 `None`。
    pub backup: Option<Backup>,
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("读写 {path} 失败：{source}")]
    Io { path: PathBuf, source: io::Error },
    #[error(transparent)]
    Patch(#[from] PatchError),
    #[error("{path} 在写入期间被别的程序改了，没有写入。请重新打开后再试")]
    Conflict { path: PathBuf },
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> EngineError + '_ {
    move |source| EngineError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// 由写前内容（和它的 hash）算出写后内容。
type Compute<'a> = dyn FnMut(Option<&[u8]>, Option<&Digest>) -> Result<Vec<u8>, EngineError> + 'a;

/// 换登录文件时由写前内容算出写后内容（[`Engine::write_login`]）。
pub type LoginCompute<'a> = dyn FnMut(Option<&[u8]>) -> Result<Vec<u8>, EngineError> + 'a;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Patch,
    Edit,
    Restore,
    /// 换官方账号：换下来的登录已经存进账号里了，不备份（备份里也不该留令牌）。
    Login,
}

pub struct Engine {
    data: DataDir,
    in_process: Mutex<()>,
}

/// 写入期间持有：进程内的锁和跨进程的文件锁。
pub(crate) struct WriteLock<'a> {
    _in_process: MutexGuard<'a, ()>,
    _file: fs::File,
}

impl Engine {
    pub fn new(data: DataDir) -> Self {
        Self {
            data,
            in_process: Mutex::new(()),
        }
    }

    pub fn data(&self) -> &DataDir {
        &self.data
    }

    pub(crate) fn lock(&self) -> Result<WriteLock<'_>, EngineError> {
        let in_process = self
            .in_process
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let path = self.data.lock_file();
        fs::create_dir_all(self.data.root()).map_err(io_error(self.data.root()))?;
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(io_error(&path))?;
        file.lock().map_err(io_error(&path))?;
        Ok(WriteLock {
            _in_process: in_process,
            _file: file,
        })
    }

    /// 读文件当前的内容（跟着软链接）。
    pub fn read(&self, file: &LiveFile) -> Result<Snapshot, EngineError> {
        let target = fsx::resolve(&file.path).map_err(io_error(&file.path))?;
        let bytes = fsx::read_optional(&target).map_err(io_error(&target))?;
        Ok(Snapshot::of(bytes))
    }

    pub fn status(&self, file: &LiveFile) -> Result<FileStatus, EngineError> {
        let target = fsx::resolve(&file.path).map_err(io_error(&file.path))?;
        let snapshot = self.read(file)?;
        let state = State::load(&self.data.state());
        Ok(match (&snapshot.digest, state.get(&target)) {
            (None, _) => FileStatus::Missing,
            (Some(_), None) => FileStatus::Untracked,
            (Some(_), Some(_)) if state.is_ours(&target, snapshot.digest.as_ref()) => {
                FileStatus::Ours
            }
            (Some(_), Some(_)) => FileStatus::ChangedOutside,
        })
    }

    /// 单选 agent 上次在 confdeck 里选的是哪家：`None` 是没选过，`Some(None)` 是「默认」。
    pub fn selection(&self, agent: &str) -> Option<Option<String>> {
        State::load(&self.data.state())
            .selection(agent)
            .map(|id| id.map(str::to_string))
    }

    /// 记下单选 agent 选了哪家（`None` 是「默认」）。只存在本机。
    pub fn set_selection(&self, agent: &str, provider_id: Option<&str>) -> Result<(), EngineError> {
        let _lock = self.lock()?;
        let path = self.data.state();
        let mut state = State::load(&path);
        state.select(agent, provider_id);
        state.save(&path).map_err(io_error(&path))
    }

    /// 设置页「显示哪些 agent」里在这一处关掉的 agent（id）。
    pub fn hidden(&self, place: Place) -> BTreeSet<String> {
        State::load(&self.data.state()).hidden(place).clone()
    }

    /// 在 Agent 页或面板里显示 / 藏起一个 agent。只存在本机。
    pub fn set_hidden(&self, place: Place, agent: &str, hidden: bool) -> Result<(), EngineError> {
        let _lock = self.lock()?;
        let path = self.data.state();
        let mut state = State::load(&path);
        state.set_hidden(place, agent, hidden);
        state.save(&path).map_err(io_error(&path))
    }

    /// 设置页的出站代理。
    pub fn proxy(&self) -> Proxy {
        State::load(&self.data.state()).proxy().clone()
    }

    /// 改出站代理。只存在本机。
    pub fn set_proxy(&self, proxy: Proxy) -> Result<(), EngineError> {
        let _lock = self.lock()?;
        let path = self.data.state();
        let mut state = State::load(&path);
        state.set_proxy(proxy);
        state.save(&path).map_err(io_error(&path))
    }

    /// 额度查询的设置（键见 `state.json` 的 `checks`）。
    pub fn checks(&self) -> std::collections::BTreeMap<String, crate::quota::Check> {
        State::load(&self.data.state()).checks().clone()
    }

    /// 改一家供应商或一个账号的额度查询设置。只存在本机。
    pub fn set_check(&self, key: &str, check: crate::quota::Check) -> Result<(), EngineError> {
        let _lock = self.lock()?;
        let path = self.data.state();
        let mut state = State::load(&path);
        state.set_check(key, check);
        state.save(&path).map_err(io_error(&path))
    }

    /// 上次切换时原始片段写进这个 agent 共用位置的键和值（见 `state.json` 的 `owned`）。
    pub fn owned(&self, agent: &str) -> Vec<(KeyPath, serde_json::Value)> {
        State::load(&self.data.state())
            .owned(agent)
            .iter()
            .map(|owned| (owned.path.clone(), owned.value.clone()))
            .collect()
    }

    /// 记下这次写进去的键和值，替换上一次的。只存在本机。
    pub fn set_owned(
        &self,
        agent: &str,
        entries: &[(KeyPath, serde_json::Value)],
    ) -> Result<(), EngineError> {
        let _lock = self.lock()?;
        let path = self.data.state();
        let mut state = State::load(&path);
        let entries = entries
            .iter()
            .map(|(path, value)| Owned {
                path: path.clone(),
                value: value.clone(),
            })
            .collect();
        state.own(agent, entries);
        state.save(&path).map_err(io_error(&path))
    }

    /// 按键补丁写入：切换供应商、勾选供应商。
    pub fn write(&self, file: &LiveFile, patch: &dyn LivePatch) -> Result<Outcome, EngineError> {
        self.commit(file, Kind::Patch, &mut |pre, _| {
            Ok(patch.apply(&file.path, pre)?)
        })
    }

    /// 整份替换（「原始文件」页保存）。`base` 是打开编辑器时读到的内容；文件在这之后被改过就报冲突，
    /// 不覆盖。新内容按文件的格式解析不了就报错（带行列），不写，见 [`patch::check`]。
    pub fn replace(
        &self,
        file: &LiveFile,
        bytes: &[u8],
        base: &Snapshot,
    ) -> Result<Outcome, EngineError> {
        patch::check(file.format, &file.path, bytes)?;
        self.commit(file, Kind::Edit, &mut |_, current| {
            if current != base.digest.as_ref() {
                return Err(EngineError::Conflict {
                    path: file.path.clone(),
                });
            }
            Ok(bytes.to_vec())
        })
    }

    /// 换 agent 的登录文件（[`crate::account`]）。`compute` 在锁里拿到文件现在的内容，先把它存好，再给出要写的内容；
    /// 文件在这期间被 agent 改了（比如刚换了新令牌）就拿新内容再来一次。不备份。
    pub fn write_login(
        &self,
        file: &LiveFile,
        compute: &mut LoginCompute<'_>,
    ) -> Result<Outcome, EngineError> {
        self.commit(file, Kind::Login, &mut |pre, _| compute(pre))
    }

    /// 用一份备份替换当前文件。当前内容先备份，恢复错了还能再退回来。
    pub fn restore(&self, file: &LiveFile, backup: &Backup) -> Result<Outcome, EngineError> {
        let bytes = fs::read(&backup.path).map_err(io_error(&backup.path))?;
        self.commit(file, Kind::Restore, &mut |_, _| Ok(bytes.clone()))
    }

    fn commit(
        &self,
        file: &LiveFile,
        kind: Kind,
        compute: &mut Compute<'_>,
    ) -> Result<Outcome, EngineError> {
        let _lock = self.lock()?;
        let target = fsx::resolve(&file.path).map_err(io_error(&file.path))?;
        let state_path = self.data.state();
        let mut state = State::load(&state_path);

        for _ in 0..ATTEMPTS {
            let pre = Snapshot::of(fsx::read_optional(&target).map_err(io_error(&target))?);
            let next = compute(pre.bytes.as_deref(), pre.digest.as_ref())?;
            if pre.bytes.as_deref() == Some(next.as_slice()) {
                return Ok(Outcome {
                    changed: false,
                    backup: None,
                });
            }

            let now = Local::now().naive_local();
            let reason = match kind {
                Kind::Patch if state.is_ours(&target, pre.digest.as_ref()) => None,
                Kind::Patch => Some(Reason::External),
                Kind::Edit => Some(Reason::BeforeEdit),
                Kind::Restore => Some(Reason::BeforeRestore),
                Kind::Login => None,
            };
            let backup = match (&pre.bytes, reason) {
                (Some(bytes), Some(reason)) => {
                    let shelf = file.shelf();
                    let shelf_dir = shelf.dir(&self.data);
                    // confdeck 从没写过、也没备份过的文件，这一份就是原件。
                    let first = state.get(&target).is_none()
                        && backup::list(&self.data, &shelf)
                            .map_err(io_error(&shelf_dir))?
                            .is_empty();
                    let reason = if first { Reason::Original } else { reason };
                    backup::take(&self.data, &shelf, bytes, reason, now)
                        .map_err(io_error(&shelf_dir))?
                }
                _ => None,
            };

            let staged = fsx::stage(&target, &next, file.mode).map_err(io_error(&target))?;
            let current = fsx::read_optional(&target).map_err(io_error(&target))?;
            if current.as_deref().map(Digest::of) != pre.digest {
                // 被外部改了：丢掉临时文件，以新内容为底重来。
                drop(staged);
                continue;
            }
            staged.commit().map_err(io_error(&target))?;

            let at = now.format("%Y-%m-%dT%H:%M:%S").to_string();
            state.record(&target, &Digest::of(&next), &at);
            state.save(&state_path).map_err(io_error(&state_path))?;
            return Ok(Outcome {
                changed: true,
                backup,
            });
        }
        Err(EngineError::Conflict {
            path: file.path.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use serde_json::json;

    use super::*;
    use crate::patch::KeyPath;
    use crate::patch::json::JsonPatch;

    struct Setup {
        _dir: tempfile::TempDir,
        engine: Engine,
        file: LiveFile,
    }

    fn setup() -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::new(DataDir::new(dir.path().join(".confdeck")));
        let file = LiveFile {
            owner: "claude-code",
            path: dir.path().join(".claude").join("settings.json"),
            mode: Mode::Private,
            format: Format::Json,
        };
        Setup {
            _dir: dir,
            engine,
            file,
        }
    }

    fn set(key: &str, value: &str) -> JsonPatch {
        JsonPatch {
            set: vec![(KeyPath::new(&["env", key]), json!(value))],
            ..JsonPatch::default()
        }
    }

    fn backups(s: &Setup) -> Vec<Reason> {
        backup::list(s.engine.data(), &s.file.shelf())
            .unwrap()
            .iter()
            .map(|b| b.reason)
            .collect()
    }

    fn text(s: &Setup) -> String {
        fs::read_to_string(&s.file.path).unwrap()
    }

    #[test]
    fn a_new_file_is_written_without_a_backup() {
        let s = setup();
        let outcome = s.engine.write(&s.file, &set("A", "1")).unwrap();
        assert!(outcome.changed && outcome.backup.is_none());
        assert_eq!(text(&s), "{\n  \"env\": {\n    \"A\": \"1\"\n  }\n}\n");
        assert_eq!(s.engine.status(&s.file).unwrap(), FileStatus::Ours);
    }

    #[test]
    fn switching_back_and_forth_backs_up_only_new_content() {
        let s = setup();
        fs::create_dir_all(s.file.path.parent().unwrap()).unwrap();
        fs::write(&s.file.path, "{\"hooks\": {}}").unwrap();
        assert_eq!(s.engine.status(&s.file).unwrap(), FileStatus::Untracked);

        // 第一次写：备份原件。
        let first = s.engine.write(&s.file, &set("A", "1")).unwrap();
        assert_eq!(first.backup.unwrap().reason, Reason::Original);

        // 连续切换：文件还是 confdeck 写的样子，不备份。
        for value in ["2", "3", "4", "1"] {
            let outcome = s.engine.write(&s.file, &set("A", value)).unwrap();
            assert!(outcome.changed && outcome.backup.is_none());
        }

        // 外部改过：下次写之前备份。
        let edited = text(&s).replace("\"hooks\": {}", "\"hooks\": {\"Stop\": []}");
        fs::write(&s.file.path, &edited).unwrap();
        assert_eq!(
            s.engine.status(&s.file).unwrap(),
            FileStatus::ChangedOutside
        );
        let outcome = s.engine.write(&s.file, &set("A", "2")).unwrap();
        let backup = outcome.backup.unwrap();
        assert_eq!(backup.reason, Reason::External);
        assert_eq!(fs::read_to_string(&backup.path).unwrap(), edited);
        assert!(
            text(&s).contains("\"Stop\": []"),
            "the external edit is kept"
        );

        assert_eq!(backups(&s), [Reason::External, Reason::Original]);
    }

    #[test]
    fn unchanged_content_writes_nothing() {
        let s = setup();
        s.engine.write(&s.file, &set("A", "1")).unwrap();
        let before = fs::metadata(&s.file.path).unwrap().modified().unwrap();
        let outcome = s.engine.write(&s.file, &set("A", "1")).unwrap();
        assert!(!outcome.changed);
        assert_eq!(
            fs::metadata(&s.file.path).unwrap().modified().unwrap(),
            before
        );
    }

    #[test]
    fn broken_files_are_left_alone() {
        let s = setup();
        fs::create_dir_all(s.file.path.parent().unwrap()).unwrap();
        fs::write(&s.file.path, "{ broken").unwrap();
        let err = s.engine.write(&s.file, &set("A", "1")).unwrap_err();
        assert!(
            matches!(err, EngineError::Patch(PatchError::Parse { .. })),
            "{err:?}"
        );
        assert_eq!(text(&s), "{ broken");
        assert!(backups(&s).is_empty());
    }

    /// 第一次 apply 时顺手改一下文件，模拟 agent 恰好在 confdeck 写入期间写了配置。
    struct RacingPatch<'a> {
        inner: JsonPatch,
        path: &'a Path,
        raced: Cell<bool>,
    }

    impl LivePatch for RacingPatch<'_> {
        fn apply(&self, path: &Path, pre: Option<&[u8]>) -> Result<Vec<u8>, PatchError> {
            if !self.raced.replace(true) {
                fs::write(self.path, "{\"model\": \"set-by-agent\"}").unwrap();
            }
            self.inner.apply(path, pre)
        }
    }

    #[test]
    fn a_concurrent_external_write_is_rebased_not_overwritten() {
        let s = setup();
        s.engine.write(&s.file, &set("A", "1")).unwrap();
        let patch = RacingPatch {
            inner: set("A", "2"),
            path: &s.file.path,
            raced: Cell::new(false),
        };
        let outcome = s.engine.write(&s.file, &patch).unwrap();
        assert!(outcome.changed);
        let value: serde_json::Value = serde_json::from_str(&text(&s)).unwrap();
        assert_eq!(value, json!({"model": "set-by-agent", "env": {"A": "2"}}));
        // 外部写入的内容在重来那一轮备份了。
        assert_eq!(outcome.backup.unwrap().reason, Reason::External);
    }

    #[test]
    fn replace_backs_up_first_and_refuses_a_stale_base() {
        let s = setup();
        s.engine.write(&s.file, &set("A", "1")).unwrap();
        let base = s.engine.read(&s.file).unwrap();

        let outcome = s
            .engine
            .replace(&s.file, b"{\"edited\": true}\n", &base)
            .unwrap();
        // 文件是 confdeck 新建的，没有原件。
        assert_eq!(outcome.backup.unwrap().reason, Reason::BeforeEdit);
        assert_eq!(text(&s), "{\"edited\": true}\n");

        // 解析不了的内容不写，也不备份。
        let current = s.engine.read(&s.file).unwrap();
        let err = s
            .engine
            .replace(&s.file, b"{\"edited\": true,}", &current)
            .unwrap_err();
        assert!(
            matches!(err, EngineError::Patch(PatchError::Parse { line: 1, .. })),
            "{err:?}"
        );
        assert_eq!(text(&s), "{\"edited\": true}\n");
        assert_eq!(backups(&s).len(), 1);

        // 用旧的 base 再保存一次：文件已经变了，报冲突，不覆盖。
        let err = s.engine.replace(&s.file, b"{}", &base).unwrap_err();
        assert!(matches!(err, EngineError::Conflict { .. }), "{err:?}");
        assert_eq!(text(&s), "{\"edited\": true}\n");
    }

    #[test]
    fn restore_brings_back_a_backup_and_keeps_the_current_content() {
        let s = setup();
        fs::create_dir_all(s.file.path.parent().unwrap()).unwrap();
        fs::write(&s.file.path, "{\"mine\": 1}").unwrap();
        let original = s
            .engine
            .write(&s.file, &set("A", "1"))
            .unwrap()
            .backup
            .unwrap();
        let switched = text(&s);

        let outcome = s.engine.restore(&s.file, &original).unwrap();
        assert_eq!(text(&s), "{\"mine\": 1}");
        let saved = outcome.backup.unwrap();
        assert_eq!(saved.reason, Reason::BeforeRestore);
        assert_eq!(fs::read_to_string(saved.path).unwrap(), switched);
    }

    #[cfg(unix)]
    #[test]
    fn writes_go_through_symlinks() {
        let s = setup();
        let dotfiles = s
            .file
            .path
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("dotfiles");
        fs::create_dir_all(&dotfiles).unwrap();
        fs::write(dotfiles.join("settings.json"), "{}").unwrap();
        fs::create_dir_all(s.file.path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(dotfiles.join("settings.json"), &s.file.path).unwrap();

        s.engine.write(&s.file, &set("A", "1")).unwrap();
        assert!(
            fs::symlink_metadata(&s.file.path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            fs::read_to_string(dotfiles.join("settings.json"))
                .unwrap()
                .contains("\"A\"")
        );
        assert_eq!(s.engine.status(&s.file).unwrap(), FileStatus::Ours);
    }
}
