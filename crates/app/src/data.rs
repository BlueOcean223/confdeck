//! 界面的数据层。供应商、每个 agent 现在连的是谁、本机设置、官方账号都来自 core 的一份快照（`status::snapshot`）：
//! 启动时在界面线程读第一份（免得窗口先空着闪一下），之后每次 `reload` 都在后台读，读完换上。
//! 读和写排在同一队里（`enqueue`），一个跑完才开始下一个。
//! agent 的清单（排序和图标）在这里写死；配置文件、单选多选、协议、要不要 Key 都问 core 的适配器（`AgentKind::adapter`）。
//!
//! 用户点出来的写操作（选一家、存供应商、开关……，列表见 app.md「写」）走 `write.rs` 的 `Store::run`：内存里先改，写在后台线程，
//! 写完再读一份快照，文件里的实际状态盖掉先改的。写了什么、失败的原因做成 [`Notice`] 事件发出去，主窗口做成提示条。
//! 不走 `run` 的：`state.json` 里当场写的几样（代理、额度查询设置，[`Store::set_local`]）、原始文件（`replace_file`）、
//! 清理备份、官方账号的改名、删除、登录（`accounts.rs`）；启动时重写 Codex 模型目录也排进同一队。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use confdeck_core::account;
use confdeck_core::agent::codex::{self, catalog::Skipped};
use confdeck_core::agent::{self, AdaptError, AgentKind, ApplyError, FileSetup, Mode};
use confdeck_core::apply::{self, UsedBy};
use confdeck_core::backup::{self, Backup, Reason};
use confdeck_core::current;
use confdeck_core::data::DataDir;
use confdeck_core::engine::{Engine, EngineError, LiveFile, Local, Outcome, Place, Snapshot};
use confdeck_core::preset;
use confdeck_core::provider::{self, Protocol, Provider};
use confdeck_core::status::{self, AgentState, AgentStatus};
use confdeck_core::store;
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

pub struct Agent {
    pub kind: AgentKind,
    pub name: &'static str,
    pub logo: Logo,
    /// 显示用的配置文件路径（`~/…`）。
    config: String,
    pub mode: Mode,
    /// 现在连的是谁（core 的快照；切换时先改成选的那家，不等写完）。
    pub status: AgentStatus,
    /// 设置页「显示哪些 agent」里关掉了：Agent 页不列它。
    pub hidden_on_page: bool,
    /// 设置页「显示哪些 agent」里关掉了：菜单栏面板不列它。
    pub hidden_in_panel: bool,
}

impl Agent {
    /// 配置文件存在。
    pub fn installed(&self) -> bool {
        self.status.installed()
    }

    /// 能在界面上选供应商：配置文件在。
    pub fn selectable(&self) -> bool {
        self.installed()
    }

    /// 显示用的配置文件路径。
    pub fn config_path(&self) -> String {
        self.config.clone()
    }
}

/// 界面上列的 agent 和图标，按这个顺序：只有 core 里有适配器的（decisions「Agent 页」：还没接上的不列）。
const AGENTS: [(AgentKind, Logo); 5] = [
    (
        AgentKind::ClaudeCode,
        Logo::Color("icons/brand/claudecode-color.svg"),
    ),
    (AgentKind::Codex, Logo::Color("icons/brand/codex-color.svg")),
    (AgentKind::Opencode, Logo::Mono("icons/brand/opencode.svg")),
    (AgentKind::Pi, Logo::Mono("icons/brand/pi.svg")),
    (AgentKind::GrokBuild, Logo::Mono("icons/brand/grok.svg")),
];

fn agent((kind, logo): (AgentKind, Logo), state: &AgentState, home: &Path, local: &Local) -> Agent {
    Agent {
        kind,
        name: kind.name(),
        logo,
        config: tilde(home, &state.config),
        mode: kind.adapter().mode(),
        status: state.status.clone(),
        hidden_on_page: hidden(local, Place::Page, kind),
        hidden_in_panel: hidden(local, Place::Panel, kind),
    }
}

