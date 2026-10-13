//! 官方账号多账号（10-13，阶段 4）：Codex、Grok Build、Claude Code（10-13 后加，只在 macOS）各存几个官方登录，
//! 切换时把选中的那个写回 agent 的登录。
//!
//! - **存在哪**（`Storage`，arch-perf R5 按存储方式分两种）：Codex、Grok Build 的登录是一个文件（`files`），
//!   Claude Code 的令牌在钥匙串里、是谁在 `~/.claude.json`（[`claude`]）。`~/.confdeck/accounts.json` 记有哪些账号（[`Account`]），
//!   每个账号的登录原样存在 `~/.confdeck/accounts/<agent>/<id>.json`（只有自己能读；Claude Code 的存在钥匙串里）。不同步、不导出。
//!   下面存、切、加、改名、删的算法不分 agent，只经 `Storage` 碰登录；按 agent 挑 `Storage` 只在 `storage` 一处。
//! - **锁**：钥匙串的读写都在引擎的锁外面（`security` 可能等授权框），存下的登录（每个账号一份，整份写）也在锁外读写；
//!   引擎的锁只包住改 `accounts.json` 和写 agent 的登录文件。同一个 agent 的账号操作在这个进程里一个一个来（`serial`），
//!   钥匙串等授权框时只挡同一个 agent 的账号操作。
//! - **登录**（[`Login`]）：不自己做 OAuth，用 agent 自己的 `login` 命令，把它的家目录（`CODEX_HOME`、`GROK_HOME`）
//!   指到一个临时目录；登录完读那里的登录存成账号（[`add`]），删掉临时目录。
//! - **换下来的登录先存好**：agent 用着用着会换新令牌（刷新令牌用一次就作废），所以切换（[`switch`]）时先把登录文件
//!   现在的内容存回它所属的账号（比存着的新才存；不认识的账号存成新账号），再写入要切的账号。每次读快照时（`read_all`）也这样存。
//! - **认账号**（[`identify`]）：Codex 看 `id_token` 里的用户和工作区（同一个人的个人、团队工作区算两个），
//!   Grok 看 `user_id` 和 `team_id`，Claude Code 看 `oauthAccount` 的账号和组织。账号 ID 是这两样的 hash。
//! - **不换令牌**：confdeck 不用刷新令牌（同「订阅额度」）。存着不用的账号放久了令牌可能失效，到时候重新登录。
//! - **名字和备注**（[`label`]，10-13 提案 13）：用户自己起的，只存在 `accounts.json`，不碰登录文件。

pub mod claude;
mod files;
mod login;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::AgentKind;
use crate::data::DataDir;
use crate::engine::{Engine, EngineError, LiveFile, Outcome};
use crate::fsx::{self, Digest, Mode};
use crate::keychain::Keychain;
use crate::quota::{Check, CheckKey};

pub(crate) use files::grok_entry;
pub use login::{Login, LoginHandle, clean_stale_logins, program};

/// 能存多个官方账号。Claude Code 不是 macOS 时不能（令牌在系统的钥匙串里，没做），只看登着的是谁（[`Live::ViewOnly`]）。
pub fn supported(kind: AgentKind) -> bool {
    storage(kind).is_some_and(Storage::storable)
}

/// 存下的一个官方账号。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub agent: AgentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// 订阅（Codex 的 `plus`、`pro` 等）；Grok 的登录里没有。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// 第一次存下的时间（毫秒）。
    pub added: i64,
    /// 用户起的名字（「工作号」），界面上替换邮箱。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 一段备注，悬停看。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// 名字最长几个字。
pub const NAME_MAX: usize = 24;
/// 备注最长几个字。
pub const NOTE_MAX: usize = 200;

/// 一个登录是谁的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// 账号 ID，同 [`Account::id`]。
    pub id: String,
    pub email: Option<String>,
    pub plan: Option<String>,
    /// 这份登录有多新（毫秒）：Codex 的 `last_refresh`，Grok 的 `expires_at`，Claude Code 的令牌的 `expiresAt`。
    fresh: Option<i64>,
}

impl Identity {
    /// `key` 是认人的那几样拼起来的（Codex 的用户和工作区等），账号 ID 是它和 agent 一起的 hash。
    fn new(
        kind: AgentKind,
        key: &str,
        email: Option<String>,
        plan: Option<String>,
        fresh: Option<i64>,
    ) -> Self {
        let digest = Digest::of(format!("{}:{key}", kind.id()).as_bytes()).0;
        Self {
            id: digest[..16].to_string(),
            email,
            plan,
            fresh,
        }
    }
}

/// 登录文件现在是谁。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Live {
    /// 没有登录文件，或者里面没有认得出的账号（比如 Codex 只有 API Key）。
    LoggedOut,
    /// 存下的账号。
    Known(String),
    /// 还没存的账号：切到别的账号时会存下来。
    Unknown(Identity),
    /// 登着的是谁，但存不了、切不了（不是 macOS 时的 Claude Code）。
    ViewOnly(Identity),
}

#[derive(Debug, thiserror::Error)]
pub enum AccountError {
    #[error(transparent)]
    Engine(#[from] EngineError),
    #[error("这个 agent 不能存多个账号")]
    Unsupported,
    #[error("没有这个账号")]
    Missing,
    #[error("登录文件里没有认得出的账号")]
    Unrecognized,
    #[error("找不到 {0} 命令")]
    NoProgram(&'static str),
    /// 登录命令没登录成功就退出了（或者被取消了），带它最后输出的几行。
    #[error("登录没有完成：{0}")]
    Login(String),
    /// 等了 15 分钟还没登录完（[`Login::wait`]），停掉了。
    #[error("登录超时，已停止")]
    LoginTimedOut,
}

pub(crate) fn io_error(path: &Path) -> impl FnOnce(io::Error) -> EngineError + '_ {
    move |source| EngineError::Io {
        path: path.to_path_buf(),
        source,
    }
}

// ── 存储方式 ─────────────────────────────────────────

/// 一个 agent 的官方登录存在哪、怎么读写。下面的算法只经它碰登录。
///
/// 一份「登录」是存、比、切的单位：Codex、Grok Build 是 `auth.json` 原样的内容，Claude Code 是令牌和 `oauthAccount`
/// 拼成的一份（[`claude::bundle`]）。
pub(crate) trait Storage: Sync {
    fn kind(&self) -> AgentKind;

    /// 能存多个账号（见 [`supported`]）。
    fn storable(&self) -> bool {
        true
    }

    /// 认出一份登录是谁的；没有登录（Codex 只有 API Key 的也算）是 `None`。
    fn identify(&self, login: &Value) -> Option<Identity>;

    /// agent 的登录文件（换账号时引擎写的那个）：Codex、Grok Build 是 `auth.json`，Claude Code 是 `~/.claude.json`。
    fn live_file(&self, home: &Path) -> LiveFile;

    /// 登录文件里看得出的：是谁，和整份登录（令牌不在文件里的是 `None`）。不读钥匙串。
    fn in_file(&self, file: &[u8]) -> Option<Found>;

    /// 现在登着的整份登录；没登录、只有一半的是 `None`。`file` 是读好的登录文件，令牌在钥匙串里的在这里读（锁外调）。
    fn read_live(&self, home: &Path, file: Option<&[u8]>) -> Result<Option<Vec<u8>>, EngineError>;

    /// 登录有一半不在登录文件里（Claude Code 的令牌在钥匙串里），换账号时先换那一半（[`Storage::put_secret`]）。
    fn secret(&self) -> bool {
        false
    }

    /// 换成 `login` 时先换不在登录文件里的那一半（锁外调，在写登录文件之前）。[`Storage::secret`] 为假的什么也不做。
    fn put_secret(&self, _home: &Path, _login: &[u8]) -> Result<(), EngineError> {
        Ok(())
    }

    /// 换成 `login` 时登录文件的新内容（在引擎的锁里调，`file` 是文件现在的内容）。
    fn put_file(
        &self,
        home: &Path,
        file: Option<&[u8]>,
        login: &[u8],
    ) -> Result<Vec<u8>, EngineError>;

    /// 存下的一个账号的登录；没有是 `None`。
    fn read_stored(
        &self,
        data: &DataDir,
        home: &Path,
        id: &str,
    ) -> Result<Option<Vec<u8>>, EngineError>;

