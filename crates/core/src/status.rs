//! 各 agent 现在的样子，一次读全（10-13，arch-perf R1）：[`snapshot`]。
//!
//! 界面每次重读都要的几样放在一起读：`providers.json`、`state.json`、每个 agent 的配置文件、
//! `~/.claude.json`、`accounts.json`，每个文件只读一次。每个 agent 认成一个 [`AgentStatus`]，
//! 胶囊、选择器、官方账号、额度都看它，不再各自拿「选了哪家」「文件里是什么」「登录文件是谁」去拼。
//!
//! 读的时候顺带写两样，和以前界面每次重读时做的一样：
//! - 单选 agent 现在连的是哪家，记进切换记录（`history.jsonl`，和上一笔一样不记），在别处改了配置，用量也能对上是哪家。
//! - 登录文件是存过的账号时，把 agent 新换的令牌存回去（`account::read_all`）。

use std::io;
use std::path::{Path, PathBuf};

use crate::account::{self, Accounts};
use crate::agent::{AgentKind, Connection, Credential, Disk, Mode};
use crate::current::{self, Current, Recognized};
use crate::engine::{Engine, EngineError, Local};
use crate::fsx;
use crate::provider::Provider;
use crate::store::{self, StoreError};
use crate::usage::history::{self, Target};

/// 一次读到的全部。
#[derive(Debug)]
pub struct Snapshot {
    pub providers: Vec<Provider>,
    /// `providers.json` 读不了的原因。读不了时 `providers` 是空的，不能保存，不然会把文件盖掉。
    pub providers_error: Option<StoreError>,
    /// `state.json` 里本机的设置。
    pub local: Local,
    /// 每个 agent 一项，按 [`AgentKind::ALL`] 的顺序。
    pub agents: Vec<AgentState>,
    pub accounts: Accounts,
}

impl Snapshot {
    pub fn agent(&self, kind: AgentKind) -> &AgentState {
        self.agents
            .iter()
            .find(|agent| agent.kind == kind)
            .expect("every agent is in the snapshot")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentState {
    pub kind: AgentKind,
    /// 配置文件在哪（OpenCode 有 `opencode.jsonc` 时是它，要看磁盘）。
    pub config: PathBuf,
    pub status: AgentStatus,
}

/// 一个 agent 现在连的是谁。
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)] // 一次快照每个 agent 一份，装箱省不了什么。
pub enum AgentStatus {
    /// 配置文件不在：没装。
    Absent,
    /// 配置文件读不了、解析不了（手改坏了）。
    Unreadable,
    /// 单选 agent 用着自带的官方登录（文件里没有指定地址和凭据）。登的是哪个账号见 [`Accounts::live`]。
    Official,
    /// 单选 agent 连着列表里的这家。
    Provider { id: String },
    /// 地址是这家的，Key 不一样；`credential` 是文件里写的（没写是 `None`）。见 [`Current::KeyMismatch`]。
    KeyMismatch {
        id: String,
        credential: Option<Credential>,
    },
    /// 和列表里哪家都对不上：「自定义 · 主机名」。`connection` 是文件里的原样，
    /// `draft` 是按文件拼好的一家新供应商（存成供应商用；文件连的不是一个地址时没有）。
    Custom {
        host: Option<String>,
        connection: Connection,
        draft: Option<Provider>,
    },
    /// 多选 agent 勾了哪几家（供应商 id，按文件里的顺序）。
    Multi { ids: Vec<String> },
}

impl AgentStatus {
    /// 配置文件在。
    pub fn installed(&self) -> bool {
        *self != Self::Absent
    }

    /// 单选 agent 连着的那家（「Key 不一致」不算）。
    pub fn provider(&self) -> Option<&str> {
        match self {
            Self::Provider { id } => Some(id),
            _ => None,
        }
    }

    /// 切换记录里记成什么；读不了、多选 agent 不记。
    fn target(&self, providers: &[Provider]) -> Option<Target> {
        match self {
            Self::Official => Some(Target::Official),
            Self::Provider { id } | Self::KeyMismatch { id, .. } => {
                let provider = providers.iter().find(|provider| &provider.id == id)?;
                Some(Target::of(Some(provider)))
            }
            Self::Custom { .. } => Some(Target::Other),
            Self::Absent | Self::Unreadable | Self::Multi { .. } => None,
        }
    }
}

