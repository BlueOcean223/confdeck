//! 把供应商写进 agent 的配置文件：适配器算补丁，引擎写。
//!
//! 单选 agent 用 [`switch`]（`None` 是「默认」）；多选 agent 用 [`enable`] / [`disable`]，
//! 勾一下写一次。Grok Build、OpenCode 的适配器还没做，调用会报 [`ApplyError::Unsupported`]。

use std::path::Path;

use crate::agent::{AdaptError, AgentKind, Mode, claude, codex, pi};
use crate::engine::{Engine, EngineError, LiveFile, Outcome};
use crate::fsx;
use crate::patch::PatchError;
use crate::provider::Provider;

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
            let auth_path = codex::auth_path(home);
            let auth = fsx::read_optional(&auth_path).map_err(|source| EngineError::Io {
                path: auth_path.clone(),
                source,
            })?;
            let patch = codex::patch(provider, codex::login_on_disk(auth.as_deref()))?;
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

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::Value;

    use super::*;
    use crate::backup::{self, Reason};
    use crate::data::DataDir;
    use crate::engine::FileStatus;
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
