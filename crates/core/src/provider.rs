//! 供应商：Key、各协议的地址、模型目录，以及每个 agent 的覆盖。只配一次，写进哪个 agent
//! 由 [`crate::agent`] 里的适配器翻译。
//!
//! 地址的规范写法：Anthropic 兼容地址不带 `/v1`（Claude Code、Pi 都自己拼 `/v1/messages`），
//! OpenAI 兼容地址带 `/v1`。[`Provider::normalize`] 负责把用户粘贴的地址整理成这个样子。

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::agent::AgentKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    Anthropic,
    OpenaiChat,
    OpenaiResponses,
    Gemini,
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Anthropic => "Anthropic",
            Self::OpenaiChat => "OpenAI Chat",
            Self::OpenaiResponses => "OpenAI Responses",
            Self::Gemini => "Gemini",
        })
    }
}

/// 强弱档，自动填槽位时用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tier {
    Flagship,
    Balanced,
    Fast,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Modality {
    Text,
    Image,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    /// 显示名；没有时显示 `id`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 常用模型：列表型 agent 默认只取打星的。
    #[serde(default, skip_serializing_if = "is_false")]
    pub star: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<Tier>,
    /// 上下文长度（token）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<u64>,
    /// 输出上限（token）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<u64>,
    /// 输入模态；空表示只收文本。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input: Vec<Modality>,
    /// 支持的推理档位（`low`、`high` 等）；空表示不支持推理。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning: Vec<String>,
}

/// 某个 agent 对这家供应商的覆盖。每一项都可以不填，不填就用适配器算出来的。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<Protocol>,
    /// 列表型 agent 用哪些模型（目录里的 id）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,
    /// 默认模型（目录里的 id）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// 槽位 → 模型 id，比如 `"haiku": "glm-4.5-air"`。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub slots: BTreeMap<String, String>,
    /// 原始片段：合并进这家供应商在配置文件里的那一节，值为 `null` 表示删掉生成的那个键。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Provider {
    /// 小写字母、数字和连字符；写进 agent 配置时用作键（比如 Pi 的 `confdeck-<id>`）。
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
    pub endpoints: BTreeMap<Protocol, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<Model>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub overrides: BTreeMap<AgentKind, AgentOverride>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    #[error("供应商 ID「{id}」只能用小写字母、数字和连字符，且不能以连字符开头或结尾")]
    BadId { id: String },
    #[error("ID 重复：{id}")]
    DuplicateId { id: String },
    #[error("供应商 {id} 没有名字")]
    EmptyName { id: String },
    #[error("供应商 {id} 一个接口地址都没有")]
    NoEndpoint { id: String },
    #[error("供应商 {id} 的 {protocol} 地址不是 http(s) 地址：{url}")]
    BadUrl {
        id: String,
        protocol: Protocol,
        url: String,
    },
    #[error("供应商 {id} 的模型目录里有空的模型 ID")]
    EmptyModelId { id: String },
    #[error("供应商 {id} 的模型目录里 {model} 出现了不止一次")]
    DuplicateModel { id: String, model: String },
    #[error("供应商 {id} 给 {agent} 的覆盖用了目录里没有的模型 {model}")]
    UnknownModel {
        id: String,
        agent: AgentKind,
        model: String,
    },
}

impl Provider {
    pub fn endpoint(&self, protocol: Protocol) -> Option<&str> {
        self.endpoints.get(&protocol).map(String::as_str)
    }

    pub fn model(&self, id: &str) -> Option<&Model> {
        self.models.iter().find(|model| model.id == id)
    }

    pub fn override_for(&self, agent: AgentKind) -> Option<&AgentOverride> {
        self.overrides.get(&agent)
    }

    /// Anthropic 官方：连它时 Claude Code 的槽位全不写，Key 走 `ANTHROPIC_API_KEY`。
    pub fn is_anthropic_official(&self) -> bool {
        self.endpoint(Protocol::Anthropic)
            .and_then(host)
            .is_some_and(|host| host.eq_ignore_ascii_case("api.anthropic.com"))
    }