    fn write_stored(
        &self,
        data: &DataDir,
        home: &Path,
        id: &str,
        login: &[u8],
    ) -> Result<(), EngineError>;

    /// 删掉存下的登录；本来就没有不算错。
    fn delete_stored(&self, data: &DataDir, home: &Path, id: &str) -> Result<(), EngineError>;

    /// 切不了：Codex 的登录存在钥匙串里（`config` 是读好的 agent 配置文件）。
    fn blocked(&self, _config: Option<&[u8]>) -> bool {
        false
    }

    /// 登录命令的程序名。
    fn program(&self) -> &'static str;

    /// 让登录命令把登录写进临时家目录 `dir`（参数、环境变量）。返回要不要开着标准输入。
    fn login_command(&self, command: &mut Command, dir: &Path) -> bool;

    /// 登录命令跑完以后临时家目录里的登录（读完删掉钥匙串里的临时那条）。
    fn read_login(&self, dir: &Path, keychain: &Keychain) -> Option<Vec<u8>>;

    /// 取消、出错时收拾临时家目录以外的（钥匙串里的临时那条）。
    fn forget_login(&self, _dir: &Path, _keychain: &Keychain) {}
}

/// 这个 agent 的官方登录存在哪；没有官方账号的（Pi、OpenCode）是 `None`。唯一按 agent 挑的地方。
pub(crate) fn storage(kind: AgentKind) -> Option<&'static dyn Storage> {
    match kind {
        AgentKind::Codex => Some(&files::CODEX),
        AgentKind::GrokBuild => Some(&files::GROK),
        AgentKind::ClaudeCode => Some(&claude::Claude),
        AgentKind::Pi | AgentKind::Opencode => None,
    }
}

/// 同一个 agent 的账号操作（切、加、改名、删，快照里存回登录）在这个进程里一个一个来：存下的登录、钥匙串都在引擎的锁外读写，
/// 靠它不交错（比如删账号和存回它的新令牌同时进行）。先拿它再拿引擎的锁。
fn serial(kind: AgentKind) -> MutexGuard<'static, ()> {
    static SERIAL: [Mutex<()>; AgentKind::ALL.len()] =
        [const { Mutex::new(()) }; AgentKind::ALL.len()];
    let n = AgentKind::ALL
        .iter()
        .position(|known| *known == kind)
        .expect("every kind is in ALL");
    SERIAL[n].lock().unwrap_or_else(PoisonError::into_inner)
}

/// 能存多个账号的这个 agent 的 [`Storage`]；不能的报 [`AccountError::Unsupported`]。
fn storable(kind: AgentKind) -> Result<&'static dyn Storage, AccountError> {
    storage(kind)
        .filter(|store| store.storable())
        .ok_or(AccountError::Unsupported)
}

/// 登录文件里认出来的：是谁，和整份登录（Claude Code 的令牌在钥匙串里，没有）。
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Found {
    identity: Identity,
    login: Option<Vec<u8>>,
}

/// 认出一份登录是谁的；没有登录（Codex 只有 API Key 的也算）是 `None`。
pub fn identify(kind: AgentKind, login: &[u8]) -> Option<Identity> {
    identify_with(storage(kind)?, login)
}

fn identify_with(store: &dyn Storage, login: &[u8]) -> Option<Identity> {
    store.identify(&serde_json::from_slice(login).ok()?)
}

/// 现在登着的那份登录（额度查询用）；没有、读不了是 `None`。
pub(crate) fn live_login(home: &Path, kind: AgentKind) -> Option<Vec<u8>> {
    let store = storage(kind)?;
    let file = fsx::read_optional(&store.live_file(home).path).ok()?;
    store.read_live(home, file.as_deref()).ok()?
}

/// 存下的一个账号的登录（和登录文件一个格式），查这个账号的额度用。
pub fn stored_login(
    data: &DataDir,
    home: &Path,
    kind: AgentKind,
    id: &str,
) -> Result<Vec<u8>, AccountError> {
    let store = storage(kind).ok_or(AccountError::Unsupported)?;
    store
        .read_stored(data, home, id)?
        .ok_or(AccountError::Missing)
}

// ── 存下的账号 ───────────────────────────────────────

#[derive(Debug, Default, Serialize, Deserialize)]
struct Book {
    #[serde(default)]
    accounts: Vec<Account>,
}

fn load(data: &DataDir) -> Result<Book, EngineError> {
    let path = data.accounts();
    match fsx::read_optional(&path).map_err(io_error(&path))? {
        None => Ok(Book::default()),
        Some(bytes) => serde_json::from_slice(&bytes)
            .map_err(|err| io_error(&path)(io::Error::new(io::ErrorKind::InvalidData, err))),
    }
}

fn save(data: &DataDir, book: &Book) -> Result<(), EngineError> {
    let path = data.accounts();
    let mut bytes = serde_json::to_vec_pretty(book).expect("accounts serialize");
    bytes.push(b'\n');
    fs::create_dir_all(data.root()).map_err(io_error(data.root()))?;
    fsx::write_atomic(&path, &bytes, Mode::Private).map_err(io_error(&path))
}

/// 建一个只有自己能进的目录（存登录的地方）。
fn private_dir(dir: &Path) -> Result<(), EngineError> {
    fs::create_dir_all(dir).map_err(io_error(dir))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(io_error(dir))?;
    }
    Ok(())
}

/// 存下的登录放在哪（Claude Code 的在钥匙串里，出错时报这个路径）。
fn login_path(data: &DataDir, kind: AgentKind, id: &str) -> PathBuf {
    data.account_logins()
        .join(kind.id())
        .join(format!("{id}.json"))
}

/// 存下的全部账号，先存的在前。
pub fn list(data: &DataDir) -> Result<Vec<Account>, AccountError> {
    Ok(load(data)?.accounts)
}

/// 怎么存一份登录。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Keep {
    /// 只更新存过的账号，而且比存着的新才换。
    Known,
    /// 没存过的也存下来（切换时换下来的登录）。
    Any,
    /// 刚登录的：一定换掉存着的。
    Fresh,
}

/// 把一份登录存进它的账号，返回写了没有（存着的登录、`accounts.json` 有一样变了）。调的时候拿着这个 agent 的 `serial`。
/// 存着的登录在引擎的锁外读写；`accounts.json` 要改的时候才拿锁，在锁里重读一次再改。
fn keep(
    engine: &Engine,
    store: &dyn Storage,
    home: &Path,
    identity: &Identity,
    login: &[u8],
    how: Keep,
) -> Result<bool, EngineError> {
    let data = engine.data();
    let kind = store.kind();
    let book = load(data)?;
    let account = book
        .accounts
        .iter()
        .find(|account| account.agent == kind && account.id == identity.id);
    if account.is_none() && how == Keep::Known {
        return Ok(false);
    }
    let stored = store.read_stored(data, home, &identity.id)?;
    let replace = replaces(store, identity, stored.as_deref(), login, how);
    if replace {
        store.write_stored(data, home, &identity.id, login)?;
    }
    let profile = account.is_none_or(|account| profile_changed(account, identity));
    if profile {
        let _lock = engine.lock()?;
        let mut book = load(data)?;
        if enter(&mut book, kind, identity, how) {
            save(data, &book)?;
        }
    }
    Ok(replace || profile)
}

/// 邮箱、套餐和存着的不一样（登录里没有的不算）。
fn profile_changed(account: &Account, identity: &Identity) -> bool {
    [
        (&account.email, &identity.email),
        (&account.plan, &identity.plan),
    ]
    .into_iter()
    .any(|(field, value)| value.is_some() && field != value)
}

/// 把 `identity` 记进 `book`：没存过的加上（[`Keep::Known`] 不加），存过的换上新的邮箱、套餐。返回改了没有。
fn enter(book: &mut Book, kind: AgentKind, identity: &Identity, how: Keep) -> bool {
    match book
        .accounts
        .iter_mut()
        .find(|account| account.agent == kind && account.id == identity.id)
    {
        None if how == Keep::Known => false,
        None => {
            book.accounts.push(Account {
                id: identity.id.clone(),
                agent: kind,
                email: identity.email.clone(),
                plan: identity.plan.clone(),
                added: chrono::Utc::now().timestamp_millis(),
                name: None,
                note: None,
            });
            true
        }
        Some(account) => {
            let changed = profile_changed(account, identity);
            if identity.email.is_some() {
                account.email.clone_from(&identity.email);
            }
            if identity.plan.is_some() {
                account.plan.clone_from(&identity.plan);
            }
            changed
        }
    }
}

