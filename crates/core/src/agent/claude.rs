//! Claude Code：`~/.claude/settings.json`。单选 agent，槽位型。
//!
//! 关键字段的清单移植自 ccs-lite 的 `live/floor.rs`（Claude Code 2.1.282 核实）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::slots::{self, Slot, Strength};
use super::{
    AdaptError, AgentKind, Connection, Credential, FileSetup, choose_protocol, require_key,
};
use crate::engine::LiveFile;
use crate::fsx::Mode;
use crate::patch::json::{self, ClearScope, JsonPatch};
use crate::patch::{Format, KeyPath, PatchError};
use crate::provider::{Model, Protocol, Provider, normalize_url};

const AGENT: AgentKind = AgentKind::ClaudeCode;

pub fn settings_path(home: &Path) -> PathBuf {
    home.join(".claude").join("settings.json")
}

/// 里面有 Key，按 0600 写。
pub fn live_file(home: &Path) -> LiveFile {
    LiveFile {
        owner: AGENT.id(),
        path: settings_path(home),
        mode: Mode::Private,
        format: Format::Json,
    }
}

/// Claude Code 的协议选择器。
///
/// `CLAUDE_CODE_USE_` 不能按前缀匹配：同一前缀下还有 `USE_POWERSHELL_TOOL`、
/// `USE_NATIVE_FILE_SEARCH` 这类与供应商无关的功能开关。
pub const PROTOCOL_SELECTORS: &[&str] = &[
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_GATEWAY",
    "CLAUDE_CODE_USE_MANTLE",
    "CLAUDE_CODE_USE_ANTHROPIC_AWS",
    "CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD",
];

/// `env` 里整个前缀都属于连接和鉴权的前缀：`ANTHROPIC_*` 是地址、凭据、各档模型名、
/// 自定义头；`AWS_*` 是 Bedrock 的区域与凭据；`VERTEX_REGION_*` 是 Vertex 的分模型区域。
pub const FLOOR_ENV_PREFIXES: &[&str] = &["ANTHROPIC_", "AWS_", "VERTEX_REGION_"];

/// `env` 里按名字列出的关键字段（协议选择器之外）。
pub const FLOOR_ENV_KEYS: &[&str] = &[
    "CLAUDE_CODE_SUBAGENT_MODEL",
    "CLAUDE_CODE_SUBAGENT_MODEL_FORCE",
    "CLOUD_ML_REGION",
    // Vertex 的凭据路径。
    "GOOGLE_APPLICATION_CREDENTIALS",
    // 订阅账号的长期 token 及其配套键。
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_REFRESH_TOKEN",
    "CLAUDE_CODE_OAUTH_SCOPES",
    // 与顶层 apiKeyHelper 配套。
    "CLAUDE_CODE_API_KEY_HELPER_TTL_MS",
];

/// `settings.json` 顶层的关键字段。
pub const FLOOR_TOP: &[&str] = &[
    "apiKeyHelper",
    "apiBaseUrl",
    "primaryModel",
    "smallFastModel",
    "apiKey",
    // `/model` 保存的选择，属于当时所在的那一家。
    "model",
    // 备用模型链；模型 ID → 供应商专属 ID（如 Bedrock ARN）。
    "fallbackModel",
    "modelOverrides",
    "modelPicker",
    // advisor 只在 Anthropic API 上可用。
    "advisorModel",
    // Bedrock / Vertex 的凭据命令。
    "awsAuthRefresh",
    "awsCredentialExport",
    "gcpAuthRefresh",
];

pub fn is_floor_env(key: &str) -> bool {
    FLOOR_ENV_PREFIXES
        .iter()
        .any(|prefix| key.starts_with(prefix))
        || PROTOCOL_SELECTORS.contains(&key)
        || FLOOR_ENV_KEYS.contains(&key)
        || (key.starts_with("CLAUDE_CODE_SKIP_") && key.ends_with("_AUTH"))
}

