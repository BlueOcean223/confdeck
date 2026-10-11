//! 内置的供应商预设：常用供应商的地址、模型目录，和给各 agent 的推荐值。
//!
//! 数据在 `presets.toml`，编进程序里。用预设新建供应商（[`Preset::provider`]）时，名字、地址和模型目录照抄，
//! 之后用户随便改；供应商记下预设的 ID（`Provider::preset`）。推荐值不抄，写配置文件时现查，
//! 排在用户的覆盖和自动之间（decisions「供应商和 agent 的关系」「模型」）：
//!
//! - 协议：用户覆盖的 > 推荐的 > agent 偏好顺序里这家提供的第一种；
//! - 默认模型、槽位：用户填的 > 推荐的 > 这家的默认模型（没选就是目录第一个）。
//!
//! 用户填的值不对（模型不在目录里、协议没有地址）报错；推荐的值不对就当没有推荐：
//! 那是用户自己删了这个模型或地址，按自动的来。

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::agent::AgentKind;
use crate::provider::{Model, Protocol, Provider, normalize_url};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preset {
    pub id: String,
    pub name: String,
    /// 官网。
    pub website: String,
    /// 拿 Key 的页面。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_url: Option<String>,
    pub endpoints: BTreeMap<Protocol, String>,
    pub models: Vec<Model>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agents: BTreeMap<AgentKind, Recommended>,
}

/// 预设给某个 agent 的推荐值，是用户覆盖（`AgentOverride`）里的几项。列表型 agent 用哪些模型还没有预设要推荐，
/// 按打星的来。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Recommended {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<Protocol>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// 槽位 → 模型 id。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub slots: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct File {
    presets: Vec<Preset>,
}

/// 全部预设，按 `presets.toml` 里的顺序。
pub fn all() -> &'static [Preset] {
    static PRESETS: OnceLock<Vec<Preset>> = OnceLock::new();
    PRESETS.get_or_init(|| {
        toml_edit::de::from_str::<File>(include_str!("../presets.toml"))
            .expect("presets.toml is checked by the tests")
            .presets
    })
}

pub fn get(id: &str) -> Option<&'static Preset> {
    all().iter().find(|preset| preset.id == id)
}

/// 这个协议的地址是哪个预设的。几个预设共用这个地址时（智谱的按量和 Coding Plan 共用 Anthropic 地址）认不出，为 `None`。
pub fn matching(protocol: Protocol, url: &str) -> Option<&'static Preset> {
    let url = normalize_url(protocol, url);
    let mut found = all()
        .iter()
        .filter(|preset| preset.endpoints.get(&protocol) == Some(&url));
    let first = found.next()?;
    found.next().is_none().then_some(first)
}

impl Preset {
    /// 用这个预设新建一家供应商：名字、地址、模型目录照抄，Key 由用户填。
    pub fn provider(&self, id: &str, key: &str) -> Provider {
        let mut provider = Provider {
            id: id.to_string(),
            name: self.name.clone(),
            preset: Some(self.id.clone()),
            key: key.to_string(),
            endpoints: self.endpoints.clone(),
            models: self.models.clone(),
            default_model: None,
            overrides: BTreeMap::new(),
        };
        provider.normalize();
        provider
    }

    /// 给这个 agent 的推荐值。
    pub fn recommended(&self, agent: AgentKind) -> Option<&Recommended> {
        self.agents.get(&agent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::claude;

    #[test]
    fn every_preset_makes_a_valid_provider() {
        let presets = all();
        assert!(!presets.is_empty());
        for (index, preset) in presets.iter().enumerate() {
            assert!(
                presets[..index].iter().all(|seen| seen.id != preset.id),
                "duplicate preset {}",
                preset.id
            );
            let provider = preset.provider(&preset.id, "sk-test");
            provider
                .validate()
                .unwrap_or_else(|err| panic!("{}: {err}", preset.id));
            // 地址已经是规范写法，`matching` 才对得上。
            assert_eq!(provider.endpoints, preset.endpoints, "{}", preset.id);
        }
    }

    #[test]
    fn recommendations_point_at_models_and_endpoints_that_exist() {
        for preset in all() {
            for (&agent, recommended) in &preset.agents {
                let at = format!("{} → {}", preset.id, agent.id());
                if let Some(protocol) = recommended.protocol {
                    assert!(agent.protocols().contains(&protocol), "{at}");
                    assert!(preset.endpoints.contains_key(&protocol), "{at}");
                }
                let models = recommended
                    .default_model
                    .iter()
                    .chain(recommended.slots.values());
                for model in models {
                    assert!(
                        preset.models.iter().any(|m| &m.id == model),
                        "{at}: {model}"
                    );
                }
                for slot in recommended.slots.keys() {
                    assert_eq!(agent, AgentKind::ClaudeCode, "{at}");
                    assert!(
                        claude::SLOTS.iter().any(|(s, _)| s.key == slot),
                        "{at}: {slot}"
                    );
                }
            }
        }
    }

    /// 写错的键会被 serde 悄悄丢掉：解析出来再序列化回去，应该和文件里的一样。
    #[test]
    fn the_file_has_no_unknown_or_default_keys() {
        let raw: serde_json::Value =
            toml_edit::de::from_str(include_str!("../presets.toml")).unwrap();
        let parsed = serde_json::json!({ "presets": serde_json::to_value(all()).unwrap() });
        assert_eq!(raw, parsed);
    }

    #[test]
    fn an_address_finds_its_preset_unless_it_is_shared() {
        assert_eq!(
            matching(Protocol::Anthropic, "https://api.deepseek.com/anthropic/").map(|p| &p.id),
            Some(&"deepseek".to_string())
        );
        assert_eq!(
            matching(Protocol::Anthropic, "https://api.kimi.com/coding/").map(|p| &p.id),
            Some(&"kimi-coding".to_string())
        );
        assert!(
            matching(
                Protocol::Anthropic,
                "https://open.bigmodel.cn/api/anthropic"
            )
            .is_none()
        );
        assert_eq!(
            matching(Protocol::OpenaiChat, "https://open.bigmodel.cn/api/paas/v4").map(|p| &p.id),
            Some(&"zhipu".to_string())
        );
        assert_eq!(
            matching(Protocol::Anthropic, "https://api.minimax.io/anthropic").map(|p| &p.id),
            Some(&"minimax-intl".to_string())
        );
        assert_eq!(
            matching(Protocol::OpenaiChat, "https://api.z.ai/api/coding/paas/v4").map(|p| &p.id),
            Some(&"zai-coding".to_string())
        );
        assert!(matching(Protocol::Anthropic, "https://api.z.ai/api/anthropic").is_none());
        assert!(matching(Protocol::OpenaiChat, "https://relay.example/v1").is_none());
    }
}
