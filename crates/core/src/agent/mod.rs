//! 适配器：把一家供应商翻译成某个 agent 的配置补丁。只算补丁，不读写磁盘（写入见 [`crate::apply`]）。
//!
//! 每个 agent 的配置里，「关键字段」（请求发到哪、凭什么鉴权、哪个模型、说哪种协议）
//! 完全归供应商所有：切换时一律清空，再写目标供应商的值。其余键归用户，confdeck 不写也不删。

pub mod claude;
pub mod codex;
pub mod grok;
pub mod opencode;
pub mod pi;
pub mod slots;

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::preset::{self, Recommended};
use crate::provider::{AgentOverride, Model, Protocol, Provider};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentKind {
    ClaudeCode,
    Codex,
    Pi,
    GrokBuild,
    Opencode,
}

/// 单选：同一时间只用一家供应商。多选：可以同时配多家、同时生效。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Switch,
    Coexist,
}

impl AgentKind {
    pub const ALL: [AgentKind; 5] = [
        Self::ClaudeCode,
        Self::Codex,
        Self::Pi,
        Self::GrokBuild,
        Self::Opencode,
    ];

    /// 和 serde 的写法一样，用在备份目录名等地方。
    pub fn id(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Pi => "pi",
            Self::GrokBuild => "grok-build",
            Self::Opencode => "opencode",
        }
    }

    pub fn mode(self) -> Mode {
        match self {
            Self::ClaudeCode | Self::Codex | Self::GrokBuild => Mode::Switch,
            Self::Pi | Self::Opencode => Mode::Coexist,
        }
    }

    /// agent 能说的协议，按偏好排序：供应商提供好几种时取第一种。
    pub fn protocols(self) -> &'static [Protocol] {
        use Protocol::*;
        match self {
            Self::ClaudeCode => &[Anthropic],
            Self::Codex => &[OpenaiResponses],
            Self::GrokBuild => &[OpenaiResponses, OpenaiChat, Anthropic],
            Self::Pi | Self::Opencode => &[OpenaiChat, Anthropic, OpenaiResponses, Gemini],
        }
    }
}

impl fmt::Display for AgentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
            Self::Pi => "Pi",
            Self::GrokBuild => "Grok Build",
            Self::Opencode => "OpenCode",
        })
    }
}

/// 从 agent 的配置文件里读出来的「现在连的是哪」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Connection {
    /// 没有指定地址和凭据：agent 自带的登录。
    Default,
    /// 连到一个地址。
    Endpoint {
        base_url: String,
        credential: Option<Credential>,
    },
    /// confdeck 不写的接法：Claude Code 走 Bedrock / Vertex、Codex 用内置的 ollama 等。
    Other { host: Option<String> },
}

/// 配置文件里的凭据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    /// Key 本身。
    Key(String),
    /// 运行这个命令拿 Key：Claude Code 的 `apiKeyHelper`、Grok Build 的 `auth_provider`。
    Helper(String),
    /// Key 在这个环境变量里：Codex、Grok Build 的 `env_key`。
    EnvVar(String),
}

impl Credential {
    pub fn key(&self) -> Option<&str> {
        match self {
            Self::Key(key) => Some(key),
            Self::Helper(_) | Self::EnvVar(_) => None,
        }
    }
}

/// 从配置文件里读出的一套设置，用来「把当前配置存成供应商」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSetup {
    pub protocol: Protocol,
    pub base_url: String,
    /// 文件里是 Key 本身时才有；`apiKeyHelper`、`env_key` 这类读不到 Key。
    pub key: Option<String>,
    /// 文件里出现的模型，按出现顺序，去重。看得出上下文长度的带上（Claude Code 的 `[1m]`、Grok 的 `context_window`）。
    pub models: Vec<Model>,
    /// 槽位型 agent 的槽位 → 模型名。
    pub slots: BTreeMap<String, String>,
    /// 列表型 agent 的默认模型。
    pub default_model: Option<String>,
}

impl Connection {
    pub fn base_url(&self) -> Option<&str> {
        match self {
            Self::Endpoint { base_url, .. } => Some(base_url),
            Self::Default | Self::Other { .. } => None,
        }
    }

    pub fn credential(&self) -> Option<&Credential> {
        match self {
            Self::Endpoint { credential, .. } => credential.as_ref(),
            Self::Default | Self::Other { .. } => None,
        }
    }

