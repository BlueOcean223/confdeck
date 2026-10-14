//! 用量：扫各 agent 的本地会话记录，每次请求存一行进 SQLite（[`Db`]，`~/.confdeck/cache/usage.db`，丢了能重算），
//! 查的时候再认是哪家供应商、按现在的价格算花费（[`summary`]）。
//!
//! - [`scan`]：增量扫，记着每个文件读到哪。各家记录的格式在 `claude`、`codex`、`pi`、`grok`、`opencode` 里。
//! - [`history`]：confdeck 把单选 agent 切到哪家的记录。Claude Code、Codex、Grok Build 的会话记录里没有接口地址，
//!   靠它认一段时间里连的是哪家；它不能重算，所以放数据目录、不放缓存。
//! - [`summary`]：认供应商、算花费，一段时间的总量和按天、按供应商 / agent / 模型的分组。
//! - 扫的时候顺带记下每个会话的概况（目录、第一句提问、标题、最后活动时间），会话页用（[`crate::session`]）。

mod claude;
mod codex;
mod db;
mod grok;
pub mod history;
mod opencode;
mod pi;
mod scan;
pub mod summary;

use std::path::PathBuf;

use crate::agent::AgentKind;

pub use db::Db;
pub use scan::{Report, scan};

#[derive(Debug, thiserror::Error)]
pub enum UsageError {
    #[error("{path}: {source}")]
    Db {
        path: PathBuf,
        source: rusqlite::Error,
    },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// 会话概况的一笔更新：解析器读到新内容时交出来，库里合进去——ID、目录、第一句提问、开始时间已经有了就不改；
/// 名字、标题用新的；最后活动时间取文件的修改时间。
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct SessionPatch {
    pub id: Option<String>,
    pub cwd: Option<String>,
    pub prompt: Option<String>,
    /// 用户起的名字（Claude 的 /rename、Pi 的 `session_info`）；`Some("")` 是清掉了。
    pub name: Option<String>,
    /// agent 自己起的标题（Claude 的 `ai-title`、Grok 的 `session_summary`）。
    pub title: Option<String>,
    pub started: Option<i64>,
}

/// 第一句提问：去掉首尾空白、连续的空白并成一个，最多 200 个字。是空的、或者是 agent 自己插的（`<` 开头）返回 `None`。
pub(crate) fn prompt_text(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() || text.starts_with('<') {
        return None;
    }
    let mut out = String::new();
    for word in text.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
        if out.chars().count() >= 200 {
            break;
        }
    }
    Some(out.chars().take(200).collect())
}

/// 一次请求的 token，各家的口径统一成这样（见各 agent 的模块）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tokens {
    /// 没命中缓存、也不是写缓存的输入。
    pub input: u64,
    /// 命中缓存的输入。
    pub cache_read: u64,
    /// 写进缓存的输入。
    pub cache_write: u64,
    /// 输出，含推理。
    pub output: u64,
    /// 推理（含在 `output` 里）。
    pub reasoning: u64,
}

impl Tokens {
    pub fn add(&mut self, other: &Tokens) {
        self.input += other.input;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.output += other.output;
        self.reasoning += other.reasoning;
    }

    /// 不算缓存的输入加输出：用量页的「Token」。
    pub fn in_out(&self) -> u64 {
        self.input + self.output
    }

    /// 全部输入（含缓存）加输出。
    pub fn total(&self) -> u64 {
        self.input + self.cache_read + self.cache_write + self.output
    }

    /// 缓存命中率：命中缓存的占全部输入的比例；没有输入是 `None`。
    pub fn hit_rate(&self) -> Option<f64> {
        let all = self.input + self.cache_read + self.cache_write;
        (all > 0).then(|| self.cache_read as f64 / all as f64)
    }

    fn is_zero(&self) -> bool {
        self.total() == 0
    }
}

/// 会话记录里的一次请求。
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    /// 去重用的键：同一次请求在几个文件里出现（resume、fork 复制了历史）时一样。
    pub key: String,
    pub agent: AgentKind,
    /// 毫秒时间戳。
    pub at: i64,
    pub session: String,
    pub model: String,
    /// 记录里写的供应商：Pi、OpenCode 是配置里的键（confdeck 写的是 `confdeck-<id>`），Codex 是 `model_provider`，
    /// Grok Build 是 confdeck 写的模型表时为 `confdeck`。
    pub provider: Option<String>,
    pub tokens: Tokens,
    /// agent 自己算的花费（美元）：Pi、Grok Build 有。
    pub cost: Option<f64>,
}

/// ISO 8601 时间（`2026-10-12T05:09:24.482Z`）转毫秒时间戳。
fn parse_time(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|time| time.timestamp_millis())
}

/// `[1m]` 这类 Claude Code 的后缀去掉，供应商目录里的模型 ID 不带它。
fn clean_model(model: &str) -> String {
    let model = model.trim();
    let lower = model.to_ascii_lowercase();
    match lower.strip_suffix("[1m]") {
        Some(rest) => model[..rest.len()].trim().to_string(),
        None => model.to_string(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn model_suffixes_are_dropped() {
        assert_eq!(super::clean_model("deepseek-v4-pro[1m]"), "deepseek-v4-pro");
        assert_eq!(super::clean_model("claude-opus-5-5[1M]"), "claude-opus-5-5");
        assert_eq!(super::clean_model(" glm-5.3 "), "glm-5.3");
    }

    #[test]
    fn hit_rate_counts_all_input() {
        let tokens = super::Tokens {
            input: 10,
            cache_read: 80,
            cache_write: 10,
            output: 5,
            reasoning: 0,
        };
        assert_eq!(tokens.hit_rate(), Some(0.8));
        assert_eq!(super::Tokens::default().hit_rate(), None);
    }
}
