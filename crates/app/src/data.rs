//! 界面的数据层。供应商来自 confdeck-core 的 `providers.json`；每个 agent 现在用哪家，
//! 从它的配置文件里认出来（`current`）。agent 的清单在这里写死：core 里有适配器的五个能选供应商，
//! 其余三个（阶段 7）只列出来。
//!
//! 选了一家就写文件（`switch` / `toggle`）：写在后台线程，写完按文件重读一遍，内存里先改的胶囊会被
//! 文件里的实际状态盖掉；写了什么、有没有备份、失败的原因做成 [`Notice`] 事件发出去，主窗口做成提示条，面板写在底栏。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use confdeck_core::agent::codex::catalog::Skipped;
use confdeck_core::agent::{self, AgentKind, Connection, FileSetup, Mode};
use confdeck_core::apply::{self, ApplyError, Switched, Synced, UsedBy};
use confdeck_core::backup::{self, Backup, Reason};
use confdeck_core::current::{self, Current};
use confdeck_core::data::DataDir;
use confdeck_core::engine::{Engine, EngineError, LiveFile, Outcome, Place, Snapshot};
use confdeck_core::preset;
use confdeck_core::provider::{self, Protocol, Provider};
use confdeck_core::store::{self, StoreError};
use confdeck_core::toggles::{self, ClaudeToggle};
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
    pub kind: AgentKind,
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
    /// 设置页「显示哪些 agent」里关掉了：Agent 页不列它。
    pub hidden_on_page: bool,
    /// 设置页「显示哪些 agent」里关掉了：菜单栏面板不列它。
    pub hidden_in_panel: bool,
}

impl Agent {
    /// 能在界面上选供应商：配置文件在。
    pub fn selectable(&self) -> bool {
        self.installed
    }

    /// 显示用的配置文件路径。
    pub fn config_path(&self) -> String {
        format!("~/{}", self.config)
    }
}

struct AgentDef {
    kind: AgentKind,
    name: &'static str,
    logo: Logo,
    config: &'static str,
    mode: Mode,
    protocols: &'static [Protocol],
    requires_key: bool,
}

/// 界面上列的 agent，按这个顺序：只有 core 里有适配器的（decisions「Agent 页」：还没接上的不列）。
const AGENTS: &[AgentDef] = &[
    AgentDef {
        kind: AgentKind::ClaudeCode,
        name: "Claude Code",
        logo: Logo::Color("icons/brand/claudecode-color.svg"),
        config: ".claude/settings.json",
        mode: Mode::Switch,
        protocols: &[Protocol::Anthropic],
        requires_key: true,
    },
    AgentDef {
        kind: AgentKind::Codex,
        name: "Codex",
        logo: Logo::Color("icons/brand/codex-color.svg"),
        config: ".codex/config.toml",
        mode: Mode::Switch,
        protocols: &[Protocol::OpenaiResponses],
        requires_key: true,
    },
    AgentDef {
        kind: AgentKind::Opencode,
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
        kind: AgentKind::Pi,
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
        kind: AgentKind::GrokBuild,
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
];

/// 某个 agent 为什么用不了某家供应商。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Unusable {
    /// agent 要的接口这家一种都没有。
    Incompatible,
    /// agent 必须有 Key，这家没填。
    NoKey,
}

/// 设置 › 备份的下拉框里的一项：文件归谁。接上的 agent 各一项，再加 confdeck 自己（`providers.json`）。
#[derive(Clone, Copy)]
pub struct BackupOwner {
    /// `LiveFile::owner`，也是备份目录的第一层。
    pub id: &'static str,
    pub name: &'static str,
    pub logo: Logo,
}

/// 设置 › 备份里的一行：一个有备份的文件和它的备份（最新的在前）。
pub struct FileBackups {
    pub file: LiveFile,
    pub backups: Vec<Backup>,
}

impl FileBackups {
    /// 原件以外还有几份（「清理」会删的）。
    pub fn cleanable(&self) -> usize {
        self.backups
            .iter()
            .filter(|backup| backup.reason != Reason::Original)
            .count()
    }
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
    pub(crate) fn ok(text: impl Into<SharedString>) -> Self {
        Self {
            kind: NoticeKind::Success,
            text: text.into(),
        }
    }

    pub(crate) fn err(text: impl Into<SharedString>) -> Self {
        Self {
            kind: NoticeKind::Error,
            text: text.into(),
        }
    }
}