/// 读一遍各 agent 现在的样子（见 module 的说明，顺带记切换记录、存回新令牌）。读不了的各自记在结果里，不报错。
pub fn snapshot(engine: &Engine, home: &Path) -> Snapshot {
    let (providers, providers_error) = match store::load(engine.data()) {
        Ok(providers) => (providers, None),
        Err(err) => (Vec::new(), Some(err)),
    };
    let local = engine.local();
    // 每个 agent 的配置文件和补丁要看的登录文件（Codex 的 `auth.json`，认官方账号也要用）先读好，只读一次。
    let files: Vec<(AgentKind, Files)> = AgentKind::ALL
        .into_iter()
        .map(|kind| (kind, Files::read(engine, home, kind)))
        .collect();
    let mut agents = Vec::new();
    for &(kind, ref files) in &files {
        let record = local.selection(kind).flatten();
        let status = match files.disk() {
            None => AgentStatus::Unreadable,
            Some(Disk { config: None, .. }) => AgentStatus::Absent,
            Some(disk) => match current::read(home, kind, &providers, disk, record) {
                Ok(recognized) => status_of(recognized),
                Err(_) => AgentStatus::Unreadable,
            },
        };
        agents.push(AgentState {
            kind,
            config: files.path.clone(),
            status,
        });
    }

    let noted = agents
        .iter()
        .filter(|agent| agent.kind.writes().mode() == Mode::Switch)
        .filter_map(|agent| Some((agent.kind, agent.status.target(&providers)?)))
        .collect();
    // 记不下来只是用量页少一条线索。
    let _ = history::note_all(engine.data(), noted);

    let installed: Vec<AgentKind> = agents
        .iter()
        .filter(|agent| agent.status.installed())
        .map(|agent| agent.kind)
        .collect();
    let (_, codex) = files
        .into_iter()
        .find(|(kind, _)| *kind == AgentKind::Codex)
        .expect("Codex is in ALL");
    let codex_disk = Disk {
        config: codex.config.as_ref().ok().and_then(Option::as_deref),
        login: codex
            .login
            .as_ref()
            .and_then(|(_, login)| login.as_ref().ok()?.as_deref()),
    };
    let mut accounts = account::read_all(engine, home, &installed, codex_disk);
    // `auth.json` 读不了：不知道登的是谁（不是「未登录」），原因记下来。Codex 没装时不用说。
    if installed.contains(&AgentKind::Codex)
        && let Some((path, Err(source))) = codex.login
    {
        accounts.live.remove(&AgentKind::Codex);
        accounts.error = Some(EngineError::Io { path, source }.into());
    }
    Snapshot {
        providers,
        providers_error,
        local,
        agents,
        accounts,
    }
}

/// 一个 agent 读好的文件：配置文件（Codex 是 `config.toml`）和补丁要看的登录文件（适配器的 `login`）。
/// 不存在是 `None`。
struct Files {
    path: PathBuf,
    config: Result<Option<Vec<u8>>, EngineError>,
    login: Option<(PathBuf, io::Result<Option<Vec<u8>>>)>,
}

impl Files {
    fn read(engine: &Engine, home: &Path, kind: AgentKind) -> Self {
        let adapter = kind.writes();
        let file = adapter.config(home);
        Self {
            config: engine.read(&file).map(|snapshot| snapshot.bytes),
            path: file.path,
            login: adapter.login(home).map(|path| {
                let read = fsx::read_optional(&path);
                (path, read)
            }),
        }
    }

    /// 认连着谁要的样子；读不了是 `None`（配置文件不在时登录文件读不了也不要紧：没装）。
    fn disk(&self) -> Option<Disk<'_>> {
        let config = self.config.as_ref().ok()?.as_deref();
        let login = match &self.login {
            Some((_, Ok(login))) => login.as_deref(),
            Some((_, Err(_))) if config.is_some() => return None,
            _ => None,
        };
        Some(Disk { config, login })
    }
}

