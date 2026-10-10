//! 认出 agent 现在用的是哪家：单选 agent 连的是哪家，多选 agent 勾了哪几家。
//!
//! **以文件为准**，confdeck 的记录只用来在几家都对得上时挑一家。单选 agent 按这个顺序认：
//!
//! 1. 文件里没有指定地址和凭据：「默认」（agent 自带的登录）。
//! 2. 上次在 confdeck 里选的那家，地址和 Key 和文件对得上：就是它。
//! 3. 列表里关键字段和文件完全一致的一家；没有的话，地址和 Key 对得上的第一家。
//! 4. 地址是列表里某一家的，只有 Key 不一样：「DeepSeek · Key 不一致」（上次选的那家优先）。
//! 5. 都对不上：「自定义 · 主机名」（被别的工具或手改成了别的地址）。
//!
//! 只比地址和凭据，不比模型名：在 Claude Code 里用 `/model` 换个模型，还算同一家。
//! 同一个地址和 Key 配了几家（比如同一个中转按模型分成几张卡片），靠第 2、3 步区分。
//!
//! 「自定义」时可以用 [`draft`] 把文件里的配置拼成一家新供应商，给新建表单预先填好。

use std::path::Path;

use crate::agent::{AgentKind, Connection, FileSetup, claude, codex, grok, opencode, pi};
use crate::apply::{ApplyError, PatchFor, Writer, writer};
use crate::engine::{Engine, LiveFile};
use crate::patch::PatchError;
use crate::preset;
use crate::provider::{self, AgentOverride, Model, Provider};

