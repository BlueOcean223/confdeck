//! Pi：`~/.pi/agent/models.json`。多选 agent，列表型。
//!
//! 每家供应商是 `providers` 下的一项，键是 `confdeck-<id>`；勾上就整项写入，取消就整项删掉，
//! 用户自己写的其他项不动。Pi 读这个文件时允许注释（JSONC），写回后注释会丢。
//! 默认模型存在 Pi 自己的 `settings.json`，confdeck 只读不写。

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use super::{AdaptError, AgentKind, choose_protocol, list_models};
use crate::patch::KeyPath;
use crate::patch::json::JsonPatch;
use crate::provider::{Model, Protocol, Provider};

const AGENT: AgentKind = AgentKind::Pi;

/// confdeck 写的项的键前缀，用来和用户自己写的项区分。
pub const KEY_PREFIX: &str = "confdeck-";

pub fn models_path(home: &Path) -> PathBuf {
    home.join(".pi").join("agent").join("models.json")
}

pub fn provider_key(provider_id: &str) -> String {
    format!("{KEY_PREFIX}{provider_id}")
}

/// Pi 的 `api` 字段。
pub fn api_name(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Anthropic => "anthropic-messages",
        Protocol::OpenaiChat => "openai-completions",
        Protocol::OpenaiResponses => "openai-responses",
        Protocol::Gemini => "google-generative-ai",
    }
}

/// 勾上这家供应商：写入（或整项替换）`providers.confdeck-<id>`。
pub fn add(provider: &Provider) -> Result<JsonPatch, AdaptError> {
    Ok(JsonPatch {
        json5: true,
        set: vec![(path(&provider.id), entry(provider)?)],
        ..JsonPatch::default()
    })
}

/// 取消勾选：删掉 `providers.confdeck-<id>`。
pub fn remove(provider_id: &str) -> JsonPatch {
    JsonPatch {
        json5: true,
        remove: vec![path(provider_id)],
        ..JsonPatch::default()
    }
}

fn path(provider_id: &str) -> KeyPath {
    KeyPath(vec!["providers".into(), provider_key(provider_id)])
}

fn entry(provider: &Provider) -> Result<Value, AdaptError> {
    let (protocol, base_url) = choose_protocol(provider, AGENT)?;
    let models: Vec<Value> = list_models(provider, AGENT)?
        .into_iter()
        .map(model)
        .collect();

    let mut entry = Map::new();
    entry.insert("name".into(), json!(provider.name));
    entry.insert("baseUrl".into(), json!(base_url));
    entry.insert("api".into(), json!(api_name(protocol)));
    // 没有 Key 时不写，Pi 会去 auth.json 和环境变量里找。
    if !provider.key.is_empty() {
        entry.insert("apiKey".into(), json!(provider.key));
    }
    entry.insert("models".into(), Value::Array(models));

    let mut entry = Value::Object(entry);
    if let Some(raw) = provider.override_for(AGENT).and_then(|o| o.raw.as_ref()) {
        merge(&mut entry, raw);
    }
    Ok(entry)
}

fn model(model: &Model) -> Value {
    let mut out = Map::new();
    out.insert("id".into(), json!(model.id));
    if let Some(name) = &model.name {
        out.insert("name".into(), json!(name));
    }
    out.insert("reasoning".into(), json!(!model.reasoning.is_empty()));
    if !model.input.is_empty() {
        out.insert("input".into(), json!(model.input));
    }
    if let Some(context) = model.context {
        out.insert("contextWindow".into(), json!(context));
    }
    if let Some(output) = model.output {
        out.insert("maxTokens".into(), json!(output));
    }
    Value::Object(out)
}

