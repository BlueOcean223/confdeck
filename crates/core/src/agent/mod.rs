//! 适配器：把一家供应商翻译成某个 agent 的配置补丁。只算补丁，不读写磁盘（写入见 [`crate::apply`]）。
//!
//! 每个 agent 的配置里，「关键字段」（请求发到哪、凭什么鉴权、哪个模型、说哪种协议）
//! 完全归供应商所有：切换时一律清空，再写目标供应商的值。其余键归用户，confdeck 不写也不删。

pub mod claude;
pub mod codex;
pub mod pi;
pub mod slots;

use std::fmt;

use serde::{Deserialize, Serialize};

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
            Self::GrokBuild => &[OpenaiResponses, OpenaiChat],
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
    #[error("{agent} 的原始片段还不支持")]
    RawUnsupported { agent: AgentKind },
    #[error("{agent} 的原始片段里 {key} 的值含有 null，TOML 里写不了")]
    RawNotToml { agent: AgentKind, key: String },
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

/// 用哪种协议：用户覆盖的 > agent 偏好顺序里这家提供的第一种。返回协议和地址。
///
/// 预设推荐的协议排在两者之间，等预设做了再加。
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
    supported
        .iter()
        .find_map(|&protocol| provider.endpoint(protocol).map(|url| (protocol, url)))
        .ok_or_else(incompatible)
}

/// 列表型 agent 取哪些模型：用户覆盖的 > 打星的 > 整个目录。
pub fn list_models(provider: &Provider, agent: AgentKind) -> Result<Vec<&Model>, AdaptError> {
    let chosen: Vec<&Model> = match override_of(provider, agent).and_then(|o| o.models.as_ref()) {
        Some(ids) => ids
            .iter()
            .map(|id| resolve(provider, id))
            .collect::<Result<_, _>>()?,
        None => {
            let starred: Vec<&Model> = provider.models.iter().filter(|m| m.star).collect();
            if starred.is_empty() {
                provider.models.iter().collect()
            } else {
                starred
            }
        }
    };
    if chosen.is_empty() {
        return Err(AdaptError::NoModels {
            agent,
            provider: provider.name.clone(),
        });
    }
    Ok(chosen)
}

/// 默认模型：用户覆盖的 > 列表里最强的。
pub fn default_model(provider: &Provider, agent: AgentKind) -> Result<&Model, AdaptError> {
    if let Some(id) = override_of(provider, agent).and_then(|o| o.default_model.as_ref()) {
        return resolve(provider, id);
    }
    let list = list_models(provider, agent)?;
    Ok(slots::strongest(&list).expect("list_models never returns an empty list"))
}

fn resolve<'a>(provider: &'a Provider, id: &str) -> Result<&'a Model, AdaptError> {
    provider.model(id).ok_or_else(|| AdaptError::UnknownModel {
        provider: provider.name.clone(),
        model: id.to_string(),
    })
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
    fn list_takes_the_override_then_starred_then_everything() {
        let ids = |models: Vec<&Model>| models.iter().map(|m| m.id.clone()).collect::<Vec<_>>();

        assert_eq!(
            ids(list_models(&relay(), AgentKind::Pi).unwrap()),
            ["glm-4.5-air", "glm-4.6"]
        );

        let mut unstarred = relay();
        unstarred.models.iter_mut().for_each(|m| m.star = false);
        assert_eq!(list_models(&unstarred, AgentKind::Pi).unwrap().len(), 3);

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

    #[test]
    fn default_model_is_the_override_or_the_strongest_listed() {
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
    }
}
