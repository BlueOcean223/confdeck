//! 界面的数据层。供应商来自 confdeck-core 的 `providers.json`；每个 agent 现在用哪家，
//! 从它的配置文件里认出来（`current`）。agent 的清单在这里写死：core 里有适配器的五个能选供应商，
//! 其余三个（阶段 7）只列出来。
//!
//! 选了一家就写文件（`switch` / `toggle`）：写在后台线程，写完按文件重读一遍，内存里先改的胶囊会被
//! 文件里的实际状态盖掉；写了什么、有没有备份、失败的原因做成 [`Notice`] 事件发出去，主窗口做成提示条，面板写在底栏。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use confdeck_core::agent::codex::catalog::Skipped;
use confdeck_core::agent::{self, AgentKind, Connection, Mode};
use confdeck_core::apply::{self, ApplyError, Switched, Synced, UsedBy};
use confdeck_core::current::{self, Current};
use confdeck_core::data::DataDir;
use confdeck_core::engine::{Engine, LiveFile, Outcome};
use confdeck_core::preset;
use confdeck_core::provider::{self, Protocol, Provider};
use confdeck_core::store::{self, StoreError};
use gpui_kit::*;
use rust_i18n::t;

/// 图标。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Logo {
    /// 单色 SVG，按文字颜色着色。
    Mono(&'static str),
    /// 彩色图标（SVG 或 PNG），按原色画。
    Color(&'static str),
    /// 还没有图标的供应商：名字的第一个字装在小方块里。
    Letter(char),
}

impl Logo {
    pub fn letter(name: &str) -> Self {
        let first = name.trim().chars().next().unwrap_or('?');
        Self::Letter(first.to_uppercase().next().unwrap_or(first))
    }
}

/// agent 用哪家：单选 agent 一家（`None` 是「默认」，agent 自带的登录），多选 agent 勾了哪几家。
/// 存的是 `Store::providers` 里的下标。
pub enum Selection {
    Single(Option<usize>),
    Multi(Vec<usize>),
}

pub struct Agent {
    /// core 里的适配器；`None` 是还没接上的 agent，只列出来，选不了。
    pub kind: Option<AgentKind>,
    pub name: &'static str,
    pub logo: Logo,
    /// 配置文件，相对家目录。
    pub config: &'static str,
    /// 配置文件存在。
    pub installed: bool,
    pub mode: Mode,
    /// 能说的协议，按偏好排序：供应商提供好几种时用第一种（decisions「供应商和 agent 的关系」）。
    pub protocols: &'static [Protocol],
    /// 连第三方必须有 Key：core 的适配器没 Key 就拒绝写（Claude Code、Codex、Grok Build）。
    pub requires_key: bool,
    pub selection: Selection,
    /// 单选 agent 从文件里认出来的原样结果，「自定义 · 主机名」「Key 不一致」要用。
    pub current: Option<Current>,
    /// 在 confdeck 里给它选过供应商（`state.json` 的 `selections`）。
    /// 菜单栏面板只列这样的单选 agent（decisions「没有『接管』」）。
    pub chosen: bool,
}

impl Agent {
    /// 能在界面上选供应商：有适配器、配置文件也在。
    pub fn selectable(&self) -> bool {
        self.kind.is_some() && self.installed
    }

    /// 显示用的配置文件路径。
    pub fn config_path(&self) -> String {
        format!("~/{}", self.config)
    }
}

struct AgentDef {
    kind: Option<AgentKind>,
    name: &'static str,
    logo: Logo,
    config: &'static str,
    mode: Mode,
    protocols: &'static [Protocol],
    requires_key: bool,
}

/// 界面上列的 agent，按这个顺序。前五个是 core 里有适配器的；后三个是阶段 7 的，协议照 fork，到时再核对。
const AGENTS: &[AgentDef] = &[
    AgentDef {
        kind: Some(AgentKind::ClaudeCode),
        name: "Claude Code",
        logo: Logo::Color("icons/brand/claudecode-color.svg"),
        config: ".claude/settings.json",
        mode: Mode::Switch,
        protocols: &[Protocol::Anthropic],
        requires_key: true,
    },
    AgentDef {
        kind: Some(AgentKind::Codex),
        name: "Codex",
        logo: Logo::Color("icons/brand/codex-color.svg"),
        config: ".codex/config.toml",
        mode: Mode::Switch,
        protocols: &[Protocol::OpenaiResponses],
        requires_key: true,
    },
    AgentDef {
        kind: None,
        name: "Gemini CLI",
        logo: Logo::Color("icons/brand/geminicli-color.svg"),
        config: ".gemini/.env",
        mode: Mode::Switch,
        protocols: &[Protocol::Gemini],
        requires_key: true,
    },
    AgentDef {
        kind: Some(AgentKind::Opencode),
        name: "OpenCode",
        logo: Logo::Mono("icons/brand/opencode.svg"),
        config: ".config/opencode/opencode.json",
        mode: Mode::Coexist,
        protocols: &[
            Protocol::OpenaiChat,
            Protocol::Anthropic,
            Protocol::OpenaiResponses,
            Protocol::Gemini,
        ],
        requires_key: false,
    },
    AgentDef {
        kind: Some(AgentKind::Pi),
        name: "Pi",
        logo: Logo::Mono("icons/brand/pi.svg"),
        config: ".pi/agent/models.json",
        mode: Mode::Coexist,
        protocols: &[
            Protocol::OpenaiChat,
            Protocol::Anthropic,
            Protocol::OpenaiResponses,
            Protocol::Gemini,
        ],
        requires_key: false,
    },
    AgentDef {
        kind: Some(AgentKind::GrokBuild),
        name: "Grok Build",
        logo: Logo::Mono("icons/brand/grok.svg"),
        config: ".grok/config.toml",
        mode: Mode::Switch,
        protocols: &[
            Protocol::OpenaiResponses,
            Protocol::OpenaiChat,
            Protocol::Anthropic,
        ],
        requires_key: true,
    },
    AgentDef {
        kind: None,
        name: "OpenClaw",
        logo: Logo::Color("icons/brand/openclaw-color.svg"),
        config: ".openclaw/openclaw.json",
        mode: Mode::Coexist,
        protocols: &[Protocol::OpenaiChat, Protocol::Anthropic],
        requires_key: false,
    },
    AgentDef {
        kind: None,
        name: "Hermes",
        logo: Logo::Color("icons/brand/hermes.png"),
        config: ".hermes/config.yaml",
        mode: Mode::Coexist,
        protocols: &[Protocol::OpenaiChat],
        requires_key: false,
    },
];

/// 某个 agent 为什么用不了某家供应商。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Unusable {
    /// agent 要的接口这家一种都没有。
    Incompatible,
    /// agent 必须有 Key，这家没填。
    NoKey,
}