pub fn is_floor_top(key: &str) -> bool {
    FLOOR_TOP.contains(&key)
}

/// 六个槽位和它们写进 `env` 的键。
pub const SLOTS: &[(Slot, &str)] = &[
    (slot("main", Strength::Strong), "ANTHROPIC_MODEL"),
    (
        slot("opus", Strength::Strong),
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
    ),
    (
        slot("sonnet", Strength::Strong),
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
    ),
    (
        slot("fable", Strength::Strong),
        "ANTHROPIC_DEFAULT_FABLE_MODEL",
    ),
    (
        slot("haiku", Strength::Fast),
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    ),
    (
        slot("subagent", Strength::Fast),
        "CLAUDE_CODE_SUBAGENT_MODEL",
    ),
];

const fn slot(key: &'static str, strength: Strength) -> Slot {
    Slot { key, strength }
}

/// 切到这家供应商的补丁；`None` 是「默认」：清掉关键字段，还原成 Claude Code 自带的登录。
pub fn patch(provider: Option<&Provider>) -> Result<JsonPatch, AdaptError> {
    let mut patch = JsonPatch {
        clear: vec![
            ClearScope {
                parent: KeyPath::root(),
                is_floor: is_floor_top,
            },
            ClearScope {
                parent: KeyPath::new(&["env"]),
                is_floor: is_floor_env,
            },
        ],
        drop_if_emptied: vec![KeyPath::new(&["env"])],
        ..JsonPatch::default()
    };
    let Some(provider) = provider else {
        return Ok(patch);
    };
    if provider
        .override_for(AGENT)
        .is_some_and(|o| o.raw.is_some())
    {
        // 原始片段里的非关键字段切走时要能删掉，得先记下写过哪些键。
        return Err(AdaptError::RawUnsupported { agent: AGENT });
    }

    let (_, base_url) = choose_protocol(provider, AGENT)?;
    let key = require_key(provider, AGENT)?;
    let mut env = |name: &str, value: &str| {
        patch
            .set
            .push((KeyPath::new(&["env", name]), Value::from(value)));
    };

    let official = provider.is_anthropic_official();
    if official {
        env("ANTHROPIC_API_KEY", key);
    } else {
        env("ANTHROPIC_BASE_URL", base_url);
        env("ANTHROPIC_AUTH_TOKEN", key);
    }

    let slot_list: Vec<Slot> = SLOTS.iter().map(|(slot, _)| *slot).collect();
    for (slot, id) in slots::fill(provider, AGENT, &slot_list, !official)? {
        let (_, name) = SLOTS
            .iter()
            .find(|(known, _)| *known == slot)
            .expect("fill returns known slots");
        let long = !official
            && provider
                .model(id)
                .and_then(|model| model.context)
                .is_some_and(|tokens| tokens >= ONE_M_TOKENS);
        if long && !strip_one_m(id).1 {
            env(name, &format!("{id}{ONE_M}"));
        } else {
            env(name, id);
        }
    }
    Ok(patch)
}

/// Claude Code 认模型名末尾的 `[1m]`（不分大小写）：带了按 1M 上下文算，不带按 200K；
/// 发请求前去掉它，另加一个 `context-1m-2025-08-07` beta 头（Claude Code 2.1.293 抓包核实）。
/// 智谱、MiniMax 官方的 Claude Code 配置就这么写。
const ONE_M: &str = "[1m]";

/// 目录里上下文至少这么长的模型，写进槽位时加 `[1m]`。比这短的不加：标成 1M 会让 Claude Code 塞超了。
/// 连 Anthropic 官方时不加，1M 要看账号有没有开通。
const ONE_M_TOKENS: u64 = 1_000_000;

/// 去掉模型名末尾的 `[1m]`，返回（模型名，原来有没有）。
pub fn strip_one_m(model: &str) -> (&str, bool) {
    let cut = model.len().saturating_sub(ONE_M.len());
    match model.get(cut..) {
        Some(tail) if tail.eq_ignore_ascii_case(ONE_M) => (model[..cut].trim_end(), true),
        _ => (model, false),
    }
}

