//! Grok Build：`~/.grok/sessions/<编码的 cwd>/<会话>/updates.jsonl`（归档的在 `archived_sessions`）。
//!
//! `params.update.sessionUpdate == "turn_completed"` 的行带这一轮的用量（不是累计值，别做差），`modelUsage` 按模型分，
//! 一个模型记一条。`inputTokens` 含 `cachedReadTokens`、`cacheCreationTokens`，`reasoningTokens` 含在 `outputTokens` 里；
//! `costUsdTicks` 是 Grok 自己算的花费，1 tick = 1e-10 美元。
//! 子代理会话（`summary.json` 的 `session_kind` 以 `subagent` 开头）的用量已经算进父会话那一轮，不读；
//! fork 出来的会话复制了全部事件，键用 `prompt_id` 去重。
//! confdeck 写的模型表叫 `confdeck-<模型 id>`：去掉前缀，供应商记成 `confdeck`，靠切换记录认是哪家。
//!
//! 会话概况在同一个目录的 `summary.json` 里：`info.id`、`info.cwd`、`session_summary`（Grok 起的标题）、`created_at`；
//! 第一句提问是第一条 `user_message_chunk`（一句话可能拆成几块，只取第一块）。

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use super::scan::{Parsed, Parser, walk};
use super::{Request, SessionPatch, Tokens, clean_model, parse_time, prompt_text};
use crate::agent::AgentKind;
use crate::agent::grok::TABLE_PREFIX;

pub(super) fn files(home: &Path) -> Vec<PathBuf> {
    let grok = home.join(".grok");
    let mut files = walk(&grok.join("sessions"), 3, is_updates);
    files.extend(walk(&grok.join("archived_sessions"), 3, is_updates));
    files
}

fn is_updates(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == "updates.jsonl")
}

pub(super) fn open(path: &Path, state: &str) -> Box<dyn Parser> {
    let summary = path
        .parent()
        .and_then(|dir| std::fs::read_to_string(dir.join("summary.json")).ok())
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or_default();
    let skip = summary["session_kind"]
        .as_str()
        .is_some_and(|kind| kind.starts_with("subagent"));
    let text = |value: &Value| {
        value
            .as_str()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
    };
    // 老版本没有 summary.json：会话 ID 是目录名。
    let id = text(&summary["info"]["id"]).or_else(|| {
        path.parent()?
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
    });
    Box::new(Grok {
        skip,
        requests: Vec::new(),
        patch: SessionPatch {
            id,
            cwd: text(&summary["info"]["cwd"]),
            title: text(&summary["session_summary"]),
            started: summary["created_at"].as_str().and_then(parse_time),
            ..SessionPatch::default()
        },
        prompted: state == "prompted",
    })
}

struct Grok {
    skip: bool,
    requests: Vec<Request>,
    patch: SessionPatch,
    /// 第一句提问已经读到了。
    prompted: bool,
}

#[derive(Deserialize)]
struct Line {
    /// 秒。
    timestamp: Option<i64>,
    params: Params,
}

#[derive(Deserialize)]
struct Params {
    #[serde(rename = "sessionId", default)]
    session: String,
    update: Value,
    #[serde(rename = "_meta")]
    meta: Option<Meta>,
}

#[derive(Deserialize)]
struct Meta {
    #[serde(rename = "agentTimestampMs")]
    at: Option<i64>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(rename = "inputTokens", default)]
    input: u64,
    #[serde(rename = "outputTokens", default)]
    output: u64,
    #[serde(rename = "cachedReadTokens", default)]
    cache_read: u64,
    #[serde(rename = "cacheCreationTokens", default)]
    cache_write: u64,
    #[serde(rename = "reasoningTokens", default)]
    reasoning: u64,
    #[serde(rename = "costUsdTicks")]
    cost: Option<u64>,
}