/// 存着的登录 `stored` 要不要换成 `login`（是 `identity` 的）：一样的不换，旧的不换（刚登录的除外）。
fn replaces(
    store: &dyn Storage,
    identity: &Identity,
    stored: Option<&[u8]>,
    login: &[u8],
    how: Keep,
) -> bool {
    match stored {
        None => true,
        Some(old) if old == login => false,
        _ if how == Keep::Fresh => true,
        // 分不出新旧时信登录文件：那是 agent 最后写的。
        Some(old) => match (
            identity.fresh,
            identify_with(store, old).and_then(|old| old.fresh),
        ) {
            (Some(new), Some(old)) => new >= old,
            (None, Some(_)) => false,
            _ => true,
        },
    }
}

/// 登录文件里认出来的是存过的账号，就把文件里的登录存回去（比存着的新才存；没存过的不存）。`book` 是读好的存下的账号：
/// 先拿它看要不要存（大多数时候不用，`accounts.json` 就只读快照开头那一次），要存才拿 `serial`、经 `keep` 再读一次
/// （这期间可能删了）。第二项是存回去了没有（存了的话 `accounts.json` 里的邮箱、套餐可能变了）。
fn sync_found(
    engine: &Engine,
    store: &dyn Storage,
    home: &Path,
    book: &[Account],
    found: Found,
) -> Result<(Live, bool), AccountError> {
    let Found { identity, login } = found;
    let kind = store.kind();
    let Some(account) = book
        .iter()
        .find(|account| account.agent == kind && account.id == identity.id)
    else {
        return Ok((Live::Unknown(identity), false));
    };
    let Some(login) = login else {
        return Ok((Live::Known(identity.id), false));
    };
    let stored = store.read_stored(engine.data(), home, &identity.id)?;
    let kept = if replaces(store, &identity, stored.as_deref(), &login, Keep::Known)
        || profile_changed(account, &identity)
    {
        let _serial = serial(kind);
        keep(engine, store, home, &identity, &login, Keep::Known)?
    } else {
        false
    };
    Ok((Live::Known(identity.id), kept))
}

// ── 快照 ─────────────────────────────────────────────

/// 一次快照里的官方账号（[`crate::status`]）。
#[derive(Debug, Default)]
pub struct Accounts {
    /// 存下的全部账号，先存的在前。
    pub list: Vec<Account>,
    /// 装了的、有官方账号的 agent 的登录文件现在是谁。
    pub live: HashMap<AgentKind, Live>,
    /// Codex 的登录存在钥匙串里（`config.toml` 的 `cli_auth_credentials_store` 不是 `file`）：切不了。
    pub blocked: HashSet<AgentKind>,
    /// `accounts.json`、登录文件读不了、登录存不回去的原因（有好几个时是最后一个）。
    pub error: Option<AccountError>,
}

/// 快照已经读好的一个 agent 的文件（[`crate::status`]），读官方账号时不再读一遍。
pub(crate) struct Read<'a> {
    pub kind: AgentKind,
    /// agent 的配置文件（看 Codex 的登录在不在钥匙串里）。
    pub config: Option<&'a [u8]>,
    /// 读过的登录文件和读的结果（Codex 的 `auth.json`，补丁也要看）。
    pub login: Option<(&'a Path, &'a io::Result<Option<Vec<u8>>>)>,
}

/// 读官方账号：每个装了的、有官方账号的 agent 的登录文件现在是谁；是存过的账号，顺带把 agent 新换的令牌存回去。
/// `installed` 是装了的 agent，`read` 是快照已经读好的文件。`accounts.json`、每个登录文件各读一次。
/// Claude Code 的只看 `~/.claude.json` 是谁，不读钥匙串：它的令牌在切走的时候存（[`switch`]）。
pub(crate) fn read_all(
    engine: &Engine,
    home: &Path,
    installed: &[AgentKind],
    read: &[Read],
) -> Accounts {
    let mut accounts = Accounts::default();
    let book = list(engine.data());
    let mut saved = false;
    for kind in AgentKind::ALL {
        let Some(store) = storage(kind).filter(|_| installed.contains(&kind)) else {
            continue;
        };
        let read = read.iter().find(|read| read.kind == kind);
        if store.blocked(read.and_then(|read| read.config)) {
            accounts.blocked.insert(kind);
            continue;
        }
        let file = store.live_file(home).path;
        let bytes = match read
            .and_then(|read| read.login)
            .filter(|(path, _)| *path == file)
        {
            Some((_, Ok(bytes))) => Ok(bytes.as_deref().map(<[u8]>::to_vec)),
            Some((_, Err(err))) => Err(io::Error::new(err.kind(), err.to_string())),
            None => fsx::read_optional(&file),
        };
        // 读不了：不知道登的是谁（不是「未登录」），原因记下来。
        let found = match bytes {
            Ok(bytes) => bytes.as_deref().and_then(|bytes| store.in_file(bytes)),
            Err(source) => {
                accounts.error = Some(io_error(&file)(source).into());
                continue;
            }
        };
        if !store.storable() {
            let live = found.map_or(Live::LoggedOut, |found| Live::ViewOnly(found.identity));
            accounts.live.insert(kind, live);
            continue;
        }
        let Ok(book) = &book else {
            continue;
        };
        let live = match found {
            None => Ok((Live::LoggedOut, false)),
            Some(found) => sync_found(engine, store, home, book, found),
        };
        match live {
            Ok((live, kept)) => {
                saved |= kept;
                accounts.live.insert(kind, live);
            }
            Err(err) => accounts.error = Some(err),
        }
    }
    // 存回去的时候邮箱、套餐可能改了：再读一次。
    let book = if saved { list(engine.data()) } else { book };
    match book {
        Ok(list) => accounts.list = list,
        Err(err) => accounts.error = Some(err),
    }
    accounts
}

// ── 切、加、改名、删 ─────────────────────────────────

/// 写登录文件时发现它在读完以后变了（agent 刚换了令牌、换了人），最多从头再来几次。
const ATTEMPTS: usize = 3;

/// 切到一个存下的账号：先把登着的存好（没存过的账号存成新账号），再写入这个账号的登录。
/// 钥匙串在锁外读写；锁里只写登录文件。登录整份在文件里的，文件在这期间变了就从头再来（先存好变了以后的）；
/// 有一半在钥匙串里的，先核对登录文件改得了（解析不了就停，钥匙串不动），钥匙串换了以后文件一定跟着换，令牌和人才对得上；
/// 文件还是没写成就把钥匙串换回原来的令牌。
pub fn switch(
    engine: &Engine,
    home: &Path,
    kind: AgentKind,
    id: &str,
) -> Result<Outcome, AccountError> {
    switch_with(engine, storable(kind)?, home, id)
}

fn switch_with(
    engine: &Engine,
    store: &dyn Storage,
    home: &Path,
    id: &str,
) -> Result<Outcome, AccountError> {
    let _serial = serial(store.kind());
    let data = engine.data();
    let file = store.live_file(home);
    if store.read_stored(data, home, id)?.is_none() {
        return Err(AccountError::Missing);
    }
    for _ in 0..ATTEMPTS {
        let pre = fsx::read_optional(&file.path).map_err(io_error(&file.path))?;
        let live = store.read_live(home, pre.as_deref())?;
        if let Some(live) = &live
            && let Some(identity) = identify_with(store, live)
        {
            keep(engine, store, home, &identity, live, Keep::Any)?;
        }
        // 要切的就是登着的这个：上面可能刚把更新的登录存进去，所以在这之后读。
        let login = store
            .read_stored(data, home, id)?
            .ok_or(AccountError::Missing)?;
        switch_secret(store, home, pre.as_deref(), &login)?;
        let mut moved = false;
        let written = engine.write_login(&file, &mut |now| {
            if !store.secret() && now != pre.as_deref() {
                moved = true;
                return Err(conflict(&file));
            }
            store.put_file(home, now, &login)
        });
        match written {
            Err(_) if moved => continue,
            Err(err) => {
                restore_secret(store, home, live.as_deref());
                return Err(err.into());
            }
            Ok(outcome) => return Ok(outcome),
        }
    }
    Err(conflict(&file).into())
}