fn status_of(recognized: Recognized) -> AgentStatus {
    match recognized {
        Recognized::Multi(ids) => AgentStatus::Multi { ids },
        Recognized::Single {
            current,
            connection,
            draft,
        } => match current {
            Current::Default => AgentStatus::Official,
            Current::Provider(id) => AgentStatus::Provider { id },
            Current::KeyMismatch { provider } => AgentStatus::KeyMismatch {
                id: provider,
                credential: match connection {
                    Connection::Endpoint { credential, .. } => credential,
                    _ => None,
                },
            },
            Current::Custom { host } => AgentStatus::Custom {
                host,
                connection,
                draft,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use super::*;
    use crate::account::Live;
    use crate::account::tests::codex_auth;
    use crate::apply::{enable, switch};
    use crate::data::DataDir;
    use crate::engine::Place;
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

        /// 一个存了这几家供应商的家目录。
        fn with(providers: &[Provider]) -> Self {
            let home = Self::new();
            store::save(&home.engine, providers).unwrap();
            home
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

        fn snapshot(&self) -> Snapshot {
            snapshot(&self.engine, self.path())
        }

        fn status(&self, kind: AgentKind) -> AgentStatus {
            self.snapshot().agent(kind).status.clone()
        }
    }

    #[test]
    fn nothing_installed_is_absent() {
        let snapshot = Home::new().snapshot();
        let kinds: Vec<AgentKind> = snapshot.agents.iter().map(|agent| agent.kind).collect();
        assert_eq!(kinds, AgentKind::ALL);
        assert!(
            snapshot
                .agents
                .iter()
                .all(|agent| agent.status == AgentStatus::Absent)
        );
        assert!(snapshot.providers.is_empty() && snapshot.providers_error.is_none());
        assert!(snapshot.accounts.live.is_empty() && snapshot.accounts.error.is_none());
    }

    #[test]
    fn a_file_without_an_address_is_official() {
        let home = Home::with(&[relay()]);
        home.edit(".claude/settings.json", |_| "{}".into());
        home.edit(".codex/config.toml", |_| "model = \"gpt-5\"\n".into());
        assert_eq!(home.status(AgentKind::ClaudeCode), AgentStatus::Official);
        assert_eq!(home.status(AgentKind::Codex), AgentStatus::Official);
        assert_eq!(home.status(AgentKind::GrokBuild), AgentStatus::Absent);
    }

    #[test]
    fn the_provider_switched_to_is_recognized() {
        let home = Home::with(&[relay()]);
        for agent in [
            AgentKind::ClaudeCode,
            AgentKind::Codex,
            AgentKind::GrokBuild,
        ] {
            switch(&home.engine, home.path(), agent, Some(&relay())).unwrap();
            assert_eq!(
                home.status(agent),
                AgentStatus::Provider { id: "relay".into() }
            );
        }
    }

    #[test]
    fn the_record_picks_between_providers_with_the_same_address_and_key() {
        let mut twin = relay();
        twin.id = "twin".into();
        let home = Home::with(&[relay(), twin.clone()]);
        switch(&home.engine, home.path(), AgentKind::Codex, Some(&twin)).unwrap();
        assert_eq!(
            home.status(AgentKind::Codex),
            AgentStatus::Provider { id: "twin".into() }
        );
    }

    #[test]
    fn another_key_at_a_known_address_is_a_key_mismatch_with_the_file_key() {
        let home = Home::with(&[relay()]);
        switch(&home.engine, home.path(), AgentKind::Codex, Some(&relay())).unwrap();
        home.edit(".codex/config.toml", |text| {
            text.replace("sk-relay", "sk-rotated")
        });
        assert_eq!(
            home.status(AgentKind::Codex),
            AgentStatus::KeyMismatch {
                id: "relay".into(),
                credential: Some(Credential::Key("sk-rotated".into())),
            }
        );
    }

    #[test]
    fn an_unknown_address_is_custom_with_a_draft() {
        let home = Home::with(&[relay()]);
        switch(
            &home.engine,
            home.path(),
            AgentKind::ClaudeCode,
            Some(&relay()),
        )
        .unwrap();
        home.edit(".claude/settings.json", |text| {
            text.replace("https://relay.example", "https://elsewhere.example")
        });
        let AgentStatus::Custom {
            host,
            connection,
            draft,
        } = home.status(AgentKind::ClaudeCode)
        else {
            panic!("not custom");
        };
        assert_eq!(host.as_deref(), Some("elsewhere.example"));
        assert_eq!(connection.base_url(), Some("https://elsewhere.example"));
        let draft = draft.unwrap();
        assert_eq!(draft.id, "elsewhere");
        assert_eq!(
            draft.endpoints,
            BTreeMap::from([(Protocol::Anthropic, "https://elsewhere.example".into())])
        );
    }

    #[test]
    fn a_key_mismatch_without_the_key_in_the_file_says_how_the_file_gets_it() {
        let home = Home::with(&[relay()]);
        switch(
            &home.engine,
            home.path(),
            AgentKind::ClaudeCode,
            Some(&relay()),
        )
        .unwrap();
        let edit = |change: &dyn Fn(&mut serde_json::Value)| {
            home.edit(".claude/settings.json", |text| {
                let mut doc: serde_json::Value = serde_json::from_str(&text).unwrap();
                change(&mut doc);
                doc.to_string()
            })
        };
        edit(&|doc| {
            doc["env"]
                .as_object_mut()
                .unwrap()
                .remove("ANTHROPIC_AUTH_TOKEN");
        });
        assert_eq!(
            home.status(AgentKind::ClaudeCode),
            AgentStatus::KeyMismatch {
                id: "relay".into(),
                credential: None,
            }
        );
        edit(&|doc| doc["apiKeyHelper"] = "~/bin/key".into());
        assert_eq!(
            home.status(AgentKind::ClaudeCode),
            AgentStatus::KeyMismatch {
                id: "relay".into(),
                credential: Some(Credential::Helper("~/bin/key".into())),
            }
        );
    }

    #[test]
    fn a_connection_without_an_address_is_custom_without_a_draft() {
        let home = Home::with(&[relay()]);
        home.edit(".claude/settings.json", |_| {
            r#"{"env": {"CLAUDE_CODE_USE_BEDROCK": "1", "AWS_REGION": "us-east-1"}}"#.into()
        });
        assert_eq!(
            home.status(AgentKind::ClaudeCode),
            AgentStatus::Custom {
                host: None,
                connection: Connection::Other { host: None },
                draft: None,
            }
        );
    }

    #[test]
    fn an_unreadable_codex_login_is_reported_not_logged_out() {
        let home = Home::new();
        home.edit(".codex/config.toml", |_| String::new());
        fs::create_dir_all(home.path().join(".codex/auth.json")).unwrap();
        let snapshot = home.snapshot();
        assert_eq!(
            snapshot.agent(AgentKind::Codex).status,
            AgentStatus::Unreadable
        );
        assert!(!snapshot.accounts.live.contains_key(&AgentKind::Codex));
        assert!(snapshot.accounts.error.is_some());
    }

    #[test]
    fn coexist_agents_list_what_is_enabled() {
        let mut other = relay();
        other.id = "other".into();
        let home = Home::with(&[relay(), other.clone()]);
        enable(&home.engine, home.path(), AgentKind::Pi, &other).unwrap();
        enable(&home.engine, home.path(), AgentKind::Pi, &relay()).unwrap();
        assert_eq!(
            home.status(AgentKind::Pi),
            AgentStatus::Multi {
                ids: vec!["other".into(), "relay".into()]
            }
        );
        assert_eq!(home.status(AgentKind::Opencode), AgentStatus::Absent);
    }

    #[test]
    fn a_broken_file_is_unreadable_and_the_rest_still_read() {
        let home = Home::with(&[relay()]);
        switch(&home.engine, home.path(), AgentKind::Codex, Some(&relay())).unwrap();
        home.edit(".claude/settings.json", |_| "{ broken".into());
        let snapshot = home.snapshot();
        assert_eq!(
            snapshot.agent(AgentKind::ClaudeCode).status,
            AgentStatus::Unreadable
        );
        assert!(snapshot.agent(AgentKind::ClaudeCode).status.installed());
        assert_eq!(
            snapshot.agent(AgentKind::Codex).status,
            AgentStatus::Provider { id: "relay".into() }
        );
    }

    #[test]
    fn a_broken_provider_list_is_reported() {
        let home = Home::new();
        home.edit(".confdeck/providers.json", |_| "{".into());
        home.edit(".claude/settings.json", |_| "{}".into());
        let snapshot = home.snapshot();
        assert!(snapshot.providers.is_empty());
        assert!(matches!(
            snapshot.providers_error,
            Some(StoreError::Parse { .. })
        ));
        assert_eq!(
            snapshot.agent(AgentKind::ClaudeCode).status,
            AgentStatus::Official
        );
    }

    #[test]
    fn local_settings_come_with_the_snapshot() {
        let home = Home::new();
        home.engine.set_hidden(Place::Page, "pi", true).unwrap();
        let snapshot = home.snapshot();
        assert!(snapshot.local.hidden(Place::Page).contains("pi"));
        assert!(snapshot.local.hidden(Place::Panel).is_empty());
    }

    #[test]
    fn switches_are_noted_only_when_they_change() {
        let home = Home::with(&[relay()]);
        home.edit(".claude/settings.json", |_| "{}".into());
        home.snapshot();
        home.snapshot();
        let noted = |home: &Home| {
            history::load(home.engine.data())
                .unwrap()
                .into_iter()
                .map(|switch| (switch.agent, switch.to))
                .collect::<Vec<_>>()
        };
        assert_eq!(noted(&home), [(AgentKind::ClaudeCode, Target::Official)]);
        // 在别处改成了这家。
        home.edit(".claude/settings.json", |_| {
            r#"{"env": {"ANTHROPIC_BASE_URL": "https://relay.example", "ANTHROPIC_AUTH_TOKEN": "sk-relay"}}"#.into()
        });
        home.snapshot();
        home.snapshot();
        assert_eq!(
            noted(&home),
            [
                (AgentKind::ClaudeCode, Target::Official),
                (AgentKind::ClaudeCode, Target::of(Some(&relay()))),
            ]
        );
    }

    #[test]
    fn logins_come_with_the_snapshot() {
        let home = Home::new();
        let auth = |bytes: &[u8]| {
            home.edit(".codex/auth.json", |_| {
                String::from_utf8(bytes.to_vec()).unwrap()
            })
        };
        home.edit(".codex/config.toml", |_| String::new());
        // 没装的 agent 不看登录文件。
        home.edit(".grok/auth.json", |_| "{}".into());
        let alice = codex_auth("alice", "ws", 0);
        auth(&alice);
        let snapshot = home.snapshot();
        let identity = account::identify(AgentKind::Codex, &alice).unwrap();
        assert_eq!(
            snapshot.accounts.live,
            [(AgentKind::Codex, Live::Unknown(identity.clone()))].into()
        );

        // 存过的账号：Codex 换了令牌，读的时候存回去。
        account::add(&home.engine, home.path(), AgentKind::Codex, &alice).unwrap();
        let later = codex_auth("alice", "ws", 1);
        auth(&later);
        let snapshot = home.snapshot();
        assert_eq!(
            snapshot.accounts.live[&AgentKind::Codex],
            Live::Known(identity.id.clone())
        );
        assert_eq!(snapshot.accounts.list.len(), 1);
        assert_eq!(
            account::stored_login(home.engine.data(), AgentKind::Codex, &identity.id).unwrap(),
            later
        );

        // 登录存在钥匙串里：切不了，不认。
        home.edit(".codex/config.toml", |_| {
            "cli_auth_credentials_store = \"keyring\"\n".into()
        });
        let snapshot = home.snapshot();
        assert!(snapshot.accounts.blocked.contains(&AgentKind::Codex));
        assert!(snapshot.accounts.live.is_empty());
    }
}
