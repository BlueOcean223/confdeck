//! 认出 agent 现在用的是哪家：单选 agent 连的是哪家，多选 agent 勾了哪几家。
//!
//! **以文件为准**，confdeck 的记录只用来在几家都对得上时挑一家。单选 agent 按这个顺序认：
//!
//! 1. 文件里没有指定地址和凭据：「默认」（agent 自带的登录）。
//! 2. 上次在 confdeck 里选的那家，地址和 Key 和文件对得上：就是它。
//! 3. 列表里关键字段和文件完全一致的一家；没有的话，地址和 Key 对得上的第一家。
//! 4. 都对不上：「自定义 · 主机名」（被别的工具或手改成了别的地址）。
//!
//! 只比地址和凭据，不比模型名：在 Claude Code 里用 `/model` 换个模型，还算同一家。
//! 同一个地址和 Key 配了几家（比如同一个中转按模型分成几张卡片），靠第 2、3 步区分。

use std::path::Path;

use crate::agent::{AgentKind, Connection, Mode, claude, codex, pi};
use crate::apply::ApplyError;
use crate::engine::{Engine, EngineError, LiveFile};
use crate::fsx;
use crate::patch::LivePatch;
use crate::provider::Provider;

/// 单选 agent 现在用的是哪家。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Current {
    /// agent 自带的登录。
    Default,
    /// 供应商 id。
    Provider(String),
    /// 和列表里哪家都对不上，显示「自定义 · 主机名」。
    Custom { host: Option<String> },
}

/// 一家供应商写进去会是什么样。
struct Candidate<'a> {
    id: &'a str,
    expected: Connection,
    /// 关键字段和文件完全一致（这家的补丁对当前文件不改任何东西）。
    exact: bool,
}

/// 单选 agent 现在用的是哪家。配置文件不存在算「默认」；解析不了报错。
pub fn current(
    engine: &Engine,
    home: &Path,
    agent: AgentKind,
    providers: &[Provider],
) -> Result<Current, ApplyError> {
    let (file, read, patch): (LiveFile, Reader, Box<PatchFor>) = match agent {
        AgentKind::ClaudeCode => (
            claude::live_file(home),
            claude::connection,
            Box::new(|provider| Ok(Box::new(claude::patch(Some(provider))?))),
        ),
        AgentKind::Codex => {
            let auth_path = codex::auth_path(home);
            let auth = fsx::read_optional(&auth_path).map_err(|source| EngineError::Io {
                path: auth_path.clone(),
                source,
            })?;
            let login = codex::login_on_disk(auth.as_deref());
            (
                codex::live_file(home),
                codex::connection,
                Box::new(move |provider| Ok(Box::new(codex::patch(Some(provider), login)?))),
            )
        }
        AgentKind::GrokBuild => return Err(ApplyError::Unsupported { agent }),
        AgentKind::Pi | AgentKind::Opencode => return Err(ApplyError::WrongMode { agent }),
    };

    let snapshot = engine.read(&file)?;
    let live = read(&file.path, snapshot.bytes.as_deref())?;
    let candidates: Vec<Candidate> = providers
        .iter()
        .filter_map(|provider| {
            // 这个 agent 用不了的供应商（接口对不上、没有 Key）不参与。
            let patch = patch(provider).ok()?;
            let fresh = patch.apply(&file.path, None).ok()?;
            let expected = read(&file.path, Some(&fresh)).ok()?;
            let exact = snapshot.bytes.as_deref().is_some_and(|bytes| {
                patch.apply(&file.path, Some(bytes)).ok().as_deref() == Some(bytes)
            });
            Some(Candidate {
                id: &provider.id,
                expected,
                exact,
            })
        })
        .collect();
    let record = engine.selection(agent.id()).flatten();
    Ok(identify(&live, &candidates, record.as_deref()))
}

type Reader = fn(&Path, Option<&[u8]>) -> Result<Connection, crate::patch::PatchError>;
type PatchFor = dyn Fn(&Provider) -> Result<Box<dyn LivePatch>, crate::agent::AdaptError>;

fn identify(live: &Connection, candidates: &[Candidate], record: Option<&str>) -> Current {
    if *live == Connection::Default {
        return Current::Default;
    }
    let matching: Vec<&Candidate> = candidates.iter().filter(|c| c.expected == *live).collect();
    let chosen = record
        .and_then(|id| matching.iter().find(|c| c.id == id))
        .or_else(|| matching.iter().find(|c| c.exact))
        .or_else(|| matching.first());
    match chosen {
        Some(candidate) => Current::Provider(candidate.id.to_string()),
        None => Current::Custom { host: live.host() },
    }
}

