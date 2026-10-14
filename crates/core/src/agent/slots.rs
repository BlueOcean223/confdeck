//! 槽位型 agent（Claude Code 等）怎么从目录里给每个槽位挑模型。
//!
//! 用户填的 > 预设推荐的 > 全填这家的默认模型（[`Provider::default_or_first`]）。
//! 推荐的模型不在目录里了就当没推荐。用户填成空串的槽位是「留空」：不写，agent 用它自己的默认
//! （槽位不是必填的）。不自动填的槽位（[`Slot::auto`] 为假）用户填了才写。

use std::collections::BTreeMap;

use crate::provider::Provider;

use super::{AdaptError, AgentKind, recommended};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    /// 覆盖里 `slots` 用的名字。
    pub key: &'static str,
    /// 用户没填时按预设推荐、默认模型填上。为假的默认留空（Claude Code 的子代理）。
    pub auto: bool,
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

    let default = provider.default_or_first();
    if auto && default.is_none() {
        return Err(AdaptError::NoModels {
            agent,
            provider: provider.name.clone(),
        });
    }

    let mut filled = Vec::new();
    for &slot in slots {
        let model = match chosen.get(slot.key) {
            // 留空：不写这个槽位。
            Some(id) if id.is_empty() => continue,
            Some(id) => provider.model(id).ok_or_else(|| AdaptError::UnknownModel {
                provider: provider.name.clone(),
                model: id.clone(),
            })?,
            None if auto && slot.auto => suggested
                .get(slot.key)
                .and_then(|id| provider.model(id))
                .or(default)
                .expect("checked above: the catalog is not empty"),
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
            key: "opus",
            auto: true,
        },
        Slot {
            key: "haiku",
            auto: true,
        },
        Slot {
            key: "subagent",
            auto: false,
        },
    ];

    fn ids(filled: &[(Slot, &str)]) -> Vec<(&'static str, String)> {
        filled
            .iter()
            .map(|(slot, id)| (slot.key, id.to_string()))
            .collect()
    }

    #[test]
    fn every_slot_gets_the_default_model() {
        let provider = relay();
        let filled = fill(&provider, AgentKind::ClaudeCode, SLOTS, true).unwrap();
        assert_eq!(
            ids(&filled),
            [("opus", "glm-4.6".into()), ("haiku", "glm-4.6".into())]
        );

        // 没选默认模型：目录第一个。选的那个不在目录里了也一样。
        for default in [None, Some("gone".to_string())] {
            let mut provider = relay();
            provider.default_model = default;
            let filled = fill(&provider, AgentKind::ClaudeCode, SLOTS, true).unwrap();
            assert_eq!(
                ids(&filled),
                [
                    ("opus", "glm-4.5-air".into()),
                    ("haiku", "glm-4.5-air".into())
                ]
            );
        }
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
            [("opus", "glm-4.6".into()), ("haiku", "glm-4.5".into())]
        );

        let only_user = fill(&provider, AgentKind::ClaudeCode, SLOTS, false).unwrap();
        assert_eq!(ids(&only_user), [("haiku", "glm-4.5".into())]);

        provider
            .overrides
            .get_mut(&AgentKind::ClaudeCode)
            .unwrap()
            .slots
            .insert("main".into(), "glm-4.6".into());
        assert!(matches!(
            fill(&provider, AgentKind::ClaudeCode, SLOTS, true),
            Err(AdaptError::UnknownSlot { .. })
        ));
    }

    #[test]
    fn an_empty_choice_leaves_the_slot_out() {
        let mut provider = relay();
        provider.overrides.insert(
            AgentKind::ClaudeCode,
            AgentOverride {
                slots: BTreeMap::from([("haiku".into(), String::new())]),
                ..AgentOverride::default()
            },
        );
        let filled = fill(&provider, AgentKind::ClaudeCode, SLOTS, true).unwrap();
        assert_eq!(ids(&filled), [("opus", "glm-4.6".into())]);
        let filled = fill(&provider, AgentKind::ClaudeCode, SLOTS, false).unwrap();
        assert!(filled.is_empty());
    }

    #[test]
    fn preset_slots_sit_between_the_users_and_the_automatic_ones() {
        let slots: Vec<Slot> = claude::SLOTS.iter().map(|(slot, _)| *slot).collect();
        let provider = preset::get("kimi-coding")
            .unwrap()
            .provider("kimi-coding", "sk");
        // 预设推荐 Haiku 用 kimi-for-coding，它也是目录第一个：所有槽位都是它。子代理默认留空。
        let filled = ids(&fill(&provider, AgentKind::ClaudeCode, &slots, true).unwrap());
        assert!(
            filled.iter().all(|(_, id)| id == "kimi-for-coding"),
            "{filled:?}"
        );
        assert!(filled.iter().all(|(slot, _)| *slot != "subagent"));

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
        assert!(filled.contains(&("opus", "kimi-for-coding".into())));

        // 推荐的模型被删了：全填默认模型，也就是剩下的第一个。
        let mut removed = provider.clone();
        removed.models.retain(|model| model.id != "kimi-for-coding");
        let filled = ids(&fill(&removed, AgentKind::ClaudeCode, &slots, true).unwrap());
        assert!(
            filled
                .iter()
                .all(|(_, id)| id == "kimi-for-coding-highspeed"),
            "{filled:?}"
        );

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
