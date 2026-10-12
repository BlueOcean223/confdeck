//! Codex：`~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`，归档的挪到 `~/.codex/archived_sessions/`（文件名不变）。
//!
//! - `session_meta`：线程 ID、`model_provider`（confdeck 写的第三方都叫 `custom`，官方是 `openai`）；
//!   `turn_context.model` 是当前模型。这几样要跨行记着，存在文件的状态里，下次接着读时还在。
//! - 0.153 起每次响应写一行 `token_usage_record`，按（线程，`response_id`，`turn_id`）去重。
//! - 更早的只有 `event_msg` 里的 `token_count`，`total_token_usage` 是这个文件里的累计值，前后两条做差。
//!   本机 9 月前的 111 个文件里，每个文件的累计值都从零开始（子代理文件复制了父会话的消息，但没复制带用量的
//!   `token_count`），所以按文件做差就够了，键用文件名 + 行的位置（归档挪了目录、文件名不变）。
//!   有了 `token_usage_record` 的文件不再看 `token_count`，它们记的是同一批。
//! - `input_tokens` 含缓存读写，`output_tokens` 含推理。
//! - `token_count` 还带官方订阅的 `rate_limits`，存成 `codex` 的额度快照。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::scan::{Parsed, Parser, is_jsonl, walk};
use super::{Limit, Request, Tokens, clean_model, parse_time};
use crate::agent::AgentKind;

pub(super) fn files(home: &Path) -> Vec<PathBuf> {
    let codex = home.join(".codex");
    let mut files = walk(&codex.join("sessions"), 4, is_jsonl);
    files.extend(walk(&codex.join("archived_sessions"), 1, is_jsonl));
    files
}

pub(super) fn open(path: &Path, state: &str) -> Box<dyn Parser> {
    Box::new(Codex {
        file: path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default(),
        state: serde_json::from_str(state).unwrap_or_default(),
        requests: Vec::new(),
        limit: None,
    })
}

/// 跨行要记着的东西。
#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    thread: String,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: String,
    /// 见过 `token_usage_record` 了：之后的 `token_count` 不算。
    #[serde(default)]
    records: bool,
    /// 上一条 `token_count` 的累计值。
    #[serde(default)]
    total: Option<Raw>,
}

/// Codex 原样的用量。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Raw {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
    #[serde(default)]
    cache_write_input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    reasoning_output_tokens: u64,
}

impl Raw {
    fn tokens(&self) -> Tokens {
        Tokens {
            input: self
                .input_tokens
                .saturating_sub(self.cached_input_tokens + self.cache_write_input_tokens),
            cache_read: self.cached_input_tokens,
            cache_write: self.cache_write_input_tokens,
            output: self.output_tokens,
            reasoning: self.reasoning_output_tokens,
        }
    }

    /// 从 `before` 到这次多了多少；有哪一项变少了（累计重新开始）是 `None`。
    fn since(&self, before: &Raw) -> Option<Raw> {
        Some(Raw {
            input_tokens: self.input_tokens.checked_sub(before.input_tokens)?,
            cached_input_tokens: self
                .cached_input_tokens
                .checked_sub(before.cached_input_tokens)?,
            cache_write_input_tokens: self
                .cache_write_input_tokens
                .checked_sub(before.cache_write_input_tokens)?,
            output_tokens: self.output_tokens.checked_sub(before.output_tokens)?,
            reasoning_output_tokens: self
                .reasoning_output_tokens
                .checked_sub(before.reasoning_output_tokens)?,
        })
    }
}

struct Codex {
    file: String,
    state: State,
    requests: Vec<Request>,
    limit: Option<Limit>,
}

#[derive(Deserialize)]
struct Line {
    timestamp: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    payload: Value,
}

impl Codex {
    fn push(&mut self, key: String, at: i64, tokens: Tokens) {
        if tokens.is_zero() {
            return;
        }
        self.requests.push(Request {
            key,
            agent: AgentKind::Codex,
            at,
            session: self.state.thread.clone(),
            model: clean_model(&self.state.model),
            provider: self.state.provider.clone(),
            tokens,
            cost: None,
        });
    }