/// 写完文件给人看的一句话。`Store` 把它当事件发出去，谁订阅谁显示：主窗口做成底部提示条，面板写在底栏。
#[derive(Clone, Debug)]
pub struct Notice {
    pub kind: NoticeKind,
    pub text: SharedString,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeKind {
    /// 几秒后自己消失。
    Success,
    /// 留着，点 × 才关（写失败的要让人看见）。
    Error,
}

impl Notice {
    fn ok(text: impl Into<SharedString>) -> Self {
        Self {
            kind: NoticeKind::Success,
            text: text.into(),
        }
    }

    fn err(text: impl Into<SharedString>) -> Self {
        Self {
            kind: NoticeKind::Error,
            text: text.into(),
        }
    }
}

pub struct Store {
    /// 写文件在后台线程，和界面线程共用一个引擎（进程内的锁才管用）。
    engine: Arc<Engine>,
    home: PathBuf,
    pub providers: Vec<Provider>,
    pub agents: Vec<Agent>,
    /// `providers.json` 读不了的原因。读不了时列表是空的，不许保存，不然会把文件盖掉。
    pub load_error: Option<String>,
}

impl EventEmitter<Notice> for Store {}

impl Store {
    /// 读数据目录和各 agent 的配置文件。
    pub fn load(data: DataDir, home: PathBuf) -> Self {
        let mut store = Self {
            engine: Arc::new(Engine::new(data)),
            home,
            providers: Vec::new(),
            agents: Vec::new(),
            load_error: None,
        };
        store.reload();
        store
    }

    /// 重新读供应商和每个 agent 现在用的是哪家。
    pub fn reload(&mut self) {
        match store::load(self.engine.data()) {
            Ok(providers) => {
                self.providers = providers;
                self.load_error = None;
            }
            Err(err) => {
                self.providers = Vec::new();
                self.load_error = Some(err.to_string());
            }
        }
        self.agents = AGENTS.iter().map(|def| self.agent(def)).collect();
    }

