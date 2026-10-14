//! 每个 agent 一个适配器：配置文件在哪、单选还是多选、说哪几种协议、要不要 Key、能不能用某一家、
//! 写进去的补丁、读文件认连着谁、写前写后要做的事。补丁和读文件的算法还在各 agent 自己的 module 里。
//!
//! 分两半：界面也用的 [`Adapter`]（经 [`AgentKind::adapter`]），和只在 core 里用的 `Writes`（经 `AgentKind::writes`，
//! `apply`、`current`、`status` 用它写、认文件；界面要写经 [`crate::apply`]，切换还要记选了哪家、切换记录）。
//! 单选、多选两种各有一组读写（`Role`）：单选的切换、认连接（`Single`），多选的勾上、取消、认勾了哪几家（`Multi`）。

use std::path::{Path, PathBuf};

use super::codex::catalog::Skipped;
use super::{
    AdaptError, AgentKind, Connection, FileSetup, Mode, choose_protocol, claude, codex, grok,
    opencode, pi, require_key,
};
use crate::engine::{Engine, EngineError, LiveFile, Outcome};
use crate::fsx;
use crate::patch::{self, LivePatch, PatchError};
use crate::provider::{Protocol, Provider};

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
    match agent.adapter().mode() {
        Mode::Switch => "单选 agent",
        Mode::Coexist => "多选 agent",
    }
}

/// 单选 agent 切换的结果（[`crate::apply::switch`]）。
#[derive(Debug)]
pub struct Switched {
    /// agent 的配置文件写了没有。
    pub outcome: Outcome,
    /// Codex 切到第三方时的模型目录文件：写了，或者没写成的原因。没写成时界面要说一声：
    /// `/model` 列的还是 GPT 模型，这家的模型按 272K 窗口算。别的 agent、Codex 切回默认时为 `None`。
    pub catalog: Option<Result<Outcome, Skipped>>,
}

impl Switched {
    fn only(outcome: Outcome) -> Self {
        Self {
            outcome,
            catalog: None,
        }
    }
}

/// 「把一家写进去」的补丁：单选 agent 是切到这家，多选 agent 是勾上这家。
pub(crate) type PatchFor = dyn Fn(&Provider) -> Result<Box<dyn LivePatch>, AdaptError>;

/// 补丁要看的、已经读好的文件（不存在是 `None`）：配置文件和 [`Writes::login`]。
#[derive(Default, Clone, Copy)]
pub(crate) struct Disk<'a> {
    pub config: Option<&'a [u8]>,
    pub login: Option<&'a [u8]>,
}

/// 单选还是多选，和各自的读写。
pub(crate) enum Role<'a> {
    Single(&'a dyn Single),
    Multi(&'a dyn Multi),
}

/// 适配器里界面也用的那一半：文件、单选还是多选、协议、要不要 Key、能不能用某一家。
pub trait Adapter: Sync {
    fn kind(&self) -> AgentKind;

    /// 能说的协议，按偏好排序：供应商提供好几种时取第一种（用户覆盖的、预设推荐的优先，见 [`choose_protocol`]）。
    fn protocols(&self) -> &'static [Protocol];

    /// 连第三方地址必须有 Key：单选 agent 不写 Key 时会拿官方登录的凭据去请求第三方（[`AdaptError::MissingKey`]）。
    fn requires_key(&self) -> bool;

    /// confdeck 写的文件：第一个是配置文件（装没装、连着谁都看它），Codex 还有模型目录（备份页用）。
    fn files(&self, home: &Path) -> Vec<LiveFile>;

    /// 单选还是多选（和 `Writes::role` 一致）。
    fn mode(&self) -> Mode;

    /// 配置文件（[`Self::files`] 的第一个）。
    fn config(&self, home: &Path) -> LiveFile {
        self.files(home).swap_remove(0)
    }

    /// 能不能用这家：能用是用哪种协议、哪个地址，不能用是原因（接口对不上、没填 Key）。
    /// 协议和 Key 这两条和写进文件的是同一条规则；别的写不进去的（Grok Build 连 Anthropic 官方、没有模型的）写的时候报。
    fn usable<'a>(&self, provider: &'a Provider) -> Result<(Protocol, &'a str), AdaptError> {
        let chosen = choose_protocol(provider, self.kind())?;
        if self.requires_key() {
            require_key(provider, self.kind())?;
        }
        Ok(chosen)
    }
}