/// 换钥匙串那一半之前先在内存里改一遍登录文件：改不了（解析不了）就报错，钥匙串不动。
fn switch_secret(
    store: &dyn Storage,
    home: &Path,
    file: Option<&[u8]>,
    login: &[u8],
) -> Result<(), EngineError> {
    if store.secret() {
        store.put_file(home, file, login)?;
        store.put_secret(home, login)?;
    }
    Ok(())
}

/// 钥匙串换了、登录文件没写成：换回原来的令牌（原来没有整份登录的换不回去，报的错里会说文件没写成）。
fn restore_secret(store: &dyn Storage, home: &Path, live: Option<&[u8]>) {
    if store.secret()
        && let Some(live) = live
    {
        let _ = store.put_secret(home, live);
    }
}

fn conflict(file: &LiveFile) -> EngineError {
    EngineError::Conflict {
        path: file.path.clone(),
    }
}

/// 存下一次登录（[`Login::wait`] 读到的）。重新登录存过的账号就换掉它存着的登录；
/// agent 现在没有登录，或者登着的就是这个账号，也写进 agent 的登录。
pub fn add(
    engine: &Engine,
    home: &Path,
    kind: AgentKind,
    login: &[u8],
) -> Result<Account, AccountError> {
    let store = storable(kind)?;
    let identity = identify_with(store, login).ok_or(AccountError::Unrecognized)?;
    let _serial = serial(kind);
    let data = engine.data();
    keep(engine, store, home, &identity, login, Keep::Fresh)?;
    let other = |live: Option<Identity>| live.is_some_and(|live| live.id != identity.id);
    let file = store.live_file(home);
    let pre = fsx::read_optional(&file.path).map_err(io_error(&file.path))?;
    let live = store.read_live(home, pre.as_deref())?;
    if !other(live.as_deref().and_then(|live| identify_with(store, live))) {
        switch_secret(store, home, pre.as_deref(), login)?;
        let written = engine.write_login(&file, &mut |now| {
            // 这期间登了别人：不动。钥匙串已经换了的一定写（同 `switch_with`）。
            let taken = !store.secret()
                && now.is_some_and(|now| other(store.in_file(now).map(|found| found.identity)));
            match now {
                Some(now) if taken => Ok(now.to_vec()),
                _ => store.put_file(home, now, login),
            }
        });
        if let Err(err) = written {
            restore_secret(store, home, live.as_deref());
            return Err(err.into());
        }
    }
    find(data, kind, &identity.id)
}

fn find(data: &DataDir, kind: AgentKind, id: &str) -> Result<Account, AccountError> {
    load(data)?
        .accounts
        .into_iter()
        .find(|account| account.agent == kind && account.id == id)
        .ok_or(AccountError::Missing)
}

/// 给账号起名字、写备注；空的就是去掉。登录文件里还没存的那个先存下（名字要有地方放）。
pub fn label(
    engine: &Engine,
    home: &Path,
    kind: AgentKind,
    id: &str,
    name: &str,
    note: &str,
) -> Result<Account, AccountError> {
    let store = storable(kind)?;
    let _serial = serial(kind);
    let data = engine.data();
    if !load(data)?
        .accounts
        .iter()
        .any(|account| account.agent == kind && account.id == id)
    {
        let file = store.live_file(home).path;
        let pre = fsx::read_optional(&file).map_err(io_error(&file))?;
        let live = store
            .read_live(home, pre.as_deref())?
            .ok_or(AccountError::Missing)?;
        let identity = identify_with(store, &live).ok_or(AccountError::Missing)?;
        if identity.id != id {
            return Err(AccountError::Missing);
        }
        keep(engine, store, home, &identity, &live, Keep::Any)?;
    }
    let _lock = engine.lock()?;
    let mut book = load(data)?;
    let account = book
        .accounts
        .iter_mut()
        .find(|account| account.agent == kind && account.id == id)
        .ok_or(AccountError::Missing)?;
    let clip = |text: &str, max: usize| {
        let text: String = text.trim().chars().take(max).collect();
        (!text.is_empty()).then_some(text)
    };
    account.name = clip(name, NAME_MAX);
    account.note = clip(note, NOTE_MAX);
    let account = account.clone();
    save(data, &book)?;
    Ok(account)
}

/// 删掉一个存下的账号。agent 的登录文件不动：正在用的账号删了也还登着。
pub fn remove(engine: &Engine, home: &Path, kind: AgentKind, id: &str) -> Result<(), AccountError> {
    let store = storable(kind)?;
    let _serial = serial(kind);
    let data = engine.data();
    {
        let _lock = engine.lock()?;
        let mut book = load(data)?;
        let before = book.accounts.len();
        book.accounts
            .retain(|account| !(account.agent == kind && account.id == id));
        if book.accounts.len() == before {
            return Err(AccountError::Missing);
        }
        save(data, &book)?;
    }
    let removed = store.delete_stored(data, home, id);
    // 这个账号的额度查询设置不留（同 `apply::delete_provider`）。`set_check` 自己拿锁，放在锁外面。
    let _ = engine.set_check(&CheckKey::Account(kind, id.to_string()), Check::default());
    Ok(removed?)
}

#[cfg(test)]
pub(crate) mod tests {

    use serde_json::json;

    use super::*;