/// 单选 agent 现在用的是哪家。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Current {
    /// agent 自带的登录。
    Default,
    /// 供应商 id。
    Provider(String),
    /// 地址是这家（供应商 id）的，Key 不一样，显示「DeepSeek · Key 不一致」。
    ///
    /// 选择器里可以用 confdeck 的 Key 重写（[`crate::apply::switch`]），或者把文件里的 Key
    /// 存进这家（Key 从 [`connection`] 读，用 [`crate::apply::save_provider`] 存，其他在用这家的
    /// agent 跟着换成这个 Key）。文件里没写 Key、或者写的是 `apiKeyHelper`、
    /// `env_key` 这类读不到 Key 的写法，也算这一种，只是没有第二个选项。
    KeyMismatch { provider: String },
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
    let source = source(home, agent)?;
    let file = &source.file;
    let snapshot = engine.read(file)?;
    let live = (source.read)(&file.path, snapshot.bytes.as_deref())?;
    let candidates: Vec<Candidate> = providers
        .iter()
        .filter_map(|provider| {
            // 这个 agent 用不了的供应商（接口对不上、没有 Key）不参与。
            let patch = (source.patch)(provider).ok()?;
            let fresh = patch.apply(&file.path, None).ok()?;
            let expected = (source.read)(&file.path, Some(&fresh)).ok()?;
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

/// 单选 agent 的配置文件现在连的是哪、用的什么凭据。「Key 不一致」时从这里拿文件里的 Key。
pub fn connection(
    engine: &Engine,
    home: &Path,
    agent: AgentKind,
) -> Result<Connection, ApplyError> {
    let source = source(home, agent)?;
    let snapshot = engine.read(&source.file)?;
    Ok((source.read)(&source.file.path, snapshot.bytes.as_deref())?)
}

/// 「把当前配置存成供应商」：从单选 agent 的配置文件里读出地址、Key 和模型，拼成一家新供应商，
/// 给新建供应商的表单预先填好，不保存。
///
/// 地址是某个内置预设的（[`preset::matching`]）：名字、各协议的地址、模型目录照预设，记下预设，
/// ID 用预设的；文件里有、目录里没有的模型补在目录后面。这样存好就能给别的 agent 用。
/// 认不出预设时，ID 从主机名起（`api.example.com` → `example`），名字是主机名，只有文件里这一个地址。
/// ID 和 `providers` 重复时加 `-2`、`-3`。槽位型 agent 的槽位、列表型 agent 的默认模型写进这个 agent 的覆盖，
/// 存好后再切换，文件里写了的槽位不变，没写的按目录补上。
/// 文件里读不到 Key（`apiKeyHelper`、`env_key`）时 Key 留空。
/// 文件连的不是一个地址（「默认」、Bedrock 等）时为 `None`。
pub fn draft(
    engine: &Engine,
    home: &Path,
    agent: AgentKind,
    providers: &[Provider],
) -> Result<Option<Provider>, ApplyError> {
    let source = source(home, agent)?;
    let snapshot = engine.read(&source.file)?;
    let Some(setup) = (source.setup)(&source.file.path, snapshot.bytes.as_deref())? else {
        return Ok(None);
    };
    let host = provider::host(&setup.base_url)
        .unwrap_or_default()
        .to_string();
    let overrides = AgentOverride {
        default_model: setup.default_model,
        slots: setup.slots,
        ..AgentOverride::default()
    };
    let key = setup.key.unwrap_or_default();
    let mut draft = match preset::matching(setup.protocol, &setup.base_url) {
        Some(preset) => preset.provider(&unique_id(&preset.id, providers), &key),
        None => Provider {
            id: unique_id(&id_base(&host), providers),
            name: if host.is_empty() {
                setup.base_url.clone()
            } else {
                host
            },
            preset: None,
            key,
            endpoints: [(setup.protocol, setup.base_url)].into(),
            models: Vec::new(),
            overrides: Default::default(),
        },
    };
    for id in setup.models {
        if draft.model(&id).is_none() {
            draft.models.push(Model {
                id,
                ..Model::default()
            });
        }
    }
    if overrides != AgentOverride::default() {
        draft.overrides.insert(agent, overrides);
    }
    draft.normalize();
    Ok(Some(draft))
}

/// 单选 agent 的配置文件，和读它、往里写某一家的办法。
struct Source {
    file: LiveFile,
    read: Reader,
    setup: SetupReader,
    patch: Box<PatchFor>,
}

type Reader = fn(&Path, Option<&[u8]>) -> Result<Connection, PatchError>;
type SetupReader = fn(&Path, Option<&[u8]>) -> Result<Option<FileSetup>, PatchError>;
type EnabledReader = fn(&Path, Option<&[u8]>) -> Result<Vec<String>, PatchError>;

fn source(home: &Path, agent: AgentKind) -> Result<Source, ApplyError> {
    let (read, setup): (Reader, SetupReader) = match agent {
        AgentKind::ClaudeCode => (claude::connection, claude::setup),
        AgentKind::Codex => (codex::connection, codex::setup),
        AgentKind::GrokBuild => (grok::connection, grok::setup),
        AgentKind::Pi | AgentKind::Opencode => return Err(ApplyError::WrongMode { agent }),
    };
    let Writer { file, patch } = writer(home, agent)?;
    Ok(Source {
        file,
        read,
        setup,
        patch,
    })
}

fn identify(live: &Connection, candidates: &[Candidate], record: Option<&str>) -> Current {
    if *live == Connection::Default {
        return Current::Default;
    }
    let matching: Vec<&Candidate> = candidates.iter().filter(|c| c.expected == *live).collect();
    let chosen = record
        .and_then(|id| matching.iter().find(|c| c.id == id))
        .or_else(|| matching.iter().find(|c| c.exact))
        .or_else(|| matching.first());
    if let Some(candidate) = chosen {
        return Current::Provider(candidate.id.to_string());
    }

    let same_address: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| c.expected.base_url().is_some() && c.expected.base_url() == live.base_url())
        .collect();
    let chosen = record
        .and_then(|id| same_address.iter().find(|c| c.id == id))
        .or_else(|| same_address.first());
    match chosen {
        Some(candidate) => Current::KeyMismatch {
            provider: candidate.id.to_string(),
        },
        None => Current::Custom { host: live.host() },
    }
}

/// 从主机名起一个供应商 ID：去掉开头的 `api.`、`www.` 和末尾的顶级域名，其余的点换成连字符；
/// IP 地址整个保留。和 `taken` 重复时加 `-2`、`-3`。
fn id_base(host: &str) -> String {
    let host = host.to_ascii_lowercase();
    let mut labels: Vec<&str> = host.split('.').filter(|label| !label.is_empty()).collect();
    let is_ip = host.contains(':')
        || labels
            .iter()
            .all(|label| label.bytes().all(|b| b.is_ascii_digit()));
    if !is_ip {
        if labels.len() > 1 && matches!(labels[0], "api" | "www") {
            labels.remove(0);
        }
        if labels.len() > 1 {
            labels.pop();
        }
    }
    let mut base = String::new();
    for ch in labels.join("-").chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            base.push(ch);
        } else if !base.is_empty() && !base.ends_with('-') {
            base.push('-');
        }
    }
    match base.trim_end_matches('-') {
        "" => "custom",
        trimmed => trimmed,
    }
    .to_string()
}