/// 适配器里写、认文件的那一半，只在 core 里用。
pub(crate) trait Writes: Adapter {
    /// 补丁要看的登录文件（Codex 的 `auth.json`：有没有 ChatGPT 登录决定 `requires_openai_auth`）；不看的是 `None`。
    fn login(&self, _home: &Path) -> Option<PathBuf> {
        None
    }

    /// 「把一家写进去」的补丁怎么算（认当前连着哪家、改了供应商只写变了的键时用）。`disk` 是读好的文件
    /// （一次快照里每个文件只读一次）；`None` 是要看文件的自己读（只有 Codex 看）。
    fn writer(&self, home: &Path, disk: Option<Disk>) -> Result<Box<PatchFor>, EngineError>;

    fn role(&self) -> Role<'_>;

    /// 改了在用的一家（`after`），写配置文件之前要先写的：Codex 指着 confdeck 的模型目录时重写目录（找不到 `codex` 就留着旧的）。
    fn before_sync(
        &self,
        _engine: &Engine,
        _home: &Path,
        _after: &Provider,
    ) -> Result<(), ApplyError> {
        Ok(())
    }

    /// 把 `provider` 写进配置文件以后要记的（`None` 是切回默认）：Claude Code 记下原始片段写了哪些键，下次切走时删。
    /// 切换（`apply::switch`）、改了供应商同步（`apply::save_provider`）以后调。
    fn after_write(
        &self,
        _engine: &Engine,
        _provider: Option<&Provider>,
    ) -> Result<(), ApplyError> {
        Ok(())
    }

    /// 单选的读写；多选 agent 报 [`ApplyError::WrongMode`]。
    fn single(&self) -> Result<&dyn Single, ApplyError> {
        match self.role() {
            Role::Single(single) => Ok(single),
            Role::Multi(_) => Err(ApplyError::WrongMode { agent: self.kind() }),
        }
    }

    /// 多选的读写；单选 agent 报 [`ApplyError::WrongMode`]。
    fn multi(&self) -> Result<&dyn Multi, ApplyError> {
        match self.role() {
            Role::Multi(multi) => Ok(multi),
            Role::Single(_) => Err(ApplyError::WrongMode { agent: self.kind() }),
        }
    }
}

/// 单选 agent：同一时间只连一家。
pub(crate) trait Single: Sync {
    /// 切到这一家，`None` 是还原成 agent 自带的登录。写完要记的（[`Writes::after_write`]）由 `apply::switch` 记。
    fn switch(
        &self,
        engine: &Engine,
        home: &Path,
        provider: Option<&Provider>,
    ) -> Result<Switched, ApplyError>;

    /// 配置文件里现在连的是哪、用的什么凭据。
    fn connection(&self, path: &Path, bytes: Option<&[u8]>) -> Result<Connection, PatchError>;

    /// 配置文件里的一套设置（存成供应商、配置页的下拉框用）；连的不是一个地址时是 `None`。
    fn setup(&self, path: &Path, bytes: Option<&[u8]>) -> Result<Option<FileSetup>, PatchError>;
}

/// 多选 agent：可以同时勾几家。
pub(crate) trait Multi: Sync {
    /// 勾上这一家（已有就整项更新）。
    fn add(&self, provider: &Provider) -> Result<Box<dyn LivePatch>, AdaptError>;

    /// 取消勾选这一家。
    fn remove(&self, provider_id: &str) -> Box<dyn LivePatch>;

    /// 勾了哪几家（供应商 id，按文件里的顺序）。
    fn enabled(&self, path: &Path, bytes: Option<&[u8]>) -> Result<Vec<String>, PatchError>;

    /// 配置里一项的名字是哪家供应商的（用量记录认供应商用）；不是 confdeck 写的是 `None`。
    fn provider_of<'a>(&self, name: &'a str) -> Option<&'a str>;
}

impl AgentKind {
    /// 这个 agent 的适配器（界面也用的那一半）。
    pub fn adapter(self) -> &'static dyn Adapter {
        self.writes()
    }

    /// 这个 agent 的适配器（写、认文件的那一半也在）：唯一按 agent 挑的地方。
    pub(crate) fn writes(self) -> &'static dyn Writes {
        match self {
            Self::ClaudeCode => &Claude,
            Self::Codex => &Codex,
            Self::Pi => &Pi,
            Self::GrokBuild => &Grok,
            Self::Opencode => &Opencode,
        }
    }
}

/// 读一个文件，不存在是 `None`。
fn read(path: PathBuf) -> Result<Option<Vec<u8>>, EngineError> {
    fsx::read_optional(&path).map_err(|source| EngineError::Io { path, source })
}

