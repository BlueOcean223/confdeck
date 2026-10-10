//! 把供应商写进 agent 的配置文件：适配器算补丁，引擎写。
//!
//! 单选 agent 用 [`switch`]（`None` 是「默认」）；多选 agent 用 [`enable`] / [`disable`]，
//! 勾一下写一次。改了供应商用 [`save_provider`]：存好以后把改动写进所有在用它的 agent。
//! Grok Build、OpenCode 的适配器还没做，调用会报 [`ApplyError::Unsupported`]。

use std::path::{Path, PathBuf};

use crate::agent::{AdaptError, AgentKind, Mode, claude, codex, pi};
use crate::current::{self, Current};
use crate::engine::{Engine, EngineError, LiveFile, Outcome};
use crate::fsx;
use crate::patch::{self, LivePatch, PatchError};
use crate::provider::Provider;
use crate::store::{self, StoreError};

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error(transparent)]
    Adapt(#[from] AdaptError),
    #[error(transparent)]
    Engine(#[from] EngineError),
    /// 读 agent 的配置文件时解析不了（认当前是哪家时）。
    #[error(transparent)]
    Parse(#[from] PatchError),
    #[error("{agent} 的适配器还没做")]
    Unsupported { agent: AgentKind },
    #[error("{agent} 是{}，不能这样写", mode_name(*.agent))]
    WrongMode { agent: AgentKind },
}

fn mode_name(agent: AgentKind) -> &'static str {
    match agent.mode() {
        Mode::Switch => "单选 agent",
        Mode::Coexist => "多选 agent",
    }
}

/// 这个 agent 由 confdeck 写的配置文件（备份页、状态检查用）。
pub fn live_files(home: &Path, agent: AgentKind) -> Vec<LiveFile> {
    match agent {
        AgentKind::ClaudeCode => vec![claude::live_file(home)],
        AgentKind::Codex => vec![codex::live_file(home)],
        AgentKind::Pi => vec![pi::live_file(home)],
        AgentKind::GrokBuild | AgentKind::Opencode => Vec::new(),
    }
}

/// 单选 agent 换供应商；`None` 还原成 agent 自带的登录。写完记下选了哪家（只存在本机）。
pub fn switch(
    engine: &Engine,
    home: &Path,
    agent: AgentKind,
    provider: Option<&Provider>,
) -> Result<Outcome, ApplyError> {
    let outcome = write_switch(engine, home, agent, provider)?;
    engine.set_selection(agent.id(), provider.map(|provider| provider.id.as_str()))?;
    Ok(outcome)
}

fn write_switch(
    engine: &Engine,
    home: &Path,
    agent: AgentKind,
    provider: Option<&Provider>,
) -> Result<Outcome, ApplyError> {
    match agent {
        AgentKind::ClaudeCode => {
            let patch = claude::patch(provider)?;
            Ok(engine.write(&claude::live_file(home), &patch)?)
        }
        AgentKind::Codex => {
            let patch = codex::patch(provider, codex_login(home)?)?;
            Ok(engine.write(&codex::live_file(home), &patch)?)
        }
        AgentKind::Pi | AgentKind::Opencode => Err(ApplyError::WrongMode { agent }),
        AgentKind::GrokBuild => Err(ApplyError::Unsupported { agent }),
    }
}

/// 多选 agent 勾上一家供应商（已有就整项更新）。
pub fn enable(
    engine: &Engine,
    home: &Path,
    agent: AgentKind,
    provider: &Provider,
) -> Result<Outcome, ApplyError> {
    match agent {
        AgentKind::Pi => Ok(engine.write(&pi::live_file(home), &pi::add(provider)?)?),
        AgentKind::Opencode => Err(ApplyError::Unsupported { agent }),
        _ => Err(ApplyError::WrongMode { agent }),
    }
}

/// 多选 agent 取消勾选一家供应商。
pub fn disable(
    engine: &Engine,
    home: &Path,
    agent: AgentKind,
    provider_id: &str,
) -> Result<Outcome, ApplyError> {
    match agent {
        AgentKind::Pi => Ok(engine.write(&pi::live_file(home), &pi::remove(provider_id))?),
        AgentKind::Opencode => Err(ApplyError::Unsupported { agent }),
        _ => Err(ApplyError::WrongMode { agent }),
    }
}

/// `~/.codex/auth.json` 里有没有 ChatGPT 登录，决定 `requires_openai_auth`。
fn codex_login(home: &Path) -> Result<bool, EngineError> {
    let path = codex::auth_path(home);
    let auth = fsx::read_optional(&path).map_err(|source| EngineError::Io {
        path: path.clone(),
        source,
    })?;
    Ok(codex::login_on_disk(auth.as_deref()))
}

/// 这个 agent 的配置文件，和「把某一家写进去」的补丁：单选 agent 是切到这家，多选 agent 是勾上这家。
pub(crate) struct Writer {
    pub file: LiveFile,
    pub patch: Box<PatchFor>,
}

pub(crate) type PatchFor = dyn Fn(&Provider) -> Result<Box<dyn LivePatch>, AdaptError>;

pub(crate) fn writer(home: &Path, agent: AgentKind) -> Result<Writer, ApplyError> {
    let (file, patch): (LiveFile, Box<PatchFor>) = match agent {
        AgentKind::ClaudeCode => (
            claude::live_file(home),
            Box::new(|provider| Ok(Box::new(claude::patch(Some(provider))?))),
        ),
        AgentKind::Codex => {
            let login = codex_login(home)?;
            (
                codex::live_file(home),
                Box::new(move |provider| Ok(Box::new(codex::patch(Some(provider), login)?))),
            )
        }
        AgentKind::Pi => (
            pi::live_file(home),
            Box::new(|provider| Ok(Box::new(pi::add(provider)?))),
        ),
        AgentKind::GrokBuild | AgentKind::Opencode => {
            return Err(ApplyError::Unsupported { agent });
        }
    };
    Ok(Writer { file, patch })
}

/// 改了一家供应商以后，一个在用它的 agent 的结果。
#[derive(Debug)]
pub struct Synced {
    pub agent: AgentKind,
    /// 这个 agent 的配置文件（配置里的路径，可以是软链接）。
    pub path: PathBuf,
    /// 写好了（`changed` 为假是这次的改动和这个 agent 无关，没写），或者为什么没写成。
    pub result: Result<Outcome, ApplyError>,
}

/// 保存一家供应商（新建，或按 ID 替换列表里的那一家），再把改动写进所有在用它的 agent。
///
/// `providers` 是改之前的列表。ID 是供应商的身份，改不了：改了 ID 就是新建了一家。
///
/// - **「在用」**：单选 agent 按改之前的列表认出来就是这家（[`current::current`]），
///   多选 agent 勾了这家。「Key 不一致」不算在用。
/// - **只写变了的键**：改前、改后各写到空文件上，只把两者不一样的键写进去（[`patch::delta`]）。
///   换个 Key 不会顺带清掉 Claude Code 里 `/model` 选的模型、Codex 的推理档位、手加的设置。
/// - **先存后写**：`providers.json` 存不了就报错，什么都不写；之后每个 agent 单独写，
///   一个失败（文件解析不了、这家改完以后这个 agent 用不了了）不影响别的，结果逐个返回。
///   界面据此提示「已同步到 Claude Code、Pi」，写失败的单独标出来（提案 02）。
///
/// 新建的、内容没变的，返回空列表。
pub fn save_provider(
    engine: &Engine,
    home: &Path,
    providers: &[Provider],
    provider: &Provider,
) -> Result<Vec<Synced>, StoreError> {
    let before = providers.iter().find(|old| old.id == provider.id);
    let mut after = providers.to_vec();
    match after.iter_mut().find(|old| old.id == provider.id) {
        Some(slot) => *slot = provider.clone(),
        None => after.push(provider.clone()),
    }
    store::save(engine, &after)?;

    let Some(before) = before.filter(|before| *before != provider) else {
        return Ok(Vec::new());
    };
    let mut synced = Vec::new();
    for agent in AgentKind::ALL {
        // 还没有适配器的 agent 不会在用。
        let Some(file) = live_files(home, agent).into_iter().next() else {
            continue;
        };
        let result = match in_use(engine, home, agent, providers, &before.id) {
            Ok(false) => continue,
            Ok(true) => write_changes(engine, home, agent, before, provider),
            Err(err) => Err(err),
        };
        synced.push(Synced {
            agent,
            path: file.path,
            result,
        });
    }
    Ok(synced)
}

fn in_use(
    engine: &Engine,
    home: &Path,
    agent: AgentKind,
    providers: &[Provider],
    id: &str,
) -> Result<bool, ApplyError> {
    Ok(match agent.mode() {
        Mode::Switch => {
            current::current(engine, home, agent, providers)? == Current::Provider(id.to_string())
        }
        Mode::Coexist => current::enabled(engine, home, agent)?
            .iter()
            .any(|enabled| enabled == id),
    })
}

/// 把 `before` 改成 `after` 以后，这家在 agent 配置文件里生成的内容里变了的键写进去。
fn write_changes(
    engine: &Engine,
    home: &Path,
    agent: AgentKind,
    before: &Provider,
    after: &Provider,
) -> Result<Outcome, ApplyError> {
    let Writer { file, patch } = writer(home, agent)?;
    let new = patch(after)?;
    let old = match patch(before) {
        Ok(old) => old.apply(&file.path, None)?,
        // 改之前这家就写不进这个 agent（多选 agent 里留着的旧项）：整项按改后的写。
        Err(_) => return Ok(engine.write(&file, new.as_ref())?),
    };
    let new = new.apply(&file.path, None)?;
    let delta = patch::delta(file.format, &file.path, &old, &new)?;
    Ok(engine.write(&file, delta.as_ref())?)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::Value;

    use super::*;
    use crate::agent::Credential;
    use crate::backup::{self, Reason};
    use crate::data::DataDir;
    use crate::engine::FileStatus;
    use crate::provider::Protocol;
    use crate::provider::tests::relay;

    struct Home {
        dir: tempfile::TempDir,
        engine: Engine,
    }

    impl Home {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let engine = Engine::new(DataDir::new(dir.path().join(".confdeck")));
            Self { dir, engine }
        }

        fn path(&self) -> &Path {
            self.dir.path()
        }

        fn write(&self, relative: &str, text: &str) {
            let path = self.path().join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }

        fn read(&self, relative: &str) -> String {
            fs::read_to_string(self.path().join(relative)).unwrap()
        }
    }

    #[test]
    fn claude_code_switches_and_goes_back_to_default() {
        let home = Home::new();
        home.write(
            ".claude/settings.json",
            "{\n  \"permissions\": {\"allow\": [\"Bash\"]}\n}\n",
        );

        let outcome = switch(
            &home.engine,
            home.path(),
            AgentKind::ClaudeCode,
            Some(&relay()),
        )
        .unwrap();
        assert_eq!(outcome.backup.unwrap().reason, Reason::Original);
        let value: Value = serde_json::from_str(&home.read(".claude/settings.json")).unwrap();
        assert_eq!(value["env"]["ANTHROPIC_BASE_URL"], "https://relay.example");
        assert_eq!(value["permissions"]["allow"][0], "Bash");

        let outcome = switch(&home.engine, home.path(), AgentKind::ClaudeCode, None).unwrap();
        assert!(outcome.changed && outcome.backup.is_none());
        let value: Value = serde_json::from_str(&home.read(".claude/settings.json")).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"permissions": {"allow": ["Bash"]}})
        );
    }

    #[test]
    fn switching_records_the_selection_only_after_a_successful_write() {
        let home = Home::new();
        assert_eq!(home.engine.selection("claude-code"), None);
        switch(
            &home.engine,
            home.path(),
            AgentKind::ClaudeCode,
            Some(&relay()),
        )
        .unwrap();
        assert_eq!(
            home.engine.selection("claude-code"),
            Some(Some("relay".into()))
        );

        let mut keyless = relay();
        keyless.id = "keyless".into();
        keyless.key.clear();
        assert!(
            switch(
                &home.engine,
                home.path(),
                AgentKind::ClaudeCode,
                Some(&keyless)
            )
            .is_err()
        );
        assert_eq!(
            home.engine.selection("claude-code"),
            Some(Some("relay".into()))
        );

        switch(&home.engine, home.path(), AgentKind::ClaudeCode, None).unwrap();
        assert_eq!(home.engine.selection("claude-code"), Some(None));
    }

    #[test]
    fn codex_follows_the_login_on_disk() {
        let home = Home::new();
        let outcome = switch(&home.engine, home.path(), AgentKind::Codex, Some(&relay())).unwrap();
        assert!(
            outcome.backup.is_none(),
            "a new file has nothing to back up"
        );
        assert!(
            home.read(".codex/config.toml")
                .contains("requires_openai_auth = false")
        );

        home.write(".codex/auth.json", r#"{"tokens": {"refresh_token": "rt"}}"#);
        switch(&home.engine, home.path(), AgentKind::Codex, Some(&relay())).unwrap();
        assert!(
            home.read(".codex/config.toml")
                .contains("requires_openai_auth = true")
        );
    }

    #[test]
    fn pi_enables_and_disables_one_entry() {
        let home = Home::new();
        home.write(".pi/agent/models.json", "{\n  // 我的本地模型\n  \"providers\": {\"ollama\": {\"api\": \"openai-completions\"}},\n}\n");

        enable(&home.engine, home.path(), AgentKind::Pi, &relay()).unwrap();
        let value: Value = serde_json::from_str(&home.read(".pi/agent/models.json")).unwrap();
        assert!(value["providers"]["confdeck-relay"].is_object());
        assert!(value["providers"]["ollama"].is_object());
        // 注释在第一次写入时丢了，原件里还有。
        let backups =
            backup::list(home.engine.data(), &pi::live_file(home.path()).shelf()).unwrap();
        assert!(
            fs::read_to_string(&backups[0].path)
                .unwrap()
                .contains("我的本地模型")
        );

        disable(&home.engine, home.path(), AgentKind::Pi, "relay").unwrap();
        let value: Value = serde_json::from_str(&home.read(".pi/agent/models.json")).unwrap();
        assert_eq!(value["providers"].as_object().unwrap().len(), 1);
        assert_eq!(
            home.engine.status(&pi::live_file(home.path())).unwrap(),
            FileStatus::Ours
        );
    }

    fn other() -> Provider {
        let mut other = relay();
        other.id = "other".into();
        other.name = "Other".into();
        other.key = "sk-other".into();
        other.endpoints = [
            (Protocol::Anthropic, "https://other.example".into()),
            (Protocol::OpenaiChat, "https://other.example/v1".into()),
            (Protocol::OpenaiResponses, "https://other.example/v1".into()),
        ]
        .into();
        other
    }

    impl Home {
        fn json(&self, relative: &str) -> Value {
            serde_json::from_str(&self.read(relative)).unwrap()
        }

        fn edit_json(&self, relative: &str, edit: impl FnOnce(&mut Value)) {
            let mut value = self.json(relative);
            edit(&mut value);
            self.write(relative, &serde_json::to_string_pretty(&value).unwrap());
        }

        /// Claude Code、Codex 切到 `providers[0]`，Pi 勾上全部。
        fn use_everywhere(&self, providers: &[Provider]) {
            store::save(&self.engine, providers).unwrap();
            for agent in [AgentKind::ClaudeCode, AgentKind::Codex] {
                switch(&self.engine, self.path(), agent, Some(&providers[0])).unwrap();
            }
            for provider in providers {
                enable(&self.engine, self.path(), AgentKind::Pi, provider).unwrap();
            }
        }

        fn save(&self, providers: &[Provider], provider: &Provider) -> Vec<Synced> {
            save_provider(&self.engine, self.path(), providers, provider).unwrap()
        }

        fn current(&self, agent: AgentKind, providers: &[Provider]) -> Current {
            current::current(&self.engine, self.path(), agent, providers).unwrap()
        }
    }

    fn agents(synced: &[Synced]) -> Vec<AgentKind> {
        synced.iter().map(|synced| synced.agent).collect()
    }

    fn outcome(synced: &[Synced], agent: AgentKind) -> &Outcome {
        let synced = synced.iter().find(|synced| synced.agent == agent).unwrap();
        synced.result.as_ref().unwrap()
    }

    #[test]
    fn a_new_key_reaches_every_agent_using_the_provider_and_nothing_else() {
        let home = Home::new();
        let providers = [relay(), other()];
        home.use_everywhere(&providers);
        // 在 agent 里换了模型、调了推理档位、手加了设置。
        home.edit_json(".claude/settings.json", |value| {
            value["model"] = "glm-4.5".into();
            value["env"]["ANTHROPIC_CUSTOM_HEADERS"] = "X-Team: infra".into();
        });
        let codex = home.read(".codex/config.toml");
        home.write(
            ".codex/config.toml",
            &format!("model_reasoning_effort = \"high\"\n{codex}")
                .replace("model = \"glm-4.6\"", "model = \"glm-4.5\""),
        );

        let mut rotated = relay();
        rotated.key = "sk-new".into();
        let synced = home.save(&providers, &rotated);
        assert_eq!(
            agents(&synced),
            [AgentKind::ClaudeCode, AgentKind::Codex, AgentKind::Pi]
        );
        assert_eq!(synced[0].path, home.path().join(".claude/settings.json"));
        for agent in [AgentKind::ClaudeCode, AgentKind::Codex] {
            let outcome = outcome(&synced, agent);
            assert!(outcome.changed);
            assert_eq!(
                outcome.backup.as_ref().unwrap().reason,
                Reason::External,
                "{agent} was edited after confdeck wrote it"
            );
        }
        assert!(outcome(&synced, AgentKind::Pi).backup.is_none());

        let claude = home.json(".claude/settings.json");
        assert_eq!(claude["env"]["ANTHROPIC_AUTH_TOKEN"], "sk-new");
        assert_eq!(claude["model"], "glm-4.5");
        assert_eq!(claude["env"]["ANTHROPIC_CUSTOM_HEADERS"], "X-Team: infra");
        let codex = home.read(".codex/config.toml");
        assert!(codex.contains("experimental_bearer_token = \"sk-new\""));
        assert!(codex.contains("model_reasoning_effort = \"high\""));
        assert!(codex.contains("model = \"glm-4.5\""));
        let pi = home.json(".pi/agent/models.json");
        assert_eq!(pi["providers"]["confdeck-relay"]["apiKey"], "sk-new");
        assert_eq!(pi["providers"]["confdeck-other"]["apiKey"], "sk-other");

        let saved = store::load(home.engine.data()).unwrap();
        assert_eq!(saved, [rotated, other()]);
        for agent in [AgentKind::ClaudeCode, AgentKind::Codex] {
            assert_eq!(
                home.current(agent, &saved),
                Current::Provider("relay".into())
            );
        }
    }

    #[test]
    fn an_address_change_is_written_with_each_agents_spelling() {
        let home = Home::new();
        let providers = [relay()];
        home.use_everywhere(&providers);

        let mut moved = relay();
        moved.endpoints = [
            (Protocol::Anthropic, "https://moved.example".into()),
            (Protocol::OpenaiChat, "https://moved.example/v1".into()),
            (Protocol::OpenaiResponses, "https://moved.example/v1".into()),
        ]
        .into();
        let synced = home.save(&providers, &moved);
        assert_eq!(synced.len(), 3);
        assert_eq!(
            home.json(".claude/settings.json")["env"]["ANTHROPIC_BASE_URL"],
            "https://moved.example"
        );
        assert!(
            home.read(".codex/config.toml")
                .contains("base_url = \"https://moved.example/v1\"")
        );
        assert_eq!(
            home.json(".pi/agent/models.json")["providers"]["confdeck-relay"]["baseUrl"],
            "https://moved.example/v1"
        );
    }

    #[test]
    fn agents_on_another_provider_are_left_alone() {
        let home = Home::new();
        // 和 relay 同一个地址和 Key，Claude Code 上次选的是它。
        let mut twin = relay();
        twin.id = "twin".into();
        let providers = [relay(), twin.clone(), other()];
        store::save(&home.engine, &providers).unwrap();
        switch(
            &home.engine,
            home.path(),
            AgentKind::ClaudeCode,
            Some(&twin),
        )
        .unwrap();
        switch(&home.engine, home.path(), AgentKind::Codex, Some(&other())).unwrap();
        enable(&home.engine, home.path(), AgentKind::Pi, &other()).unwrap();
        let before: Vec<String> = [
            ".claude/settings.json",
            ".codex/config.toml",
            ".pi/agent/models.json",
        ]
        .iter()
        .map(|relative| home.read(relative))
        .collect();

        let mut rotated = relay();
        rotated.key = "sk-new".into();
        assert!(home.save(&providers, &rotated).is_empty());
        assert_eq!(home.read(".claude/settings.json"), before[0]);
        assert_eq!(home.read(".codex/config.toml"), before[1]);
        assert_eq!(home.read(".pi/agent/models.json"), before[2]);
    }

    #[test]
    fn a_rename_only_touches_agents_that_write_the_name() {
        let home = Home::new();
        let providers = [relay()];
        home.use_everywhere(&providers);
        let claude = home.read(".claude/settings.json");

        let mut renamed = relay();
        renamed.name = "Relay Pro".into();
        let synced = home.save(&providers, &renamed);
        assert_eq!(synced.len(), 3);
        assert!(!outcome(&synced, AgentKind::ClaudeCode).changed);
        assert!(outcome(&synced, AgentKind::Codex).changed);
        assert!(outcome(&synced, AgentKind::Pi).changed);
        assert_eq!(home.read(".claude/settings.json"), claude);
        assert!(
            home.read(".codex/config.toml")
                .contains("name = \"Relay Pro\"")
        );
    }

    #[test]
    fn a_failure_is_reported_for_its_agent_and_the_others_are_still_written() {
        let home = Home::new();
        let providers = [relay()];
        home.use_everywhere(&providers);
        let claude = home.read(".claude/settings.json");
        home.write(".codex/config.toml", "model = \n");

        // 去掉 Anthropic 地址以后 Claude Code 用不了这家了。
        let mut changed = relay();
        changed.key = "sk-new".into();
        changed.endpoints.remove(&Protocol::Anthropic);
        let synced = home.save(&providers, &changed);
        assert_eq!(
            agents(&synced),
            [AgentKind::ClaudeCode, AgentKind::Codex, AgentKind::Pi]
        );
        assert!(matches!(
            synced[0].result,
            Err(ApplyError::Adapt(AdaptError::Incompatible { .. }))
        ));
        assert!(matches!(synced[1].result, Err(ApplyError::Parse(_))));
        assert!(outcome(&synced, AgentKind::Pi).changed);
        assert_eq!(home.read(".claude/settings.json"), claude);
        assert_eq!(home.read(".codex/config.toml"), "model = \n");
        assert_eq!(
            store::load(home.engine.data()).unwrap(),
            [changed],
            "the provider is saved even when an agent fails"
        );
    }

    #[test]
    fn a_refused_save_writes_nothing() {
        let home = Home::new();
        let providers = [relay()];
        home.use_everywhere(&providers);
        let claude = home.read(".claude/settings.json");

        let mut broken = relay();
        broken.key = "sk-new".into();
        broken.name.clear();
        assert!(matches!(
            save_provider(&home.engine, home.path(), &providers, &broken),
            Err(StoreError::Invalid { .. })
        ));
        assert_eq!(home.read(".claude/settings.json"), claude);
        assert_eq!(store::load(home.engine.data()).unwrap(), providers);
    }

    #[test]
    fn new_and_unchanged_providers_write_nothing() {
        let home = Home::new();
        let providers = [relay()];
        home.use_everywhere(&providers);
        assert!(home.save(&providers, &relay()).is_empty());
        assert!(home.save(&providers, &other()).is_empty());
        assert_eq!(store::load(home.engine.data()).unwrap(), [relay(), other()]);
    }

    #[test]
    fn taking_the_key_from_a_mismatched_file_updates_the_other_agents() {
        let home = Home::new();
        let providers = [relay()];
        home.use_everywhere(&providers);
        home.edit_json(".claude/settings.json", |value| {
            value["env"]["ANTHROPIC_AUTH_TOKEN"] = "sk-rotated".into();
        });
        assert_eq!(
            home.current(AgentKind::ClaudeCode, &providers),
            Current::KeyMismatch {
                provider: "relay".into()
            }
        );

        // 「把文件里的 Key 存进 Relay」。
        let live = current::connection(&home.engine, home.path(), AgentKind::ClaudeCode).unwrap();
        let mut adopted = relay();
        adopted.key = live.credential().and_then(Credential::key).unwrap().into();
        let synced = home.save(&providers, &adopted);
        assert_eq!(agents(&synced), [AgentKind::Codex, AgentKind::Pi]);
        assert!(
            home.read(".codex/config.toml")
                .contains("experimental_bearer_token = \"sk-rotated\"")
        );
        assert_eq!(
            home.current(AgentKind::ClaudeCode, &[adopted]),
            Current::Provider("relay".into())
        );
    }

    #[test]
    fn modes_and_missing_adapters_are_reported() {
        let home = Home::new();
        let provider = relay();
        assert!(matches!(
            switch(&home.engine, home.path(), AgentKind::Pi, Some(&provider)),
            Err(ApplyError::WrongMode { .. })
        ));
        assert!(matches!(
            enable(&home.engine, home.path(), AgentKind::ClaudeCode, &provider),
            Err(ApplyError::WrongMode { .. })
        ));
        assert!(matches!(
            switch(
                &home.engine,
                home.path(),
                AgentKind::GrokBuild,
                Some(&provider)
            ),
            Err(ApplyError::Unsupported { .. })
        ));
        let err = switch(&home.engine, home.path(), AgentKind::Opencode, None).unwrap_err();
        assert_eq!(err.to_string(), "OpenCode 是多选 agent，不能这样写");
    }
}
