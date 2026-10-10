//! 槽位型 agent（Claude Code 等）怎么从目录里给每个槽位挑模型。
//!
//! 用户填的 > 预设 > 按强弱档自动填：强槽位用最强的，快槽位用快速档；没标档位就全填同一个。
//! 预设还没做，现在是两层。

use std::collections::BTreeMap;

use crate::provider::{Model, Provider, Tier};

use super::{AdaptError, AgentKind};

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
/// `auto` 为假时只返回用户填了的槽位（连 Anthropic 官方时就是这样）。
pub fn fill<'a>(
    provider: &'a Provider,
    agent: AgentKind,
    slots: &[Slot],
    auto: bool,
) -> Result<Vec<(Slot, &'a str)>, AdaptError> {
    let none = BTreeMap::new();
    let chosen = provider.override_for(agent).map_or(&none, |o| &o.slots);
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
            None if auto => match slot.strength {
                Strength::Strong => strongest(&catalog),
                Strength::Fast => fastest(&catalog),
            }
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