// ── Claude Code ──────────────────────────────────────

struct Claude;

impl Adapter for Claude {
    fn kind(&self) -> AgentKind {
        AgentKind::ClaudeCode
    }

    fn protocols(&self) -> &'static [Protocol] {
        &[Protocol::Anthropic]
    }

    fn requires_key(&self) -> bool {
        true
    }

    fn files(&self, home: &Path) -> Vec<LiveFile> {
        vec![claude::live_file(home)]
    }

    fn mode(&self) -> Mode {
        Mode::Switch
    }
}

impl Writes for Claude {
    fn writer(&self, _home: &Path, _disk: Option<Disk>) -> Result<Box<PatchFor>, EngineError> {
        Ok(Box::new(|provider| {
            Ok(Box::new(claude::patch(Some(provider), &[])?))
        }))
    }

    fn role(&self) -> Role<'_> {
        Role::Single(self)
    }

    fn after_write(&self, engine: &Engine, provider: Option<&Provider>) -> Result<(), ApplyError> {
        engine.set_owned(self.kind().id(), &claude::raw_entries(provider)?)?;
        Ok(())
    }
}

impl Single for Claude {
    fn switch(
        &self,
        engine: &Engine,
        home: &Path,
        provider: Option<&Provider>,
    ) -> Result<Switched, ApplyError> {
        let patch = claude::patch(provider, &engine.owned(self.kind().id()))?;
        let outcome = engine.write(&claude::live_file(home), &patch)?;
        Ok(Switched::only(outcome))
    }

    fn connection(&self, path: &Path, bytes: Option<&[u8]>) -> Result<Connection, PatchError> {
        claude::connection(path, bytes)
    }

    fn setup(&self, path: &Path, bytes: Option<&[u8]>) -> Result<Option<FileSetup>, PatchError> {
        claude::setup(path, bytes)
    }
}

// ── Codex ────────────────────────────────────────────

struct Codex;

impl Codex {
    /// `~/.codex/auth.json` 里有没有 ChatGPT 登录，决定 `requires_openai_auth`。
    fn login_on_disk(home: &Path) -> Result<bool, EngineError> {
        Ok(codex::login_on_disk(
            read(codex::auth_path(home))?.as_deref(),
        ))
    }
}

impl Adapter for Codex {
    fn kind(&self) -> AgentKind {
        AgentKind::Codex
    }

    fn protocols(&self) -> &'static [Protocol] {
        &[Protocol::OpenaiResponses]
    }

    fn requires_key(&self) -> bool {
        true
    }

    fn files(&self, home: &Path) -> Vec<LiveFile> {
        vec![codex::live_file(home), codex::catalog::live_file(home)]
    }

    fn mode(&self) -> Mode {
        Mode::Switch
    }
}

impl Writes for Codex {
    fn login(&self, home: &Path) -> Option<PathBuf> {
        Some(codex::auth_path(home))
    }

    fn writer(&self, home: &Path, disk: Option<Disk>) -> Result<Box<PatchFor>, EngineError> {
        let file = codex::live_file(home);
        let (login, catalog) = match disk {
            Some(disk) => (
                codex::login_on_disk(disk.login),
                codex::uses_catalog(&file.path, disk.config),
            ),
            None => (
                Self::login_on_disk(home)?,
                codex::uses_catalog(&file.path, read(file.path.clone())?.as_deref()),
            ),
        };
        // 现在指着 confdeck 的模型目录就照样指着：认当前是哪家时逐字比较，改供应商时这一项不变。
        Ok(Box::new(move |provider| {
            let patch = codex::patch(Some(provider), login)?;
            Ok(Box::new(if catalog { patch.with_catalog() } else { patch }))
        }))
    }

    fn role(&self) -> Role<'_> {
        Role::Single(self)
    }

    fn before_sync(
        &self,
        engine: &Engine,
        home: &Path,
        after: &Provider,
    ) -> Result<(), ApplyError> {
        // Codex 的模型列表在目录文件里，先重写目录（找不到 `codex` 就留着旧的）。
        let file = codex::live_file(home);
        if codex::uses_catalog(&file.path, engine.read(&file)?.bytes.as_deref())
            && let Ok(catalog) = codex::catalog::generate(home, &codex::catalog_models(after)?)
        {
            engine.write(&codex::catalog::live_file(home), &patch::Whole(catalog))?;
        }
        Ok(())
    }
}

