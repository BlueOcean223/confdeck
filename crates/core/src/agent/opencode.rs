//! OpenCode：`~/.config/opencode/opencode.json`（已有 `opencode.jsonc` 时写它）。多选 agent，列表型。
//!
//! 每家供应商是 `provider` 下的一项，键是 `confdeck-<id>`：`npm`、`name`、`options`（`baseURL`、`apiKey`）、
//! `models`。勾上就整项写入，取消就整项删掉，用户自己写的其他项不动。
//!
//! 用 OpenCode 1.x 的写法 `provider`。2.x 新加了原生的 `providers`（`package`、`settings`），但照样读
//! `provider`：在内存里转换，不改写文件（官方文档 Migrate from V1）；`opencode.ai/config.json` 发布的
//! schema 里也只有 `provider`。所以写它 1.x 和 2.x 都能用（10-10 核实，OpenCode 2.0.26、1.18.35）。
//!
//! 包按协议选（官方 providers 文档）：Chat → `@ai-sdk/openai-compatible`；Responses → `@ai-sdk/openai`
//! （自定义供应商走 `languageModel`，这个包默认发 Responses）；Anthropic → `@ai-sdk/anthropic`，地址补 `/v1`；
//! Gemini → `@ai-sdk/google`，地址补 `/v1beta`。
//!
//! 文件允许注释（JSONC），写回时注释和排版原样保留。默认模型（顶层 `model`）只读不写；
//! 取消勾选时，`model`、`small_model` 指着这家的一并删掉，不然 OpenCode 找不到这个模型。
//!
//! 10-10 用 OpenCode 1.18.35 实际跑过：三种协议的请求地址和鉴权头都对（Anthropic 发 `x-api-key`）；
//! 没有 `$schema` 的配置，OpenCode 启动时会在最前面补一行（注释保留），有就不改文件。

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use super::{AdaptError, AgentKind, choose_protocol, list_models, merge_raw};
use crate::engine::LiveFile;
use crate::fsx::Mode;
use crate::patch::json::{self, JsonPatch};
use crate::patch::{Format, KeyPath, LivePatch, PatchError};
use crate::provider::{Modality, Model, Protocol, Provider};

const AGENT: AgentKind = AgentKind::Opencode;

/// confdeck 写的项的键前缀，用来和用户自己写的项区分。
pub const KEY_PREFIX: &str = "confdeck-";

/// 顶层按 `供应商/模型` 引用模型的键。
const MODEL_REFS: &[&str] = &["model", "small_model"];

const SCHEMA: &str = "https://opencode.ai/config.json";

pub fn config_dir(home: &Path) -> PathBuf {
    home.join(".config").join("opencode")
}

/// 已有 `opencode.jsonc` 时是它，否则是 `opencode.json`（和 ccs-lite 一样）。
pub fn config_path(home: &Path) -> PathBuf {
    let dir = config_dir(home);
    let jsonc = dir.join("opencode.jsonc");
    if jsonc.exists() {
        jsonc
    } else {
        dir.join("opencode.json")
    }
}

/// 里面有 Key，按 0600 写。
pub fn live_file(home: &Path) -> LiveFile {
    LiveFile {
        owner: AGENT.id(),
        path: config_path(home),
        mode: Mode::Private,
        format: Format::Json5,
    }
}

pub fn provider_key(provider_id: &str) -> String {
    format!("{KEY_PREFIX}{provider_id}")
}

/// `npm`：OpenCode 用哪个 AI SDK 包请求。
pub fn package(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::OpenaiChat => "@ai-sdk/openai-compatible",
        Protocol::OpenaiResponses => "@ai-sdk/openai",
        Protocol::Anthropic => "@ai-sdk/anthropic",
        Protocol::Gemini => "@ai-sdk/google",
    }
}

/// 勾上这家供应商：写入（或整项替换）`provider.confdeck-<id>`。
pub fn add(provider: &Provider) -> Result<AddPatch, AdaptError> {
    Ok(AddPatch(JsonPatch {
        json5: true,
        set: vec![(path(&provider.id), entry(provider)?)],
        ..JsonPatch::default()
    }))
}

/// 文件是 confdeck 新建的（或者是空对象）时，先写 `$schema`，和 OpenCode 自己建的一样；
/// 不写的话 OpenCode 下次启动会补上，confdeck 就认成文件被别人改过。
#[derive(Clone)]
pub struct AddPatch(JsonPatch);

impl LivePatch for AddPatch {
    fn apply(&self, path: &Path, pre: Option<&[u8]>) -> Result<Vec<u8>, PatchError> {
        json::rewrite(path, pre, true, |doc| {
            if let Some(root) = doc.as_object_mut()
                && root.is_empty()
            {
                root.insert("$schema".into(), SCHEMA.into());
            }
            self.0.apply_to(path, doc)
        })
    }
}

/// 取消勾选：删掉 `provider.confdeck-<id>`，和指着它的默认模型。
pub fn remove(provider_id: &str) -> RemovePatch {
    RemovePatch {
        patch: JsonPatch {
            json5: true,
            remove: vec![path(provider_id)],
            drop_if_emptied: vec![KeyPath::new(&["provider"])],
            ..JsonPatch::default()
        },
        refs: format!("{}/", provider_key(provider_id)),
    }
}

