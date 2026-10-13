//! 单选 agent 连到哪家的记录：`~/.confdeck/history.jsonl`，一行一次变化。
//!
//! Claude Code、Codex（第三方都叫 `custom`）、Grok Build（confdeck 写的模型表）的会话记录里看不出连的是哪家，
//! 用量页靠它认一段时间里的请求算谁的。confdeck 切换时记一笔；界面每次认出 agent 现在连的是哪家也记一笔，
//! 这样在别处改了配置，只要 confdeck 开着或下次打开，也能对上。和上一笔一样不记。
//! 这份记录重算不出来，所以放数据目录，不放缓存。

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
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
    Ok(note_all(data, vec![(agent, to)])? == 1)
}

/// 找各 agent 的上一笔时只读文件末尾这么多字节；一笔一百来字节，够一百多笔。
const TAIL: u64 = 16 * 1024;

/// 记下几个 agent 现在连到哪（界面每次重读时一起记），和各自的上一笔一样的不记。返回记了几笔。
/// 上一笔只在文件末尾 [`TAIL`] 字节里找，找不到的也记一笔：[`at`] 只认时间上最近的一笔，同样的一笔再记一次不改归属，
/// 这样每次最多读这么多。
pub fn note_all(data: &DataDir, now: Vec<(AgentKind, Target)>) -> Result<usize, UsageError> {
    if now.is_empty() {
        return Ok(0);
    }
    let path = data.history();
    let io = |source| UsageError::Io {
        path: path.clone(),
        source,
    };
    let lasts = last_of(&path).map_err(io)?;
    let at = chrono::Utc::now().timestamp_millis();
    let mut text = String::new();
    let mut count = 0;
    for (agent, to) in now {
        if lasts.iter().any(|(a, last)| *a == agent && *last == to) {
            continue;
        }
        let switch = Switch { at, agent, to };
        text.push_str(
            &serde_json::to_string(&switch).map_err(|err| io(std::io::Error::other(err)))?,
        );
        text.push('\n');
        count += 1;
    }
    if count > 0 {
        append(&path, &text).map_err(io)?;
    }
    Ok(count)
}

/// 文件末尾 [`TAIL`] 字节里各 agent 的上一笔。
fn last_of(path: &Path) -> std::io::Result<Vec<(AgentKind, Target)>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let start = file.metadata()?.len().saturating_sub(TAIL);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    // 从中间开始读的，第一行多半是半行。
    let lines = text.lines().skip(usize::from(start > 0));
    let mut lasts: Vec<(AgentKind, Target)> = Vec::new();
    for switch in lines.filter_map(|line| serde_json::from_str::<Switch>(line).ok()) {
        lasts.retain(|(agent, _)| *agent != switch.agent);
        lasts.push((switch.agent, switch.to));
    }
    Ok(lasts)
}

fn append(path: &Path, lines: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // 一次 write，追加模式下不会和别的进程写的行交错。
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(lines.as_bytes())
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
    use super::{Target, at, load, note, note_all};
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
    fn a_switch_older_than_the_tail_is_noted_again() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::new(dir.path());
        // Codex 的上一笔在很前面，末尾一段里全是 Claude Code 的。
        assert!(note(&data, AgentKind::Codex, provider("old")).unwrap());
        for n in 0..300 {
            note(&data, AgentKind::ClaudeCode, provider(&format!("p{n}"))).unwrap();
        }
        assert!(std::fs::metadata(data.history()).unwrap().len() > super::TAIL);
        let noted = note_all(
            &data,
            vec![
                (AgentKind::ClaudeCode, provider("p299")),
                (AgentKind::Codex, provider("old")),
                (AgentKind::GrokBuild, Target::Official),
            ],
        )
        .unwrap();
        assert_eq!(noted, 2, "Codex is past the tail, Grok Build changed");
        let switches = load(&data).unwrap();
        assert_eq!(switches.len(), 303);
        let at_end = switches.last().unwrap().at;
        assert_eq!(
            at(&switches, AgentKind::Codex, at_end),
            Some(&provider("old"))
        );
        // 记过一次就在末尾了，不再重记。
        let noted = note_all(&data, vec![(AgentKind::Codex, provider("old"))]).unwrap();
        assert_eq!(noted, 0);
    }

    #[test]
    fn the_tail_alone_finds_recent_switches() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::new(dir.path());
        for n in 0..300 {
            note(&data, AgentKind::ClaudeCode, provider(&format!("p{n}"))).unwrap();
        }
        assert!(note(&data, AgentKind::Codex, provider("x")).unwrap());
        assert!(std::fs::metadata(data.history()).unwrap().len() > super::TAIL);
        // 末尾那段从一行中间开始：半行跳过，两个的上一笔都在那段里。
        let noted = note_all(
            &data,
            vec![
                (AgentKind::ClaudeCode, provider("p299")),
                (AgentKind::Codex, provider("x")),
            ],
        )
        .unwrap();
        assert_eq!(noted, 0);
        let noted = note_all(&data, vec![(AgentKind::ClaudeCode, provider("p298"))]).unwrap();
        assert_eq!(noted, 1, "p298 is not the last one");
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