impl Single for Codex {
    fn switch(
        &self,
        engine: &Engine,
        home: &Path,
        provider: Option<&Provider>,
    ) -> Result<Switched, ApplyError> {
        let config = codex::live_file(home);
        let patch = codex::patch(provider, Self::login_on_disk(home)?)?;
        let catalog = match provider {
            Some(provider) => {
                codex::catalog::generate(home, &codex::catalog_models(provider)?).map(Some)
            }
            None => Ok(None),
        };
        let catalog = match catalog {
            Ok(Some(catalog)) => catalog,
            Ok(None) => return Ok(Switched::only(engine.write(&config, &patch)?)),
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

    fn connection(&self, path: &Path, bytes: Option<&[u8]>) -> Result<Connection, PatchError> {
        codex::connection(path, bytes)
    }

    fn setup(&self, path: &Path, bytes: Option<&[u8]>) -> Result<Option<FileSetup>, PatchError> {
        codex::setup(path, bytes)
    }
}

// ── Grok Build ───────────────────────────────────────

struct Grok;

impl Adapter for Grok {
    fn kind(&self) -> AgentKind {
        AgentKind::GrokBuild
    }

    fn protocols(&self) -> &'static [Protocol] {
        &[
            Protocol::OpenaiResponses,
            Protocol::OpenaiChat,
            Protocol::Anthropic,
        ]
    }

    fn requires_key(&self) -> bool {
        true
    }

    fn files(&self, home: &Path) -> Vec<LiveFile> {
        vec![grok::live_file(home)]
    }

    fn mode(&self) -> Mode {
        Mode::Switch
    }
}

impl Writes for Grok {
    fn writer(&self, _home: &Path, _disk: Option<Disk>) -> Result<Box<PatchFor>, EngineError> {
        Ok(Box::new(|provider| {
            Ok(Box::new(grok::patch(Some(provider))?))
        }))
    }

    fn role(&self) -> Role<'_> {
        Role::Single(self)
    }
}

impl Single for Grok {
    fn switch(
        &self,
        engine: &Engine,
        home: &Path,
        provider: Option<&Provider>,
    ) -> Result<Switched, ApplyError> {
        let outcome = engine.write(&grok::live_file(home), &grok::patch(provider)?)?;
        Ok(Switched::only(outcome))
    }

    fn connection(&self, path: &Path, bytes: Option<&[u8]>) -> Result<Connection, PatchError> {
        grok::connection(path, bytes)
    }

    fn setup(&self, path: &Path, bytes: Option<&[u8]>) -> Result<Option<FileSetup>, PatchError> {
        grok::setup(path, bytes)
    }
}

// ── Pi、OpenCode ─────────────────────────────────────

/// 多选 agent 都能说的协议。
const EVERY_PROTOCOL: &[Protocol] = &[
    Protocol::OpenaiChat,
    Protocol::Anthropic,
    Protocol::OpenaiResponses,
    Protocol::Gemini,
];

struct Pi;

impl Adapter for Pi {
    fn kind(&self) -> AgentKind {
        AgentKind::Pi
    }

    fn protocols(&self) -> &'static [Protocol] {
        EVERY_PROTOCOL
    }

    /// 没有 Key 时不写，Pi 会去 auth.json 和环境变量里找。
    fn requires_key(&self) -> bool {
        false
    }

    fn files(&self, home: &Path) -> Vec<LiveFile> {
        vec![pi::live_file(home)]
    }

    fn mode(&self) -> Mode {
        Mode::Coexist
    }
}

impl Writes for Pi {
    fn writer(&self, _home: &Path, _disk: Option<Disk>) -> Result<Box<PatchFor>, EngineError> {
        Ok(Box::new(|provider| Pi.add(provider)))
    }

    fn role(&self) -> Role<'_> {
        Role::Multi(self)
    }
}

impl Multi for Pi {
    fn add(&self, provider: &Provider) -> Result<Box<dyn LivePatch>, AdaptError> {
        Ok(Box::new(pi::add(provider)?))
    }

    fn remove(&self, provider_id: &str) -> Box<dyn LivePatch> {
        Box::new(pi::remove(provider_id))
    }

    fn enabled(&self, path: &Path, bytes: Option<&[u8]>) -> Result<Vec<String>, PatchError> {
        pi::enabled(path, bytes)
    }

    fn provider_of<'a>(&self, name: &'a str) -> Option<&'a str> {
        name.strip_prefix(pi::KEY_PREFIX)
    }
}

struct Opencode;

impl Adapter for Opencode {
    fn kind(&self) -> AgentKind {
        AgentKind::Opencode
    }