pub struct Store {
    /// 写文件在后台线程，和界面线程共用一个引擎（进程内的锁才管用）。
    pub(crate) engine: Arc<Engine>,
    pub(crate) home: PathBuf,
    pub providers: Vec<Provider>,
    pub agents: Vec<Agent>,
    /// `providers.json` 读不了的原因。读不了时列表是空的，不许保存，不然会把文件盖掉。
    pub load_error: Option<String>,
    /// 联网的那几样（测连通、models.dev）的状态，见 `net.rs`。
    pub(crate) net: crate::net::NetState,
    /// 用量的汇总，见 `usage.rs`。
    pub(crate) usage: crate::usage::UsageState,
    /// 官方账号，见 `accounts.rs`。
    pub(crate) accounts: crate::accounts::AccountState,
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
            net: Default::default(),
            usage: Default::default(),
            accounts: Default::default(),
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
        let hidden = (
            self.engine.hidden(Place::Page),
            self.engine.hidden(Place::Panel),
        );
        self.agents = AGENTS.iter().map(|def| self.agent(def, &hidden)).collect();
        self.note_selections();
        self.reload_accounts();
    }

    fn agent(&self, def: &AgentDef, hidden: &(BTreeSet<String>, BTreeSet<String>)) -> Agent {
        let kind = def.kind;
        let installed = self.home.join(def.config).exists();
        let index_of = |id: &str| self.providers.iter().position(|p| p.id == id);
        let mut current = None;
        let selection = match def.mode {
            Mode::Switch if installed => {
                let found = current::current(&self.engine, &self.home, kind, &self.providers).ok();
                let picked = match &found {
                    Some(Current::Provider(id)) => index_of(id),
                    _ => None,
                };
                current = found;
                Selection::Single(picked)
            }
            Mode::Coexist if installed => Selection::Multi(
                current::enabled(&self.engine, &self.home, kind)
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|id| index_of(id))
                    .collect(),
            ),
            Mode::Switch => Selection::Single(None),
            Mode::Coexist => Selection::Multi(Vec::new()),
        };
        Agent {
            kind,
            name: def.name,
            logo: def.logo,
            config: def.config,
            installed,
            mode: def.mode,
            protocols: def.protocols,
            requires_key: def.requires_key,
            selection,
            current,
            hidden_on_page: hidden.0.contains(kind.id()),
            hidden_in_panel: hidden.1.contains(kind.id()),
        }
    }

    /// 设置页「显示哪些 agent」：在 Agent 页或菜单栏面板里显示 / 藏起一个 agent。记在 `state.json`，当场写完。
    pub fn set_agent_hidden(
        &mut self,
        agent: usize,
        place: Place,
        hidden: bool,
        cx: &mut Context<Self>,
    ) {
        let kind = self.agents[agent].kind;
        let notice = self
            .engine
            .set_hidden(place, kind.id(), hidden)
            .err()
            .map(|err| {
                Notice::err(t!(
                    "toast.sync_failed",
                    agent = kind.to_string(),
                    error = err.to_string()
                ))
            });
        self.reload();
        self.announce(notice.as_slice(), cx);
    }

    /// 单选 agent 的配置文件现在连到哪：地址和凭据的原样，选择器「现在」那组要用。读不了就是 `None`。
    pub fn connection(&self, agent: usize) -> Option<Connection> {
        let kind = self.agents[agent].kind;
        current::connection(&self.engine, &self.home, kind).ok()
    }

    /// 「把当前配置存成供应商」：按这个 agent 的配置文件拼好的一家新供应商（core 的 `draft`），
    /// 交给新建表单预先填好。文件连的不是一个地址（Bedrock 这类）时是 `None`。
    pub fn draft(&self, agent: usize) -> Option<Provider> {
        let kind = self.agents[agent].kind;
        current::draft(&self.engine, &self.home, kind, &self.providers)
            .ok()
            .flatten()
    }

    /// 单选 agent 的配置文件连的就是选中的那家。刚选了别家、文件还没写完时不是，
    /// 这时文件里还是上一家的模型，「配置 ›」页不拿来显示。
    pub fn settled(&self, agent: usize) -> bool {
        let kind = self.agents[agent].kind;
        current::current(&self.engine, &self.home, kind, &self.providers)
            .is_ok_and(|on_disk| Some(&on_disk) == self.agents[agent].current.as_ref())
    }

    /// 「配置 ›」页编辑框里的内容写的模型（下拉框显示的就是它）。解析不了（手改到一半）是 `Err`；
    /// 连的不是一个地址、多选 agent 是 `Ok(None)`。
    pub fn setup_of(&self, agent: usize, text: &str) -> Result<Option<FileSetup>, ApplyError> {
        current::setup_of(&self.home, self.agents[agent].kind, text.as_bytes())
    }

    /// 「配置 ›」页还没保存的改动：一家从 `before` 改成 `after`，编辑框里的 `text` 会变成什么样（core 的 `apply::preview`）。
    pub fn preview(
        &self,
        agent: usize,
        before: &Provider,
        after: &Provider,
        text: &str,
    ) -> Result<String, ApplyError> {
        let kind = self.agents[agent].kind;
        let bytes = apply::preview(&self.home, kind, before, after, text.as_bytes())?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// 这个 agent 的配置文件（Codex 有两个，「原始文件」只编辑 config.toml）。
    pub fn live_file(&self, agent: usize) -> Option<LiveFile> {
        let kind = self.agents[agent].kind;
        apply::live_files(&self.home, kind).into_iter().next()
    }

    pub fn read_file(&self, file: &LiveFile) -> Result<Snapshot, EngineError> {
        self.engine.read(file)
    }

    /// 「原始文件」保存：整份替换，读出来以后文件被改过就报冲突、解析不了报行列（core 的 `Engine::replace`），
    /// 都不写。写了就重读一遍；`announce` 时说一声「已保存 settings.json」（同一次保存已经说过「已更新」就不说）。
    pub fn replace_file(
        &mut self,
        file: &LiveFile,
        bytes: &[u8],
        base: &Snapshot,
        announce: bool,
        cx: &mut Context<Self>,
    ) -> Result<Outcome, EngineError> {
        let outcome = self.engine.replace(file, bytes, base)?;
        self.reload();
        if announce {
            self.announce(
                &[Notice::ok(t!("write.file_saved", file = file_name(file)))],
                cx,
            );
        } else {
            cx.notify();
        }
        Ok(outcome)
    }

    /// 「配置 ›」页改了一项（槽位、列表、默认模型、接口）：`provider` 是改好覆盖的那家。
    /// 内存里先换掉，界面立刻显示新值；文件在后台写（Codex 要重写模型目录，不能卡住界面），
    /// 写完按文件重读。写了这个 agent 就说一句「Claude Code 已更新」，没写成的单独报。
    pub fn save_override(
        &mut self,
        agent: usize,
        provider: Provider,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let kind = self.agents[agent].kind;
        // `save_provider` 要拿改之前的列表认出在用这家的 agent。
        let before = self.providers.clone();
        if let Some(slot) = self.providers.iter_mut().find(|p| p.id == provider.id) {
            *slot = provider.clone();
        }
        cx.notify();

        let engine = self.engine.clone();
        let home = self.home.clone();
        let write = cx
            .background_executor()
            .spawn(async move { apply::save_provider(&engine, &home, &before, &provider) });
        cx.spawn(async move |this, cx| {
            let result = write.await;
            this.update(cx, |store, cx| {
                store.reload();
                let notices = store.describe_update(kind, result);
                store.announce(&notices, cx);
            })
            .ok();
        })
    }

    /// 设置页 › Claude Code 的一个开关现在是开是关。文件不存在、键不在算关；文件读不了也先显示成关，
    /// 点的时候才报错。
    pub fn claude_toggle(&self, toggle: ClaudeToggle) -> bool {
        toggle.read(&self.engine, &self.home).unwrap_or(false)
    }

    /// 开关写的是哪个文件，`~/…` 的写法（行里的小字）。
    pub fn claude_toggle_path(&self, toggle: ClaudeToggle) -> String {
        self.tilde(&toggle.file(&self.home).path)
    }

    /// 设置页点了一个开关：写那个文件（就改一个键，当场写完），写了说一句「Claude Code 已更新」，没写成报错。
    pub fn set_claude_toggle(&mut self, toggle: ClaudeToggle, on: bool, cx: &mut Context<Self>) {
        let agent = AgentKind::ClaudeCode.to_string();
        let notice = match toggle.set(&self.engine, &self.home, on) {
            Ok(outcome) if outcome.changed => Some(Notice::ok(t!("write.updated", agent = agent))),
            Ok(_) => None,
            Err(err) => Some(Notice::err(t!(
                "toast.sync_failed",
                agent = agent,
                error = err.to_string()
            ))),
        };
        self.reload();
        self.announce(notice.as_slice(), cx);
    }

    /// 设置 › 备份的下拉框：接上的 agent（按标签的顺序）和 confdeck 自己。
    pub fn backup_owners(&self) -> Vec<BackupOwner> {
        let mut owners: Vec<BackupOwner> = self
            .agents
            .iter()
            .map(|agent| BackupOwner {
                id: agent.kind.id(),
                name: agent.name,
                logo: agent.logo,
            })
            .collect();
        owners.push(BackupOwner {
            id: store::live_file(self.engine.data()).owner,
            name: "confdeck",
            logo: Logo::Mono("icons/confdeck.svg"),
        });
        owners
    }

    /// 设置 › 备份：confdeck 会写的每个文件（各 agent 的、开关的两个、`providers.json`）里有备份的那些。
    /// 文件归谁看 `file.owner`，和 [`Self::backup_owners`] 对。
    pub fn backups(&self) -> Vec<FileBackups> {
        let mut files: Vec<LiveFile> = self
            .agents
            .iter()
            .flat_map(|agent| apply::live_files(&self.home, agent.kind))
            .collect();
        files.extend(toggles::extra_files(&self.home));
        files.push(store::live_file(self.engine.data()));
        files
            .into_iter()
            .filter_map(|file| {
                let backups = backup::list(self.engine.data(), &file.shelf()).ok()?;
                (!backups.is_empty()).then_some(FileBackups { file, backups })
            })
            .collect()
    }

    /// `~/.confdeck/backups`，备份卡的小字。
    pub fn backups_dir(&self) -> PathBuf {
        self.engine.data().backups()
    }

    /// 一份备份的内容（读不了算空）。
    pub fn read_backup(&self, backup: &Backup) -> String {
        std::fs::read(&backup.path)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default()
    }

    /// 文件现在的内容（不存在、读不了算空）。
    pub fn read_text(&self, file: &LiveFile) -> String {
        self.engine
            .read(file)
            .ok()
            .and_then(|snapshot| snapshot.bytes)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default()
    }

    /// 用一份备份替换文件（core 先把现在的内容备份成「恢复前」）。写了重读，说一句「已恢复 settings.json」。
    pub fn restore(&mut self, file: &LiveFile, backup: &Backup, cx: &mut Context<Self>) {
        let notice = match self.engine.restore(file, backup) {
            Ok(_) => Notice::ok(t!("write.file_restored", file = file_name(file))),
            Err(err) => Notice::err(err.to_string()),
        };
        self.reload();
        self.announce(&[notice], cx);
    }

    /// 「清理」：删掉这个文件原件以外的备份，说一句清理了几份。
    pub fn clean(&mut self, file: &LiveFile, cx: &mut Context<Self>) {
        let shelf = file.shelf();
        let data = self.engine.data();
        let count = backup::list(data, &shelf)
            .map(|backups| {
                backups
                    .iter()
                    .filter(|backup| backup.reason != Reason::Original)
                    .count()
            })
            .unwrap_or(0);
        let notice = match backup::clean(data, &shelf) {
            Ok(_) if count == 1 => Some(Notice::ok(t!("write.cleaned_one"))),
            Ok(_) if count > 1 => Some(Notice::ok(t!("write.cleaned_other", count = count))),
            Ok(_) => None,
            Err(err) => Some(Notice::err(err.to_string())),
        };
        self.announce(notice.as_slice(), cx);
    }

    /// 胶囊画成虚线边框：文件连的和列表里哪家都对不上（decisions「『自定义』的样子」）。
    pub fn pill_dashed(&self, agent: usize) -> bool {
        let agent = &self.agents[agent];
        matches!(agent.selection, Selection::Single(None))
            && matches!(agent.current, Some(Current::Custom { .. }))
    }

    pub fn agent_index(&self, kind: AgentKind) -> Option<usize> {
        self.agents.iter().position(|agent| agent.kind == kind)
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
        let kind = self.agents[agent].kind;
        // 选的就是现在这家、文件是 confdeck 写的样子、state.json 也记着它：没什么可写，也不提示「已切到」。
        // 文件认出来是这家但 state 里没记（比如刚把文件里的连接存成供应商）还是要走一遍，把选择记下来。
        let picked_id = provider.map(|p| self.providers[p].id.as_str());
        let recorded = self
            .engine
            .selection(kind.id())
            .is_some_and(|recorded| recorded.as_deref() == picked_id);
        let unchanged = recorded
            && match (&self.agents[agent].selection, &self.agents[agent].current) {
                (Selection::Single(current), Some(Current::Provider(id))) => {
                    *current == provider && picked_id == Some(id.as_str())
                }
                (Selection::Single(None), Some(Current::Default)) => provider.is_none(),
                _ => false,
            };
        if unchanged {
            return Task::ready(Vec::new());
        }
        let picked = provider.map(|p| self.providers[p].clone());
        let name = picked.as_ref().map(|p| p.name.clone());
        let row = &mut self.agents[agent];
        row.selection = Selection::Single(provider);
        row.current = Some(match &picked {
            Some(p) => Current::Provider(p.id.clone()),
            None => Current::Default,
        });
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
        let kind = self.agents[agent].kind;
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
        let kind = self.agents[agent].kind;
        let mut picked = self.providers[provider].clone();
        picked.key = key;
        let name = picked.name.clone();
        let row = &mut self.agents[agent];
        row.selection = Selection::Single(Some(provider));
        row.current = Some(Current::Provider(picked.id.clone()));
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
    /// 重写了、没写成说一声；不用写、或者本机根本没有 codex 就不吭声（旧目录照样能用，没必要每次启动都报）。
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
                    Ok(Some(Err(Skipped::NotFound))) => None,
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
        let headline = match name {
            Some(name) => t!("write.switched", agent = agent, provider = name),
            None => t!("write.restored", agent = agent),
        };
        let mut notices = vec![Notice::ok(headline)];
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
            Ok(_) => {
                let headline = if enabling {
                    t!("write.enabled", agent = agent, provider = name)
                } else {
                    t!("write.disabled", agent = agent, provider = name)
                };
                vec![Notice::ok(headline)]
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

    fn describe_update(
        &self,
        kind: AgentKind,
        result: Result<Vec<Synced>, StoreError>,
    ) -> Vec<Notice> {
        let synced = match result {
            Ok(synced) => synced,
            Err(err) => {
                return vec![Notice::err(t!(
                    "toast.sync_failed",
                    agent = kind.to_string(),
                    error = err.to_string()
                ))];
            }
        };
        let mut notices = Vec::new();
        for entry in &synced {
            match &entry.result {
                // 只有改的这个 agent 的文件会变；别的在用这家的 agent 覆盖没动，`changed` 为假。
                Ok(outcome) if outcome.changed && entry.agent == kind => {
                    notices.push(Notice::ok(t!("write.updated", agent = kind.to_string())));
                }
                Ok(_) => {}
                Err(err) => notices.push(Notice::err(t!(
                    "toast.sync_failed",
                    agent = entry.agent.to_string(),
                    error = err.to_string()
                ))),
            }
        }
        notices
    }

    pub(crate) fn announce(&self, notices: &[Notice], cx: &mut Context<Self>) {
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
        agent::default_model(provider, self.agents[agent].kind)
            .ok()
            .map(|model| model.id.clone())
    }

    /// 胶囊里画什么：要叠的图标（单选是一个）和文字。
    pub fn pill(&self, agent: usize) -> (Vec<Logo>, SharedString) {
        let index = agent;
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
                // 官方登录写成一家供应商（「Anthropic 官方」，厂商的图标）；存过几个官方账号时写现在登的是哪个。
                _ => {
                    let logo = crate::official::logo(self, index);
                    match self.live_email(index) {
                        Some(email) => (vec![logo], email.into()),
                        None => (vec![logo], crate::official::name(agent.kind)),
                    }
                }
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
/// 提示条里只说文件名（`settings.json`），路径太长。
fn file_name(file: &LiveFile) -> String {
    file.path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

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
        "bailian" => Logo::Mono("icons/brand/bailian.svg"),
        "volcengine" => Logo::Color("icons/brand/volcengine.png"),
        "byteplus" => Logo::Color("icons/brand/byteplus.png"),
        "siliconflow" => Logo::Color("icons/brand/siliconflow-color.svg"),
        "ant" => Logo::Color("icons/brand/antling.png"),
        "anthropic" => Logo::Mono("icons/brand/anthropic.svg"),
        "openai" => Logo::Mono("icons/brand/openai.svg"),
        "google" => Logo::Color("icons/brand/gemini-color.svg"),
        "xai" => Logo::Mono("icons/brand/xai.svg"),
        "mistral" => Logo::Color("icons/brand/mistral-color.svg"),
        "meta" => Logo::Color("icons/brand/meta-color.svg"),
        "openrouter" => Logo::Mono("icons/brand/openrouter.svg"),
        "vercel" => Logo::Mono("icons/brand/vercel.svg"),
        "opencode" => Logo::Mono("icons/brand/opencode.svg"),
        "huggingface" => Logo::Color("icons/brand/huggingface-color.svg"),
        "nvidia" => Logo::Color("icons/brand/nvidia-color.svg"),
        // 小米（cc-switch 那张是「Xiaomi MiMo」字标，20px 看不清）、Groq、Cerebras、Together、Fireworks、Baseten
        // 还没有图标，用首字。
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
            let kind = def.kind;
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