    fn agent(&self, def: &AgentDef) -> Agent {
        let installed = self.home.join(def.config).exists();
        let index_of = |id: &str| self.providers.iter().position(|p| p.id == id);
        let mut current = None;
        let mut chosen = false;
        let selection = match (def.kind, def.mode) {
            (Some(kind), Mode::Switch) if installed => {
                chosen = self.engine.selection(kind.id()).is_some();
                let found = current::current(&self.engine, &self.home, kind, &self.providers).ok();
                let picked = match &found {
                    Some(Current::Provider(id)) => index_of(id),
                    _ => None,
                };
                current = found;
                Selection::Single(picked)
            }
            (Some(kind), Mode::Coexist) if installed => Selection::Multi(
                current::enabled(&self.engine, &self.home, kind)
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|id| index_of(id))
                    .collect(),
            ),
            (_, Mode::Switch) => Selection::Single(None),
            (_, Mode::Coexist) => Selection::Multi(Vec::new()),
        };
        Agent {
            kind: def.kind,
            name: def.name,
            logo: def.logo,
            config: def.config,
            installed,
            mode: def.mode,
            protocols: def.protocols,
            requires_key: def.requires_key,
            selection,
            current,
            chosen,
        }
    }

    /// 单选 agent 的配置文件现在连到哪：地址和凭据的原样，选择器「现在」那组要用。读不了就是 `None`。
    pub fn connection(&self, agent: usize) -> Option<Connection> {
        let kind = self.agents[agent].kind?;
        current::connection(&self.engine, &self.home, kind).ok()
    }

    /// 「把当前配置存成供应商」：按这个 agent 的配置文件拼好的一家新供应商（core 的 `draft`），
    /// 交给新建表单预先填好。文件连的不是一个地址（Bedrock 这类）时是 `None`。
    pub fn draft(&self, agent: usize) -> Option<Provider> {
        let kind = self.agents[agent].kind?;
        current::draft(&self.engine, &self.home, kind, &self.providers)
            .ok()
            .flatten()
    }

    /// 胶囊画成虚线边框：文件连的和列表里哪家都对不上（decisions「『自定义』的样子」）。
    pub fn pill_dashed(&self, agent: usize) -> bool {
        let agent = &self.agents[agent];
        matches!(agent.selection, Selection::Single(None))
            && matches!(agent.current, Some(Current::Custom { .. }))
    }

    pub fn agent_index(&self, kind: AgentKind) -> Option<usize> {
        self.agents
            .iter()
            .position(|agent| agent.kind == Some(kind))
    }

    /// 保存一家（新建或替换同 ID 的），写进在用它的 agent，然后重新读一遍。
    pub fn save_provider(&mut self, provider: &Provider) -> Result<Vec<Synced>, StoreError> {
        let synced = apply::save_provider(&self.engine, &self.home, &self.providers, provider);
        self.reload();
        synced
    }

    /// 删掉一家，多选 agent 里的那一项一起删，然后重新读一遍。
    pub fn delete_provider(&mut self, id: &str) -> Result<Vec<Synced>, StoreError> {
        let synced = apply::delete_provider(&self.engine, &self.home, &self.providers, id);
        self.reload();
        synced
    }

    /// 哪些 agent 在用这家（按配置文件认，删除前确认用）。
    pub fn used_by(&self, id: &str) -> UsedBy {
        apply::used_by(&self.engine, &self.home, &self.providers, id)
    }

    /// 新供应商的 ID：`base` 没被占就用它，占了加 `-2`、`-3`。
    pub fn unique_id(&self, base: &str) -> String {
        let taken = |id: &str| self.providers.iter().any(|p| p.id == id);
        if !taken(base) {
            return base.to_string();
        }
        (2..)
            .map(|n| format!("{base}-{n}"))
            .find(|id| !taken(id))
            .expect("some suffix is free")
    }

    /// 用这个预设建过供应商。
    pub fn has_preset(&self, preset_id: &str) -> bool {
        self.providers
            .iter()
            .any(|p| p.preset.as_deref() == Some(preset_id))
    }

