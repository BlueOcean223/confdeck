//! 把供应商写进 agent 的配置文件：适配器算补丁，引擎写。
//!
//! 单选 agent 用 [`switch`]（`None` 是「默认」）；多选 agent 用 [`enable`] / [`disable`]，
//! 勾一下写一次。改了供应商用 [`save_provider`]：存好以后把改动写进所有在用它的 agent；
//! 删掉用 [`delete_provider`]，之前用 [`used_by`] 列出在用的 agent 请用户确认。
//! 启动时调 [`refresh_codex_catalog`]：Codex 升级以后，模型目录里的提示词要跟着换。

use std::path::{Path, PathBuf};

use crate::agent::codex::catalog::Skipped;
use crate::agent::{AdaptError, AgentKind, Mode, claude, codex, grok, opencode, pi};
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
        AgentKind::Codex => vec![codex::live_file(home), codex::catalog::live_file(home)],
        AgentKind::Pi => vec![pi::live_file(home)],
        AgentKind::GrokBuild => vec![grok::live_file(home)],
        AgentKind::Opencode => vec![opencode::live_file(home)],
    }
}

/// [`switch`] 的结果。
#[derive(Debug)]
pub struct Switched {
    /// agent 的配置文件写了没有。
    pub outcome: Outcome,
    /// Codex 切到第三方时的模型目录文件：写了，或者没写成的原因。没写成时界面要说一声：
    /// `/model` 列的还是 GPT 模型，这家的模型按 272K 窗口算。别的 agent、Codex 切回默认时为 `None`。
    pub catalog: Option<Result<Outcome, Skipped>>,
}

/// 单选 agent 换供应商；`None` 还原成 agent 自带的登录。写完记下选了哪家（只存在本机）。
pub fn switch(
    engine: &Engine,
    home: &Path,
    agent: AgentKind,
    provider: Option<&Provider>,
) -> Result<Switched, ApplyError> {
    let switched = write_switch(engine, home, agent, provider)?;
    engine.set_selection(agent.id(), provider.map(|provider| provider.id.as_str()))?;
    Ok(switched)
}

fn write_switch(
    engine: &Engine,
    home: &Path,
    agent: AgentKind,
    provider: Option<&Provider>,
) -> Result<Switched, ApplyError> {
    let only = |outcome| Switched {
        outcome,
        catalog: None,
    };
    match agent {
        AgentKind::ClaudeCode => {
            let patch = claude::patch(provider, &engine.owned(agent.id()))?;
            let outcome = engine.write(&claude::live_file(home), &patch)?;
            engine.set_owned(agent.id(), &claude::raw_entries(provider)?)?;
            Ok(only(outcome))
        }
        AgentKind::Codex => {
            let config = codex::live_file(home);
            let patch = codex::patch(provider, codex_login(home)?)?;
            let catalog = match provider {
                Some(provider) => {
                    codex::catalog::generate(home, &codex::catalog_models(provider)?).map(Some)
                }
                None => Ok(None),
            };
            let catalog = match catalog {
                Ok(Some(catalog)) => catalog,
                Ok(None) => return Ok(only(engine.write(&config, &patch)?)),
                Err(skipped) => {
                    return Ok(Switched {
                        outcome: engine.write(&config, &patch)?,
                        catalog: Some(Err(skipped)),
                    });
                }
            };
            // 先试着改 config.toml：改不了（解析不了、profile 盖过了路由）就什么都不写。
            // 再先写目录、后改指针：指针指着不存在的文件，Codex 起不来。
            let patch = patch.with_catalog();
            patch.apply(&config.path, engine.read(&config)?.bytes.as_deref())?;
            let written = engine.write(&codex::catalog::live_file(home), &patch::Whole(catalog))?;
            Ok(Switched {
                outcome: engine.write(&config, &patch)?,
                catalog: Some(Ok(written)),
            })
        }
        AgentKind::GrokBuild => Ok(only(
            engine.write(&grok::live_file(home), &grok::patch(provider)?)?,
        )),
        AgentKind::Pi | AgentKind::Opencode => Err(ApplyError::WrongMode { agent }),
    }
}