#[derive(Clone)]
pub struct RemovePatch {
    patch: JsonPatch,
    /// 顶层模型引用以它开头的（`confdeck-<id>/`）一并删掉。
    refs: String,
}

impl LivePatch for RemovePatch {
    fn apply(&self, path: &Path, pre: Option<&[u8]>) -> Result<Vec<u8>, PatchError> {
        json::rewrite(path, pre, true, |doc| {
            self.patch.apply_to(path, doc)?;
            if let Some(root) = doc.as_object_mut() {
                for key in MODEL_REFS {
                    let dangling = root
                        .get(*key)
                        .and_then(Value::as_str)
                        .is_some_and(|model| model.starts_with(&self.refs));
                    if dangling {
                        root.shift_remove(*key);
                    }
                }
            }
            Ok(())
        })
    }
}

fn path(provider_id: &str) -> KeyPath {
    KeyPath(vec!["provider".into(), provider_key(provider_id)])
}

fn entry(provider: &Provider) -> Result<Value, AdaptError> {
    let (protocol, base_url) = choose_protocol(provider, AGENT)?;
    let base_url = match protocol {
        // `@ai-sdk/anthropic` 在地址后面只拼 `/messages`，confdeck 存的 Anthropic 地址不带 `/v1`。
        Protocol::Anthropic => format!("{base_url}/v1"),
        // `@ai-sdk/google` 也一样，只拼 `/models/<模型>:streamGenerateContent`。
        Protocol::Gemini => format!("{base_url}/v1beta"),
        _ => base_url.to_string(),
    };
    let models: Map<String, Value> = list_models(provider, AGENT)?
        .into_iter()
        .map(|model| (model.id.clone(), self::model(model)))
        .collect();

    let mut options = Map::new();
    options.insert("baseURL".into(), json!(base_url));
    // 没有 Key 时不写，OpenCode 会用 `/connect` 存的凭据和环境变量。
    if !provider.key.is_empty() {
        options.insert("apiKey".into(), json!(provider.key));
    }
    let mut entry = Map::new();
    entry.insert("npm".into(), json!(package(protocol)));
    entry.insert("name".into(), json!(provider.name));
    entry.insert("options".into(), Value::Object(options));
    entry.insert("models".into(), Value::Object(models));

    let mut entry = Value::Object(entry);
    if let Some(raw) = provider.override_for(AGENT).and_then(|o| o.raw.as_ref()) {
        merge_raw(&mut entry, raw);
    }
    Ok(entry)
}

fn model(model: &Model) -> Value {
    let mut out = Map::new();
    if let Some(name) = &model.name {
        out.insert("name".into(), json!(name));
    }
    if !model.reasoning.is_empty() {
        out.insert("reasoning".into(), json!(true));
    }
    if model.input.contains(&Modality::Image) {
        out.insert("attachment".into(), json!(true));
        out.insert(
            "modalities".into(),
            json!({"input": model.input, "output": ["text"]}),
        );
    }
    // OpenCode 的 `limit` 两项都要有。
    if let (Some(context), Some(output)) = (model.context, model.output) {
        out.insert(
            "limit".into(),
            json!({"context": context, "output": output}),
        );
    }
    Value::Object(out)
}

