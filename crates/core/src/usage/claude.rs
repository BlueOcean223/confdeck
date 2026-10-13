//! Claude Code：`~/.claude/projects/<编码的 cwd>/<session>.jsonl`，子代理在 `<session>/subagents/agent-*.jsonl`。
//!
//! `type == "assistant"` 的行带 `message.usage`。同一次回复按内容块拆成几行，用量都一样，按（`message.id`，`requestId`）去重；
//! resume 出来的新文件带着旧消息，也靠这个键去重。`input_tokens` 不含缓存。
//! 模型取 `requestedModel`（Claude Code 请求时用的、也就是配置里写的那个），老版本没有它时取回复里的 `message.model`。
//!
//! 会话概况：会话 ID 是文件名，目录和开始时间取第一句提问那行的 `cwd`、`timestamp`；`custom-title` 行是 /rename 起的名字，
//! `ai-title` 行是 Claude Code 自己起的标题（都会反复追加，取最新的）。子代理的文件不是会话。

use std::path::{Path, PathBuf};

use serde::Deserialize;

use serde_json::Value;

use super::scan::{Parsed, Parser, is_jsonl, walk};
use super::{Request, SessionPatch, Tokens, clean_model, parse_time, prompt_text};
use crate::agent::AgentKind;

pub(super) fn files(home: &Path) -> Vec<PathBuf> {
    walk(&home.join(".claude").join("projects"), 4, is_jsonl)
}

pub(super) fn open(path: &Path, state: &str) -> Box<dyn Parser> {
    let subagent = path
        .components()
        .any(|part| part.as_os_str() == "subagents");
    Box::new(Claude {
        requests: Vec::new(),
        session: (!subagent).then(|| SessionPatch {
            id: path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned()),
            ..SessionPatch::default()
        }),
        // 状态只记一件事：第一句提问已经读到了，后面的提问不用再解析。
        prompted: state == "prompted",
    })
}

struct Claude {
    requests: Vec<Request>,
    session: Option<SessionPatch>,
    prompted: bool,
}

impl Claude {
    fn session(&mut self, line: &str) {
        let Some(patch) = self.session.as_mut() else {
            return;
        };
        let titled = line.contains("-title\"");
        let asks = !self.prompted && line.contains("\"user\"");
        if !titled && !asks {
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let text = |key: &str| value[key].as_str().map(|t| t.trim().to_string());
        match value["type"].as_str() {
            Some("custom-title") => patch.name = text("customTitle"),
            Some("ai-title") => patch.title = text("aiTitle").filter(|t| !t.is_empty()),
            Some("user") if asks && value["isMeta"] != true => {
                let content = &value["message"]["content"];
                let first = content.as_str().or_else(|| {
                    content
                        .as_array()?
                        .iter()
                        .find(|block| block["type"] == "text")?["text"]
                        .as_str()
                });
                if let Some(prompt) = first.and_then(prompt_text) {
                    patch.prompt = Some(prompt);
                    patch.cwd = text("cwd");
                    patch.started = value["timestamp"].as_str().and_then(parse_time);
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
    kind: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "sessionId", default)]
    session: String,
    #[serde(rename = "requestId")]
    request: Option<String>,
    #[serde(rename = "requestedModel")]
    requested: Option<String>,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    id: Option<String>,
    model: Option<String>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    output_tokens_details: Option<OutputDetails>,
}

#[derive(Deserialize)]
struct OutputDetails {
    #[serde(default)]
    thinking_tokens: u64,
}

impl Parser for Claude {
    fn line(&mut self, offset: u64, line: &str) {
        self.session(line);
        if !line.contains("\"usage\"") || !line.contains("\"assistant\"") {
            return;
        }
        let Ok(line) = serde_json::from_str::<Line>(line) else {
            return;
        };
        if line.kind.as_deref() != Some("assistant") {
            return;
        }
        let Some(message) = line.message else {
            return;
        };
        let (Some(usage), Some(at)) = (
            message.usage,
            line.timestamp.as_deref().and_then(parse_time),
        ) else {
            return;
        };
        let model = line
            .requested
            .filter(|model| !model.trim().is_empty())
            .or(message.model)
            .unwrap_or_default();
        // 不是真的请求：Claude Code 自己插的提示。
        if model.is_empty() || model == "<synthetic>" {
            return;
        }
        let tokens = Tokens {
            input: usage.input_tokens,
            cache_read: usage.cache_read_input_tokens,
            cache_write: usage.cache_creation_input_tokens,
            output: usage.output_tokens,
            reasoning: usage.output_tokens_details.map_or(0, |d| d.thinking_tokens),
        };
        if tokens.is_zero() {
            return;
        }
        let key = match (message.id, line.request) {
            (Some(id), Some(request)) => format!("claude:{id}:{request}"),
            (Some(id), None) => format!("claude:{id}"),
            _ => format!("claude:{}:{offset}", line.session),
        };
        self.requests.push(Request {
            key,
            agent: AgentKind::ClaudeCode,
            at,
            session: line.session,
            model: clean_model(&model),
            provider: None,
            tokens,
            cost: None,
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
            session: self.session,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::open;
    use crate::usage::Tokens;

    #[test]
    fn assistant_lines_become_requests() {
        let mut parser = open("x.jsonl".as_ref(), "");
        parser.line(0, r#"{"type":"user","message":{"role":"user","content":"hi"},"timestamp":"2026-10-12T05:09:20.000Z"}"#);
        parser.line(
            100,
            r#"{"parentUuid":"p","message":{"model":"deepseek-v4-pro","id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"a"}],"usage":{"input_tokens":2,"cache_creation_input_tokens":16,"cache_read_input_tokens":23,"output_tokens":179,"output_tokens_details":{"thinking_tokens":20}}},"requestId":"req_1","requestedModel":"deepseek-v4-pro[1m]","type":"assistant","timestamp":"2026-10-12T05:09:24.482Z","sessionId":"s1"}"#,
        );
        parser.line(
            200,
            r#"{"message":{"model":"<synthetic>","id":"msg_2","role":"assistant","content":[],"usage":{"input_tokens":0,"output_tokens":0}},"type":"assistant","timestamp":"2026-10-12T05:09:25.000Z","sessionId":"s1"}"#,
        );
        let parsed = parser.finish();
        assert_eq!(parsed.requests.len(), 1);
        let request = &parsed.requests[0];
        assert_eq!(request.key, "claude:msg_1:req_1");
        assert_eq!(request.model, "deepseek-v4-pro");
        assert_eq!(request.session, "s1");
        assert_eq!(request.at, 1791781764482);
        assert_eq!(
            request.tokens,
            Tokens {
                input: 2,
                cache_read: 23,
                cache_write: 16,
                output: 179,
                reasoning: 20,
            }
        );
    }
}
