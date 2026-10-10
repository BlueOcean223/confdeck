//! confdeck 自己的数据目录 `~/.confdeck/`。
//!
//! - `providers.json`：供应商（[`crate::store`]）。
//! - `state.json`：每个 agent 配置文件上次由 confdeck 写入后的 hash，只属于这台设备，不同步。
//! - `backups/`：备份（[`crate::backup`]），有 Key，不同步。
//! - `.lock`：写入时的跨进程锁。

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDir {
    root: PathBuf,
}

impl DataDir {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `~/.confdeck/`；找不到家目录时为 `None`。
    pub fn default_location() -> Option<Self> {
        std::env::home_dir().map(|home| Self::new(home.join(".confdeck")))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn providers(&self) -> PathBuf {
        self.root.join("providers.json")
    }

    pub fn state(&self) -> PathBuf {
        self.root.join("state.json")
    }

    pub fn backups(&self) -> PathBuf {
        self.root.join("backups")
    }

    pub fn lock_file(&self) -> PathBuf {
        self.root.join(".lock")
    }
}