/// 勾了哪几家：`provider` 下以 `confdeck-` 开头的项，返回供应商 id。多选 agent 以文件为准，不另记。
pub fn enabled(path: &Path, bytes: Option<&[u8]>) -> Result<Vec<String>, PatchError> {
    let (doc, _) = json::parse(path, bytes, true)?;
    Ok(doc
        .get("provider")
        .and_then(Value::as_object)
        .map(|providers| {
            providers
                .keys()
                .filter_map(|key| key.strip_prefix(KEY_PREFIX))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use super::*;
    use crate::provider::AgentOverride;
    use crate::provider::tests::relay;

    const LIVE: &str = r#"{
  "$schema": "https://opencode.ai/config.json",
  // 本地模型
  "provider": {
    "ollama": {
      "npm": "@ai-sdk/openai-compatible",
      "options": { "baseURL": "http://localhost:11434/v1" },
      "models": { "qwen2.5-coder:7b": {} },
    },
  },
  "model": "ollama/qwen2.5-coder:7b",
}
"#;

    fn apply(patch: &dyn LivePatch, pre: Option<&str>) -> Value {
        let out = patch
            .apply(Path::new("opencode.json"), pre.map(str::as_bytes))
            .unwrap();
        json5::from_str(std::str::from_utf8(&out).unwrap()).unwrap()
    }

    fn only(protocol: Protocol, url: &str) -> Provider {
        let mut provider = relay();
        provider.endpoints = BTreeMap::from([(protocol, url.into())]);
        provider
    }

    #[test]
    fn adding_writes_one_entry_and_keeps_the_rest() {
        let value = apply(&add(&relay()).unwrap(), Some(LIVE));
        assert_eq!(
            value["provider"]["confdeck-relay"],
            json!({
                "npm": "@ai-sdk/openai-compatible",
                "name": "Relay",
                "options": {"baseURL": "https://relay.example/v1", "apiKey": "sk-relay"},
                "models": {
                    "glm-4.5-air": {},
                    "glm-4.6": {
                        "name": "GLM 4.6",
                        "reasoning": true,
                        "attachment": true,
                        "modalities": {"input": ["text", "image"], "output": ["text"]},
                        "limit": {"context": 200000, "output": 128000}
                    },
                    "glm-4.5": {}
                }
            })
        );
        assert!(value["provider"]["ollama"].is_object());
        assert_eq!(value["model"], "ollama/qwen2.5-coder:7b");
        assert_eq!(value["$schema"], "https://opencode.ai/config.json");
    }

    #[test]
    fn a_new_file_starts_with_the_schema() {
        let value = apply(&add(&relay()).unwrap(), None);
        let keys: Vec<&String> = value.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["$schema", "provider"]);
        // 已有的文件没有也不补，OpenCode 自己会补。
        let value = apply(&add(&relay()).unwrap(), Some(r#"{"theme": "x"}"#));
        assert!(value.get("$schema").is_none());
    }

    #[test]
    fn the_package_follows_the_protocol() {
        let cases = [
            (
                Protocol::OpenaiResponses,
                "https://r.example/v1",
                "@ai-sdk/openai",
                "https://r.example/v1",
            ),
            (
                Protocol::Anthropic,
                "https://a.example",
                "@ai-sdk/anthropic",
                "https://a.example/v1",
            ),
            (
                Protocol::Gemini,
                "https://g.example",
                "@ai-sdk/google",
                "https://g.example/v1beta",
            ),
        ];
        for (protocol, url, npm, written) in cases {
            let value = apply(&add(&only(protocol, url)).unwrap(), None);
            let entry = &value["provider"]["confdeck-relay"];
            assert_eq!(entry["npm"], npm);
            assert_eq!(entry["options"]["baseURL"], written);
        }
    }

    #[test]
    fn without_a_key_opencode_looks_for_one_itself() {
        let mut keyless = relay();
        keyless.key.clear();
        let value = apply(&add(&keyless).unwrap(), None);
        assert_eq!(
            value["provider"]["confdeck-relay"]["options"],
            json!({"baseURL": "https://relay.example/v1"})
        );
    }

    #[test]
    fn raw_override_is_deep_merged() {
        let mut provider = relay();
        provider.overrides.insert(
            AgentKind::Opencode,
            AgentOverride {
                raw: Some(
                    json!({"options": {"timeout": 600000}, "name": null})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
                ..AgentOverride::default()
            },
        );
        let value = apply(&add(&provider).unwrap(), None);
        let entry = &value["provider"]["confdeck-relay"];
        assert_eq!(entry["options"]["timeout"], 600000);
        assert_eq!(entry["options"]["apiKey"], "sk-relay");
        assert!(entry.get("name").is_none());
    }

    #[test]
    fn removing_drops_the_entry_and_the_default_that_points_at_it() {
        let mut live = apply(&add(&relay()).unwrap(), Some(LIVE));
        live["model"] = json!("confdeck-relay/glm-4.6");
        live["small_model"] = json!("ollama/qwen2.5-coder:7b");
        let live = serde_json::to_string_pretty(&live).unwrap();

        let value = apply(&remove("relay"), Some(&live));
        assert!(value["provider"].get("confdeck-relay").is_none());
        assert!(value["provider"]["ollama"].is_object());
        assert!(value.get("model").is_none());
        assert_eq!(value["small_model"], "ollama/qwen2.5-coder:7b");

        // 只剩 confdeck 那一项时，删完不留空的 `provider`。
        let ours = serde_json::to_string(&apply(&add(&relay()).unwrap(), None)).unwrap();
        assert_eq!(
            apply(&remove("relay"), Some(&ours)),
            json!({"$schema": SCHEMA})
        );
    }

    #[test]
    fn enabled_lists_confdeck_entries_only() {
        let mut other = relay();
        other.id = "other".into();
        let once = apply(&add(&relay()).unwrap(), Some(LIVE));
        let twice = apply(
            &add(&other).unwrap(),
            Some(&serde_json::to_string(&once).unwrap()),
        );
        let text = serde_json::to_vec(&twice).unwrap();
        assert_eq!(
            enabled(Path::new("opencode.json"), Some(&text)).unwrap(),
            ["relay", "other"]
        );
        assert!(
            enabled(Path::new("opencode.json"), None)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn an_existing_jsonc_file_is_the_one_written() {
        let home = tempfile::tempdir().unwrap();
        assert!(config_path(home.path()).ends_with("opencode/opencode.json"));
        fs::create_dir_all(config_dir(home.path())).unwrap();
        fs::write(config_dir(home.path()).join("opencode.jsonc"), "{}").unwrap();
        assert!(config_path(home.path()).ends_with("opencode/opencode.jsonc"));
    }
}