    pub fn host(&self) -> Option<String> {
        match self {
            Self::Default => None,
            Self::Endpoint { base_url, .. } => crate::provider::host(base_url).map(str::to_string),
            Self::Other { host } => host.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdaptError {
    #[error(
        "{provider} 没有 {agent} 能用的接口（{agent} 支持：{}）",
        join(supported)
    )]
    Incompatible {
        agent: AgentKind,
        provider: String,
        supported: &'static [Protocol],
    },
    #[error("{provider} 的模型目录是空的，{agent} 至少要一个模型")]
    NoModels { agent: AgentKind, provider: String },
    #[error("{provider} 的模型目录里没有 {model}")]
    UnknownModel { provider: String, model: String },
    #[error("{agent} 没有叫 {slot} 的模型槽位")]
    UnknownSlot { agent: AgentKind, slot: String },
    /// 不写 Key 时 agent 会拿官方登录的凭据去请求第三方地址，所以拒绝。
    #[error("{provider} 没有填 Key，{agent} 连第三方地址必须有 Key")]
    MissingKey { agent: AgentKind, provider: String },
    #[error("{agent} 的原始片段里 {key} 应该是{expected}")]
    RawShape {
        agent: AgentKind,
        key: String,
        expected: &'static str,
    },
    #[error("{agent} 的原始片段里 {key} 的值含有 null，TOML 里写不了")]
    RawNotToml { agent: AgentKind, key: String },
    /// Codex 的路由表没有自己的凭据却要求官方登录，请求会带上 ChatGPT 登录发给第三方。
    #[error(
        "{agent} 的原始片段把 requires_openai_auth 设成了 true，但路由表没有自己的 Key，ChatGPT 登录会被发给第三方地址"
    )]
    RawSendsLogin { agent: AgentKind },
    /// Grok Build 走 Anthropic 协议时 Key 只会放进 `Authorization: Bearer`，Anthropic 官方要 `x-api-key`。
    #[error("{agent} 连 Anthropic 官方要把 Key 放进 x-api-key 头，还不支持")]
    AnthropicOfficial { agent: AgentKind },
}

fn join(protocols: &[Protocol]) -> String {
    protocols
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("、")
}

fn override_of(provider: &Provider, agent: AgentKind) -> Option<&AgentOverride> {
    provider.override_for(agent)
}

/// 供应商的预设给这个 agent 的推荐值，见 [`crate::preset`]。
pub(crate) fn recommended(provider: &Provider, agent: AgentKind) -> Option<&'static Recommended> {
    preset::get(provider.preset.as_deref()?)?.recommended(agent)
}

/// 用哪种协议：用户覆盖的 > 预设推荐的 > agent 偏好顺序里这家提供的第一种。返回协议和地址。
pub fn choose_protocol(
    provider: &Provider,
    agent: AgentKind,
) -> Result<(Protocol, &str), AdaptError> {
    let supported = agent.protocols();
    let incompatible = || AdaptError::Incompatible {
        agent,
        provider: provider.name.clone(),
        supported,
    };
    if let Some(protocol) = override_of(provider, agent).and_then(|o| o.protocol) {
        // 覆盖的协议 agent 不支持或供应商没有地址，不悄悄换成别的。
        if !supported.contains(&protocol) {
            return Err(incompatible());
        }
        return provider
            .endpoint(protocol)
            .map(|url| (protocol, url))
            .ok_or_else(incompatible);
    }
    if let Some(protocol) = recommended(provider, agent).and_then(|r| r.protocol)
        && supported.contains(&protocol)
        && let Some(url) = provider.endpoint(protocol)
    {
        return Ok((protocol, url));
    }
    supported
        .iter()
        .find_map(|&protocol| provider.endpoint(protocol).map(|url| (protocol, url)))
        .ok_or_else(incompatible)
}

/// 列表型 agent 取哪些模型：用户覆盖的 > 整个目录。
pub fn list_models(provider: &Provider, agent: AgentKind) -> Result<Vec<&Model>, AdaptError> {
    let chosen: Vec<&Model> = match override_of(provider, agent).and_then(|o| o.models.as_ref()) {
        Some(ids) => ids
            .iter()
            .map(|id| resolve(provider, id))
            .collect::<Result<_, _>>()?,
        None => provider.models.iter().collect(),
    };
    if chosen.is_empty() {
        return Err(AdaptError::NoModels {
            agent,
            provider: provider.name.clone(),
        });
    }
    Ok(chosen)
}