    /// 有的版本在第一轮快结束时才写 `turn_context`，这之前的请求没有模型：按后来知道的补上。
    fn fill_model(&mut self) {
        let model = clean_model(&self.state.model);
        if model.is_empty() {
            return;
        }
        for request in &mut self.requests {
            if request.model.is_empty() {
                request.model.clone_from(&model);
            }
        }
    }

    fn token_count(&mut self, offset: u64, at: i64, payload: &Value) {
        if let Some(limits) = payload.get("rate_limits").filter(|v| v.is_object())
            && self.limit.as_ref().is_none_or(|limit| at > limit.at)
        {
            self.limit = Some(Limit {
                at,
                data: limits.clone(),
            });
        }
        if self.state.records {
            return;
        }
        let info = &payload["info"];
        let Ok(total) = Raw::deserialize(&info["total_token_usage"]) else {
            return;
        };
        let delta = match &self.state.total {
            None => Some(total),
            Some(before) => total.since(before),
        };
        let delta = match delta {
            Some(delta) => delta,
            // 累计值变少了：重新开始计，这一次按 `last_token_usage` 算。
            None => Raw::deserialize(&info["last_token_usage"]).unwrap_or_default(),
        };
        self.state.total = Some(total);
        let key = format!("codex:{}:{offset}", self.file);
        self.push(key, at, delta.tokens());
    }

    fn record(&mut self, offset: u64, at: i64, payload: &Value) {
        self.state.records = true;
        let Ok(usage) = Raw::deserialize(&payload["usage"]) else {
            return;
        };
        let text = |key: &str| payload[key].as_str().unwrap_or_default().to_string();
        let thread = Some(text("thread_id"))
            .filter(|thread| !thread.is_empty())
            .unwrap_or_else(|| self.state.thread.clone());
        let response = text("response_id");
        let key = if response.is_empty() {
            format!("codex:{}:{offset}", self.file)
        } else {
            format!("codex:{thread}:{response}:{}", text("turn_id"))
        };
        self.push(key, at, usage.tokens());
    }
}

impl Parser for Codex {
    fn line(&mut self, offset: u64, line: &str) {
        // 先按行头的类型筛掉消息、工具输出这些大行，只解析要用的几种。
        let mut end = line.len().min(160);
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        let head = &line[..end];
        let wanted = [
            "session_meta",
            "turn_context",
            "token_usage_record",
            "event_msg",
        ];
        if !wanted.iter().any(|kind| head.contains(kind)) {
            return;
        }
        if head.contains("\"event_msg\"") && !line.contains("\"token_count\"") {
            return;
        }
        let Ok(line) = serde_json::from_str::<Line>(line) else {
            return;
        };
        let payload = &line.payload;
        match line.kind.as_str() {
            "session_meta" => {
                if self.state.thread.is_empty()
                    && let Some(id) = payload["id"].as_str()
                {
                    self.state.thread = id.to_string();
                    self.state.provider = payload["model_provider"].as_str().map(str::to_string);
                }
            }
            "turn_context" => {
                if let Some(model) = payload["model"].as_str() {
                    self.state.model = model.to_string();
                    self.fill_model();
                }
            }
            "token_usage_record" => {
                if let Some(at) = line.timestamp.as_deref().and_then(parse_time) {
                    self.record(offset, at, payload);
                }
            }
            "event_msg" if payload["type"] == "token_count" => {
                if let Some(at) = line.timestamp.as_deref().and_then(parse_time) {
                    self.token_count(offset, at, payload);
                }
            }
            _ => {}
        }
    }