impl Parser for Grok {
    fn line(&mut self, _: u64, line: &str) {
        if !self.skip && !self.prompted && line.contains("\"user_message_chunk\"") {
            let prompt = serde_json::from_str::<Value>(line).ok().and_then(|value| {
                prompt_text(value["params"]["update"]["content"]["text"].as_str()?)
            });
            if let Some(prompt) = prompt {
                self.patch.prompt = Some(prompt);
                self.prompted = true;
            }
            return;
        }
        if self.skip || !line.contains("\"turn_completed\"") {
            return;
        }
        let Ok(line) = serde_json::from_str::<Line>(line) else {
            return;
        };
        let update = &line.params.update;
        if update["sessionUpdate"] != "turn_completed" {
            return;
        }
        let Some(at) = line
            .params
            .meta
            .and_then(|meta| meta.at)
            .or(line.timestamp.map(|seconds| seconds * 1000))
        else {
            return;
        };
        let Some(prompt) = update["prompt_id"].as_str() else {
            return;
        };
        let Some(models) = update["usage"]["modelUsage"].as_object() else {
            return;
        };
        for (model, usage) in models {
            let Ok(usage) = Usage::deserialize(usage) else {
                continue;
            };
            let tokens = Tokens {
                input: usage
                    .input
                    .saturating_sub(usage.cache_read + usage.cache_write),
                cache_read: usage.cache_read,
                cache_write: usage.cache_write,
                output: usage.output,
                reasoning: usage.reasoning,
            };
            if tokens.is_zero() {
                continue;
            }
            let (name, provider) = match model.strip_prefix(TABLE_PREFIX) {
                Some(name) => (name, Some("confdeck".to_string())),
                None => (model.as_str(), None),
            };
            self.requests.push(Request {
                key: format!("grok:{prompt}:{model}"),
                agent: AgentKind::GrokBuild,
                at,
                session: line.params.session.clone(),
                model: clean_model(name),
                provider,
                tokens,
                cost: usage
                    .cost
                    .filter(|ticks| *ticks > 0)
                    .map(|ticks| ticks as f64 * 1e-10),
            });
        }
    }

    fn finish(self: Box<Self>) -> Parsed {
        Parsed {
            requests: self.requests,
            state: if self.prompted {
                "prompted".into()
            } else {
                String::new()
            },
            session: (!self.skip).then_some(self.patch),
            ..Parsed::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::open;
    use crate::usage::Tokens;

    const TURN: &str = r#"{"timestamp":1789620532,"method":"_x.ai/session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"turn_completed","prompt_id":"p1","stop_reason":"end_turn","usage":{"inputTokens":1618315,"outputTokens":39868,"cachedReadTokens":1431552,"cacheCreationTokens":0,"reasoningTokens":38619,"costUsdTicks":4516934000,"modelUsage":{"grok-4.6-build":{"inputTokens":1600000,"outputTokens":39000,"cachedReadTokens":1431552,"cacheCreationTokens":0,"reasoningTokens":38619,"costUsdTicks":4500000000},"confdeck-glm-4.6":{"inputTokens":18315,"outputTokens":868,"cachedReadTokens":0,"cacheCreationTokens":0,"reasoningTokens":0}}}},"_meta":{"agentTimestampMs":1789620532310}}}"#;

    #[test]
    fn a_turn_counts_once_per_model() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("updates.jsonl");
        let mut parser = open(&path, "");
        parser.line(0, r#"{"timestamp":1789620500,"method":"_x.ai/session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk"}}}"#);
        parser.line(10, TURN);
        let mut requests = parser.finish().requests;
        requests.sort_by(|a, b| a.key.cmp(&b.key));
        assert_eq!(requests.len(), 2);

        let glm = &requests[0];
        assert_eq!(glm.key, "grok:p1:confdeck-glm-4.6");
        assert_eq!(glm.model, "glm-4.6");
        assert_eq!(glm.provider.as_deref(), Some("confdeck"));
        assert_eq!(glm.cost, None);

        let grok = &requests[1];
        assert_eq!(grok.at, 1789620532310);
        assert_eq!(grok.session, "s1");
        assert_eq!(grok.provider, None);
        assert!((grok.cost.unwrap() - 0.45).abs() < 1e-9);
        assert_eq!(
            grok.tokens,
            Tokens {
                input: 168448,
                cache_read: 1431552,
                cache_write: 0,
                output: 39000,
                reasoning: 38619
            }
        );
    }

    #[test]
    fn subagent_sessions_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("summary.json"),
            r#"{"session_kind":"subagent"}"#,
        )
        .unwrap();
        let mut parser = open(&dir.path().join("updates.jsonl"), "");
        parser.line(0, TURN);
        assert!(parser.finish().requests.is_empty());
    }
}