    fn protocols(&self) -> &'static [Protocol] {
        EVERY_PROTOCOL
    }

    /// 没有 Key 时不写，OpenCode 会用 `/connect` 存的凭据和环境变量。
    fn requires_key(&self) -> bool {
        false
    }

    fn files(&self, home: &Path) -> Vec<LiveFile> {
        vec![opencode::live_file(home)]
    }

    fn mode(&self) -> Mode {
        Mode::Coexist
    }
}

impl Writes for Opencode {
    fn writer(&self, _home: &Path, _disk: Option<Disk>) -> Result<Box<PatchFor>, EngineError> {
        Ok(Box::new(|provider| Opencode.add(provider)))
    }

    fn role(&self) -> Role<'_> {
        Role::Multi(self)
    }
}

impl Multi for Opencode {
    fn add(&self, provider: &Provider) -> Result<Box<dyn LivePatch>, AdaptError> {
        Ok(Box::new(opencode::add(provider)?))
    }

    fn remove(&self, provider_id: &str) -> Box<dyn LivePatch> {
        Box::new(opencode::remove(provider_id))
    }

    fn enabled(&self, path: &Path, bytes: Option<&[u8]>) -> Result<Vec<String>, PatchError> {
        opencode::enabled(path, bytes)
    }

    fn provider_of<'a>(&self, name: &'a str) -> Option<&'a str> {
        name.strip_prefix(opencode::KEY_PREFIX)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::Value;

    use super::*;
    use crate::agent::Credential;
    use crate::preset;
    use crate::provider::AgentOverride;
    use crate::provider::tests::relay;

    const PROTOCOLS: [Protocol; 4] = [
        Protocol::Anthropic,
        Protocol::OpenaiChat,
        Protocol::OpenaiResponses,
        Protocol::Gemini,
    ];

    /// 写进空文件以后，文件里这家用的是哪种协议：单选看文件里的那套设置，多选看自己那一项。
    fn written_protocol(adapter: &dyn Writes, path: &Path, bytes: &[u8], id: &str) -> Protocol {
        match adapter.role() {
            Role::Single(single) => {
                single
                    .setup(path, Some(bytes))
                    .unwrap()
                    .expect("a provider is an address")
                    .protocol
            }
            Role::Multi(_) => {
                let doc: Value = json5::from_str(std::str::from_utf8(bytes).unwrap()).unwrap();
                let (entry, field, name): (&Value, &str, fn(Protocol) -> &'static str) =
                    match adapter.kind() {
                        AgentKind::Pi => {
                            (&doc["providers"][pi::provider_key(id)], "api", pi::api_name)
                        }
                        AgentKind::Opencode => (
                            &doc["provider"][opencode::provider_key(id)],
                            "npm",
                            opencode::package,
                        ),
                        other => unreachable!("{other} is not multi"),
                    };
                let written = entry[field].as_str().unwrap();
                PROTOCOLS
                    .into_iter()
                    .find(|protocol| name(*protocol) == written)
                    .unwrap()
            }
        }
    }

    fn write(
        adapter: &dyn Writes,
        home: &Path,
        provider: &Provider,
    ) -> Result<Vec<u8>, AdaptError> {
        let patch = adapter.writer(home, Some(Disk::default())).unwrap()(provider)?;
        Ok(patch.apply(&adapter.config(home).path, None).unwrap())
    }

    #[test]
    fn every_adapter_describes_itself() {
        let home = Path::new("/home/x");
        for kind in AgentKind::ALL {
            let adapter = kind.writes();
            assert_eq!(adapter.kind(), kind);
            let files = adapter.files(home);
            assert_eq!(adapter.config(home).path, files[0].path, "{kind}");
            assert!(
                files
                    .iter()
                    .all(|file| file.owner == kind.id() && file.path.starts_with(home))
            );
            assert!(!adapter.protocols().is_empty());
            // 单选的连第三方都要 Key；多选的没有 Key 时 agent 自己去找。
            assert_eq!(
                adapter.requires_key(),
                adapter.mode() == Mode::Switch,
                "{kind}"
            );
            match adapter.role() {
                Role::Single(_) => {
                    assert_eq!(adapter.mode(), Mode::Switch, "{kind}");
                    assert!(adapter.multi().is_err());
                }
                Role::Multi(_) => {
                    assert_eq!(adapter.mode(), Mode::Coexist, "{kind}");
                    assert!(adapter.single().is_err());
                }
            }
        }
    }

    /// 界面说的协议（`usable`）就是写进文件的协议，全部预设 × 五个 agent，预设本身和用户在配置页给这个 agent 选了每一种协议的。
    /// 用不了的写也写不进去。
    #[test]
    fn the_protocol_shown_is_the_protocol_written() {
        let home = Path::new("/home/x");
        let mut compared = 0;
        for preset in preset::all() {
            for key in ["sk-test", ""] {
                let plain = preset.provider(&preset.id, key);
                for kind in AgentKind::ALL {
                    let adapter = kind.writes();
                    let chosen = PROTOCOLS.into_iter().map(|protocol| {
                        let mut provider = plain.clone();
                        let chosen = AgentOverride {
                            protocol: Some(protocol),
                            ..Default::default()
                        };
                        provider.overrides.insert(kind, chosen);
                        provider
                    });
                    for provider in std::iter::once(plain.clone()).chain(chosen) {
                        let chosen = provider.override_for(kind).and_then(|o| o.protocol);
                        let at = format!("{} × {kind} (key {key:?}, chosen {chosen:?})", preset.id);
                        match (adapter.usable(&provider), write(adapter, home, &provider)) {
                            (Ok((protocol, _)), Ok(bytes)) => {
                                let path = adapter.config(home).path;
                                assert_eq!(
                                    written_protocol(adapter, &path, &bytes, &provider.id),
                                    protocol,
                                    "{at}"
                                );
                                compared += 1;
                            }
                            (Err(err), Ok(_)) => panic!("{at}: unusable ({err}) but written"),
                            // 能用但写不进去的只有 Grok Build 连 Anthropic 官方，写的时候报。
                            (Ok(_), Err(AdaptError::AnthropicOfficial { .. })) => {}
                            (Ok(_), Err(err)) => panic!("{at}: usable but not written ({err})"),
                            (Err(_), Err(_)) => {}
                        }
                    }
                }
            }
        }
        assert!(compared > 400, "only {compared} compared");
    }

    #[test]
    fn a_provider_written_reads_back() {
        let home = Path::new("/home/x");
        let provider = relay();
        for kind in AgentKind::ALL {
            let adapter = kind.writes();
            let path = adapter.config(home).path;
            let bytes = write(adapter, home, &provider).unwrap();
            match adapter.role() {
                Role::Single(single) => {
                    let (_, url) = adapter.usable(&provider).unwrap();
                    let connection = single.connection(&path, Some(&bytes)).unwrap();
                    assert_eq!(connection.base_url(), Some(url), "{kind}");
                    assert_eq!(
                        connection.credential().and_then(Credential::key),
                        Some("sk-relay"),
                        "{kind}"
                    );
                    let setup = single.setup(&path, Some(&bytes)).unwrap().unwrap();
                    assert_eq!(setup.base_url, url, "{kind}");
                }
                Role::Multi(multi) => {
                    assert_eq!(multi.enabled(&path, Some(&bytes)).unwrap(), ["relay"]);
                    let removed = multi.remove("relay").apply(&path, Some(&bytes)).unwrap();
                    assert!(multi.enabled(&path, Some(&removed)).unwrap().is_empty());
                    let name = match kind {
                        AgentKind::Pi => pi::provider_key("relay"),
                        AgentKind::Opencode => opencode::provider_key("relay"),
                        other => unreachable!("{other} is not multi"),
                    };
                    assert_eq!(multi.provider_of(&name), Some("relay"));
                    assert_eq!(multi.provider_of("relay"), None);
                }
            }
        }
    }

    #[test]
    fn a_provider_without_a_key_is_unusable_where_a_key_is_required() {
        let home = Path::new("/home/x");
        let mut provider = relay();
        provider.key.clear();
        for kind in AgentKind::ALL {
            let adapter = kind.writes();
            // 写的时候也是这条规则。
            assert_eq!(
                write(adapter, home, &provider).is_err(),
                adapter.requires_key(),
                "{kind}"
            );
            let usable = adapter.usable(&provider);
            if adapter.requires_key() {
                assert!(
                    matches!(usable, Err(AdaptError::MissingKey { .. })),
                    "{kind}"
                );
            } else {
                assert!(usable.is_ok(), "{kind}");
            }
        }
        // 接口对不上排在没 Key 前面。
        provider
            .endpoints
            .retain(|protocol, _| *protocol == Protocol::Anthropic);
        assert!(matches!(
            AgentKind::Codex.adapter().usable(&provider),
            Err(AdaptError::Incompatible { .. })
        ));
    }
}