/// 照本机现在的 Codex 重写模型目录，启动时调一次。目录里的提示词是切换时从本机 Codex 取的，
/// Codex 升级以后还是旧的；confdeck 升级也可能改了生成的内容。
///
/// `config.toml` 指着 confdeck 的目录、认得出是哪家（地址对得上就行，Key 不一致也算）才写。
/// 生成的和文件里一样就不写，所以每次启动都跑一遍（约 50 毫秒），不用记 Codex 的版本。
/// 返回 `None` 是不用写；`Some(Err(_))` 是这次没生成出来（比如 `codex` 卸了），旧目录留着照样能用。
pub fn refresh_codex_catalog(
    engine: &Engine,
    home: &Path,
    providers: &[Provider],
) -> Result<Option<Result<Outcome, Skipped>>, ApplyError> {
    let config = codex::live_file(home);
    if !codex::uses_catalog(&config.path, engine.read(&config)?.bytes.as_deref()) {
        return Ok(None);
    }
    let id = match current::current(engine, home, AgentKind::Codex, providers)? {
        Current::Provider(id) | Current::KeyMismatch { provider: id } => id,
        Current::Default | Current::Custom { .. } => return Ok(None),
    };
    let Some(provider) = providers.iter().find(|provider| provider.id == id) else {
        return Ok(None);
    };
    let catalog = match codex::catalog::generate(home, &codex::catalog_models(provider)?) {
        Ok(catalog) => catalog,
        Err(skipped) => return Ok(Some(Err(skipped))),
    };
    let file = codex::catalog::live_file(home);
    Ok(Some(Ok(engine.write(&file, &patch::Whole(catalog))?)))
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
        AgentKind::Opencode => {
            Ok(engine.write(&opencode::live_file(home), &opencode::add(provider)?)?)
        }
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
        AgentKind::Opencode => {
            Ok(engine.write(&opencode::live_file(home), &opencode::remove(provider_id))?)
        }
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
            Box::new(|provider| Ok(Box::new(claude::patch(Some(provider), &[])?))),
        ),
        AgentKind::Codex => {
            let login = codex_login(home)?;
            let file = codex::live_file(home);
            // 现在指着 confdeck 的模型目录就照样指着：认当前是哪家时逐字比较，改供应商时这一项不变。
            let current = fsx::read_optional(&file.path).map_err(|source| EngineError::Io {
                path: file.path.clone(),
                source,
            })?;
            let catalog = codex::uses_catalog(&file.path, current.as_deref());
            (
                file,
                Box::new(move |provider| {
                    let patch = codex::patch(Some(provider), login)?;
                    Ok(Box::new(if catalog { patch.with_catalog() } else { patch }))
                }),
            )
        }
        AgentKind::GrokBuild => (
            grok::live_file(home),
            Box::new(|provider| Ok(Box::new(grok::patch(Some(provider))?))),
        ),
        AgentKind::Pi => (
            pi::live_file(home),
            Box::new(|provider| Ok(Box::new(pi::add(provider)?))),
        ),
        AgentKind::Opencode => (
            opencode::live_file(home),
            Box::new(|provider| Ok(Box::new(opencode::add(provider)?))),
        ),
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
    for (agent, file, in_use) in scan(engine, home, providers, &before.id, AgentKind::ALL) {
        let result = match in_use {
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

/// 在用一家供应商的 agent。
#[derive(Debug, Default)]
pub struct UsedBy {
    pub agents: Vec<AgentKind>,
    /// 配置文件读不了、解析不了，认不出在不在用的。
    pub unknown: Vec<(AgentKind, ApplyError)>,
}

/// 哪些 agent 在用这家（`providers` 是现在的列表）：删除前列出来请用户确认，供应商页的「X 在用」。
/// 「在用」的认法同 [`save_provider`]。
pub fn used_by(engine: &Engine, home: &Path, providers: &[Provider], id: &str) -> UsedBy {
    let mut used_by = UsedBy::default();
    for (agent, _, in_use) in scan(engine, home, providers, id, AgentKind::ALL) {
        match in_use {
            Ok(true) => used_by.agents.push(agent),
            Ok(false) => {}
            Err(err) => used_by.unknown.push((agent, err)),
        }
    }
    used_by
}

/// 删掉一家供应商（10-10 用户定的）。先存，存不了就什么都不写。
///
/// - 单选 agent 的文件不动：之后显示「自定义 · 主机名」，还能「存成供应商」找回来。
/// - 多选 agent 删掉勾的这一项，不然会留下一项 confdeck 再也管不到的配置。每个写了的（或该写没写成的）返回一项。
///
/// 不在列表里的 ID 什么都不做。
pub fn delete_provider(
    engine: &Engine,
    home: &Path,
    providers: &[Provider],
    id: &str,
) -> Result<Vec<Synced>, StoreError> {
    let after: Vec<Provider> = providers
        .iter()
        .filter(|provider| provider.id != id)
        .cloned()
        .collect();
    if after.len() == providers.len() {
        return Ok(Vec::new());
    }
    store::save(engine, &after)?;

    let coexist = AgentKind::ALL
        .into_iter()
        .filter(|agent| agent.mode() == Mode::Coexist);
    let mut synced = Vec::new();
    for (agent, file, in_use) in scan(engine, home, providers, id, coexist) {
        let result = match in_use {
            Ok(false) => continue,
            Ok(true) => disable(engine, home, agent, id),
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

/// 逐个 agent 看在不在用 `id` 这家。
fn scan(
    engine: &Engine,
    home: &Path,
    providers: &[Provider],
    id: &str,
    agents: impl IntoIterator<Item = AgentKind>,
) -> Vec<(AgentKind, LiveFile, Result<bool, ApplyError>)> {
    agents
        .into_iter()
        .filter_map(|agent| {
            let file = live_files(home, agent).into_iter().next()?;
            Some((agent, file, in_use(engine, home, agent, providers, id)))
        })
        .collect()
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
    // Codex 的模型列表在目录文件里，先重写目录（找不到 `codex` 就留着旧的）。
    if agent == AgentKind::Codex
        && codex::uses_catalog(&file.path, engine.read(&file)?.bytes.as_deref())
        && let Ok(catalog) = codex::catalog::generate(home, &codex::catalog_models(after)?)
    {
        engine.write(&codex::catalog::live_file(home), &patch::Whole(catalog))?;
    }
    let outcome = engine.write(&file, delta.as_ref())?;
    if agent == AgentKind::ClaudeCode {
        engine.set_owned(agent.id(), &claude::raw_entries(Some(after))?)?;
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::Value;

    use super::*;
    use crate::agent::Credential;
    use crate::backup::Reason;
    use crate::data::DataDir;
    use crate::engine::FileStatus;
    use crate::provider::tests::relay;
    use crate::provider::{AgentOverride, Protocol};

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
        .unwrap()
        .outcome;
        assert_eq!(outcome.backup.unwrap().reason, Reason::Original);
        let value: Value = serde_json::from_str(&home.read(".claude/settings.json")).unwrap();
        assert_eq!(value["env"]["ANTHROPIC_BASE_URL"], "https://relay.example");
        assert_eq!(value["permissions"]["allow"][0], "Bash");

        let outcome = switch(&home.engine, home.path(), AgentKind::ClaudeCode, None)
            .unwrap()
            .outcome;
        assert!(outcome.changed && outcome.backup.is_none());
        let value: Value = serde_json::from_str(&home.read(".claude/settings.json")).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"permissions": {"allow": ["Bash"]}})
        );
    }

    #[test]
    fn claude_code_raw_keys_follow_the_provider() {
        let home = Home::new();
        let raw = |value: Value| AgentOverride {
            raw: Some(value.as_object().unwrap().clone()),
            ..AgentOverride::default()
        };
        let mut provider = relay();
        provider.overrides.insert(
            AgentKind::ClaudeCode,
            raw(serde_json::json!({"env": {
                "API_TIMEOUT_MS": "600000",
                "CLAUDE_CODE_DISABLE_ARTIFACT": "1"
            }})),
        );
        let providers = vec![provider.clone()];
        switch(
            &home.engine,
            home.path(),
            AgentKind::ClaudeCode,
            Some(&provider),
        )
        .unwrap();
        let env = |home: &Home| -> Value {
            let value: Value = serde_json::from_str(&home.read(".claude/settings.json")).unwrap();
            value["env"].clone()
        };
        assert_eq!(env(&home)["CLAUDE_CODE_DISABLE_ARTIFACT"], "1");
        assert_eq!(
            current::current(&home.engine, home.path(), AgentKind::ClaudeCode, &providers).unwrap(),
            Current::Provider("relay".into())
        );

        // 改了原始片段：去掉一个、改一个，记录跟着换成新的。
        let mut edited = provider.clone();
        edited.overrides.insert(
            AgentKind::ClaudeCode,
            raw(serde_json::json!({"env": {"API_TIMEOUT_MS": "900000"}})),
        );
        save_provider(&home.engine, home.path(), &providers, &edited).unwrap();
        assert_eq!(env(&home)["API_TIMEOUT_MS"], "900000");
        assert!(env(&home).get("CLAUDE_CODE_DISABLE_ARTIFACT").is_none());

        switch(&home.engine, home.path(), AgentKind::ClaudeCode, None).unwrap();
        assert!(
            !home
                .read(".claude/settings.json")
                .contains("API_TIMEOUT_MS")
        );
        assert!(home.engine.owned("claude-code").is_empty());
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
        let switched = switch(&home.engine, home.path(), AgentKind::Codex, Some(&relay())).unwrap();
        // 测试里找不到 `codex`：不写目录，告诉界面为什么。
        assert_eq!(switched.catalog, Some(Err(Skipped::NotFound)));
        assert!(
            switched.outcome.backup.is_none(),
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
    fn codex_writes_the_catalog_before_pointing_at_it() {
        let home = Home::new();
        // 测试里默认找不到 `codex`：不写目录，也不指过去。
        switch(&home.engine, home.path(), AgentKind::Codex, Some(&relay())).unwrap();
        assert!(
            !home
                .read(".codex/config.toml")
                .contains("model_catalog_json")
        );
        assert!(!home.path().join(".codex/confdeck-models.json").exists());

        codex::catalog::tests::use_codex(home.path(), &codex::catalog::tests::sample());
        switch(&home.engine, home.path(), AgentKind::Codex, Some(&relay())).unwrap();
        assert!(
            home.read(".codex/config.toml")
                .contains("model_catalog_json = \"confdeck-models.json\"")
        );
        let slugs = |home: &Home| -> Vec<String> {
            let catalog: Value =
                serde_json::from_str(&home.read(".codex/confdeck-models.json")).unwrap();
            catalog["models"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| entry["slug"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(slugs(&home), ["glm-4.5-air", "glm-4.6"]);

        // 改了模型目录：目录文件跟着重写。
        let providers = vec![relay()];
        let mut edited = relay();
        edited.models[2].star = true;
        save_provider(&home.engine, home.path(), &providers, &edited).unwrap();
        assert_eq!(slugs(&home), ["glm-4.5-air", "glm-4.6", "glm-4.5"]);

        // 改不了 config.toml 时目录文件也不动。
        let before = home.read(".codex/confdeck-models.json");
        home.write(
            ".codex/config.toml",
            "profile = \"work\"\n[profiles.work]\nmodel_provider = \"azure\"\n",
        );
        let mut other = relay();
        other.models.retain(|model| model.id == "glm-4.6");
        assert!(switch(&home.engine, home.path(), AgentKind::Codex, Some(&other)).is_err());
        assert_eq!(home.read(".codex/confdeck-models.json"), before);

        home.write(".codex/config.toml", "");
        switch(&home.engine, home.path(), AgentKind::Codex, None).unwrap();
        assert!(
            !home
                .read(".codex/config.toml")
                .contains("model_catalog_json")
        );
    }

    #[test]
    fn the_codex_catalog_follows_the_installed_codex() {
        use codex::catalog::tests::{no_codex, sample, use_codex};
        let home = Home::new();
        let providers = vec![relay()];
        let refresh = || refresh_codex_catalog(&home.engine, home.path(), &providers).unwrap();
        let prompt = |home: &Home| -> String {
            let catalog: Value =
                serde_json::from_str(&home.read(".codex/confdeck-models.json")).unwrap();
            catalog["models"][0]["base_instructions"]
                .as_str()
                .unwrap()
                .to_string()
        };
        // 没指着 confdeck 的目录：不用写。
        assert_eq!(refresh(), None);

        use_codex(home.path(), &sample());
        switch(&home.engine, home.path(), AgentKind::Codex, Some(&relay())).unwrap();
        assert!(matches!(
            refresh(),
            Some(Ok(Outcome { changed: false, .. }))
        ));

        // Codex 升级，内置的提示词变了：目录跟着重写。
        let mut upgraded = sample();
        upgraded["models"][1]["base_instructions"] =
            "You are Codex, based on GPT-5. Plan first.".into();
        use_codex(home.path(), &upgraded);
        assert!(matches!(refresh(), Some(Ok(Outcome { changed: true, .. }))));
        assert_eq!(
            prompt(&home),
            "You are Codex, based on glm-4.5-air. Plan first."
        );

        // 找不到 `codex` 了：旧目录留着。
        no_codex();
        assert_eq!(refresh(), Some(Err(Skipped::NotFound)));
        assert_eq!(
            prompt(&home),
            "You are Codex, based on glm-4.5-air. Plan first."
        );

        // 认不出是哪家：不写。
        assert_eq!(
            refresh_codex_catalog(&home.engine, home.path(), &[]).unwrap(),
            None
        );
    }

    #[test]
    fn pi_enables_and_disables_one_entry() {
        let home = Home::new();
        home.write(".pi/agent/models.json", "{\n  // 我的本地模型\n  \"providers\": {\"ollama\": {\"api\": \"openai-completions\"}},\n}\n");

        enable(&home.engine, home.path(), AgentKind::Pi, &relay()).unwrap();
        let text = home.read(".pi/agent/models.json");
        let value: Value = json5::from_str(&text).unwrap();
        assert!(value["providers"]["confdeck-relay"].is_object());
        assert!(value["providers"]["ollama"].is_object());
        assert!(text.contains("// 我的本地模型"));

        disable(&home.engine, home.path(), AgentKind::Pi, "relay").unwrap();
        let value: Value = json5::from_str(&home.read(".pi/agent/models.json")).unwrap();
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

        /// 单选 agent 切到 `providers[0]`，多选 agent 勾上全部。
        fn use_everywhere(&self, providers: &[Provider]) {
            store::save(&self.engine, providers).unwrap();
            for agent in [
                AgentKind::ClaudeCode,
                AgentKind::Codex,
                AgentKind::GrokBuild,
            ] {
                switch(&self.engine, self.path(), agent, Some(&providers[0])).unwrap();
            }
            for provider in providers {
                for agent in [AgentKind::Pi, AgentKind::Opencode] {
                    enable(&self.engine, self.path(), agent, provider).unwrap();
                }
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
        let grok = home.read(".grok/config.toml");
        home.write(
            ".grok/config.toml",
            &grok.replace(
                "default = \"confdeck-glm-4.6\"",
                "default = \"confdeck-glm-4.5-air\"",
            ),
        );
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
            [
                AgentKind::ClaudeCode,
                AgentKind::Codex,
                AgentKind::Pi,
                AgentKind::GrokBuild,
                AgentKind::Opencode
            ]
        );
        assert_eq!(synced[0].path, home.path().join(".claude/settings.json"));
        for agent in [
            AgentKind::ClaudeCode,
            AgentKind::Codex,
            AgentKind::GrokBuild,
        ] {
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
        let grok = home.read(".grok/config.toml");
        assert_eq!(grok.matches("api_key = \"sk-new\"").count(), 2, "{grok}");
        assert!(grok.contains("default = \"confdeck-glm-4.5-air\""));
        let pi = home.json(".pi/agent/models.json");
        assert_eq!(pi["providers"]["confdeck-relay"]["apiKey"], "sk-new");
        let opencode = home.json(".config/opencode/opencode.json");
        assert_eq!(
            opencode["provider"]["confdeck-relay"]["options"]["apiKey"],
            "sk-new"
        );
        assert_eq!(
            opencode["provider"]["confdeck-other"]["options"]["apiKey"],
            "sk-other"
        );
        assert_eq!(pi["providers"]["confdeck-other"]["apiKey"], "sk-other");

        let saved = store::load(home.engine.data()).unwrap();
        assert_eq!(saved, [rotated, other()]);
        for agent in [
            AgentKind::ClaudeCode,
            AgentKind::Codex,
            AgentKind::GrokBuild,
        ] {
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
        assert_eq!(synced.len(), 5);
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
        assert!(
            !home.read(".grok/config.toml").contains("relay.example"),
            "every model table moves"
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
        assert_eq!(synced.len(), 5);
        assert!(!outcome(&synced, AgentKind::ClaudeCode).changed);
        assert!(outcome(&synced, AgentKind::Codex).changed);
        assert!(outcome(&synced, AgentKind::Pi).changed);
        assert!(outcome(&synced, AgentKind::GrokBuild).changed);
        assert!(
            home.read(".grok/config.toml")
                .contains("name = \"GLM 4.6 (Relay Pro)\"")
        );
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
            [
                AgentKind::ClaudeCode,
                AgentKind::Codex,
                AgentKind::Pi,
                AgentKind::GrokBuild,
                AgentKind::Opencode
            ]
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
        assert_eq!(
            agents(&synced),
            [
                AgentKind::Codex,
                AgentKind::Pi,
                AgentKind::GrokBuild,
                AgentKind::Opencode
            ]
        );
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
    fn deleting_a_provider_drops_its_multi_select_entries_and_leaves_switch_files_alone() {
        let home = Home::new();
        let providers = [relay(), other()];
        home.use_everywhere(&providers);
        let claude = home.read(".claude/settings.json");

        let used = used_by(&home.engine, home.path(), &providers, "relay");
        assert_eq!(
            used.agents,
            [
                AgentKind::ClaudeCode,
                AgentKind::Codex,
                AgentKind::Pi,
                AgentKind::GrokBuild,
                AgentKind::Opencode
            ]
        );
        assert!(used.unknown.is_empty());
        let used = used_by(&home.engine, home.path(), &providers, "other");
        assert_eq!(used.agents, [AgentKind::Pi, AgentKind::Opencode]);

        let synced = delete_provider(&home.engine, home.path(), &providers, "relay").unwrap();
        assert_eq!(agents(&synced), [AgentKind::Pi, AgentKind::Opencode]);
        assert!(outcome(&synced, AgentKind::Pi).changed);
        assert_eq!(store::load(home.engine.data()).unwrap(), [other()]);
        assert_eq!(home.read(".claude/settings.json"), claude);
        let pi = home.json(".pi/agent/models.json");
        assert!(pi["providers"].get("confdeck-relay").is_none());
        let opencode = home.json(".config/opencode/opencode.json");
        assert!(opencode["provider"].get("confdeck-relay").is_none());
        assert!(opencode["provider"]["confdeck-other"].is_object());
        assert!(pi["providers"]["confdeck-other"].is_object());
        // Claude Code 还连着原来的地址，现在认作「自定义」。
        assert_eq!(
            home.current(AgentKind::ClaudeCode, &[other()]),
            Current::Custom {
                host: Some("relay.example".into())
            }
        );

        assert!(
            delete_provider(&home.engine, home.path(), &[other()], "relay")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn files_that_cannot_be_read_are_listed_as_unknown() {
        let home = Home::new();
        let providers = [relay()];
        home.use_everywhere(&providers);
        home.write(".pi/agent/models.json", "{ broken");

        let used = used_by(&home.engine, home.path(), &providers, "relay");
        assert_eq!(
            used.agents,
            [
                AgentKind::ClaudeCode,
                AgentKind::Codex,
                AgentKind::GrokBuild,
                AgentKind::Opencode
            ]
        );
        assert_eq!(used.unknown.len(), 1);
        assert_eq!(used.unknown[0].0, AgentKind::Pi);

        let synced = delete_provider(&home.engine, home.path(), &providers, "relay").unwrap();
        assert_eq!(agents(&synced), [AgentKind::Pi, AgentKind::Opencode]);
        assert!(matches!(synced[0].result, Err(ApplyError::Parse(_))));
        assert!(store::load(home.engine.data()).unwrap().is_empty());
    }

    #[test]
    fn modes_are_reported() {
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
        let err = switch(&home.engine, home.path(), AgentKind::Opencode, None).unwrap_err();
        assert_eq!(err.to_string(), "OpenCode 是多选 agent，不能这样写");
    }
}