/// 多选 agent 勾了哪几家（供应商 id，按文件里的顺序）。
pub fn enabled(engine: &Engine, home: &Path, agent: AgentKind) -> Result<Vec<String>, ApplyError> {
    match agent {
        AgentKind::Pi => {
            let file = pi::live_file(home);
            let snapshot = engine.read(&file)?;
            Ok(pi::enabled(&file.path, snapshot.bytes.as_deref())?)
        }
        AgentKind::Opencode => Err(ApplyError::Unsupported { agent }),
        _ if agent.mode() == Mode::Switch => Err(ApplyError::WrongMode { agent }),
        _ => Err(ApplyError::Unsupported { agent }),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use super::*;
    use crate::apply::{enable, switch};
    use crate::data::DataDir;
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

        fn edit(&self, relative: &str, edit: impl FnOnce(String) -> String) {
            let path = self.path().join(relative);
            let text = fs::read_to_string(&path).unwrap_or_default();
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, edit(text)).unwrap();
        }

        fn current(&self, agent: AgentKind, providers: &[Provider]) -> Current {
            current(&self.engine, self.path(), agent, providers).unwrap()
        }
    }

    /// 和 relay 同一个地址和 Key，只是模型不同（同一个中转按模型分成两张卡片）。
    fn twin() -> Provider {
        let mut twin = relay();
        twin.id = "twin".into();
        twin.models.retain(|model| model.id == "glm-4.5");
        twin
    }

    fn other() -> Provider {
        let mut other = relay();
        other.id = "other".into();
        other.key = "sk-other".into();
        other.endpoints = BTreeMap::from([
            (Protocol::Anthropic, "https://other.example".into()),
            (Protocol::OpenaiResponses, "https://other.example/v1".into()),
        ]);
        other
    }

    #[test]
    fn nothing_written_yet_is_default() {
        let home = Home::new();
        assert_eq!(
            home.current(AgentKind::ClaudeCode, &[relay()]),
            Current::Default
        );
        assert_eq!(home.current(AgentKind::Codex, &[relay()]), Current::Default);
    }

    #[test]
    fn what_confdeck_switched_to_is_recognized() {
        let home = Home::new();
        let providers = [relay(), other()];
        for agent in [AgentKind::ClaudeCode, AgentKind::Codex] {
            switch(&home.engine, home.path(), agent, Some(&providers[1])).unwrap();
            assert_eq!(
                home.current(agent, &providers),
                Current::Provider("other".into())
            );
            switch(&home.engine, home.path(), agent, None).unwrap();
            assert_eq!(home.current(agent, &providers), Current::Default);
        }
    }

    #[test]
    fn changing_the_model_inside_the_agent_keeps_the_provider() {
        let home = Home::new();
        switch(
            &home.engine,
            home.path(),
            AgentKind::ClaudeCode,
            Some(&relay()),
        )
        .unwrap();
        // Claude Code 的 /model 写的是顶层 model，也是关键字段。
        home.edit(".claude/settings.json", |text| {
            text.replacen('{', "{\n  \"model\": \"glm-4.5\",", 1)
        });
        assert_eq!(
            home.current(AgentKind::ClaudeCode, &[relay()]),
            Current::Provider("relay".into())
        );
    }

    #[test]
    fn an_address_changed_outside_shows_custom_with_its_host() {
        let home = Home::new();
        switch(&home.engine, home.path(), AgentKind::Codex, Some(&relay())).unwrap();
        home.edit(".codex/config.toml", |text| {
            text.replace("https://relay.example/v1", "https://elsewhere.example/v1")
        });
        assert_eq!(
            home.current(AgentKind::Codex, &[relay()]),
            Current::Custom {
                host: Some("elsewhere.example".into())
            }
        );
    }

    #[test]
    fn a_different_key_is_custom_too() {
        let home = Home::new();
        switch(
            &home.engine,
            home.path(),
            AgentKind::ClaudeCode,
            Some(&relay()),
        )
        .unwrap();
        home.edit(".claude/settings.json", |text| {
            text.replace("sk-relay", "sk-rotated")
        });
        assert_eq!(
            home.current(AgentKind::ClaudeCode, &[relay()]),
            Current::Custom {
                host: Some("relay.example".into())
            }
        );
    }

    #[test]
    fn the_record_picks_between_providers_with_the_same_address_and_key() {
        let home = Home::new();
        let providers = [relay(), twin()];
        switch(
            &home.engine,
            home.path(),
            AgentKind::ClaudeCode,
            Some(&providers[1]),
        )
        .unwrap();
        assert_eq!(
            home.current(AgentKind::ClaudeCode, &providers),
            Current::Provider("twin".into())
        );

        // 记录丢了：关键字段完全一致的那家胜出，不是列表里的第一家。
        fs::remove_file(home.engine.data().state()).unwrap();
        assert_eq!(
            home.current(AgentKind::ClaudeCode, &providers),
            Current::Provider("twin".into())
        );
    }

    #[test]
    fn a_provider_set_up_by_hand_the_same_way_is_recognized() {
        let home = Home::new();
        home.edit(".claude/settings.json", |_| {
            r#"{"env": {"ANTHROPIC_BASE_URL": "https://other.example/", "ANTHROPIC_AUTH_TOKEN": "sk-other"}}"#.into()
        });
        assert_eq!(
            home.current(AgentKind::ClaudeCode, &[relay(), other()]),
            Current::Provider("other".into())
        );
    }

    #[test]
    fn bedrock_and_built_in_routes_are_custom_without_a_host() {
        let home = Home::new();
        home.edit(".claude/settings.json", |_| {
            r#"{"env": {"CLAUDE_CODE_USE_BEDROCK": "1"}}"#.into()
        });
        assert_eq!(
            home.current(AgentKind::ClaudeCode, &[relay()]),
            Current::Custom { host: None }
        );
    }

    #[test]
    fn broken_files_are_reported() {
        let home = Home::new();
        home.edit(".claude/settings.json", |_| "{ broken".into());
        assert!(matches!(
            current(&home.engine, home.path(), AgentKind::ClaudeCode, &[relay()]),
            Err(ApplyError::Parse(_))
        ));
    }

    #[test]
    fn coexist_agents_list_what_is_enabled() {
        let home = Home::new();
        enable(&home.engine, home.path(), AgentKind::Pi, &relay()).unwrap();
        enable(&home.engine, home.path(), AgentKind::Pi, &other()).unwrap();
        assert_eq!(
            enabled(&home.engine, home.path(), AgentKind::Pi).unwrap(),
            ["relay", "other"]
        );
        assert!(matches!(
            current(&home.engine, home.path(), AgentKind::Pi, &[]),
            Err(ApplyError::WrongMode { .. })
        ));
        assert!(matches!(
            enabled(&home.engine, home.path(), AgentKind::Codex),
            Err(ApplyError::WrongMode { .. })
        ));
    }
}
