//! `~/.confdeck/state.json`：只属于这台设备的记录，不同步。
//!
//! - `files`：每个 agent 配置文件上次由 confdeck 写入后的 hash。写之前拿文件的当前内容和这里比：
//!   一样说明文件还是 confdeck 上次写的样子，不用备份；不一样说明被外部改过。
//! - `selections`：单选 agent 上次在 confdeck 里选的是哪家（`null` 是「默认」，没有这一项是没选过）。
//!   认当前是哪家时，同一个地址和 Key 对得上好几家，就以它为准（见 [`crate::current`]）。
//!
//! 这份记录只是提示，丢了或读不出来就当作空的：后果只是多备份一次、认不出同地址的几家里是哪一家，
//! 所以不像用户的文件那样「读不出就报错」。

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::fsx::{self, Digest, Mode};

const VERSION: u32 = 1;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    version: u32,
    /// 键是解析过软链接的绝对路径。
    #[serde(default)]
    files: BTreeMap<String, Record>,
    /// 键是 agent 的 id。
    #[serde(default)]
    selections: BTreeMap<String, Option<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub sha256: String,
    /// 本地时间，`2026-10-10T15:00:02`。
    pub written_at: String,
}

impl State {
    pub fn load(path: &Path) -> Self {
        fsx::read_optional(path)
            .ok()
            .flatten()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&mut self, path: &Path) -> io::Result<()> {
        self.version = VERSION;
        let mut bytes = serde_json::to_vec_pretty(self).expect("state always serializes");
        bytes.push(b'\n');
        fsx::write_atomic(path, &bytes, Mode::Private)
    }

    pub fn get(&self, target: &Path) -> Option<&Record> {
        self.files.get(&key(target))
    }

    /// 文件的当前内容是不是 confdeck 上次写的。
    pub fn is_ours(&self, target: &Path, current: Option<&Digest>) -> bool {
        match (self.get(target), current) {
            (Some(record), Some(current)) => record.sha256 == current.0,
            _ => false,
        }
    }

    /// 上次选的：`None` 是没选过，`Some(None)` 是「默认」。
    pub fn selection(&self, agent: &str) -> Option<Option<&str>> {
        self.selections.get(agent).map(Option::as_deref)
    }

    pub fn select(&mut self, agent: &str, provider_id: Option<&str>) {
        self.selections
            .insert(agent.to_string(), provider_id.map(str::to_string));
    }

    pub fn record(&mut self, target: &Path, written: &Digest, at: &str) {
        self.files.insert(
            key(target),
            Record {
                sha256: written.0.clone(),
                written_at: at.to_string(),
            },
        );
    }
}

fn key(target: &Path) -> String {
    target.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn round_trips_and_tolerates_a_broken_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let target = Path::new("/home/u/.claude/settings.json");
        let digest = Digest::of(b"x");

        let mut state = State::load(&path);
        assert!(!state.is_ours(target, Some(&digest)));
        state.record(target, &digest, "2026-10-10T15:00:02");
        state.save(&path).unwrap();

        let state = State::load(&path);
        assert!(state.is_ours(target, Some(&digest)));
        assert!(!state.is_ours(target, Some(&Digest::of(b"y"))));
        assert!(!state.is_ours(target, None));

        let mut state = State::load(&path);
        assert_eq!(state.selection("codex"), None);
        state.select("codex", None);
        state.select("claude-code", Some("relay"));
        state.save(&path).unwrap();
        let state = State::load(&path);
        assert_eq!(state.selection("codex"), Some(None));
        assert_eq!(state.selection("claude-code"), Some(Some("relay")));
        assert!(state.is_ours(target, Some(&digest)), "files are kept");

        fs::write(&path, "{ broken").unwrap();
        assert!(State::load(&path).get(target).is_none());
    }
}