/// 家目录下的路径写成 `~/…`。
fn tilde(home: &Path, path: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// 设置页里在这一处藏起了这个 agent。
fn hidden(local: &Local, place: Place, kind: AgentKind) -> bool {
    local.hidden(place).contains(kind.id())
}

/// 单选 agent 的配置文件连的就是选中的那家：`disk` 是最近一份快照里的，`now` 是内存里的（先改了还没写完时两边不一样）。
fn settled(disk: Option<&AgentStatus>, now: &AgentStatus) -> bool {
    disk == Some(now) && *now != AgentStatus::Unreadable
}

/// 快照换上以后要做的（[`Store::reload_then`]）。
type AfterSnapshot = Box<dyn FnOnce(&mut Store, &mut Context<Store>)>;

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

/// 读写盘排成一队：一次写（连着它后面那份快照）、一次重读，跑完一个才开始下一个。
/// 快照读不到写了一半的，两份快照也不会同时跑（快照里会记切换记录、存回令牌，同时跑会记重）。
#[derive(Default)]
struct Queue {
    /// 排进来过几个。
    queued: u64,
    /// 最后一次排的重读开始了没有：没开始的话再要重读不用再排，它开始时读得到现在的样子。
    waiting: Option<Arc<AtomicBool>>,
    /// 最后排进来的那个的后台任务，下一个等它跑完。
    tail: Option<Task<()>>,
}

impl Queue {
    /// 排一个，返回它的号。
    fn push(&mut self) -> u64 {
        self.queued += 1;
        self.queued
    }

    /// 第 `n` 个回来时后面没有再排的：只有它的快照换上。
    fn is_latest(&self, n: u64) -> bool {
        n == self.queued
    }

    /// 要重读一次：已经排着一次还没开始的，不用再排（`None`）；不然返回新排的这次开始时要设上的标记。
    fn reload(&mut self) -> Option<Arc<AtomicBool>> {
        if let Some(begun) = &self.waiting
            && !begun.load(Ordering::Acquire)
        {
            return None;
        }
        let begun = Arc::new(AtomicBool::new(false));
        self.waiting = Some(begun.clone());
        Some(begun)
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
    /// `state.json` 里本机的设置（藏起哪些 agent、出站代理、额度查询），跟着快照读，改了换成改完的样子。
    pub(crate) local: Local,
    /// 在界面线程上改过几次 `state.json`（[`Self::set_local`]）。后台读快照之前先看一眼：读的时候之后又改过，就不用它读到的那份。
    local_edits: Arc<AtomicU64>,
    /// 最近一份快照里各 agent 的样子（文件里的），和 `agents` 一一对应。先改了内存、还没写完时两边不一样。
    disk: Vec<AgentStatus>,
    /// 换上过几份快照。配置页看它变了才重读文件。
    generation: u64,
    /// 读写盘的队（[`Self::enqueue`]）。
    queue: Queue,
    /// 下一份快照换上以后要做的（[`Self::reload_then`]）。排在后面的快照才换上时，等换上的那份。
    after_snapshot: Vec<AfterSnapshot>,
    /// 联网的那几样（测连通、models.dev）的状态，见 `net.rs`。
    pub(crate) net: crate::net::NetState,
    /// 用量的汇总，见 `usage.rs`。
    pub(crate) usage: crate::usage::UsageState,
    /// 官方账号，见 `accounts.rs`。
    pub(crate) accounts: crate::accounts::AccountState,
    /// 额度、余额的读数，见 `readings.rs`。单独一个 `Entity`：查回来不通知整个 Store。
    pub(crate) readings: Entity<crate::readings::Readings>,
    /// 读数说登录文件换了人（[`crate::readings::LoginChanged`]）：重读。
    _login_changed: Subscription,
    _quit: Subscription,
}

impl EventEmitter<Notice> for Store {}

impl Store {
    /// 读数据目录和各 agent 的配置文件（第一份快照在界面线程读）。上次没收拾完的登录（强退、崩了）这时收拾掉，
    /// 退出时停掉在跑的登录。
    pub fn load(data: DataDir, home: PathBuf, cx: &mut Context<Self>) -> Self {
        account::clean_stale_logins(&data);
        let engine = Arc::new(Engine::new(data));
        let snapshot = status::snapshot(&engine, &home);
        let net = crate::net::NetState::default();
        let reader = crate::readings::reader(home.clone(), engine.data().clone());
        let http = net.http.clone();
        let readings = cx.new(|_| crate::readings::Readings::new(reader, http));
        let login_changed = cx.subscribe(
            &readings,
            |store, _, _: &crate::readings::LoginChanged, cx| store.reload(cx),
        );
        let mut store = Self {
            engine,
            home,
            providers: Vec::new(),
            agents: Vec::new(),
            load_error: None,
            local: Local::default(),
            local_edits: Default::default(),
            disk: Vec::new(),
            generation: 0,
            queue: Default::default(),
            after_snapshot: Vec::new(),
            net,
            usage: Default::default(),
            accounts: Default::default(),
            readings,
            _login_changed: login_changed,
            _quit: cx.on_app_quit(|store, _| {
                store.abandon_logins();
                async {}
            }),
        };
        store.apply_snapshot(snapshot, 0);
        store.plan_readings(cx);
        store
    }

    /// 在后台重读供应商、每个 agent 现在连的是谁、本机设置和官方账号，读完换上、通知。
    /// 排在前面的写跑完才读；已经排着一次还没开始的重读，就不再排。
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(begun) = self.queue.reload() else {
            return;
        };
        self.enqueue(
            move |_, _| begun.store(true, Ordering::Release),
            |_, (), _| {},
            cx,
        );
    }

    /// 排一个读写盘的活：前面的跑完，`job` 在后台线程跑，接着读一份快照；回到界面线程，后面没有再排的那个
    /// （只有它的快照读得到前面所有的改动）换上快照，再把 `job` 的结果交给 `then`。见 `write.rs` 开头。
    /// `job` panic 时进程直接退出（gpui 的后台线程不能 unwind），这里不管。
    pub(crate) fn enqueue<R: Send + 'static>(
        &mut self,
        job: impl FnOnce(&Engine, &Path) -> R + Send + 'static,
        then: impl FnOnce(&mut Self, R, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        let n = self.queue.push();
        let engine = self.engine.clone();
        let home = self.home.clone();
        let edits = self.local_edits.clone();
        let before = self.queue.tail.take();
        let (finish, finished) = async_channel::bounded(1);
        self.queue.tail = Some(cx.background_executor().spawn(async move {
            if let Some(before) = before {
                before.await;
            }
            let result = job(&engine, &home);
            // 先看当场改过几次再读：读到的一定有这几次（`set_local` 写完才加一）。
            let edits = edits.load(Ordering::Acquire);
            let snapshot = status::snapshot(&engine, &home);
            finish.try_send((result, snapshot, edits)).ok();
        }));
        cx.spawn(async move |this, cx| {
            let Ok((result, snapshot, edits)) = finished.recv().await else {
                return;
            };
            this.update(cx, |store, cx| {
                if store.queue.is_latest(n) {
                    store.settle(snapshot, edits, cx);
                }
                then(store, result, cx);
            })
            .ok();
        })
        .detach();
    }

    /// [`Self::reload`]，读完换上以后再做 `then`（要按新读到的账号、供应商做的事）。
    pub(crate) fn reload_then(
        &mut self,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &mut Context<Self>) + 'static,
    ) {
        self.after_snapshot(then);
        self.reload(cx);
    }

    /// 下一份快照换上以后做 `then`（不管是 `reload` 读的还是写完读的）。
    pub(crate) fn after_snapshot(
        &mut self,
        then: impl FnOnce(&mut Self, &mut Context<Self>) + 'static,
    ) {
        self.after_snapshot.push(Box::new(then));
    }

    /// 改完 `state.json` 拿回来的样子换上（core 的 `set_proxy`、`set_check` 返回的）。各 agent 藏没藏不跟着它改：
    /// 藏起是排队写的，还没写进去时这份里没有，内存里的才对；快照换上时再按文件来。
    pub(crate) fn set_local(&mut self, local: Local) {
        self.local = local;
        self.local_edits.fetch_add(1, Ordering::Release);
    }

    /// 换上过几份快照：变了才说明文件可能变了。
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// 换上读好的快照，做排着等它的事（[`Self::reload_then`]），通知。
    fn settle(&mut self, snapshot: status::Snapshot, edits: u64, cx: &mut Context<Self>) {
        self.apply_snapshot(snapshot, edits);
        self.plan_readings(cx);
        for then in std::mem::take(&mut self.after_snapshot) {
            then(self, cx);
        }
        cx.notify();
    }

    /// 换上一份快照。`edits` 是开始读时的 `local_edits`：之后又改过 `state.json`，留着改完的那份。
    fn apply_snapshot(&mut self, snapshot: status::Snapshot, edits: u64) {
        if edits == self.local_edits.load(Ordering::Acquire) {
            self.local = snapshot.local.clone();
        }
        let local = &self.local;
        self.agents = AGENTS
            .into_iter()
            .map(|def| agent(def, snapshot.agent(def.0), &self.home, local))
            .collect();
        self.load_error = snapshot.providers_error.map(|err| err.to_string());
        self.providers = snapshot.providers;
        self.disk = self
            .agents
            .iter()
            .map(|agent| agent.status.clone())
            .collect();
        self.apply_accounts(snapshot.accounts);
        self.generation += 1;
    }

    /// 单选 agent 连着的那家（`providers` 的下标）；「默认」「自定义」「Key 不一致」是 `None`。
    pub fn picked(&self, agent: usize) -> Option<usize> {
        let id = self.agents[agent].status.provider()?;
        self.providers.iter().position(|p| p.id == id)
    }

    /// 多选 agent 勾着的（下标，按文件里的顺序；列表里没有的不算）。
    pub fn enabled(&self, agent: usize) -> Vec<usize> {
        match &self.agents[agent].status {
            AgentStatus::Multi { ids } => ids
                .iter()
                .filter_map(|id| self.providers.iter().position(|p| &p.id == id))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// 单选 agent 的配置文件连的就是选中的那家。刚选了别家、文件还没写完时不是，
    /// 这时文件里还是上一家的模型，「配置 ›」页不拿来显示。
    pub fn settled(&self, agent: usize) -> bool {
        settled(self.disk.get(agent), &self.agents[agent].status)
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
    pub fn live_file(&self, agent: usize) -> LiveFile {
        self.agents[agent].kind.adapter().config(&self.home)
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
        self.reload(cx);
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

    /// 设置页 › Claude Code 的一个开关现在是开是关。文件不存在、键不在算关；文件读不了也先显示成关，
    /// 点的时候才报错。
    pub fn claude_toggle(&self, toggle: ClaudeToggle) -> bool {
        toggle.read(&self.engine, &self.home).unwrap_or(false)
    }

    /// 开关写的是哪个文件，`~/…` 的写法（行里的小字）。
    pub fn claude_toggle_path(&self, toggle: ClaudeToggle) -> String {
        self.tilde(&toggle.file(&self.home).path)
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
            .flat_map(|agent| agent.kind.adapter().files(&self.home))
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
        // 备份列表变了：重读一份，设置页按新的代数重列。
        self.reload(cx);
        self.announce(notice.as_slice(), cx);
    }

    /// 胶囊画成虚线边框：文件连的和列表里哪家都对不上（decisions「『自定义』的样子」）。
    pub fn pill_dashed(&self, agent: usize) -> bool {
        matches!(self.agents[agent].status, AgentStatus::Custom { .. })
    }

    pub fn agent_index(&self, kind: AgentKind) -> Option<usize> {
        self.agents.iter().position(|agent| agent.kind == kind)
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

    /// 启动时在后台按本机的 Codex 重写模型目录（core 的 `refresh_codex_catalog`）：
    /// 重写了、没写成说一声；不用写、或者本机根本没有 codex 就不吭声（旧目录照样能用，没必要每次启动都报）。
    pub fn refresh_codex_catalog(&mut self, cx: &mut Context<Self>) {
        let providers = self.providers.clone();
        // 排进写的队：和用户刚点的 Codex 切换不同时写。
        self.enqueue(
            move |engine, home| apply::refresh_codex_catalog(engine, home, &providers),
            |store, result, cx| {
                let notice = match result {
                    Ok(Some(Ok(outcome))) if outcome.changed => {
                        let file = store.tilde(&codex::catalog::live_file(&store.home).path);
                        Some(Notice::ok(t!("write.codex_catalog_refreshed", file = file)))
                    }
                    Ok(Some(Err(Skipped::NotFound))) => None,
                    Ok(Some(Err(skipped))) => Some(Notice::err(t!(
                        "write.codex_catalog_refresh_failed",
                        reason = crate::write::describe_skipped(skipped)
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
            },
            cx,
        );
    }

    pub(crate) fn announce(&self, notices: &[Notice], cx: &mut Context<Self>) {
        for notice in notices {
            cx.emit(notice.clone());
        }
        cx.notify();
    }

    /// 家目录下的路径写成 `~/…`。
    pub fn tilde(&self, path: &Path) -> String {
        tilde(&self.home, path)
    }

    pub fn is_enabled(&self, agent: usize, provider: usize) -> bool {
        let id = &self.providers[provider].id;
        match &self.agents[agent].status {
            AgentStatus::Provider { id: picked } => picked == id,
            AgentStatus::Multi { ids } => ids.contains(id),
            _ => false,
        }
    }

    /// 这个 agent 连这家会用哪种协议（和写进文件的一样：用户选的 > 预设推荐的 > agent 偏好顺序里这家提供的第一种）；用不了是 `None`。
    pub fn protocol_for(&self, agent: usize, provider: usize) -> Option<Protocol> {
        let usable = self.agents[agent]
            .kind
            .adapter()
            .usable(&self.providers[provider]);
        usable.ok().map(|(protocol, _)| protocol)
    }

    /// 这个 agent 用不了这家的原因；能用是 `None`。
    pub fn unusable(&self, agent: usize, provider: usize) -> Option<Unusable> {
        match self.agents[agent]
            .kind
            .adapter()
            .usable(&self.providers[provider])
        {
            Ok(_) => None,
            Err(AdaptError::MissingKey { .. }) => Some(Unusable::NoKey),
            Err(_) => Some(Unusable::Incompatible),
        }
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
        let found = |id: &str| self.providers.iter().find(|p| p.id == id);
        match &agent.status {
            AgentStatus::Provider { id } => match found(id) {
                Some(p) => (vec![logo(p)], p.name.clone().into()),
                // 刚删掉、快照还没读回来的一瞬间。
                None => (vec![agent.logo], id.clone().into()),
            },
            // 文件连的不是列表里的哪家：「自定义 · 主机名」；地址对得上、Key 不一样：「DeepSeek · Key 不一致」。
            AgentStatus::Custom { host, .. } => {
                let label = match host {
                    Some(host) => t!("pill.custom", host = host),
                    None => t!("pill.custom_bare"),
                };
                (vec![agent.logo], label.into())
            }
            AgentStatus::KeyMismatch { id, .. } => {
                let found = found(id);
                let name = found.map(|p| p.name.clone()).unwrap_or_default();
                let logos = found.map(logo).into_iter().collect();
                (logos, t!("pill.key_mismatch", name = name).into())
            }
            _ if agent.mode == Mode::Coexist => {
                let enabled = self.enabled(index);
                if enabled.is_empty() {
                    return (vec![agent.logo], t!("multi.none").into());
                }
                let logos = enabled.iter().map(|&p| logo(&self.providers[p])).collect();
                let label = if enabled.len() == 1 {
                    t!("multi.count_one")
                } else {
                    t!("multi.count_other", count = enabled.len())
                };
                (logos, label.into())
            }
            // 配置文件解析不了：不能冒充「默认」，选一家时的报错会说明哪一行坏了。
            AgentStatus::Unreadable => (vec![agent.logo], t!("pill.unreadable").into()),
            // 官方登录写成一家供应商（「Anthropic 官方」，厂商的图标）；存过几个官方账号时写现在登的是哪个。
            AgentStatus::Official | AgentStatus::Absent | AgentStatus::Multi { .. } => {
                let logo = crate::official::logo(self, index);
                match self.live_label(index) {
                    Some(label) => (vec![logo], label.into()),
                    None => (vec![logo], crate::official::name(agent.kind)),
                }
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

/// 提示条里只说文件名（`settings.json`），路径太长。
pub(crate) fn file_name(file: &LiveFile) -> String {
    file.path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
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

    use std::sync::atomic::Ordering;

    use confdeck_core::preset;
    use confdeck_core::status::AgentStatus;

    use super::{Queue, key_tail, logo, settled};

    #[test]
    fn settled_means_the_file_is_what_is_shown() {
        let relay = AgentStatus::Provider { id: "relay".into() };
        assert!(settled(Some(&relay), &relay));
        // 刚选了别家、文件还没写完。
        assert!(!settled(Some(&AgentStatus::Official), &relay));
        assert!(!settled(None, &relay));
        // 文件读不了时不拿文件里的值。
        assert!(!settled(
            Some(&AgentStatus::Unreadable),
            &AgentStatus::Unreadable
        ));
    }

    #[test]
    fn only_the_last_queued_is_latest() {
        let mut queue = Queue::default();
        let first = queue.push();
        let second = queue.push();
        assert!(!queue.is_latest(first));
        assert!(queue.is_latest(second));
    }

    #[test]
    fn a_reload_that_has_not_begun_covers_the_next_one() {
        let mut queue = Queue::default();
        let begun = queue.reload().expect("the first reload is queued");
        assert!(queue.reload().is_none(), "one is waiting already");
        // 后面又排了一个写：再要重读还是不排。写完读的那份更晚，最后换上的是它。
        queue.push();
        assert!(queue.reload().is_none());
        begun.store(true, Ordering::Release);
        assert!(queue.reload().is_some(), "it began before this change");
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