    /// 单选 agent 换一家（`None` 是「默认」）：在后台写文件，写完按文件重读一遍，提示发给订阅者，也返回给调用的窗口。
    /// 胶囊先改过来，不等写完；写失败了重读会把它退回文件里的实际状态。
    pub fn switch(
        &mut self,
        agent: usize,
        provider: Option<usize>,
        cx: &mut Context<Self>,
    ) -> Task<Vec<Notice>> {
        let Some(kind) = self.agents[agent].kind else {
            return Task::ready(Vec::new());
        };
        let picked = provider.map(|p| self.providers[p].clone());
        let name = picked.as_ref().map(|p| p.name.clone());
        let row = &mut self.agents[agent];
        row.selection = Selection::Single(provider);
        row.current = Some(match &picked {
            Some(p) => Current::Provider(p.id.clone()),
            None => Current::Default,
        });
        row.chosen = true;
        cx.notify();

        let engine = self.engine.clone();
        let home = self.home.clone();
        let write = cx
            .background_executor()
            .spawn(async move { apply::switch(&engine, &home, kind, picked.as_ref()) });
        cx.spawn(async move |this, cx| {
            let result = write.await;
            this.update(cx, |store, cx| {
                store.reload();
                let notices = store.describe_switch(kind, name, result);
                store.announce(&notices, cx);
                notices
            })
            .unwrap_or_default()
        })
    }

    /// 多选 agent 勾上或取消一家：和 [`Self::switch`] 一样，勾选框先改、后台写、写完重读。
    pub fn toggle(
        &mut self,
        agent: usize,
        provider: usize,
        cx: &mut Context<Self>,
    ) -> Task<Vec<Notice>> {
        let Some(kind) = self.agents[agent].kind else {
            return Task::ready(Vec::new());
        };
        let Selection::Multi(enabled) = &mut self.agents[agent].selection else {
            return Task::ready(Vec::new());
        };
        let enabling = !enabled.contains(&provider);
        if enabling {
            enabled.push(provider);
        } else {
            enabled.retain(|&p| p != provider);
        }
        cx.notify();

        let picked = self.providers[provider].clone();
        let name = picked.name.clone();
        let engine = self.engine.clone();
        let home = self.home.clone();
        let write = cx.background_executor().spawn(async move {
            if enabling {
                apply::enable(&engine, &home, kind, &picked)
            } else {
                apply::disable(&engine, &home, kind, &picked.id)
            }
        });
        cx.spawn(async move |this, cx| {
            let result = write.await;
            this.update(cx, |store, cx| {
                store.reload();
                let notices = store.describe_toggle(kind, name, enabling, result);
                store.announce(&notices, cx);
                notices
            })
            .unwrap_or_default()
        })
    }

    /// 「Key 不一致」的第二个选项：把这个 agent 配置文件里的 Key 存进那家供应商。
    /// 这个 agent 的文件本来就是这个 Key，不用写，只记下它选的是这家；别的在用这家的 agent
    /// 由 core 的 `save_provider` 跟着换成新 Key（core.md「改了供应商以后」）。
    pub fn adopt_key(
        &mut self,
        agent: usize,
        provider: usize,
        key: String,
        cx: &mut Context<Self>,
    ) -> Task<Vec<Notice>> {
        let Some(kind) = self.agents[agent].kind else {
            return Task::ready(Vec::new());
        };
        let mut picked = self.providers[provider].clone();
        picked.key = key;
        let name = picked.name.clone();
        let row = &mut self.agents[agent];
        row.selection = Selection::Single(Some(provider));
        row.current = Some(Current::Provider(picked.id.clone()));
        row.chosen = true;
        cx.notify();

        let engine = self.engine.clone();
        let home = self.home.clone();
        let providers = self.providers.clone();
        let write = cx.background_executor().spawn(async move {
            let synced = apply::save_provider(&engine, &home, &providers, &picked)?;
            engine
                .set_selection(kind.id(), Some(&picked.id))
                .map_err(StoreError::Lock)?;
            Ok::<_, StoreError>(synced)
        });
        cx.spawn(async move |this, cx| {
            let result = write.await;
            this.update(cx, |store, cx| {
                store.reload();
                let notices = store.describe_adopt(kind, name, result);
                store.announce(&notices, cx);
                notices
            })
            .unwrap_or_default()
        })
    }