/// 没写 `ANTHROPIC_BASE_URL` 时 Claude Code 连的地址。
pub const OFFICIAL_URL: &str = "https://api.anthropic.com";

/// 从 `settings.json` 读出现在连的是哪。只看地址和凭据，不看模型名：
/// 在 Claude Code 里用 `/model` 换个模型，不算换了供应商。
pub fn connection(path: &Path, bytes: Option<&[u8]>) -> Result<Connection, PatchError> {
    let (doc, _) = json::parse(path, bytes, false)?;
    let env = doc.get("env").and_then(Value::as_object);
    let env_value = |key: &str| {
        env.and_then(|env| env.get(key))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    let selector_on = PROTOCOL_SELECTORS
        .iter()
        .any(|key| env_value(key).is_some_and(|value| !matches!(value, "0" | "false")));
    if selector_on {
        return Ok(Connection::Other { host: None });
    }

    let credential = env_value("ANTHROPIC_AUTH_TOKEN")
        .or_else(|| env_value("ANTHROPIC_API_KEY"))
        .map(|key| Credential::Key(key.to_string()))
        .or_else(|| {
            doc.get("apiKeyHelper")
                .and_then(Value::as_str)
                .map(|command| Credential::Helper(command.to_string()))
        });
    let base_url =
        env_value("ANTHROPIC_BASE_URL").map(|url| normalize_url(Protocol::Anthropic, url));
    Ok(match (base_url, credential) {
        (None, None) => Connection::Default,
        (base_url, credential) => Connection::Endpoint {
            base_url: base_url.unwrap_or_else(|| OFFICIAL_URL.to_string()),
            credential,
        },
    })
}

/// 「把当前配置存成供应商」：读出地址、Key 和各槽位的模型名。连的不是一个地址时为 `None`。
pub fn setup(path: &Path, bytes: Option<&[u8]>) -> Result<Option<FileSetup>, PatchError> {
    let Connection::Endpoint {
        base_url,
        credential,
    } = connection(path, bytes)?
    else {
        return Ok(None);
    };
    let (doc, _) = json::parse(path, bytes, false)?;
    let env = doc.get("env").and_then(Value::as_object);
    let mut models: Vec<Model> = Vec::new();
    let mut slots = BTreeMap::new();
    for (slot, name) in SLOTS {
        let value = env
            .and_then(|env| env.get(*name))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|model| !model.is_empty());
        let Some((id, long)) = value.map(strip_one_m).filter(|(id, _)| !id.is_empty()) else {
            continue;
        };
        slots.insert(slot.key.to_string(), id.to_string());
        // 带 `[1m]` 的记成 1M 上下文，存成供应商后再写回来还是带着。
        let context = long.then_some(ONE_M_TOKENS);
        match models.iter_mut().find(|seen| seen.id == id) {
            Some(seen) => seen.context = seen.context.or(context),
            None => models.push(Model {
                id: id.to_string(),
                context,
                ..Model::default()
            }),
        }
    }
    Ok(Some(FileSetup {
        protocol: Protocol::Anthropic,
        base_url,
        key: credential
            .as_ref()
            .and_then(Credential::key)
            .map(str::to_string),
        models,
        slots,
        default_model: None,
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::patch::LivePatch;
    use crate::provider::tests::relay;
    use crate::provider::{AgentOverride, Protocol};

    fn apply(provider: Option<&Provider>, pre: &str) -> Value {
        let out = patch(provider)
            .unwrap()
            .apply(Path::new("settings.json"), Some(pre.as_bytes()))
            .unwrap();
        serde_json::from_slice(&out).unwrap()
    }

    const LIVE: &str = r#"{
  "env": {
    "ANTHROPIC_BASE_URL": "https://old.example",
    "ANTHROPIC_AUTH_TOKEN": "sk-old",
    "ANTHROPIC_DEFAULT_OPUS_MODEL": "old-opus",
    "CLAUDE_CODE_USE_BEDROCK": "1",
    "AWS_REGION": "us-east-1",
    "CLAUDE_CODE_USE_POWERSHELL_TOOL": "1",
    "DISABLE_TELEMETRY": "1"
  },
  "model": "opus",
  "permissions": {"allow": ["Bash"]},
  "statusLine": {"type": "command", "command": "~/bin/status"}
}"#;

    #[test]
    fn third_party_fills_every_slot_and_keeps_user_keys() {
        let provider = relay();
        assert_eq!(
            apply(Some(&provider), LIVE),
            json!({
                "env": {
                    "ANTHROPIC_BASE_URL": "https://relay.example",
                    "ANTHROPIC_AUTH_TOKEN": "sk-relay",
                    "ANTHROPIC_DEFAULT_OPUS_MODEL": "glm-4.6",
                    "CLAUDE_CODE_USE_POWERSHELL_TOOL": "1",
                    "DISABLE_TELEMETRY": "1",
                    "ANTHROPIC_MODEL": "glm-4.6",
                    "ANTHROPIC_DEFAULT_SONNET_MODEL": "glm-4.6",
                    "ANTHROPIC_DEFAULT_FABLE_MODEL": "glm-4.6",
                    "ANTHROPIC_DEFAULT_HAIKU_MODEL": "glm-4.5-air",
                    "CLAUDE_CODE_SUBAGENT_MODEL": "glm-4.5-air"
                },
                "permissions": {"allow": ["Bash"]},
                "statusLine": {"type": "command", "command": "~/bin/status"}
            })
        );
    }

    #[test]
    fn official_anthropic_writes_only_the_key() {
        let mut provider = relay();
        provider.endpoints =
            BTreeMap::from([(Protocol::Anthropic, "https://api.anthropic.com".into())]);
        provider.key = "sk-ant".into();
        let out = apply(Some(&provider), LIVE);
        assert_eq!(
            out["env"],
            json!({
                "CLAUDE_CODE_USE_POWERSHELL_TOOL": "1",
                "DISABLE_TELEMETRY": "1",
                "ANTHROPIC_API_KEY": "sk-ant"
            })
        );

        // 用户自己填的槽位照写。
        provider.overrides.insert(
            AGENT,
            AgentOverride {
                slots: BTreeMap::from([("haiku".into(), "glm-4.5-air".into())]),
                ..AgentOverride::default()
            },
        );
        let out = apply(Some(&provider), LIVE);
        assert_eq!(
            out["env"]["ANTHROPIC_DEFAULT_HAIKU_MODEL"],
            json!("glm-4.5-air")
        );
        assert!(out["env"].get("ANTHROPIC_MODEL").is_none());
    }

    #[test]
    fn default_clears_the_floor_only() {
        assert_eq!(
            apply(None, LIVE),
            json!({
                "env": {
                    "CLAUDE_CODE_USE_POWERSHELL_TOOL": "1",
                    "DISABLE_TELEMETRY": "1"
                },
                "permissions": {"allow": ["Bash"]},
                "statusLine": {"type": "command", "command": "~/bin/status"}
            })
        );
    }

    #[test]
    fn switching_twice_is_byte_stable() {
        let provider = relay();
        let once = patch(Some(&provider))
            .unwrap()
            .apply(Path::new("s.json"), Some(LIVE.as_bytes()))
            .unwrap();
        let twice = patch(Some(&provider))
            .unwrap()
            .apply(Path::new("s.json"), Some(&once))
            .unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn refuses_what_it_cannot_write_safely() {
        let mut provider = relay();
        provider.key.clear();
        assert!(matches!(
            patch(Some(&provider)),
            Err(AdaptError::MissingKey { .. })
        ));

        let mut provider = relay();
        provider.endpoints.remove(&Protocol::Anthropic);
        assert!(matches!(
            patch(Some(&provider)),
            Err(AdaptError::Incompatible { .. })
        ));

        let mut provider = relay();
        provider.models.clear();
        assert!(matches!(
            patch(Some(&provider)),
            Err(AdaptError::NoModels { .. })
        ));

        let mut provider = relay();
        provider.overrides.insert(
            AGENT,
            AgentOverride {
                raw: Some(serde_json::Map::new()),
                ..AgentOverride::default()
            },
        );
        assert!(matches!(
            patch(Some(&provider)),
            Err(AdaptError::RawUnsupported { .. })
        ));
    }

    fn read(text: &str) -> Connection {
        connection(Path::new("settings.json"), Some(text.as_bytes())).unwrap()
    }

    #[test]
    fn connection_reads_the_address_and_credential() {
        assert_eq!(
            connection(Path::new("s.json"), None).unwrap(),
            Connection::Default
        );
        assert_eq!(
            read(r#"{"env": {"ANTHROPIC_MODEL": "opus"}, "model": "x"}"#),
            Connection::Default
        );
        assert_eq!(
            read(
                r#"{"env": {"ANTHROPIC_BASE_URL": "https://relay.example/v1/", "ANTHROPIC_AUTH_TOKEN": "sk"}}"#
            ),
            Connection::Endpoint {
                base_url: "https://relay.example".into(),
                credential: Some(Credential::Key("sk".into()))
            }
        );
        assert_eq!(
            read(r#"{"env": {"ANTHROPIC_API_KEY": "sk-ant"}}"#),
            Connection::Endpoint {
                base_url: OFFICIAL_URL.into(),
                credential: Some(Credential::Key("sk-ant".into()))
            }
        );
        assert_eq!(
            read(r#"{"apiKeyHelper": "~/bin/key"}"#),
            Connection::Endpoint {
                base_url: OFFICIAL_URL.into(),
                credential: Some(Credential::Helper("~/bin/key".into()))
            }
        );
        assert_eq!(
            read(r#"{"env": {"CLAUDE_CODE_USE_BEDROCK": "1", "AWS_REGION": "us-east-1"}}"#),
            Connection::Other { host: None }
        );
        assert_eq!(
            read(r#"{"env": {"CLAUDE_CODE_USE_BEDROCK": "0"}}"#),
            Connection::Default
        );
    }

    #[test]
    fn models_with_a_million_tokens_of_context_get_the_1m_suffix() {
        let mut provider = relay();
        provider.models[1].context = Some(1_048_576); // glm-4.6，强槽位
        provider.models[0].context = Some(262_144); // glm-4.5-air，快槽位
        let env = &apply(Some(&provider), "{}")["env"];
        assert_eq!(env["ANTHROPIC_MODEL"], "glm-4.6[1m]");
        assert_eq!(env["ANTHROPIC_DEFAULT_OPUS_MODEL"], "glm-4.6[1m]");
        assert_eq!(env["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "glm-4.5-air");

        // 目录里的 ID 已经带了后缀，不再加一次。
        provider.models[1].id = "glm-4.6[1M]".into();
        provider.overrides.insert(
            AGENT,
            AgentOverride {
                slots: [("main".into(), "glm-4.6[1M]".into())].into(),
                ..AgentOverride::default()
            },
        );
        assert_eq!(
            apply(Some(&provider), "{}")["env"]["ANTHROPIC_MODEL"],
            "glm-4.6[1M]"
        );

        // 连 Anthropic 官方不加。
        let mut official = relay();
        official.endpoints = [(Protocol::Anthropic, OFFICIAL_URL.to_string())].into();
        official.models[1].context = Some(1_000_000);
        official.overrides.insert(
            AGENT,
            AgentOverride {
                slots: [("main".into(), "glm-4.6".into())].into(),
                ..AgentOverride::default()
            },
        );
        assert_eq!(
            apply(Some(&official), "{}")["env"]["ANTHROPIC_MODEL"],
            "glm-4.6"
        );
    }

    #[test]
    fn the_1m_suffix_is_stripped_in_any_case() {
        assert_eq!(strip_one_m("glm-5.3[1m]"), ("glm-5.3", true));
        assert_eq!(strip_one_m("MiniMax-M3[1M]"), ("MiniMax-M3", true));
        assert_eq!(strip_one_m("glm-5.3"), ("glm-5.3", false));
        assert_eq!(strip_one_m("[1m]"), ("", true));
        assert_eq!(strip_one_m("模型"), ("模型", false));
    }

    #[test]
    fn setup_reads_the_address_key_and_slots() {
        // 带 `[1m]` 的去掉后缀，记成 1M 上下文；同一个模型别的槽位没带也算。
        let text = r#"{"env": {
            "ANTHROPIC_BASE_URL": "https://relay.elsewhere.io",
            "ANTHROPIC_AUTH_TOKEN": "sk-x",
            "ANTHROPIC_MODEL": "glm-4.6",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL": "glm-4.5-air",
            "ANTHROPIC_DEFAULT_OPUS_MODEL": "glm-4.6[1M]"
        }}"#;
        let read = setup(Path::new("s.json"), Some(text.as_bytes()))
            .unwrap()
            .unwrap();
        assert_eq!(read.base_url, "https://relay.elsewhere.io");
        assert_eq!(read.key.as_deref(), Some("sk-x"));
        let models: Vec<(&str, Option<u64>)> = read
            .models
            .iter()
            .map(|model| (model.id.as_str(), model.context))
            .collect();
        assert_eq!(
            models,
            [("glm-4.6", Some(1_000_000)), ("glm-4.5-air", None)]
        );
        assert_eq!(
            read.slots,
            BTreeMap::from([
                ("main".into(), "glm-4.6".into()),
                ("opus".into(), "glm-4.6".into()),
                ("haiku".into(), "glm-4.5-air".into()),
            ])
        );

        let helper =
            r#"{"env": {"ANTHROPIC_BASE_URL": "https://r.example"}, "apiKeyHelper": "~/bin/key"}"#;
        let read = setup(Path::new("s.json"), Some(helper.as_bytes()))
            .unwrap()
            .unwrap();
        assert_eq!(read.key, None, "a helper command is not a key");

        assert_eq!(setup_of("{}"), None);
        assert_eq!(
            setup_of(r#"{"env": {"CLAUDE_CODE_USE_VERTEX": "1"}}"#),
            None
        );
    }

    fn setup_of(text: &str) -> Option<FileSetup> {
        setup(Path::new("s.json"), Some(text.as_bytes())).unwrap()
    }

    #[test]
    fn what_patch_writes_reads_back_as_the_same_connection() {
        let provider = relay();
        let out = patch(Some(&provider))
            .unwrap()
            .apply(Path::new("s.json"), None)
            .unwrap();
        assert_eq!(
            connection(Path::new("s.json"), Some(&out)).unwrap(),
            Connection::Endpoint {
                base_url: "https://relay.example".into(),
                credential: Some(Credential::Key("sk-relay".into()))
            }
        );
    }

    #[test]
    fn floor_covers_connection_keys_but_not_feature_switches() {
        for key in [
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_CUSTOM_HEADERS",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_SKIP_BEDROCK_AUTH",
            "AWS_BEARER_TOKEN_BEDROCK",
            "VERTEX_REGION_CLAUDE_4_5_SONNET",
            "CLAUDE_CODE_SUBAGENT_MODEL",
        ] {
            assert!(is_floor_env(key), "{key}");
        }
        for key in [
            "CLAUDE_CODE_USE_POWERSHELL_TOOL",
            "CLAUDE_CODE_DISABLE_ARTIFACT",
            "DISABLE_TELEMETRY",
            "HTTPS_PROXY",
        ] {
            assert!(!is_floor_env(key), "{key}");
        }
    }
}