/// 默认模型：用户覆盖的 > 预设推荐的（目录里还有的）> 这家的默认模型（在这个 agent 的列表里时）> 列表第一个。
pub fn default_model(provider: &Provider, agent: AgentKind) -> Result<&Model, AdaptError> {
    if let Some(id) = override_of(provider, agent).and_then(|o| o.default_model.as_ref()) {
        return resolve(provider, id);
    }
    if let Some(model) = recommended(provider, agent)
        .and_then(|r| r.default_model.as_deref())
        .and_then(|id| provider.model(id))
    {
        return Ok(model);
    }
    let list = list_models(provider, agent)?;
    let preferred = provider
        .default_or_first()
        .filter(|model| list.iter().any(|listed| listed.id == model.id));
    Ok(preferred.unwrap_or(list[0]))
}

fn resolve<'a>(provider: &'a Provider, id: &str) -> Result<&'a Model, AdaptError> {
    provider.model(id).ok_or_else(|| AdaptError::UnknownModel {
        provider: provider.name.clone(),
        model: id.to_string(),
    })
}

/// 把原始片段深合并进生成的那一节（JSON 写法的 agent）：对象逐键合并，`null` 删掉这个键，
/// 其余（含数组）整个替换。
pub(crate) fn merge_raw(target: &mut Value, raw: &Map<String, Value>) {
    let Some(target) = target.as_object_mut() else {
        return;
    };
    for (key, value) in raw {
        match (target.get_mut(key), value) {
            (_, Value::Null) => {
                target.shift_remove(key);
            }
            (Some(existing @ Value::Object(_)), Value::Object(inner)) => merge_raw(existing, inner),
            (Some(existing), _) => *existing = value.clone(),
            (None, _) => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

/// 第三方地址必须有 Key（见 [`AdaptError::MissingKey`]）。
fn require_key(provider: &Provider, agent: AgentKind) -> Result<&str, AdaptError> {
    if provider.key.is_empty() {
        return Err(AdaptError::MissingKey {
            agent,
            provider: provider.name.clone(),
        });
    }
    Ok(&provider.key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::tests::relay;

    fn with_override(agent: AgentKind, value: AgentOverride) -> Provider {
        let mut provider = relay();
        provider.overrides.insert(agent, value);
        provider
    }

    #[test]
    fn serializes_as_kebab_case() {
        let names: Vec<String> = AgentKind::ALL
            .iter()
            .map(|agent| {
                serde_json::to_value(agent)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(
            names,
            ["claude-code", "codex", "pi", "grok-build", "opencode"]
        );
        let ids: Vec<&str> = AgentKind::ALL.iter().map(|agent| agent.id()).collect();
        assert_eq!(ids, names);
    }

    #[test]
    fn protocol_follows_the_agent_preference() {
        let provider = relay();
        assert_eq!(
            choose_protocol(&provider, AgentKind::ClaudeCode).unwrap(),
            (Protocol::Anthropic, "https://relay.example")
        );
        assert_eq!(
            choose_protocol(&provider, AgentKind::Pi).unwrap(),
            (Protocol::OpenaiChat, "https://relay.example/v1")
        );
        assert_eq!(
            choose_protocol(&provider, AgentKind::Codex).unwrap().0,
            Protocol::OpenaiResponses
        );
    }

    #[test]
    fn protocol_override_wins_but_must_be_usable() {
        let provider = with_override(
            AgentKind::Pi,
            AgentOverride {
                protocol: Some(Protocol::Anthropic),
                ..AgentOverride::default()
            },
        );
        assert_eq!(
            choose_protocol(&provider, AgentKind::Pi).unwrap().0,
            Protocol::Anthropic
        );

        let provider = with_override(
            AgentKind::Pi,
            AgentOverride {
                protocol: Some(Protocol::Gemini),
                ..AgentOverride::default()
            },
        );
        assert!(matches!(
            choose_protocol(&provider, AgentKind::Pi),
            Err(AdaptError::Incompatible { .. })
        ));
    }

    #[test]
    fn incompatible_providers_explain_what_the_agent_supports() {
        let mut provider = relay();
        provider
            .endpoints
            .retain(|protocol, _| *protocol == Protocol::Anthropic);
        let err = choose_protocol(&provider, AgentKind::Codex).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Relay 没有 Codex 能用的接口（Codex 支持：OpenAI Responses）"
        );
    }

    #[test]
    fn list_takes_the_override_or_the_whole_catalog() {
        let ids = |models: Vec<&Model>| models.iter().map(|m| m.id.clone()).collect::<Vec<_>>();

        assert_eq!(
            ids(list_models(&relay(), AgentKind::Pi).unwrap()),
            ["glm-4.5-air", "glm-4.6", "glm-4.5"]
        );

        let provider = with_override(
            AgentKind::Pi,
            AgentOverride {
                models: Some(vec!["glm-4.5".into()]),
                ..AgentOverride::default()
            },
        );
        assert_eq!(
            ids(list_models(&provider, AgentKind::Pi).unwrap()),
            ["glm-4.5"]
        );

        let mut empty = relay();
        empty.models.clear();
        assert!(matches!(
            list_models(&empty, AgentKind::Pi),
            Err(AdaptError::NoModels { .. })
        ));
    }

    fn from_preset(id: &str) -> Provider {
        preset::get(id).unwrap().provider(id, "sk-test")
    }

    #[test]
    fn the_preset_protocol_sits_between_the_override_and_the_preference() {
        // Kimi For Coding：Pi 偏好 Chat，预设推荐 Anthropic。
        let provider = from_preset("kimi-coding");
        assert_eq!(
            choose_protocol(&provider, AgentKind::Pi).unwrap(),
            (Protocol::Anthropic, "https://api.kimi.com/coding")
        );

        let mut chosen = provider.clone();
        chosen.overrides.insert(
            AgentKind::Pi,
            AgentOverride {
                protocol: Some(Protocol::OpenaiChat),
                ..AgentOverride::default()
            },
        );
        assert_eq!(
            choose_protocol(&chosen, AgentKind::Pi).unwrap().0,
            Protocol::OpenaiChat
        );

        // 用户删了 Anthropic 地址，或者预设不认识（新版本加的、改名了的）：按偏好来，不报错。
        let mut removed = provider.clone();
        removed.endpoints.remove(&Protocol::Anthropic);
        let mut unknown = provider;
        unknown.preset = Some("gone".into());
        for provider in [removed, unknown] {
            assert_eq!(
                choose_protocol(&provider, AgentKind::Pi).unwrap().0,
                Protocol::OpenaiChat
            );
        }
    }

    #[test]
    fn the_preset_default_model_sits_between_the_override_and_the_providers_default() {
        let provider = from_preset("deepseek");
        assert_eq!(
            default_model(&provider, AgentKind::Codex).unwrap().id,
            "deepseek-flash"
        );
        // 没给 Grok 推荐：这家的默认模型，预设没选就是目录第一个。
        assert_eq!(
            default_model(&provider, AgentKind::GrokBuild).unwrap().id,
            "deepseek-v4-pro"
        );

        let mut chosen = provider.clone();
        chosen.overrides.insert(
            AgentKind::Codex,
            AgentOverride {
                default_model: Some("deepseek-v4-pro".into()),
                ..AgentOverride::default()
            },
        );
        assert_eq!(
            default_model(&chosen, AgentKind::Codex).unwrap().id,
            "deepseek-v4-pro"
        );

        let mut removed = provider;
        removed.models.retain(|model| model.id != "deepseek-flash");
        assert_eq!(
            default_model(&removed, AgentKind::Codex).unwrap().id,
            "deepseek-v4-pro"
        );
    }

    #[test]
    fn default_model_is_the_override_or_the_providers_default() {
        assert_eq!(
            default_model(&relay(), AgentKind::Codex).unwrap().id,
            "glm-4.6"
        );
        let provider = with_override(
            AgentKind::Codex,
            AgentOverride {
                default_model: Some("glm-4.5".into()),
                ..AgentOverride::default()
            },
        );
        assert_eq!(
            default_model(&provider, AgentKind::Codex).unwrap().id,
            "glm-4.5"
        );

        // 这个 agent 的列表里没有这家的默认模型：列表第一个。
        let subset = with_override(
            AgentKind::Codex,
            AgentOverride {
                models: Some(vec!["glm-4.5".into(), "glm-4.5-air".into()]),
                ..AgentOverride::default()
            },
        );
        assert_eq!(
            default_model(&subset, AgentKind::Codex).unwrap().id,
            "glm-4.5"
        );
    }
}