/// 深合并：对象逐键合并，`null` 删掉这个键，其余（含数组）整个替换。
fn merge(target: &mut Value, raw: &Map<String, Value>) {
    let Some(target) = target.as_object_mut() else {
        return;
    };
    for (key, value) in raw {
        match (target.get_mut(key), value) {
            (_, Value::Null) => {
                target.shift_remove(key);
            }
            (Some(existing @ Value::Object(_)), Value::Object(inner)) => merge(existing, inner),
            (Some(existing), _) => *existing = value.clone(),
            (None, _) => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::LivePatch;
    use crate::provider::AgentOverride;
    use crate::provider::tests::relay;

    const LIVE: &str = r#"{
  // 本地模型
  "providers": {
    "ollama": {
      "baseUrl": "http://localhost:11434/v1",
      "api": "openai-completions",
      "apiKey": "ollama",
      "models": [{ "id": "qwen2.5-coder:7b" }],
    },
  },
}
"#;

    fn apply(patch: &JsonPatch, pre: Option<&str>) -> Value {
        let out = patch
            .apply(Path::new("models.json"), pre.map(str::as_bytes))
            .unwrap();
        serde_json::from_slice(&out).unwrap()
    }

    #[test]
    fn add_writes_one_entry_next_to_the_users_own() {
        let out = apply(&add(&relay()).unwrap(), Some(LIVE));
        assert_eq!(
            out,
            json!({"providers": {
                "ollama": {
                    "baseUrl": "http://localhost:11434/v1",
                    "api": "openai-completions",
                    "apiKey": "ollama",
                    "models": [{"id": "qwen2.5-coder:7b"}]
                },
                "confdeck-relay": {
                    "name": "Relay",
                    "baseUrl": "https://relay.example/v1",
                    "api": "openai-completions",
                    "apiKey": "sk-relay",
                    "models": [
                        {"id": "glm-4.5-air", "reasoning": false},
                        {
                            "id": "glm-4.6",
                            "name": "GLM 4.6",
                            "reasoning": true,
                            "input": ["text", "image"],
                            "contextWindow": 200000,
                            "maxTokens": 128000
                        }
                    ]
                }
            }})
        );
    }

    #[test]
    fn remove_drops_only_our_entry() {
        let added = add(&relay())
            .unwrap()
            .apply(Path::new("models.json"), Some(LIVE.as_bytes()))
            .unwrap();
        let out = remove("relay")
            .apply(Path::new("models.json"), Some(&added))
            .unwrap();
        let value: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["providers"].as_object().unwrap().len(), 1);
        assert!(value["providers"].get("ollama").is_some());
    }

    #[test]
    fn protocol_override_changes_the_api_and_the_address() {
        let mut provider = relay();
        provider.overrides.insert(
            AGENT,
            AgentOverride {
                protocol: Some(Protocol::Anthropic),
                ..AgentOverride::default()
            },
        );
        let out = apply(&add(&provider).unwrap(), None);
        let entry = &out["providers"]["confdeck-relay"];
        assert_eq!(entry["api"], json!("anthropic-messages"));
        assert_eq!(entry["baseUrl"], json!("https://relay.example"));
    }

    #[test]
    fn raw_override_is_deep_merged() {
        let mut provider = relay();
        provider.key.clear();
        provider.overrides.insert(
            AGENT,
            AgentOverride {
                raw: Some(
                    json!({
                        "headers": {"X-Team": "infra"},
                        "name": null,
                        "authHeader": true
                    })
                    .as_object()
                    .unwrap()
                    .clone(),
                ),
                ..AgentOverride::default()
            },
        );
        let out = apply(&add(&provider).unwrap(), None);
        let entry = out["providers"]["confdeck-relay"].as_object().unwrap();
        assert!(!entry.contains_key("apiKey"), "no key, no apiKey");
        assert!(!entry.contains_key("name"));
        assert_eq!(entry["headers"], json!({"X-Team": "infra"}));
        assert_eq!(entry["authHeader"], json!(true));
    }

    #[test]
    fn merge_recurses_into_objects_and_replaces_arrays() {
        let mut target = json!({"a": {"x": 1, "y": 2}, "list": [1, 2]});
        let raw = json!({"a": {"y": 3, "x": null}, "list": [9]});
        merge(&mut target, raw.as_object().unwrap());
        assert_eq!(target, json!({"a": {"y": 3}, "list": [9]}));
    }
}