/// `base`，和已有的重复时加 `-2`、`-3`。
fn unique_id(base: &str, taken: &[Provider]) -> String {
    let mut id = base.to_string();
    let mut n = 2;
    while taken.iter().any(|provider| provider.id == id) {
        id = format!("{base}-{n}");
        n += 1;
    }
    id
}

/// 多选 agent 勾了哪几家（供应商 id，按文件里的顺序）。
pub fn enabled(engine: &Engine, home: &Path, agent: AgentKind) -> Result<Vec<String>, ApplyError> {
    let (file, read): (LiveFile, EnabledReader) = match agent {
        AgentKind::Pi => (pi::live_file(home), pi::enabled),
        AgentKind::Opencode => (opencode::live_file(home), opencode::enabled),
        AgentKind::ClaudeCode | AgentKind::Codex | AgentKind::GrokBuild => {
            return Err(ApplyError::WrongMode { agent });
        }
    };
    let snapshot = engine.read(&file)?;
    Ok(read(&file.path, snapshot.bytes.as_deref())?)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use super::*;
    use crate::agent::Credential;
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
        for agent in [
            AgentKind::ClaudeCode,
            AgentKind::Codex,
            AgentKind::GrokBuild,
        ] {
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
    fn the_same_address_with_another_key_is_a_key_mismatch() {
        let home = Home::new();
        let providers = [relay(), twin(), other()];
        for agent in [
            AgentKind::ClaudeCode,
            AgentKind::Codex,
            AgentKind::GrokBuild,
        ] {
            switch(&home.engine, home.path(), agent, Some(&providers[1])).unwrap();
            let relative = match agent {
                AgentKind::ClaudeCode => ".claude/settings.json",
                AgentKind::Codex => ".codex/config.toml",
                _ => ".grok/config.toml",
            };
            home.edit(relative, |text| text.replace("sk-relay", "sk-rotated"));
            // 上次选的那家优先，不是列表里的第一家。
            assert_eq!(
                home.current(agent, &providers),
                Current::KeyMismatch {
                    provider: "twin".into()
                }
            );
            let live = connection(&home.engine, home.path(), agent).unwrap();
            assert_eq!(
                live.credential().and_then(Credential::key),
                Some("sk-rotated")
            );

            // 把文件里的 Key 存进这家，就对上了。
            let mut adopted = providers.clone();
            adopted[1].key = "sk-rotated".into();
            assert_eq!(
                home.current(agent, &adopted),
                Current::Provider("twin".into())
            );
        }
    }

    #[test]
    fn a_key_helper_at_a_known_address_is_a_key_mismatch_without_a_key() {
        let home = Home::new();
        home.edit(".claude/settings.json", |_| {
            r#"{"env": {"ANTHROPIC_BASE_URL": "https://relay.example"}, "apiKeyHelper": "~/bin/key"}"#.into()
        });
        assert_eq!(
            home.current(AgentKind::ClaudeCode, &[relay()]),
            Current::KeyMismatch {
                provider: "relay".into()
            }
        );
        let live = connection(&home.engine, home.path(), AgentKind::ClaudeCode).unwrap();
        assert_eq!(live.credential().and_then(Credential::key), None);
    }

    #[test]
    fn a_custom_setup_can_be_saved_as_a_provider() {
        let home = Home::new();
        home.edit(".claude/settings.json", |_| {
            r#"{"env": {
                "ANTHROPIC_BASE_URL": "https://api.relay-one.com/anthropic/",
                "ANTHROPIC_AUTH_TOKEN": "sk-deep",
                "ANTHROPIC_MODEL": "deepseek-chat",
                "ANTHROPIC_DEFAULT_HAIKU_MODEL": "deepseek-chat"
            }}"#
            .into()
        });
        let mut taken = relay();
        taken.id = "relay-one".into();
        let draft = draft(
            &home.engine,
            home.path(),
            AgentKind::ClaudeCode,
            &[relay(), taken],
        )
        .unwrap()
        .unwrap();
        assert_eq!(draft.id, "relay-one-2");
        assert_eq!(draft.name, "api.relay-one.com");
        assert_eq!(draft.preset, None);
        assert_eq!(draft.key, "sk-deep");
        assert_eq!(
            draft.endpoints,
            BTreeMap::from([(
                Protocol::Anthropic,
                "https://api.relay-one.com/anthropic".into()
            )])
        );
        assert_eq!(draft.models.len(), 1);
        assert_eq!(
            draft.override_for(AgentKind::ClaudeCode).unwrap().slots,
            BTreeMap::from([
                ("main".into(), "deepseek-chat".into()),
                ("haiku".into(), "deepseek-chat".into()),
            ])
        );
        draft.validate().unwrap();

        // 存好之后就认得出来，再切换一次文件也不变。
        let saved = [relay(), draft];
        assert_eq!(
            home.current(AgentKind::ClaudeCode, &saved),
            Current::Provider("relay-one-2".into())
        );
        let before = fs::read(home.path().join(".claude/settings.json")).unwrap();
        switch(
            &home.engine,
            home.path(),
            AgentKind::ClaudeCode,
            Some(&saved[1]),
        )
        .unwrap();
        let after = fs::read(home.path().join(".claude/settings.json")).unwrap();
        assert_eq!(
            claude::connection(Path::new("s.json"), Some(&after)).unwrap(),
            claude::connection(Path::new("s.json"), Some(&before)).unwrap()
        );
        assert_eq!(
            claude::setup(Path::new("s.json"), Some(&after))
                .unwrap()
                .unwrap()
                .slots
                .len(),
            6,
            "every slot is written, the two from the file and the rest filled from the catalog"
        );
    }

    #[test]
    fn a_setup_on_a_preset_address_is_saved_with_the_whole_preset() {
        let home = Home::new();
        home.edit(".claude/settings.json", |_| {
            r#"{"env": {
                "ANTHROPIC_BASE_URL": "https://api.deepseek.com/anthropic",
                "ANTHROPIC_AUTH_TOKEN": "sk-deep",
                "ANTHROPIC_MODEL": "deepseek-chat"
            }}"#
            .into()
        });
        let draft = draft(&home.engine, home.path(), AgentKind::ClaudeCode, &[])
            .unwrap()
            .unwrap();
        let deepseek = preset::get("deepseek").unwrap();
        assert_eq!(draft.id, "deepseek");
        assert_eq!(draft.name, "DeepSeek");
        assert_eq!(draft.preset.as_deref(), Some("deepseek"));
        assert_eq!(draft.key, "sk-deep");
        // 三个协议的地址都有了，存好就能给 Codex、Pi 用。
        assert_eq!(draft.endpoints, deepseek.endpoints);
        let ids: Vec<&str> = draft.models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, ["deepseek-v4-pro", "deepseek-flash", "deepseek-chat"]);
        assert_eq!(
            draft.override_for(AgentKind::ClaudeCode).unwrap().slots,
            BTreeMap::from([("main".into(), "deepseek-chat".into())])
        );
        draft.validate().unwrap();
    }

    #[test]
    fn a_codex_route_is_saved_with_its_model() {
        let home = Home::new();
        home.edit(".codex/config.toml", |_| {
            "model = \"kimi-k2\"\nmodel_provider = \"moon\"\n\n[model_providers.moon]\nbase_url = \"https://api.moonshot.cn/v1/\"\nexperimental_bearer_token = \"sk-moon\"\n".into()
        });
        let draft = draft(&home.engine, home.path(), AgentKind::Codex, &[])
            .unwrap()
            .unwrap();
        assert_eq!(draft.id, "kimi");
        assert_eq!(
            draft.endpoint(Protocol::OpenaiResponses),
            Some("https://api.moonshot.cn/v1")
        );
        assert_eq!(
            draft
                .override_for(AgentKind::Codex)
                .unwrap()
                .default_model
                .as_deref(),
            Some("kimi-k2")
        );
        assert_eq!(
            home.current(AgentKind::Codex, &[draft]),
            Current::Provider("kimi".into())
        );
    }

    #[test]
    fn a_grok_model_table_is_saved_with_its_neighbours() {
        let home = Home::new();
        home.edit(".grok/config.toml", |_| {
            "[models]\ndefault = \"kimi\"\n\n[model.kimi]\nmodel = \"kimi-k2\"\nbase_url = \"https://api.moonshot.cn/v1\"\napi_key = \"sk-moon\"\n\n[model.kimi-fast]\nmodel = \"kimi-k2-turbo\"\nbase_url = \"https://api.moonshot.cn/v1\"\napi_key = \"sk-moon\"\n".into()
        });
        let draft = draft(&home.engine, home.path(), AgentKind::GrokBuild, &[])
            .unwrap()
            .unwrap();
        assert_eq!(draft.id, "kimi");
        assert_eq!(
            draft.endpoint(Protocol::OpenaiChat),
            Some("https://api.moonshot.cn/v1")
        );
        // Kimi 预设的三个模型，加上文件里的两个。
        assert_eq!(draft.models.len(), 5);
        assert_eq!(
            home.current(AgentKind::GrokBuild, std::slice::from_ref(&draft)),
            Current::Provider("kimi".into())
        );

        // 切过去以后，用户自己的两张表还在，默认换成 confdeck 写的那张，连的还是同一个地址和 Key。
        switch(
            &home.engine,
            home.path(),
            AgentKind::GrokBuild,
            Some(&draft),
        )
        .unwrap();
        let text = fs::read_to_string(home.path().join(".grok/config.toml")).unwrap();
        assert!(text.contains("default = \"confdeck-kimi-k2\""), "{text}");
        assert!(text.contains("[model.kimi-fast]"), "{text}");
        assert_eq!(
            home.current(AgentKind::GrokBuild, &[draft]),
            Current::Provider("kimi".into())
        );
    }

    #[test]
    fn nothing_to_save_when_the_file_is_not_an_address() {
        let home = Home::new();
        assert_eq!(
            draft(&home.engine, home.path(), AgentKind::ClaudeCode, &[]).unwrap(),
            None
        );
        home.edit(".claude/settings.json", |_| {
            r#"{"env": {"CLAUDE_CODE_USE_BEDROCK": "1"}}"#.into()
        });
        assert_eq!(
            draft(&home.engine, home.path(), AgentKind::ClaudeCode, &[]).unwrap(),
            None
        );
    }

    #[test]
    fn ids_come_from_the_host() {
        let id = id_base;
        assert_eq!(id("api.deepseek.com"), "deepseek");
        assert_eq!(id("relay.elsewhere.io"), "relay-elsewhere");
        assert_eq!(id("open.bigmodel.cn"), "open-bigmodel");
        assert_eq!(id("localhost"), "localhost");
        assert_eq!(id("127.0.0.1"), "127-0-0-1");
        assert_eq!(id("My_Relay.example"), "my-relay");
        assert_eq!(id(""), "custom");
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