    /// 启动时在后台按本机的 Codex 重写模型目录（core 的 `refresh_codex_catalog`）：
    /// 重写了、没写成都说一声，不用写就不吭声。
    pub fn refresh_codex_catalog(&self, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        let home = self.home.clone();
        let providers = self.providers.clone();
        let refresh = cx
            .background_executor()
            .spawn(async move { apply::refresh_codex_catalog(&engine, &home, &providers) });
        cx.spawn(async move |this, cx| {
            let result = refresh.await;
            this.update(cx, |store, cx| {
                let notice = match result {
                    Ok(Some(Ok(outcome))) if outcome.changed => {
                        let file = apply::live_files(&store.home, AgentKind::Codex)
                            .last()
                            .map(|file| store.tilde(&file.path))
                            .unwrap_or_default();
                        Some(Notice::ok(t!("write.codex_catalog_refreshed", file = file)))
                    }
                    Ok(Some(Err(skipped))) => Some(Notice::err(t!(
                        "write.codex_catalog_refresh_failed",
                        reason = describe_skipped(skipped)
                    ))),
                    Err(err) => Some(Notice::err(t!(
                        "write.codex_catalog_refresh_failed",
                        reason = err.to_string()
                    ))),
                    _ => None,
                };
                if let Some(notice) = notice {
                    store.announce(&[notice], cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn describe_switch(
        &self,
        kind: AgentKind,
        name: Option<String>,
        result: Result<Switched, ApplyError>,
    ) -> Vec<Notice> {
        let agent = kind.to_string();
        let switched = match result {
            Ok(switched) => switched,
            Err(err) => {
                return vec![Notice::err(t!(
                    "toast.sync_failed",
                    agent = agent,
                    error = err.to_string()
                ))];
            }
        };
        let files = apply::live_files(&self.home, kind);
        let mut outcomes = vec![switched.outcome];
        if let Some(Ok(catalog)) = &switched.catalog {
            outcomes.push(catalog.clone());
        }
        let headline = match name {
            Some(name) => t!("write.switched", agent = agent, provider = name),
            None => t!("write.restored", agent = agent),
        };
        let mut notices = vec![Notice::ok(format!(
            "{headline}{}",
            self.file_note(&files, &outcomes)
        ))];
        // Codex 的模型目录没写成：config.toml 已经切过去了，但 `/model` 列的还是 GPT 模型，要说一声。
        if let Some(Err(skipped)) = switched.catalog {
            notices.push(Notice::err(t!(
                "write.codex_catalog_skipped",
                reason = describe_skipped(skipped)
            )));
        }
        notices
    }

    fn describe_toggle(
        &self,
        kind: AgentKind,
        name: String,
        enabling: bool,
        result: Result<Outcome, ApplyError>,
    ) -> Vec<Notice> {
        let agent = kind.to_string();
        match result {
            Ok(outcome) => {
                let headline = if enabling {
                    t!("write.enabled", agent = agent, provider = name)
                } else {
                    t!("write.disabled", agent = agent, provider = name)
                };
                let files = apply::live_files(&self.home, kind);
                vec![Notice::ok(format!(
                    "{headline}{}",
                    self.file_note(&files, &[outcome])
                ))]
            }
            Err(err) => vec![Notice::err(t!(
                "toast.sync_failed",
                agent = agent,
                error = err.to_string()
            ))],
        }
    }

    fn describe_adopt(
        &self,
        kind: AgentKind,
        name: String,
        result: Result<Vec<Synced>, StoreError>,
    ) -> Vec<Notice> {
        let agent = kind.to_string();
        let synced = match result {
            Ok(synced) => synced,
            Err(err) => {
                return vec![Notice::err(t!(
                    "write.key_adopt_failed",
                    provider = name,
                    error = err.to_string()
                ))];
            }
        };
        let written: Vec<String> = synced
            .iter()
            .filter(|s| s.result.as_ref().is_ok_and(|outcome| outcome.changed))
            .map(|s| s.agent.to_string())
            .collect();
        let headline = if written.is_empty() {
            t!("write.key_adopted", provider = name, agent = agent)
        } else {
            let agents = written.join(&t!("common.list_separator"));
            t!(
                "write.key_adopted_synced",
                provider = name,
                agent = agent,
                agents = agents
            )
        };
        let mut notices = vec![Notice::ok(headline)];
        for failed in synced.iter().filter(|s| s.result.is_err()) {
            let error = failed
                .result
                .as_ref()
                .err()
                .map(ToString::to_string)
                .unwrap_or_default();
            notices.push(Notice::err(t!(
                "toast.sync_failed",
                agent = failed.agent.to_string(),
                error = error
            )));
        }
        notices
    }

    /// 提示的后半句「 · 写了 ~/.claude/settings.json，先备份了」：只说实际写了的文件。
    fn file_note(&self, files: &[LiveFile], outcomes: &[Outcome]) -> String {
        let written: Vec<(String, bool)> = files
            .iter()
            .zip(outcomes)
            .filter(|(_, outcome)| outcome.changed)
            .map(|(file, outcome)| (self.tilde(&file.path), outcome.backup.is_some()))
            .collect();
        if written.is_empty() {
            return t!("write.unchanged").into_owned();
        }
        let names = written
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(&t!("common.list_separator"));
        if written.iter().any(|(_, backup)| *backup) {
            t!("write.wrote_backup", files = names).into_owned()
        } else {
            t!("write.wrote", files = names).into_owned()
        }
    }

    fn announce(&self, notices: &[Notice], cx: &mut Context<Self>) {
        for notice in notices {
            cx.emit(notice.clone());
        }
        cx.notify();
    }

    /// 家目录下的路径写成 `~/…`。
    pub fn tilde(&self, path: &Path) -> String {
        match path.strip_prefix(&self.home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        }
    }

    pub fn is_enabled(&self, agent: usize, provider: usize) -> bool {
        match &self.agents[agent].selection {
            Selection::Single(current) => *current == Some(provider),
            Selection::Multi(enabled) => enabled.contains(&provider),
        }
    }

    /// 这个 agent 连这家会用哪种协议：agent 偏好顺序里这家提供的第一种；一种都没有就是接口对不上。
    pub fn protocol_for(&self, agent: usize, provider: usize) -> Option<Protocol> {
        let provider = &self.providers[provider];
        self.agents[agent]
            .protocols
            .iter()
            .copied()
            .find(|protocol| provider.endpoints.contains_key(protocol))
    }

    /// 这个 agent 用不了这家的原因；能用是 `None`。
    pub fn unusable(&self, agent: usize, provider: usize) -> Option<Unusable> {
        if self.protocol_for(agent, provider).is_none() {
            return Some(Unusable::Incompatible);
        }
        if self.agents[agent].requires_key && self.providers[provider].key.is_empty() {
            return Some(Unusable::NoKey);
        }
        None
    }

    /// 这个 agent 用这家时的模型（在用的芯片上显示）。
    pub fn model_for(&self, agent: usize, provider: usize) -> Option<String> {
        let provider = &self.providers[provider];
        let model = match self.agents[agent].kind {
            Some(kind) => agent::default_model(provider, kind).ok(),
            None => provider.default_or_first(),
        };
        model.map(|model| model.id.clone())
    }

    /// 胶囊里画什么：要叠的图标（单选是一个）和文字。
    pub fn pill(&self, agent: usize) -> (Vec<Logo>, SharedString) {
        let agent = &self.agents[agent];
        match &agent.selection {
            Selection::Single(Some(p)) => (
                vec![logo(&self.providers[*p])],
                self.providers[*p].name.clone().into(),
            ),
            // 文件连的不是列表里的哪家：「自定义 · 主机名」；地址对得上、Key 不一样：「DeepSeek · Key 不一致」。
            Selection::Single(None) => match &agent.current {
                Some(Current::Custom { host }) => {
                    let label = match host {
                        Some(host) => t!("pill.custom", host = host),
                        None => t!("pill.custom_bare"),
                    };
                    (vec![agent.logo], label.into())
                }
                Some(Current::KeyMismatch { provider }) => {
                    let found = self.providers.iter().find(|p| &p.id == provider);
                    let name = found.map(|p| p.name.clone()).unwrap_or_default();
                    let logos = found.map(logo).into_iter().collect();
                    (logos, t!("pill.key_mismatch", name = name).into())
                }
                // 配置文件解析不了：不能冒充「默认」，选一家时的报错会说明哪一行坏了。
                None if agent.selectable() => (vec![agent.logo], t!("pill.unreadable").into()),
                _ => (vec![agent.logo], t!("common.default").into()),
            },
            Selection::Multi(enabled) if enabled.is_empty() => {
                (vec![agent.logo], t!("multi.none").into())
            }
            Selection::Multi(enabled) => {
                let logos = enabled.iter().map(|&p| logo(&self.providers[p])).collect();
                let label = if enabled.len() == 1 {
                    t!("multi.count_one")
                } else {
                    t!("multi.count_other", count = enabled.len())
                };
                (logos, label.into())
            }
        }
    }

    /// 正在用这家的 agent（下标）。
    pub fn users_of(&self, provider: usize) -> Vec<usize> {
        (0..self.agents.len())
            .filter(|&a| self.is_enabled(a, provider))
            .collect()
    }
}

/// Codex 模型目录没生成出来的原因，按界面语言说（core 的 `Skipped` 只有中文）。
fn describe_skipped(skipped: Skipped) -> std::borrow::Cow<'static, str> {
    match skipped {
        Skipped::NotFound => t!("codex.not_found"),
        Skipped::Failed => t!("codex.failed"),
        Skipped::NoTemplate => t!("codex.no_template"),
    }
}

/// 供应商的图标：预设有图标的用预设的；认不出预设、也不是 Anthropic 官方，就用名字的第一个字。
pub fn logo(provider: &Provider) -> Logo {
    let preset = provider
        .preset
        .as_deref()
        .and_then(preset::get)
        .or_else(|| {
            provider
                .endpoints
                .iter()
                .find_map(|(&protocol, url)| preset::matching(protocol, url))
        });
    if let Some(logo) = preset.and_then(|preset| preset_logo(&preset.id)) {
        return logo;
    }
    if provider.is_anthropic_official() {
        return Logo::Mono("icons/brand/anthropic.svg");
    }
    Logo::letter(&provider.name)
}

/// 预设的图标。同一家的国内站、海外站、Coding Plan 共用一个（ID 的第一段）。
/// 火山、硅基流动、BytePlus 还没有图标，用首字（提案 10）。
pub fn preset_logo(preset_id: &str) -> Option<Logo> {
    let family = preset_id.split('-').next()?;
    Some(match family {
        "deepseek" => Logo::Color("icons/brand/deepseek-color.svg"),
        "kimi" => Logo::Mono("icons/brand/kimi.svg"),
        "zhipu" | "zai" => Logo::Color("icons/brand/zhipu-color.svg"),
        "minimax" => Logo::Color("icons/brand/minimax-color.svg"),
        "qianwen" | "qwencloud" => Logo::Color("icons/brand/qwen-color.svg"),
        "openrouter" => Logo::Mono("icons/brand/openrouter.svg"),
        _ => return None,
    })
}

/// 列表里名字下面写的主机名：第一个接口地址的。
pub fn host(provider: &Provider) -> Option<&str> {
    provider
        .endpoints
        .values()
        .next()
        .and_then(|url| provider::host(url))
}

/// Key 胶囊里的字：最后四位；没填是 `None`。
pub fn key_tail(provider: &Provider) -> Option<String> {
    let key = provider.key.trim();
    if key.is_empty() {
        return None;
    }
    let tail: String = key
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    Some(format!("…{tail}"))
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：gpui 也导出一个 `test` 属性宏，会把 `#[test]` 抢走。
    use std::path::Path;

    use confdeck_core::{apply, preset};

    use super::{AGENTS, key_tail, logo};

    /// 写死的配置文件路径要和 core 适配器写的文件一致，不然「配置文件存在」和实际写的不是同一个文件。
    #[test]
    fn config_paths_match_core_live_files() {
        let home = Path::new("/home/x");
        for def in AGENTS {
            let Some(kind) = def.kind else { continue };
            let live = apply::live_files(home, kind)
                .into_iter()
                .next()
                .expect("every agent writes a file");
            assert_eq!(live.path, home.join(def.config), "{}", def.name);
            assert_eq!(def.mode, kind.mode(), "{}", def.name);
            assert_eq!(def.protocols, kind.protocols(), "{}", def.name);
        }
    }

    #[test]
    fn every_preset_family_is_known_or_uses_a_letter() {
        // 有图标的预设列在 preset_logo 里；没有的用首字。这里只保证每个预设都能算出一个图标。
        for preset in preset::all() {
            let provider = preset.provider(&preset.id, "");
            let _ = logo(&provider);
        }
    }

    #[test]
    fn key_tail_shows_the_last_four() {
        let mut provider = preset::all()[0].provider("x", "sk-abcdef1234");
        assert_eq!(key_tail(&provider).as_deref(), Some("…1234"));
        provider.key = "   ".into();
        assert_eq!(key_tail(&provider), None);
    }
}
