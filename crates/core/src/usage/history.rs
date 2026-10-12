//! 单选 agent 连到哪家的记录：`~/.confdeck/history.jsonl`，一行一次变化。
//!
//! Claude Code、Codex（第三方都叫 `custom`）、Grok Build（confdeck 写的模型表）的会话记录里看不出连的是哪家，
//! 用量页靠它认一段时间里的请求算谁的。confdeck 切换时记一笔；界面每次认出 agent 现在连的是哪家也记一笔，
//! 这样在别处改了配置，只要 confdeck 开着或下次打开，也能对上。和上一笔一样不记。
//! 这份记录重算不出来，所以放数据目录，不放缓存。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::UsageError;
use crate::agent::AgentKind;
use crate::data::DataDir;
use crate::provider::Provider;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Switch {
    /// 毫秒时间戳。
    pub at: i64,
    pub agent: AgentKind,
    pub to: Target,
}

/// 连到哪儿。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Target {
    /// confdeck 的一家供应商；名字是当时的，供应商删了还能显示。
    Provider { id: String, name: String },
    /// agent 自带的登录。
    Official,
    /// 认不出（在别处改过，confdeck 显示「自定义」）。
    Other,
}

impl Target {
    /// 切到 `provider`；`None` 是 agent 自带的登录。
    pub fn of(provider: Option<&Provider>) -> Self {
        match provider {
            Some(provider) => Self::Provider {
                id: provider.id.clone(),
                name: provider.name.clone(),
            },
            None => Self::Official,
        }
    }
}

/// 全部记录，按时间先后。文件不在是空的；坏行跳过。
pub fn load(data: &DataDir) -> Result<Vec<Switch>, UsageError> {
    let path = data.history();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(UsageError::Io { path, source }),
    };
    let mut switches: Vec<Switch> = text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    switches.sort_by_key(|switch| switch.at);
    Ok(switches)
}

/// 记下 `agent` 现在连到 `to`；和这个 agent 的上一笔一样就不记。返回记了没有。
pub fn note(data: &DataDir, agent: AgentKind, to: Target) -> Result<bool, UsageError> {
    let last = load(data)?
        .into_iter()
        .rev()
        .find(|switch| switch.agent == agent);
    if last.is_some_and(|last| last.to == to) {
        return Ok(false);
    }
    let switch = Switch {
        at: chrono::Utc::now().timestamp_millis(),
        agent,
        to,
    };
    let path = data.history();
    append(&path, &switch).map_err(|source| UsageError::Io { path, source })?;
    Ok(true)
}

fn append(path: &Path, switch: &Switch) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut line = serde_json::to_string(switch).map_err(std::io::Error::other)?;
    line.push('\n');
    // 一行一次 write，追加模式下不会和别的进程写的行交错。
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(line.as_bytes())
}

/// `switches`（按时间先后）里 `agent` 在 `at` 时连的是哪儿；那之前没有记录是 `None`。
pub fn at(switches: &[Switch], agent: AgentKind, at: i64) -> Option<&Target> {
    switches
        .iter()
        .rev()
        .find(|switch| switch.agent == agent && switch.at <= at)
        .map(|switch| &switch.to)
}

#[cfg(test)]
mod tests {
    use super::{Target, at, load, note};
    use crate::agent::AgentKind;
    use crate::data::DataDir;

    fn provider(id: &str) -> Target {
        Target::Provider {
            id: id.into(),
            name: id.to_uppercase(),
        }
    }

    #[test]
    fn only_changes_are_noted() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::new(dir.path());
        assert!(load(&data).unwrap().is_empty());
        assert!(note(&data, AgentKind::ClaudeCode, provider("a")).unwrap());
        assert!(!note(&data, AgentKind::ClaudeCode, provider("a")).unwrap());
        assert!(note(&data, AgentKind::Codex, provider("a")).unwrap());
        assert!(note(&data, AgentKind::ClaudeCode, Target::Official).unwrap());
        let switches = load(&data).unwrap();
        assert_eq!(switches.len(), 3);
        assert_eq!(switches[2].to, Target::Official);

        let line = std::fs::read_to_string(data.history()).unwrap();
        assert!(
            line.starts_with(r#"{"at":"#)
                && line.contains(
                    r#""agent":"claude-code","to":{"kind":"provider","id":"a","name":"A"}}"#
                ),
            "{line}"
        );
    }

    #[test]
    fn the_last_switch_before_a_request_wins() {
        let switch = |at, agent, to| super::Switch { at, agent, to };
        let switches = [
            switch(10, AgentKind::ClaudeCode, provider("a")),
            switch(20, AgentKind::Codex, provider("b")),
            switch(30, AgentKind::ClaudeCode, Target::Official),
        ];
        assert_eq!(at(&switches, AgentKind::ClaudeCode, 5), None);
        assert_eq!(
            at(&switches, AgentKind::ClaudeCode, 25),
            Some(&provider("a"))
        );
        assert_eq!(
            at(&switches, AgentKind::ClaudeCode, 30),
            Some(&Target::Official)
        );
        assert_eq!(at(&switches, AgentKind::Codex, 99), Some(&provider("b")));
    }
}