    pub(crate) fn b64(bytes: &[u8]) -> String {
        const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let n =
                chunk.iter().fold(0u32, |n, b| (n << 8) | u32::from(*b)) << (8 * (3 - chunk.len()));
            for i in 0..=chunk.len() {
                out.push(TABLE[((n >> (18 - 6 * i)) & 63) as usize] as char);
            }
        }
        out
    }

    /// Codex 的 `auth.json`：`user` 和工作区决定是谁，`refresh` 是第几次换的令牌。
    pub(crate) fn codex_auth(user: &str, workspace: &str, refresh: u32) -> Vec<u8> {
        let claims = json!({
            "email": format!("{user}@example.com"),
            "https://api.openai.com/auth": {
                "chatgpt_account_id": workspace,
                "chatgpt_user_id": user,
                "chatgpt_plan_type": "pro",
            },
        });
        let id_token = format!("h.{}.s", b64(claims.to_string().as_bytes()));
        serde_json::to_vec(&json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {"id_token": id_token, "access_token": "at", "refresh_token": format!("rt-{refresh}"), "account_id": workspace},
            "last_refresh": format!("2026-10-{:02}T00:00:00Z", 1 + refresh),
        }))
        .unwrap()
    }

    pub(crate) struct Setup {
        _dir: tempfile::TempDir,
        pub(crate) home: PathBuf,
        pub(crate) engine: Engine,
    }

    pub(crate) fn setup() -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let engine = Engine::new(DataDir::new(dir.path().join("data")));
        Setup {
            _dir: dir,
            home,
            engine,
        }
    }

    impl Setup {
        fn auth(&self) -> Option<Vec<u8>> {
            fs::read(self.home.join(".codex/auth.json")).ok()
        }

        fn set_auth(&self, bytes: &[u8]) {
            fs::create_dir_all(self.home.join(".codex")).unwrap();
            fs::write(self.home.join(".codex/auth.json"), bytes).unwrap();
        }

        fn stored(&self, id: &str) -> Vec<u8> {
            fs::read(login_path(self.engine.data(), AgentKind::Codex, id)).unwrap()
        }
    }

    pub(crate) fn id_of(bytes: &[u8]) -> String {
        identify(AgentKind::Codex, bytes).unwrap().id
    }

    /// 快照读官方账号的那一步（`read_all`），Codex 的两个文件照快照那样先读好。
    fn read(s: &Setup, kind: AgentKind) -> Accounts {
        let config = fs::read(s.home.join(".codex/config.toml")).ok();
        let path = s.home.join(".codex/auth.json");
        let auth = fsx::read_optional(&path);
        let codex = Read {
            kind: AgentKind::Codex,
            config: config.as_deref(),
            login: Some((&path, &auth)),
        };
        read_all(&s.engine, &s.home, &[kind], &[codex])
    }

    /// 登录文件现在是谁（[`read`]）。
    fn live(s: &Setup, kind: AgentKind) -> Live {
        let mut accounts = read(s, kind);
        assert!(accounts.error.is_none(), "{:?}", accounts.error);
        accounts.live.remove(&kind).expect("the agent is read")
    }

    #[test]
    fn identifies_codex_and_grok_logins() {
        let alice = identify(AgentKind::Codex, &codex_auth("alice", "ws1", 0)).unwrap();
        assert_eq!(alice.email.as_deref(), Some("alice@example.com"));
        assert_eq!(alice.plan.as_deref(), Some("pro"));
        assert_eq!(alice.id.len(), 16);
        // 换了令牌还是同一个账号；同一个人的另一个工作区是另一个账号。
        assert_eq!(id_of(&codex_auth("alice", "ws1", 3)), alice.id);
        assert_ne!(id_of(&codex_auth("alice", "ws2", 0)), alice.id);
        // 只有 API Key 不算登录。
        assert_eq!(
            identify(
                AgentKind::Codex,
                br#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk"}"#
            ),
            None
        );

        let grok = json!({
            "https://auth.x.ai::client": {"user_id": "u1", "team_id": "t1", "email": "g@example.com", "expires_at": "2026-10-13T00:00:00Z", "key": "k"},
        });
        let grok = identify(AgentKind::GrokBuild, grok.to_string().as_bytes()).unwrap();
        assert_eq!(grok.email.as_deref(), Some("g@example.com"));
        assert!(grok.fresh.is_some());
        assert_eq!(identify(AgentKind::GrokBuild, b"{}"), None);
    }

    #[test]
    fn switching_keeps_the_login_it_replaces() {
        let s = setup();
        let alice = codex_auth("alice", "ws", 0);
        let bob = codex_auth("bob", "ws", 0);
        s.set_auth(&alice);
        assert_eq!(
            live(&s, AgentKind::Codex),
            Live::Unknown(identify(AgentKind::Codex, &alice).unwrap())
        );
        // 没存过的不存。
        assert!(list(s.engine.data()).unwrap().is_empty());

        // 登录了 bob：alice 还登着，不动登录文件。
        let added = add(&s.engine, &s.home, AgentKind::Codex, &bob).unwrap();
        assert_eq!(added.email.as_deref(), Some("bob@example.com"));
        assert_eq!(s.auth().unwrap(), alice);

        // 切到 bob：alice 存成账号。
        let outcome = switch(&s.engine, &s.home, AgentKind::Codex, &id_of(&bob)).unwrap();
        assert!(outcome.changed && outcome.backup.is_none());
        assert_eq!(s.auth().unwrap(), bob);
        let emails: Vec<_> = list(s.engine.data())
            .unwrap()
            .into_iter()
            .map(|a| a.email.unwrap())
            .collect();
        assert_eq!(emails, ["bob@example.com", "alice@example.com"]);

        // Codex 给 bob 换了令牌；切回 alice 时存下新的，再切回来用的是新的。
        let bob_later = codex_auth("bob", "ws", 1);
        s.set_auth(&bob_later);
        switch(&s.engine, &s.home, AgentKind::Codex, &id_of(&alice)).unwrap();
        assert_eq!(s.auth().unwrap(), alice);
        assert_eq!(s.stored(&id_of(&bob)), bob_later);
        switch(&s.engine, &s.home, AgentKind::Codex, &id_of(&bob)).unwrap();
        assert_eq!(s.auth().unwrap(), bob_later);

        // 平时也存：Codex 又换了一次令牌。
        let bob_latest = codex_auth("bob", "ws", 2);
        s.set_auth(&bob_latest);
        assert_eq!(live(&s, AgentKind::Codex), Live::Known(id_of(&bob)));
        assert_eq!(s.stored(&id_of(&bob)), bob_latest);
        // 登录文件比存着的旧（比如从备份恢复了）：不往回存。
        s.set_auth(&bob);
        live(&s, AgentKind::Codex);
        assert_eq!(s.stored(&id_of(&bob)), bob_latest);
        // 这时存着的和文件不一样，但什么都没存回去：不算「存了」，不用重读 `accounts.json`。
        let book = list(s.engine.data()).unwrap();
        let found = files::CODEX.in_file(&s.auth().unwrap()).unwrap();
        let (_, kept) = sync_found(&s.engine, &files::CODEX, &s.home, &book, found).unwrap();
        assert!(!kept);
        // 不用存：和 `keep` 用的是同一个判断。
        let stored = s.stored(&id_of(&bob));
        let identity = identify(AgentKind::Codex, &bob).unwrap();
        assert!(!replaces(
            &files::CODEX,
            &identity,
            Some(&stored),
            &bob,
            Keep::Known
        ));
        assert!(replaces(
            &files::CODEX,
            &identity,
            Some(&stored),
            &bob,
            Keep::Fresh
        ));
    }

    #[test]
    fn logging_in_again_replaces_the_stored_login() {
        let s = setup();
        // 没登录时，登录的账号直接用上。
        let old = codex_auth("alice", "ws", 5);
        add(&s.engine, &s.home, AgentKind::Codex, &old).unwrap();
        assert_eq!(s.auth().unwrap(), old);
        // 重新登录（令牌失效了）：新的就算时间更早也换上，登着的也换。
        let new = codex_auth("alice", "ws", 0);
        add(&s.engine, &s.home, AgentKind::Codex, &new).unwrap();
        assert_eq!(s.stored(&id_of(&new)), new);
        assert_eq!(s.auth().unwrap(), new);
        assert_eq!(list(s.engine.data()).unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn stored_logins_are_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let s = setup();
        let alice = codex_auth("alice", "ws", 0);
        add(&s.engine, &s.home, AgentKind::Codex, &alice).unwrap();
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let logins = s.engine.data().account_logins();
        assert_eq!(mode(&logins), 0o700);
        assert_eq!(mode(&logins.join("codex")), 0o700);
        assert_eq!(
            mode(&login_path(
                s.engine.data(),
                AgentKind::Codex,
                &id_of(&alice)
            )),
            0o600
        );
        assert_eq!(mode(&s.engine.data().accounts()), 0o600);
    }

    #[test]
    fn removing_an_account_leaves_the_login_file() {
        let s = setup();
        let alice = codex_auth("alice", "ws", 0);
        add(&s.engine, &s.home, AgentKind::Codex, &alice).unwrap();
        let key = CheckKey::Account(AgentKind::Codex, id_of(&alice));
        let off = Check {
            off: true,
            ..Default::default()
        };
        s.engine.set_check(&key, off).unwrap();
        remove(&s.engine, &s.home, AgentKind::Codex, &id_of(&alice)).unwrap();
        assert!(list(s.engine.data()).unwrap().is_empty());
        // 额度查询的设置跟着删掉。
        assert!(s.engine.local().check(&key).is_default());
        assert_eq!(s.auth().unwrap(), alice);
        assert!(matches!(
            remove(&s.engine, &s.home, AgentKind::Codex, &id_of(&alice)),
            Err(AccountError::Missing)
        ));
        assert!(matches!(
            switch(&s.engine, &s.home, AgentKind::Codex, "nope"),
            Err(AccountError::Missing)
        ));
    }

    #[test]
    fn labels_stay_in_the_book() {
        let s = setup();
        let alice = codex_auth("alice", "ws", 0);
        let id = id_of(&alice);
        // 登着、还没存的账号：起名字时先存下。
        s.set_auth(&alice);
        let named = label(
            &s.engine,
            &s.home,
            AgentKind::Codex,
            &id,
            " 工作号 ",
            "11 月到期",
        )
        .unwrap();
        assert_eq!(named.name.as_deref(), Some("工作号"));
        assert_eq!(named.note.as_deref(), Some("11 月到期"));
        assert_eq!(s.stored(&id), alice);
        assert_eq!(s.auth().unwrap(), alice);
        // 再登录一次、换令牌，名字不丢。
        add(
            &s.engine,
            &s.home,
            AgentKind::Codex,
            &codex_auth("alice", "ws", 1),
        )
        .unwrap();
        let book = list(s.engine.data()).unwrap();
        assert_eq!(book[0].name.as_deref(), Some("工作号"));
        // 清空就是去掉；太长的截断。
        let long = "名".repeat(NAME_MAX + 5);
        let cleared = label(&s.engine, &s.home, AgentKind::Codex, &id, &long, "  ").unwrap();
        assert_eq!(cleared.name.unwrap().chars().count(), NAME_MAX);
        assert_eq!(cleared.note, None);
        assert!(matches!(
            label(&s.engine, &s.home, AgentKind::Codex, "nope", "x", ""),
            Err(AccountError::Missing)
        ));
    }

    #[test]
    fn keyring_store_blocks_switching() {
        let s = setup();
        let blocked = |kind| read(&s, kind).blocked.contains(&kind);
        assert!(!blocked(AgentKind::Codex));
        fs::create_dir_all(s.home.join(".codex")).unwrap();
        fs::write(
            s.home.join(".codex/config.toml"),
            "cli_auth_credentials_store = \"file\"\n",
        )
        .unwrap();
        assert!(!blocked(AgentKind::Codex));
        fs::write(
            s.home.join(".codex/config.toml"),
            "cli_auth_credentials_store = \"keyring\"\n",
        )
        .unwrap();
        assert!(blocked(AgentKind::Codex));
        assert!(!blocked(AgentKind::GrokBuild));
    }

    /// 快照只看 `~/.claude.json` 是谁，不读钥匙串。存不了多个账号时（不是 macOS）是 `ViewOnly`。
    #[test]
    fn claude_live_reads_the_profile_not_the_token() {
        let s = setup();
        let kind = AgentKind::ClaudeCode;
        assert_eq!(live(&s, kind), Live::LoggedOut);
        fs::create_dir_all(&s.home).unwrap();
        fs::write(
            s.home.join(".claude.json"),
            r#"{"numStartups": 3, "oauthAccount": {"accountUuid": "u", "emailAddress": " a@example.com ", "organizationType": "claude_max"}}"#,
        )
        .unwrap();
        let identity = match live(&s, kind) {
            Live::Unknown(identity) if supported(kind) => identity,
            Live::ViewOnly(identity) if !supported(kind) => identity,
            other => panic!("{other:?}"),
        };
        assert_eq!(identity.email.as_deref(), Some("a@example.com"));
        assert_eq!(identity.plan.as_deref(), Some("max"));
        fs::write(s.home.join(".claude.json"), r#"{"numStartups": 3}"#).unwrap();
        assert_eq!(live(&s, kind), Live::LoggedOut);
    }

    /// Claude Code 的一份登录（[`claude::bundle`] 拼的样子）：`user` 决定是谁，`refresh` 是第几次换的令牌。
    #[cfg(target_os = "macos")]
    pub(crate) fn claude_login(user: &str, refresh: i64) -> (Value, Value) {
        (
            json!({"accountUuid": format!("uuid-{user}"), "organizationUuid": "org", "emailAddress": format!("{user}@example.com"), "organizationType": "claude_max"}),
            json!({"accessToken": format!("at-{user}-{refresh}"), "refreshToken": format!("rt-{user}-{refresh}"), "expiresAt": 1000 + refresh, "scopes": ["user:inference"]}),
        )
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn claude_switching_swaps_the_token_and_the_profile() {
        let s = setup();
        let kind = AgentKind::ClaudeCode;
        // 临时家目录：钥匙串是目录里的文件，不碰真的。
        let keychain = Keychain::for_home(&s.home);
        assert!(matches!(keychain, Keychain::Fake(_)));
        let secure = || -> Value {
            serde_json::from_slice(
                &keychain
                    .read(claude::SERVICE, &claude::user())
                    .unwrap()
                    .unwrap(),
            )
            .unwrap()
        };
        let profile = || -> Value {
            serde_json::from_slice(&fs::read(s.home.join(".claude.json")).unwrap()).unwrap()
        };
        assert_eq!(live(&s, kind), Live::LoggedOut);

        // 登着 alice，钥匙串那条里还有 MCP 的登录。
        let (alice, alice_token) = claude_login("alice", 0);
        fs::create_dir_all(&s.home).unwrap();
        fs::write(
            s.home.join(".claude.json"),
            serde_json::to_vec(&json!({"numStartups": 3, "oauthAccount": alice, "projects": {}}))
                .unwrap(),
        )
        .unwrap();
        keychain
            .write(
                claude::SERVICE,
                &claude::user(),
                &serde_json::to_vec(&json!({"claudeAiOauth": alice_token, "mcpOAuth": {"x": 1}}))
                    .unwrap(),
            )
            .unwrap();
        let alice_id = match live(&s, kind) {
            Live::Unknown(identity) => {
                assert_eq!(identity.email.as_deref(), Some("alice@example.com"));
                assert_eq!(identity.plan.as_deref(), Some("max"));
                identity.id
            }
            other => panic!("{other:?}"),
        };

        // 加 bob：登着的是别人，不换。
        let (bob, bob_token) = claude_login("bob", 0);
        let bob_login = claude::bundle(&bob, &bob_token).unwrap();
        let added = add(&s.engine, &s.home, kind, &bob_login).unwrap();
        assert_eq!(added.email.as_deref(), Some("bob@example.com"));
        assert_eq!(profile()["oauthAccount"], alice);
        assert_eq!(secure()["claudeAiOauth"], alice_token);

        // 切到 bob：alice 先存下，令牌只换 `claudeAiOauth`，`.claude.json` 只换 `oauthAccount`。
        switch(&s.engine, &s.home, kind, &added.id).unwrap();
        assert_eq!(
            secure(),
            json!({"claudeAiOauth": bob_token, "mcpOAuth": {"x": 1}})
        );
        assert_eq!(
            profile(),
            json!({"numStartups": 3, "oauthAccount": bob, "projects": {}})
        );
        assert_eq!(live(&s, kind), Live::Known(added.id.clone()));
        let stored = |id: &str| -> Value {
            serde_json::from_slice(&claude::read_stored(&keychain, id).unwrap().unwrap()).unwrap()
        };
        assert_eq!(stored(&alice_id)["claudeAiOauth"], alice_token);
        assert!(
            !s.engine
                .data()
                .account_logins()
                .join("claude-code")
                .exists()
        );

        // Claude Code 换了 bob 的令牌；切回 alice 时存下新的。
        let (_, bob_fresh) = claude_login("bob", 1);
        keychain
            .write(
                claude::SERVICE,
                &claude::user(),
                &serde_json::to_vec(&json!({"claudeAiOauth": bob_fresh, "mcpOAuth": {"x": 1}}))
                    .unwrap(),
            )
            .unwrap();
        switch(&s.engine, &s.home, kind, &alice_id).unwrap();
        assert_eq!(stored(&added.id)["claudeAiOauth"], bob_fresh);
        assert_eq!(secure()["claudeAiOauth"], alice_token);
        assert_eq!(profile()["oauthAccount"], alice);

        // 删掉 bob：钥匙串里存的那条也删，登着的不动。
        remove(&s.engine, &s.home, kind, &added.id).unwrap();
        assert_eq!(claude::read_stored(&keychain, &added.id).unwrap(), None);
        assert!(matches!(
            switch(&s.engine, &s.home, kind, &added.id),
            Err(AccountError::Missing)
        ));
        assert_eq!(list(s.engine.data()).unwrap().len(), 1);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn claude_token_in_a_file_stays_in_the_file() {
        let s = setup();
        let kind = AgentKind::ClaudeCode;
        let (alice, alice_token) = claude_login("alice", 0);
        let file = s.home.join(".claude/.credentials.json");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(
            &file,
            serde_json::to_vec(&json!({"claudeAiOauth": alice_token})).unwrap(),
        )
        .unwrap();
        fs::write(
            s.home.join(".claude.json"),
            serde_json::to_vec(&json!({"oauthAccount": alice})).unwrap(),
        )
        .unwrap();
        let alice_id = match live(&s, kind) {
            Live::Unknown(identity) => identity.id,
            other => panic!("{other:?}"),
        };
        // 给登着的起名字：先存下它。
        let named = label(&s.engine, &s.home, kind, &alice_id, "工作号", "").unwrap();
        assert_eq!(named.name.as_deref(), Some("工作号"));

        let (bob, bob_token) = claude_login("bob", 0);
        let bob = add(
            &s.engine,
            &s.home,
            kind,
            &claude::bundle(&bob, &bob_token).unwrap(),
        )
        .unwrap();
        switch(&s.engine, &s.home, kind, &bob.id).unwrap();
        let secure: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        assert_eq!(secure["claudeAiOauth"], bob_token);
        assert_eq!(
            Keychain::for_home(&s.home)
                .read(claude::SERVICE, &claude::user())
                .unwrap(),
            None
        );
    }

    // ── 每种存储方式跑同一组 ─────────────────────────

    /// 一份登录：`user` 决定是谁，`refresh` 是第几次换的令牌（越大越新）。
    fn login_of(kind: AgentKind, user: &str, refresh: u32) -> Vec<u8> {
        match kind {
            AgentKind::Codex => codex_auth(user, "ws", refresh),
            AgentKind::GrokBuild => json!({
                "https://auth.x.ai::client": {"user_id": user, "team_id": "t", "email": format!("{user}@example.com"),
                    "key": format!("k-{user}-{refresh}"), "expires_at": format!("2026-10-{:02}T00:00:00Z", 1 + refresh)},
            })
            .to_string()
            .into_bytes(),
            AgentKind::ClaudeCode => claude::bundle(
                &json!({"accountUuid": format!("uuid-{user}"), "organizationUuid": "org", "emailAddress": format!("{user}@example.com")}),
                &json!({"accessToken": format!("at-{user}-{refresh}"), "expiresAt": 1000 + i64::from(refresh)}),
            )
            .unwrap(),
            other => unreachable!("{other} has no official login"),
        }
    }

    /// agent 自己登录、换了令牌：照它的存储方式写进登录文件（和钥匙串）。
    fn log_in(s: &Setup, store: &dyn Storage, login: &[u8]) {
        store.put_secret(&s.home, login).unwrap();
        let file = store.live_file(&s.home).path;
        let pre = fsx::read_optional(&file).unwrap();
        let next = store.put_file(&s.home, pre.as_deref(), login).unwrap();
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, next).unwrap();
    }

    fn logged_in(s: &Setup, store: &dyn Storage) -> Option<Vec<u8>> {
        let file = fsx::read_optional(&store.live_file(&s.home).path).unwrap();
        store.read_live(&s.home, file.as_deref()).unwrap()
    }

    #[test]
    fn every_storage_switches_and_keeps_what_it_replaced() {
        for kind in AgentKind::ALL.into_iter().filter(|kind| supported(*kind)) {
            let s = setup();
            let store = storage(kind).unwrap();
            let id = |login: &[u8]| identify(kind, login).unwrap().id;
            let stored = |id: &str| store.read_stored(s.engine.data(), &s.home, id).unwrap();
            let login = |user: &str, refresh: u32| login_of(kind, user, refresh);
            let (alice, bob) = (login("alice", 0), login("bob", 0));

            // 没人登着时加 alice：登录文件也写上。
            add(&s.engine, &s.home, kind, &alice).unwrap();
            assert_eq!(logged_in(&s, store).unwrap(), alice, "{kind}");
            assert_eq!(live(&s, kind), Live::Known(id(&alice)), "{kind}");
            // 加 bob：登着的是别人，登录文件不动。
            let added = add(&s.engine, &s.home, kind, &bob).unwrap();
            assert_eq!(added.id, id(&bob), "{kind}");
            assert_eq!(logged_in(&s, store).unwrap(), alice, "{kind}");
            // agent 给 alice 换了令牌：读快照时存回去（令牌在钥匙串里的读快照时不读，切走时才存）。
            log_in(&s, store, &login("alice", 1));
            assert_eq!(live(&s, kind), Live::Known(id(&alice)), "{kind}");
            let kept = if store.secret() { 0 } else { 1 };
            assert_eq!(stored(&id(&alice)).unwrap(), login("alice", kept), "{kind}");
            // 重新登录登着的 alice：存着的换掉，登录文件也换。
            let alice = login("alice", 2);
            add(&s.engine, &s.home, kind, &alice).unwrap();
            assert_eq!(stored(&id(&alice)).unwrap(), alice, "{kind}");
            assert_eq!(logged_in(&s, store).unwrap(), alice, "{kind}");
            // 切到 bob：alice 存下来。
            switch(&s.engine, &s.home, kind, &id(&bob)).unwrap();
            assert_eq!(logged_in(&s, store).unwrap(), bob, "{kind}");
            assert_eq!(stored(&id(&alice)).unwrap(), alice, "{kind}");
            assert_eq!(live(&s, kind), Live::Known(id(&bob)), "{kind}");
            // agent 给 bob 换了令牌：切走时存下新的，切回来用新的。
            let bob_later = login("bob", 1);
            log_in(&s, store, &bob_later);
            switch(&s.engine, &s.home, kind, &id(&alice)).unwrap();
            assert_eq!(logged_in(&s, store).unwrap(), alice, "{kind}");
            assert_eq!(stored(&id(&bob)).unwrap(), bob_later, "{kind}");
            switch(&s.engine, &s.home, kind, &id(&bob)).unwrap();
            assert_eq!(logged_in(&s, store).unwrap(), bob_later, "{kind}");
            // 起名字、删掉：登着的不动，存下的那份删掉，登着的成了没存过的。
            label(&s.engine, &s.home, kind, &id(&alice), "工作号", "").unwrap();
            remove(&s.engine, &s.home, kind, &id(&bob)).unwrap();
            assert_eq!(stored(&id(&bob)), None, "{kind}");
            assert_eq!(logged_in(&s, store).unwrap(), bob_later, "{kind}");
            assert!(
                matches!(live(&s, kind), Live::Unknown(found) if found.id == id(&bob)),
                "{kind}"
            );
            let book = list(s.engine.data()).unwrap();
            assert_eq!(book.len(), 1, "{kind}");
            assert_eq!(book[0].name.as_deref(), Some("工作号"), "{kind}");
            assert!(matches!(
                switch(&s.engine, &s.home, kind, &id(&bob)),
                Err(AccountError::Missing)
            ));
        }
    }

    /// 插进去的一步：第几次，家目录。
    type Then = dyn Fn(usize, &Path) + Sync;

    /// 包一层存储方式：读登录（`read_live`）、换钥匙串（`put_secret`）以后跑一下，模拟 agent 这时候改了登录。
    struct Hooked {
        inner: &'static dyn Storage,
        reads: std::sync::atomic::AtomicUsize,
        after_read: Box<Then>,
        after_secret: Box<Then>,
    }

    impl Hooked {
        fn after_read(
            inner: &'static dyn Storage,
            then: impl Fn(usize, &Path) + Sync + 'static,
        ) -> Self {
            Self {
                inner,
                reads: Default::default(),
                after_read: Box::new(then),
                after_secret: Box::new(|_, _| ()),
            }
        }

        fn after_secret(
            inner: &'static dyn Storage,
            then: impl Fn(usize, &Path) + Sync + 'static,
        ) -> Self {
            Self {
                after_read: Box::new(|_, _| ()),
                after_secret: Box::new(then),
                ..Self::after_read(inner, |_, _| ())
            }
        }
    }

    impl Storage for Hooked {
        fn kind(&self) -> AgentKind {
            self.inner.kind()
        }
        fn storable(&self) -> bool {
            self.inner.storable()
        }
        fn identify(&self, login: &Value) -> Option<Identity> {
            self.inner.identify(login)
        }
        fn live_file(&self, home: &Path) -> LiveFile {
            self.inner.live_file(home)
        }
        fn in_file(&self, file: &[u8]) -> Option<Found> {
            self.inner.in_file(file)
        }
        fn read_live(
            &self,
            home: &Path,
            file: Option<&[u8]>,
        ) -> Result<Option<Vec<u8>>, EngineError> {
            let live = self.inner.read_live(home, file);
            let n = self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            (self.after_read)(n, home);
            live
        }
        fn secret(&self) -> bool {
            self.inner.secret()
        }
        fn put_secret(&self, home: &Path, login: &[u8]) -> Result<(), EngineError> {
            self.inner.put_secret(home, login)?;
            (self.after_secret)(0, home);
            Ok(())
        }
        fn put_file(
            &self,
            home: &Path,
            file: Option<&[u8]>,
            login: &[u8],
        ) -> Result<Vec<u8>, EngineError> {
            self.inner.put_file(home, file, login)
        }
        fn read_stored(
            &self,
            data: &DataDir,
            home: &Path,
            id: &str,
        ) -> Result<Option<Vec<u8>>, EngineError> {
            self.inner.read_stored(data, home, id)
        }
        fn write_stored(
            &self,
            data: &DataDir,
            home: &Path,
            id: &str,
            login: &[u8],
        ) -> Result<(), EngineError> {
            self.inner.write_stored(data, home, id, login)
        }
        fn delete_stored(&self, data: &DataDir, home: &Path, id: &str) -> Result<(), EngineError> {
            self.inner.delete_stored(data, home, id)
        }
        fn blocked(&self, config: Option<&[u8]>) -> bool {
            self.inner.blocked(config)
        }
        fn program(&self) -> &'static str {
            self.inner.program()
        }
        fn login_command(&self, command: &mut Command, dir: &Path) -> bool {
            self.inner.login_command(command, dir)
        }
        fn read_login(&self, dir: &Path, keychain: &Keychain) -> Option<Vec<u8>> {
            self.inner.read_login(dir, keychain)
        }
        fn forget_login(&self, dir: &Path, keychain: &Keychain) {
            self.inner.forget_login(dir, keychain)
        }
    }

    /// 读完登录文件、写之前 agent 换了令牌：从头再来，换下来的是新令牌。
    #[test]
    fn a_login_that_changes_while_switching_is_kept() {
        let s = setup();
        let (alice, alice_later, bob) = (
            codex_auth("alice", "ws", 0),
            codex_auth("alice", "ws", 1),
            codex_auth("bob", "ws", 0),
        );
        s.set_auth(&alice);
        add(&s.engine, &s.home, AgentKind::Codex, &bob).unwrap();
        let later = alice_later.clone();
        let store = Hooked::after_read(&files::CODEX, move |n, home| {
            if n == 0 {
                fs::write(home.join(".codex/auth.json"), &later).unwrap();
            }
        });
        switch_with(&s.engine, &store, &s.home, &id_of(&bob)).unwrap();
        assert_eq!(s.auth().unwrap(), bob);
        assert_eq!(s.stored(&id_of(&alice)), alice_later);
    }

    /// agent 一直在改登录文件：试几次就不试了，报冲突，登录文件留着 agent 最后写的。
    #[test]
    fn switching_gives_up_when_the_login_keeps_changing() {
        let s = setup();
        let bob = codex_auth("bob", "ws", 0);
        s.set_auth(&codex_auth("alice", "ws", 0));
        add(&s.engine, &s.home, AgentKind::Codex, &bob).unwrap();
        let store = Hooked::after_read(&files::CODEX, |n, home| {
            let later = codex_auth("alice", "ws", n as u32 + 1);
            fs::write(home.join(".codex/auth.json"), later).unwrap();
        });
        assert!(matches!(
            switch_with(&s.engine, &store, &s.home, &id_of(&bob)),
            Err(AccountError::Engine(EngineError::Conflict { .. }))
        ));
        assert_eq!(
            s.auth().unwrap(),
            codex_auth("alice", "ws", ATTEMPTS as u32)
        );
    }

    /// Claude Code：读完登录、换了钥匙串以后 `~/.claude.json` 又被改了（agent 改了邮箱）。文件照样换，令牌和人对得上，
    /// 不会把要切的那个的令牌存到原来那个人名下。
    #[cfg(target_os = "macos")]
    #[test]
    fn claude_token_and_profile_stay_together_while_switching() {
        let s = setup();
        let kind = AgentKind::ClaudeCode;
        let store = storage(kind).unwrap();
        let id = |login: &[u8]| identify(kind, login).unwrap().id;
        let (alice, bob) = (login_of(kind, "alice", 0), login_of(kind, "bob", 0));
        log_in(&s, store, &alice);
        add(&s.engine, &s.home, kind, &bob).unwrap();
        let hooked = Hooked::after_read(store, |n, home| {
            if n == 0 {
                let path = home.join(".claude.json");
                let mut profile: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                profile["oauthAccount"]["emailAddress"] = json!("alice@new.example");
                fs::write(&path, serde_json::to_vec(&profile).unwrap()).unwrap();
            }
        });
        switch_with(&s.engine, &hooked, &s.home, &id(&bob)).unwrap();
        assert_eq!(logged_in(&s, store).unwrap(), bob);
        switch(&s.engine, &s.home, kind, &id(&alice)).unwrap();
        assert_eq!(logged_in(&s, store).unwrap(), alice);
    }

    /// Claude Code 钥匙串里那条的令牌。
    #[cfg(target_os = "macos")]
    fn claude_token(s: &Setup) -> Value {
        let keychain = Keychain::for_home(&s.home);
        let secure = keychain.read(claude::SERVICE, &claude::user()).unwrap();
        serde_json::from_slice::<Value>(&secure.unwrap()).unwrap()["claudeAiOauth"].clone()
    }

    /// Claude Code：`~/.claude.json` 解析不了时切不了，钥匙串里的令牌也不动
    /// （不然令牌换成了要切的、文件还是原来的人，原来的令牌也没存下）。
    #[cfg(target_os = "macos")]
    #[test]
    fn claude_keeps_the_token_when_the_profile_is_broken() {
        let s = setup();
        let kind = AgentKind::ClaudeCode;
        let store = storage(kind).unwrap();
        log_in(&s, store, &login_of(kind, "alice", 0));
        let bob = add(&s.engine, &s.home, kind, &login_of(kind, "bob", 0)).unwrap();
        let token = claude_token(&s);
        fs::write(store.live_file(&s.home).path, "{ nope").unwrap();
        assert!(switch(&s.engine, &s.home, kind, &bob.id).is_err());
        assert_eq!(claude_token(&s), token);
        let carol = login_of(kind, "carol", 0);
        assert!(add(&s.engine, &s.home, kind, &carol).is_err());
        assert_eq!(claude_token(&s), token);
    }

    /// Claude Code：钥匙串换了以后登录文件没写成（这期间被改坏了）：令牌换回原来的。
    #[cfg(target_os = "macos")]
    #[test]
    fn claude_puts_the_token_back_when_the_profile_is_not_written() {
        let s = setup();
        let kind = AgentKind::ClaudeCode;
        let store = storage(kind).unwrap();
        log_in(&s, store, &login_of(kind, "alice", 0));
        let bob = add(&s.engine, &s.home, kind, &login_of(kind, "bob", 0)).unwrap();
        let token = claude_token(&s);
        let hooked = Hooked::after_secret(store, |_, home| {
            fs::write(home.join(".claude.json"), "{ nope").unwrap();
        });
        assert!(switch_with(&s.engine, &hooked, &s.home, &bob.id).is_err());
        assert_eq!(claude_token(&s), token);
    }

    /// Claude Code：`~/.claude.json` 里是别人，钥匙串里却没有令牌（只有一半，不算登着）。加账号时两样都换成新登录的。
    #[cfg(target_os = "macos")]
    #[test]
    fn claude_adding_over_half_a_login_writes_both_halves() {
        let s = setup();
        let kind = AgentKind::ClaudeCode;
        let store = storage(kind).unwrap();
        let (carol, alice) = (login_of(kind, "carol", 0), login_of(kind, "alice", 0));
        let file = store.live_file(&s.home).path;
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, store.put_file(&s.home, None, &carol).unwrap()).unwrap();
        assert_eq!(logged_in(&s, store), None);
        let added = add(&s.engine, &s.home, kind, &alice).unwrap();
        assert_eq!(logged_in(&s, store).unwrap(), alice);
        assert_eq!(live(&s, kind), Live::Known(added.id));
    }

    /// 登录文件读不了：不知道登的是谁（不是「未登录」），原因记下来。
    #[test]
    fn an_unreadable_login_file_is_an_error() {
        for kind in [AgentKind::GrokBuild, AgentKind::ClaudeCode] {
            let s = setup();
            // 路径是个目录：读的时候报错。
            fs::create_dir_all(storage(kind).unwrap().live_file(&s.home).path).unwrap();
            let accounts = read(&s, kind);
            assert!(accounts.error.is_some(), "{kind}");
            assert!(!accounts.live.contains_key(&kind), "{kind}");
        }
    }
}