    fn finish(mut self: Box<Self>) -> Parsed {
        self.fill_model();
        Parsed {
            requests: self.requests,
            state: serde_json::to_string(&self.state).unwrap_or_default(),
            limit: self.limit.map(|limit| ("codex", limit)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::open;
    use crate::usage::Tokens;

    const META: &str = r#"{"timestamp":"2026-09-16T03:20:00.000Z","type":"session_meta","payload":{"id":"t1","model_provider":"custom","base_instructions":{"text":"..."}}}"#;
    const CONTEXT: &str = r#"{"timestamp":"2026-09-16T03:20:01.000Z","type":"turn_context","payload":{"model":"deepseek-v4-pro","effort":"high"}}"#;

    fn count(total: (u64, u64, u64), last: (u64, u64, u64)) -> String {
        format!(
            r#"{{"timestamp":"2026-09-16T03:20:30.000Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{},"cached_input_tokens":{},"output_tokens":{},"reasoning_output_tokens":0,"total_tokens":0}},"last_token_usage":{{"input_tokens":{},"cached_input_tokens":{},"output_tokens":{},"reasoning_output_tokens":0,"total_tokens":0}}}},"rate_limits":{{"primary":{{"used_percent":21.0,"window_minutes":300}},"plan_type":"plus"}}}}}}"#,
            total.0, total.1, total.2, last.0, last.1, last.2
        )
    }

    #[test]
    fn old_files_count_the_growth_of_the_running_total() {
        let mut parser = open("/x/rollout-a.jsonl".as_ref(), "");
        parser.line(0, META);
        parser.line(10, CONTEXT);
        parser.line(20, &count((100, 60, 10), (100, 60, 10)));
        // 同一个累计值又写了一遍：不算。
        parser.line(30, &count((100, 60, 10), (100, 60, 10)));
        let parsed = parser.finish();
        assert_eq!(parsed.requests.len(), 1);
        let first = &parsed.requests[0];
        assert_eq!(first.key, "codex:rollout-a:20");
        assert_eq!(first.model, "deepseek-v4-pro");
        assert_eq!(first.provider.as_deref(), Some("custom"));
        assert_eq!(first.session, "t1");
        assert_eq!(
            first.tokens,
            Tokens {
                input: 40,
                cache_read: 60,
                cache_write: 0,
                output: 10,
                reasoning: 0
            }
        );
        let (source, limit) = parsed.limit.unwrap();
        assert_eq!(source, "codex");
        assert_eq!(limit.data["plan_type"], "plus");

        // 下次接着读：线程、模型、累计值都还在。
        let mut parser = open("/y/rollout-a.jsonl".as_ref(), &parsed.state);
        parser.line(40, &count((250, 160, 30), (150, 100, 20)));
        let parsed = parser.finish();
        assert_eq!(parsed.requests[0].model, "deepseek-v4-pro");
        assert_eq!(parsed.requests[0].tokens.input, 50);
        assert_eq!(parsed.requests[0].tokens.output, 20);
    }

    #[test]
    fn requests_before_the_first_turn_context_get_its_model() {
        let mut parser = open("/x/rollout-c.jsonl".as_ref(), "");
        parser.line(0, META);
        parser.line(10, &count((100, 60, 10), (100, 60, 10)));
        parser.line(20, CONTEXT);
        assert_eq!(parser.finish().requests[0].model, "deepseek-v4-pro");
    }

    #[test]
    fn usage_records_replace_token_counts() {
        let mut parser = open("/x/rollout-b.jsonl".as_ref(), "");
        parser.line(0, META);
        parser.line(10, CONTEXT);
        parser.line(
            20,
            r#"{"timestamp":"2026-09-16T03:20:24.844Z","ordinal":15,"type":"token_usage_record","payload":{"thread_id":"t1","turn_id":"u1","response_id":"resp_1","usage":{"input_tokens":25074,"cached_input_tokens":18176,"cache_write_input_tokens":0,"output_tokens":234,"reasoning_output_tokens":52,"total_tokens":25308}}}"#,
        );
        parser.line(30, &count((25074, 18176, 234), (25074, 18176, 234)));
        let parsed = parser.finish();
        assert_eq!(parsed.requests.len(), 1);
        let request = &parsed.requests[0];
        assert_eq!(request.key, "codex:t1:resp_1:u1");
        assert_eq!(request.at, 1789528824844);
        assert_eq!(request.tokens.input, 25074 - 18176);
        assert_eq!(request.tokens.reasoning, 52);
        // 额度照样从 token_count 拿。
        assert!(parsed.limit.is_some());
    }
}
