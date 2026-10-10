//! 槽位型 agent（Claude Code 等）怎么从目录里给每个槽位挑模型。
//!
//! 用户填的 > 预设推荐的 > 按强弱档自动填：强槽位用最强的，快槽位用快速档；没标档位就全填同一个。
//! 推荐的模型不在目录里了就当没推荐。

use std::collections::BTreeMap;

use crate::provider::{Model, Provider, Tier};

use super::{AdaptError, AgentKind, recommended};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strength {
    Strong,
    Fast,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    /// 覆盖里 `slots` 用的名字。
    pub key: &'static str,
    pub strength: Strength,
}

/// 最强的模型：档位高的在前（没标档位的排最后），同档打星的在前，再按目录顺序。
pub fn strongest<'a>(models: &[&'a Model]) -> Option<&'a Model> {
    models
        .iter()
        .enumerate()
        .min_by_key(|(index, model)| (tier_rank(model.tier), !model.star, *index))
        .map(|(_, model)| *model)
}

/// 快速档里打星的、再按目录顺序；没有快速档就退回最强的。
pub fn fastest<'a>(models: &[&'a Model]) -> Option<&'a Model> {
    models
        .iter()
        .enumerate()
        .filter(|(_, model)| model.tier == Some(Tier::Fast))
        .min_by_key(|(index, model)| (!model.star, *index))
        .map(|(_, model)| *model)
        .or_else(|| strongest(models))
}

fn tier_rank(tier: Option<Tier>) -> u8 {
    match tier {
        Some(Tier::Flagship) => 0,
        Some(Tier::Balanced) => 1,
        Some(Tier::Fast) => 2,
        None => 3,
    }
}

