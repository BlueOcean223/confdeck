//! Pi：`~/.pi/agent/sessions/<编码的 cwd>/<时间>_<id>.jsonl`。
//!
//! `type == "message"` 且 `message.role == "assistant"` 的行带 `message.usage`；`branch_summary`、`compaction`
//! 这类总结也花 token，用量在行上的 `usage`。供应商直接写在 `message.provider` 里（confdeck 写的是 `confdeck-<id>`）。
//! `input` 不含缓存，`reasoning` 含在 `output` 里；`cost.total` 是 Pi 按它自己的价格算的美元。
//! 会话是树，换分支不复制；`/fork` 出来的新文件复制整段历史，键用（时间、供应商、模型、responseId）去重。
//!
//! 会话概况：第一行 `type == "session"` 带 `id`、`cwd`、`timestamp`；第一条 `role: user` 的消息是第一句提问；
//! `session_info` 行的 `name` 是用户起的名字（空的是清掉）。

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use super::scan::{Parsed, Parser, is_jsonl, walk};
use super::{Request, SessionPatch, Tokens, clean_model, parse_time, prompt_text};
use crate::agent::AgentKind;

pub(super) fn files(home: &Path) -> Vec<PathBuf> {
    walk(
        &home.join(".pi").join("agent").join("sessions"),
        2,
        is_jsonl,
    )
}

pub(super) fn open(path: &Path, state: &str) -> Box<dyn Parser> {
    // 文件名是 `<时间>_<会话 id>.jsonl`。
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let session = stem.rsplit('_').next().unwrap_or(&stem).to_string();
    Box::new(Pi {
        session,
        requests: Vec::new(),
        patch: SessionPatch::default(),
        // 状态只记一件事：第一句提问已经读到了。
        prompted: state == "prompted",
    })
}

struct Pi {
    session: String,
    requests: Vec<Request>,
    patch: SessionPatch,
    prompted: bool,
}

impl Pi {
    fn overview(&mut self, line: &str) {
        let header = line.starts_with("{\"type\":\"session\"");
        let named = line.contains("\"session_info\"");
        let asks = !self.prompted && line.contains("\"role\":\"user\"");
        if !header && !named && !asks {
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return;
        };
        match value["type"].as_str() {
            Some("session") => {
                self.patch.id = value["id"].as_str().map(str::to_string);
                self.patch.cwd = value["cwd"].as_str().map(str::to_string);
                self.patch.started = value["timestamp"].as_str().and_then(parse_time);
            }
            Some("session_info") => {
                self.patch.name = Some(
                    value["name"]
                        .as_str()
                        .unwrap_or_default()
                        .trim()
                        .to_string(),
                );
            }
            Some("message") if asks && value["message"]["role"] == "user" => {
                let content = &value["message"]["content"];
                let first = content.as_str().or_else(|| {
                    content
                        .as_array()?
                        .iter()
                        .find(|block| block["type"] == "text")?["text"]
                        .as_str()
                });
                if let Some(prompt) = first.and_then(prompt_text) {
                    self.patch.prompt = Some(prompt);
                    self.prompted = true;
                }
            }
            _ => {}
        }
    }
}

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type")]
    kind: String,
    timestamp: Option<String>,
    message: Option<Value>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    input: u64,
    #[serde(default)]
    output: u64,
    #[serde(rename = "cacheRead", default)]
    cache_read: u64,
    #[serde(rename = "cacheWrite", default)]
    cache_write: u64,
    #[serde(default)]
    reasoning: u64,
    cost: Option<Cost>,
}

#[derive(Deserialize)]
struct Cost {
    total: Option<f64>,
}

