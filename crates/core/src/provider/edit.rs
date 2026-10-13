//! 改供应商对某个 agent 的覆盖：配置页「模型」那几张卡点一下就是这里的一个方法（arch-perf R6）。规则：
//! - 手填的、目录里没有的模型先加进目录（decisions「模型」）；
//! - 列表型 agent 的列表和整个目录一样时不存（`None`），不然按目录的顺序存子集；最后一个不能取消；
//!   取消的是默认模型时默认也清掉（退回 [`agent::default_model`] 选的：预设推荐、这家的默认、列表第一个）；
//! - 一个 agent 的覆盖什么都没有了就删掉这一项。

use super::{AgentOverride, Model, Protocol, Provider};
use crate::agent::slots::Slot;
use crate::agent::{self, AgentKind};

impl Provider {
    /// 目录里没有的模型加进目录，排在最后。
    fn add_model(&mut self, id: &str) {
        if self.model(id).is_none() {
            self.models.push(Model {
                id: id.to_string(),
                ..Model::default()
            });
        }
    }

    /// 给槽位选模型，目录里没有的先加进目录。`None` 是「留空」：会自动填的槽位存空串（这个槽位就不写，
    /// 见 [`crate::agent::slots`]），本来就留空的槽位（[`Slot::auto`] 为假）从覆盖里删掉。
    pub fn set_slot(&mut self, agent: AgentKind, slot: Slot, model: Option<&str>) {
        if let Some(model) = model {
            self.add_model(model);
        }
        self.edit(agent, |o| match model {
            None if !slot.auto => {
                o.slots.remove(slot.key);
            }
            model => {
                o.slots
                    .insert(slot.key.to_string(), model.unwrap_or_default().to_string());
            }
        });
    }

    /// 「恢复默认」：清掉这个 agent 的槽位，回到按目录算的。
    pub fn clear_slots(&mut self, agent: AgentKind) {
        self.edit(agent, |o| o.slots.clear());
    }

    /// 列表型 agent 写哪些模型（覆盖的，或整个目录）。覆盖里有目录没有的模型、目录是空的时候是空的。
    pub fn listed(&self, agent: AgentKind) -> Vec<String> {
        agent::list_models(self, agent)
            .map(|models| models.iter().map(|model| model.id.clone()).collect())
            .unwrap_or_default()
    }

    /// 勾上或取消一个模型。最后一个不能取消：返回假，什么也不改。
    pub fn toggle_model(&mut self, agent: AgentKind, id: &str) -> bool {
        let mut listed = self.listed(agent);
        if listed.iter().any(|m| m == id) {
            if listed.len() == 1 {
                return false;
            }
            listed.retain(|m| m != id);
            self.edit(agent, |o| {
                if o.default_model.as_deref() == Some(id) {
                    o.default_model = None;
                }
            });
        } else {
            listed.push(id.to_string());
        }
        self.set_list(agent, listed);
        true
    }

    /// 手填的模型：加进目录，也勾上。
    pub fn add_listed(&mut self, agent: AgentKind, id: &str) {
        self.add_model(id);
        self.check(agent, id);
    }

    /// 这个 agent 的默认模型；没勾上的先勾上。
    pub fn set_default_model(&mut self, agent: AgentKind, id: &str) {
        self.check(agent, id);
        self.edit(agent, |o| o.default_model = Some(id.to_string()));
    }

    /// 多选 agent：这家走哪种接口。
    pub fn set_protocol(&mut self, agent: AgentKind, protocol: Protocol) {
        self.edit(agent, |o| o.protocol = Some(protocol));
    }

    fn check(&mut self, agent: AgentKind, id: &str) {
        let mut listed = self.listed(agent);
        if !listed.iter().any(|m| m == id) {
            listed.push(id.to_string());
            self.set_list(agent, listed);
        }
    }

    /// 列表存进覆盖：和整个目录一样就不存，不然按目录的顺序存子集。
    fn set_list(&mut self, agent: AgentKind, listed: Vec<String>) {
        let ordered: Vec<String> = self
            .models
            .iter()
            .map(|model| model.id.clone())
            .filter(|id| listed.contains(id))
            .collect();
        let whole = ordered.len() == self.models.len();
        self.edit(agent, |o| o.models = (!whole).then_some(ordered));
    }