/// 给槽位填模型，按 `slots` 的顺序返回（槽位，模型 id）。
///
/// `auto` 为假时只返回用户填了的槽位（连 Anthropic 官方时就是这样），预设推荐的也不用。
pub fn fill<'a>(
    provider: &'a Provider,
    agent: AgentKind,
    slots: &[Slot],
    auto: bool,
) -> Result<Vec<(Slot, &'a str)>, AdaptError> {
    let none = BTreeMap::new();
    let chosen = provider.override_for(agent).map_or(&none, |o| &o.slots);
    let suggested = recommended(provider, agent).map_or(&none, |r| &r.slots);
    for key in chosen.keys() {
        if !slots.iter().any(|slot| slot.key == key) {
            return Err(AdaptError::UnknownSlot {
                agent,
                slot: key.clone(),
            });
        }
    }

    let catalog: Vec<&Model> = provider.models.iter().collect();
    if auto && catalog.is_empty() {
        return Err(AdaptError::NoModels {
            agent,
            provider: provider.name.clone(),
        });
    }

    let mut filled = Vec::new();
    for &slot in slots {
        let model = match chosen.get(slot.key) {
            Some(id) => provider.model(id).ok_or_else(|| AdaptError::UnknownModel {
                provider: provider.name.clone(),
                model: id.clone(),
            })?,
            None if auto => suggested
                .get(slot.key)
                .and_then(|id| provider.model(id))
                .or_else(|| match slot.strength {
                    Strength::Strong => strongest(&catalog),
                    Strength::Fast => fastest(&catalog),
                })
                .expect("catalog is not empty"),
            None => continue,
        };
        filled.push((slot, model.id.as_str()));
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::claude;
    use crate::preset;
    use crate::provider::AgentOverride;
    use crate::provider::tests::relay;

    const SLOTS: &[Slot] = &[
        Slot {
            key: "main",
            strength: Strength::Strong,
        },
        Slot {
            key: "haiku",
            strength: Strength::Fast,
        },
    ];

    fn ids(filled: &[(Slot, &str)]) -> Vec<(&'static str, String)> {
        filled
            .iter()
            .map(|(slot, id)| (slot.key, id.to_string()))
            .collect()
    }

    #[test]
    fn tiers_decide_strong_and_fast_slots() {
        let provider = relay();
        let filled = fill(&provider, AgentKind::ClaudeCode, SLOTS, true).unwrap();
        assert_eq!(
            ids(&filled),
            [("main", "glm-4.6".into()), ("haiku", "glm-4.5-air".into())]
        );
    }

    #[test]
    fn without_tiers_every_slot_gets_the_same_model() {
        let mut provider = relay();
        provider.models.iter_mut().for_each(|m| m.tier = None);
        provider.models[0].star = false;
        let filled = fill(&provider, AgentKind::ClaudeCode, SLOTS, true).unwrap();
        // 没有档位时打星的在前：目录里第一个打星的是 glm-4.6。
        assert_eq!(
            ids(&filled),
            [("main", "glm-4.6".into()), ("haiku", "glm-4.6".into())]
        );
    }

    #[test]
    fn user_choices_win_and_are_checked() {
        let mut provider = relay();
        provider.overrides.insert(
            AgentKind::ClaudeCode,
            AgentOverride {
                slots: BTreeMap::from([("haiku".into(), "glm-4.5".into())]),
                ..AgentOverride::default()
            },
        );
        let filled = fill(&provider, AgentKind::ClaudeCode, SLOTS, true).unwrap();
        assert_eq!(
            ids(&filled),
            [("main", "glm-4.6".into()), ("haiku", "glm-4.5".into())]
        );

        let only_user = fill(&provider, AgentKind::ClaudeCode, SLOTS, false).unwrap();
        assert_eq!(ids(&only_user), [("haiku", "glm-4.5".into())]);

        provider
            .overrides
            .get_mut(&AgentKind::ClaudeCode)
            .unwrap()
            .slots
            .insert("opus".into(), "glm-4.6".into());
        assert!(matches!(
            fill(&provider, AgentKind::ClaudeCode, SLOTS, true),
            Err(AdaptError::UnknownSlot { .. })
        ));
    }

    #[test]
    fn preset_slots_sit_between_the_users_and_the_automatic_ones() {
        let slots: Vec<Slot> = claude::SLOTS.iter().map(|(slot, _)| *slot).collect();
        let provider = preset::get("kimi-coding")
            .unwrap()
            .provider("kimi-coding", "sk");
        // 自动会给快槽位挑 HighSpeed；官方配置所有槽位都用 kimi-for-coding。
        let filled = fill(&provider, AgentKind::ClaudeCode, &slots, true).unwrap();
        assert!(
            ids(&filled).iter().all(|(_, id)| id == "kimi-for-coding"),
            "{filled:?}"
        );

        let mut chosen = provider.clone();
        chosen.overrides.insert(
            AgentKind::ClaudeCode,
            AgentOverride {
                slots: [("haiku".into(), "kimi-for-coding-highspeed".into())].into(),
                ..AgentOverride::default()
            },
        );
        let filled = ids(&fill(&chosen, AgentKind::ClaudeCode, &slots, true).unwrap());
        assert!(filled.contains(&("haiku", "kimi-for-coding-highspeed".into())));
        assert!(filled.contains(&("subagent", "kimi-for-coding".into())));

        // 推荐的模型被删了：按档位自动填。
        let mut removed = provider.clone();
        removed.models.retain(|model| model.id != "kimi-for-coding");
        let filled = ids(&fill(&removed, AgentKind::ClaudeCode, &slots, true).unwrap());
        assert!(filled.contains(&("main", "k3".into())));
        assert!(filled.contains(&("haiku", "kimi-for-coding-highspeed".into())));

        // 不自动填的时候，推荐的也不用。
        assert!(
            fill(&provider, AgentKind::ClaudeCode, &slots, false)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn auto_fill_needs_a_catalog() {
        let mut provider = relay();
        provider.models.clear();
        assert!(matches!(
            fill(&provider, AgentKind::ClaudeCode, SLOTS, true),
            Err(AdaptError::NoModels { .. })
        ));
        assert!(
            fill(&provider, AgentKind::ClaudeCode, SLOTS, false)
                .unwrap()
                .is_empty()
        );
    }
}