    /// 整理用户输入：去掉两侧空白和地址末尾的 `/`，Anthropic 地址再去掉末尾的 `/v1`。
    pub fn normalize(&mut self) {
        self.name = self.name.trim().to_string();
        self.key = self.key.trim().to_string();
        for (protocol, url) in &mut self.endpoints {
            let mut trimmed = url.trim().trim_end_matches('/');
            if *protocol == Protocol::Anthropic {
                trimmed = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
            }
            *url = trimmed.to_string();
        }
        for model in &mut self.models {
            model.id = model.id.trim().to_string();
        }
    }

    pub fn validate(&self) -> Result<(), ProviderError> {
        let id = || self.id.clone();
        if !is_valid_id(&self.id) {
            return Err(ProviderError::BadId { id: id() });
        }
        if self.name.trim().is_empty() {
            return Err(ProviderError::EmptyName { id: id() });
        }
        if self.endpoints.is_empty() {
            return Err(ProviderError::NoEndpoint { id: id() });
        }
        for (&protocol, url) in &self.endpoints {
            if host(url).is_none_or(str::is_empty) {
                return Err(ProviderError::BadUrl {
                    id: id(),
                    protocol,
                    url: url.clone(),
                });
            }
        }
        for (index, model) in self.models.iter().enumerate() {
            if model.id.trim().is_empty() {
                return Err(ProviderError::EmptyModelId { id: id() });
            }
            if self.models[..index].iter().any(|seen| seen.id == model.id) {
                return Err(ProviderError::DuplicateModel {
                    id: id(),
                    model: model.id.clone(),
                });
            }
        }
        for (&agent, overrides) in &self.overrides {
            let referenced = overrides
                .models
                .iter()
                .flatten()
                .chain(&overrides.default_model)
                .chain(overrides.slots.values());
            for model in referenced {
                if self.model(model).is_none() {
                    return Err(ProviderError::UnknownModel {
                        id: id(),
                        agent,
                        model: model.clone(),
                    });
                }
            }
        }
        Ok(())
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('-')
        && !id.ends_with('-')
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// `http(s)://host[:port][/path]` 里的 host；不是 http(s) 地址时为 `None`。
fn host(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    // IPv6 地址带方括号，里面的冒号不是端口分隔符。
    if let Some(bracketed) = authority.strip_prefix('[') {
        return bracketed.split(']').next();
    }
    authority.split(':').next()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// 测试用的第三方供应商：两个协议，三档模型各一个。
    pub(crate) fn relay() -> Provider {
        Provider {
            id: "relay".into(),
            name: "Relay".into(),
            key: "sk-relay".into(),
            endpoints: BTreeMap::from([
                (Protocol::Anthropic, "https://relay.example".into()),
                (Protocol::OpenaiChat, "https://relay.example/v1".into()),
                (Protocol::OpenaiResponses, "https://relay.example/v1".into()),
            ]),
            models: vec![
                Model {
                    id: "glm-4.5-air".into(),
                    tier: Some(Tier::Fast),
                    star: true,
                    ..Model::default()
                },
                Model {
                    id: "glm-4.6".into(),
                    name: Some("GLM 4.6".into()),
                    tier: Some(Tier::Flagship),
                    star: true,
                    context: Some(200_000),
                    output: Some(128_000),
                    input: vec![Modality::Text, Modality::Image],
                    reasoning: vec!["low".into(), "high".into()],
                },
                Model {
                    id: "glm-4.5".into(),
                    tier: Some(Tier::Balanced),
                    ..Model::default()
                },
            ],
            overrides: BTreeMap::new(),
        }
    }

    #[test]
    fn serializes_to_the_documented_shape() {
        let mut provider = relay();
        provider.models.truncate(2);
        provider.overrides.insert(
            AgentKind::Pi,
            AgentOverride {
                protocol: Some(Protocol::Anthropic),
                ..AgentOverride::default()
            },
        );
        let value = serde_json::to_value(&provider).unwrap();
        assert_eq!(
            value,
            json!({
                "id": "relay",
                "name": "Relay",
                "key": "sk-relay",
                "endpoints": {
                    "anthropic": "https://relay.example",
                    "openai-chat": "https://relay.example/v1",
                    "openai-responses": "https://relay.example/v1"
                },
                "models": [
                    {"id": "glm-4.5-air", "star": true, "tier": "fast"},
                    {
                        "id": "glm-4.6",
                        "name": "GLM 4.6",
                        "star": true,
                        "tier": "flagship",
                        "context": 200000,
                        "output": 128000,
                        "input": ["text", "image"],
                        "reasoning": ["low", "high"]
                    }
                ],
                "overrides": {"pi": {"protocol": "anthropic"}}
            })
        );
        let back: Provider = serde_json::from_value(value).unwrap();
        assert_eq!(back, provider);
    }

    #[test]
    fn normalize_trims_and_drops_the_anthropic_v1() {
        let mut provider = relay();
        provider.name = "  Relay ".into();
        provider.key = " sk-relay\n".into();
        provider
            .endpoints
            .insert(Protocol::Anthropic, " https://relay.example/v1/ ".into());
        provider
            .endpoints
            .insert(Protocol::OpenaiChat, "https://relay.example/v1/".into());
        provider.normalize();
        assert_eq!(provider.name, "Relay");
        assert_eq!(provider.key, "sk-relay");
        assert_eq!(
            provider.endpoint(Protocol::Anthropic),
            Some("https://relay.example")
        );
        assert_eq!(
            provider.endpoint(Protocol::OpenaiChat),
            Some("https://relay.example/v1")
        );
    }

    #[test]
    fn recognizes_the_official_anthropic_host() {
        let mut provider = relay();
        assert!(!provider.is_anthropic_official());
        provider
            .endpoints
            .insert(Protocol::Anthropic, "https://API.anthropic.com".into());
        assert!(provider.is_anthropic_official());
        provider.endpoints.insert(
            Protocol::Anthropic,
            "https://api.anthropic.com.evil.example".into(),
        );
        assert!(!provider.is_anthropic_official());
    }

    #[test]
    fn host_handles_ports_userinfo_and_ipv6() {
        assert_eq!(host("http://localhost:11434/v1"), Some("localhost"));
        assert_eq!(host("https://u:p@relay.example/v1"), Some("relay.example"));
        assert_eq!(host("http://[::1]:8080"), Some("::1"));
        assert_eq!(host("ftp://relay.example"), None);
    }

    #[test]
    fn validate_rejects_bad_ids_urls_and_models() {
        assert_eq!(relay().validate(), Ok(()));

        for bad in ["", "Relay", "re lay", "-relay", "relay-", "中转"] {
            let mut provider = relay();
            provider.id = bad.into();
            assert!(
                matches!(provider.validate(), Err(ProviderError::BadId { .. })),
                "{bad:?}"
            );
        }

        let mut provider = relay();
        provider
            .endpoints
            .insert(Protocol::Gemini, "generativelanguage.googleapis.com".into());
        assert!(matches!(
            provider.validate(),
            Err(ProviderError::BadUrl {
                protocol: Protocol::Gemini,
                ..
            })
        ));

        let mut provider = relay();
        provider.endpoints.clear();
        assert!(matches!(
            provider.validate(),
            Err(ProviderError::NoEndpoint { .. })
        ));

        let mut provider = relay();
        provider.models.push(Model {
            id: "glm-4.6".into(),
            ..Model::default()
        });
        assert!(matches!(
            provider.validate(),
            Err(ProviderError::DuplicateModel { .. })
        ));

        let mut provider = relay();
        provider.overrides.insert(
            AgentKind::ClaudeCode,
            AgentOverride {
                slots: BTreeMap::from([("haiku".into(), "gpt-5".into())]),
                ..AgentOverride::default()
            },
        );
        assert_eq!(
            provider.validate(),
            Err(ProviderError::UnknownModel {
                id: "relay".into(),
                agent: AgentKind::ClaudeCode,
                model: "gpt-5".into(),
            })
        );
    }
}