impl Parser for Pi {
    fn line(&mut self, _: u64, line: &str) {
        self.overview(line);
        if !line.contains("\"usage\"") {
            return;
        }
        let Ok(line) = serde_json::from_str::<Line>(line) else {
            return;
        };
        let at = line.timestamp.as_deref().and_then(parse_time);
        let (usage, provider, model, response, at) = match (line.kind.as_str(), line.message) {
            ("message", Some(message)) => {
                if message["role"] != "assistant" {
                    return;
                }
                let Ok(usage) = Usage::deserialize(&message["usage"]) else {
                    return;
                };
                let text = |key: &str| message[key].as_str().map(str::to_string);
                // 消息里的时间是毫秒数，比行上的准（行上是写文件的时间）。
                let at = message["timestamp"].as_i64().or(at);
                (
                    usage,
                    text("provider"),
                    text("model"),
                    text("responseId"),
                    at,
                )
            }
            (_, _) => match line.usage {
                Some(usage) => (usage, None, None, None, at),
                None => return,
            },
        };
        let Some(at) = at else {
            return;
        };
        let tokens = Tokens {
            input: usage.input,
            cache_read: usage.cache_read,
            cache_write: usage.cache_write,
            output: usage.output,
            reasoning: usage.reasoning,
        };
        if tokens.is_zero() {
            return;
        }
        let key = format!(
            "pi:{at}:{}:{}:{}:{}",
            line.kind,
            provider.as_deref().unwrap_or_default(),
            model.as_deref().unwrap_or_default(),
            response.as_deref().unwrap_or_default(),
        );
        self.requests.push(Request {
            key,
            agent: AgentKind::Pi,
            at,
            session: self.session.clone(),
            model: model.map(|model| clean_model(&model)).unwrap_or_default(),
            provider,
            tokens,
            cost: usage
                .cost
                .and_then(|cost| cost.total)
                .filter(|cost| *cost > 0.),
        });
    }

    fn finish(self: Box<Self>) -> Parsed {
        Parsed {
            requests: self.requests,
            state: if self.prompted {
                "prompted".into()
            } else {
                String::new()
            },
            session: Some(self.patch),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::open;
    use crate::usage::Tokens;

    const ASSISTANT: &str = r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-09-18T05:05:08.901Z","message":{"role":"assistant","content":[{"type":"text","text":"ok"}],"api":"openai-completions","provider":"confdeck-deepseek","model":"deepseek-v4-pro","responseId":"r1","usage":{"input":4,"output":104,"cacheRead":0,"cacheWrite":5956,"totalTokens":6064,"cost":{"input":0.0000004,"output":0.000052,"cacheRead":0,"cacheWrite":0.0007445,"total":0.0007969},"reasoning":59},"stopReason":"stop","timestamp":1789707904344}}"#;

    #[test]
    fn assistant_messages_carry_their_provider() {
        let mut parser = open("/s/2026-09-18T05-05-04-120Z_abc.jsonl".as_ref(), "");
        parser.line(0, r#"{"type":"session","version":3,"id":"abc","timestamp":"2026-09-18T05:05:04.120Z","cwd":"/w"}"#);
        parser.line(10, r#"{"type":"message","id":"u1","timestamp":"2026-09-18T05:05:04.200Z","message":{"role":"user","content":"hi"}}"#);
        parser.line(20, ASSISTANT);
        let parsed = parser.finish();
        assert_eq!(parsed.requests.len(), 1);
        let request = &parsed.requests[0];
        assert_eq!(request.session, "abc");
        assert_eq!(request.provider.as_deref(), Some("confdeck-deepseek"));
        assert_eq!(request.model, "deepseek-v4-pro");
        assert_eq!(request.at, 1789707904344);
        assert_eq!(request.cost, Some(0.0007969));
        assert_eq!(
            request.tokens,
            Tokens {
                input: 4,
                cache_read: 0,
                cache_write: 5956,
                output: 104,
                reasoning: 59
            }
        );
    }

    #[test]
    fn a_forked_copy_has_the_same_key() {
        let key = |path: &str| {
            let mut parser = open(path.as_ref(), "");
            parser.line(0, ASSISTANT);
            parser.finish().requests[0].key.clone()
        };
        assert_eq!(key("/s/1_abc.jsonl"), key("/s/2_def.jsonl"));
    }
}