    /// 改这个 agent 的覆盖；改完什么都没有了就删掉这一项。
    fn edit(&mut self, agent: AgentKind, change: impl FnOnce(&mut AgentOverride)) {
        let o = self.overrides.entry(agent).or_default();
        change(o);
        if *o == AgentOverride::default() {
            self.overrides.remove(&agent);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::claude;
    use crate::provider::tests::relay;

    const PI: AgentKind = AgentKind::Pi;
    const CLAUDE: AgentKind = AgentKind::ClaudeCode;

    fn slot(key: &str) -> Slot {
        claude::SLOTS
            .iter()
            .map(|(slot, _)| *slot)
            .find(|slot| slot.key == key)
            .unwrap()
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|id| id.to_string()).collect()
    }

    #[test]
    fn a_slot_takes_a_model_and_adds_it_to_the_catalog() {
        let mut p = relay();
        p.set_slot(CLAUDE, slot("haiku"), Some("glm-4.5"));
        assert_eq!(p.overrides[&CLAUDE].slots["haiku"], "glm-4.5");
        assert_eq!(p.models.len(), 3);

        p.set_slot(CLAUDE, slot("opus"), Some("kimi-k2"));
        assert_eq!(p.overrides[&CLAUDE].slots["opus"], "kimi-k2");
        assert_eq!(p.models.last().unwrap().id, "kimi-k2");
    }

    #[test]
    fn leaving_a_slot_empty_depends_on_whether_it_fills_itself() {
        let mut p = relay();
        // 自动填的槽位留空：存空串，不写。
        p.set_slot(CLAUDE, slot("haiku"), None);
        assert_eq!(p.overrides[&CLAUDE].slots["haiku"], "");
        // 本来就留空的槽位：填了再留空就是没填，覆盖空了整项删掉。
        p.clear_slots(CLAUDE);
        assert!(p.overrides.is_empty());
        p.set_slot(CLAUDE, slot("subagent"), Some("glm-4.5"));
        p.set_slot(CLAUDE, slot("subagent"), None);
        assert!(p.overrides.is_empty());
    }

    #[test]
    fn clearing_slots_keeps_the_rest_of_the_override() {
        let mut p = relay();
        p.set_protocol(CLAUDE, Protocol::Anthropic);
        p.set_slot(CLAUDE, slot("opus"), Some("glm-4.5"));
        p.clear_slots(CLAUDE);
        assert_eq!(
            p.overrides[&CLAUDE],
            AgentOverride {
                protocol: Some(Protocol::Anthropic),
                ..AgentOverride::default()
            }
        );
    }

    #[test]
    fn the_list_is_stored_only_when_it_is_not_the_whole_catalog() {
        let mut p = relay();
        assert_eq!(p.listed(PI), ids(&["glm-4.5-air", "glm-4.6", "glm-4.5"]));
        assert!(p.toggle_model(PI, "glm-4.5-air"));
        assert_eq!(p.overrides[&PI].models, Some(ids(&["glm-4.6", "glm-4.5"])));
        // 勾回来：按目录的顺序，整个目录就不存，覆盖空了删掉。
        assert!(p.toggle_model(PI, "glm-4.5-air"));
        assert!(p.overrides.is_empty());
    }

    #[test]
    fn the_last_model_cannot_be_unchecked() {
        let mut p = relay();
        p.toggle_model(PI, "glm-4.5-air");
        p.toggle_model(PI, "glm-4.5");
        let before = p.clone();
        assert!(!p.toggle_model(PI, "glm-4.6"));
        assert_eq!(p, before);
    }

    #[test]
    fn unchecking_the_default_clears_it() {
        let mut p = relay();
        p.set_default_model(PI, "glm-4.5");
        assert_eq!(agent::default_model(&p, PI).unwrap().id, "glm-4.5");
        p.toggle_model(PI, "glm-4.5");
        assert_eq!(p.overrides[&PI].default_model, None);
        assert_eq!(
            p.overrides[&PI].models,
            Some(ids(&["glm-4.5-air", "glm-4.6"]))
        );
        // 退回这家的默认模型。
        assert_eq!(agent::default_model(&p, PI).unwrap().id, "glm-4.6");
    }

    #[test]
    fn setting_an_unchecked_default_checks_it() {
        let mut p = relay();
        p.toggle_model(PI, "glm-4.5");
        p.set_default_model(PI, "glm-4.5");
        assert_eq!(p.overrides[&PI].models, None);
        assert_eq!(p.overrides[&PI].default_model.as_deref(), Some("glm-4.5"));
    }

    #[test]
    fn a_typed_model_joins_the_catalog_and_the_list() {
        let mut p = relay();
        // 整个目录：加进目录就勾上了，覆盖还是空的。
        p.add_listed(PI, "kimi-k2");
        assert_eq!(p.models.len(), 4);
        assert!(p.overrides.is_empty());
        // 子集：加进目录，也加进子集。
        p.toggle_model(PI, "glm-4.5");
        p.add_listed(PI, "qwen3");
        assert_eq!(
            p.overrides[&PI].models,
            Some(ids(&["glm-4.5-air", "glm-4.6", "kimi-k2", "qwen3"]))
        );
    }
}
