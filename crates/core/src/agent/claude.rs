//! Claude Code：`~/.claude/settings.json`。单选 agent，槽位型。
//!
//! 关键字段的清单移植自 ccs-lite 的 `live/floor.rs`（Claude Code 2.1.282 核实）。

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::slots::{self, Slot, Strength};
use super::{AdaptError, AgentKind, choose_protocol, require_key};
use crate::engine::LiveFile;
use crate::fsx::Mode;
use crate::patch::KeyPath;
use crate::patch::json::{ClearScope, JsonPatch};
use crate::provider::Provider;

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
    for (slot, model) in slots::fill(provider, AGENT, &slot_list, !official)? {
        let (_, name) = SLOTS
            .iter()
            .find(|(known, _)| *known == slot)
            .expect("fill returns known slots");
        env(name, model);
    }
    Ok(patch)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

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
