//! 官方账号多账号（10-13，阶段 4）：Codex、Grok Build、Claude Code（10-13 后加，只在 macOS）各存几个官方登录，
//! 切换时把选中的那个写回 agent 的登录。Claude Code 的登录不是一个文件（令牌在钥匙串里），见 [`claude`]。
//!
//! - **存在哪**：`~/.confdeck/accounts.json` 记有哪些账号（[`Account`]），每个账号的登录文件原样存在
//!   `~/.confdeck/accounts/<agent>/<id>.json`（只有自己能读；Claude Code 的存在钥匙串里）。不同步、不导出。
//! - **登录**（[`Login`]）：不自己做 OAuth，用 agent 自己的 `login` 命令，把它的家目录（`CODEX_HOME`、`GROK_HOME`）
//!   指到一个临时目录；登录完读那里的 `auth.json` 存成账号（[`add`]），删掉临时目录。
//! - **换下来的登录先存好**：agent 用着用着会换新令牌（刷新令牌用一次就作废），所以切换（[`switch`]）时先把登录文件
//!   现在的内容存回它所属的账号（比存着的新才存；不认识的账号存成新账号），再写入要切的账号。每次读快照时（`read_all`）也这样存。
//! - **认账号**（[`identify`]）：Codex 看 `id_token` 里的用户和工作区（同一个人的个人、团队工作区算两个），
//!   Grok 看 `user_id` 和 `team_id`。账号 ID 是这两样的 hash。
//! - **不换令牌**：confdeck 不用刷新令牌（同「订阅额度」）。存着不用的账号放久了令牌可能失效，到时候重新登录。
//! - **名字和备注**（[`label`]，10-13 提案 13）：用户自己起的，只存在 `accounts.json`，不碰登录文件。

pub mod claude;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::{AgentKind, Disk};
use crate::data::DataDir;
use crate::engine::{Engine, EngineError, LiveFile, Outcome};
use crate::fsx::{self, Digest, Mode};
use crate::keychain::Keychain;
use crate::patch::Format;
use crate::quota::{Check, CheckKey};

/// 能存多个官方账号的 agent。
pub const SUPPORTED: [AgentKind; 3] = [
    AgentKind::Codex,
    AgentKind::GrokBuild,
    AgentKind::ClaudeCode,
];

/// Claude Code 的只在 macOS（令牌在钥匙串里；别的系统没做）。
pub fn supported(kind: AgentKind) -> bool {
    SUPPORTED.contains(&kind) && (kind != AgentKind::ClaudeCode || cfg!(target_os = "macos"))
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

/// 一个登录文件是谁的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// 账号 ID，同 [`Account::id`]。
    pub id: String,
    pub email: Option<String>,
    pub plan: Option<String>,
    /// 这份登录有多新（毫秒）：Codex 的 `last_refresh`，Grok 的 `expires_at`。
    fresh: Option<i64>,
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

/// agent 的登录文件。
pub fn login_file(home: &Path, kind: AgentKind) -> Option<LiveFile> {
    let path = match kind {
        AgentKind::Codex => home.join(".codex").join("auth.json"),
        AgentKind::GrokBuild => home.join(".grok").join("auth.json"),
        _ => return None,
    };
    Some(LiveFile {
        owner: kind.id(),
        path,
        mode: Mode::Private,
        format: Format::Json,
    })
}

/// Codex 的登录存在钥匙串里（`config.toml` 的 `cli_auth_credentials_store` 不是 `file`）时，换 `auth.json` 没用。
/// confdeck 不读钥匙串，这时不让切。`config` 是读好的 `config.toml`（没有是 `None`）。
fn codex_blocked(config: Option<&[u8]>) -> bool {
    let Some(text) = config.and_then(|bytes| std::str::from_utf8(bytes).ok()) else {
        return false;
    };
    let Ok(doc) = text.parse::<toml_edit::DocumentMut>() else {
        return false;
    };
    doc.get("cli_auth_credentials_store")
        .is_some_and(|store| store.as_str() != Some("file"))
}

/// Claude Code 登着的官方账号的资料。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub email: Option<String>,
    /// `max`、`pro`、`team`……
    pub plan: Option<String>,
}

/// Claude Code 登着的官方账号的资料：`~/.claude.json` 的 `oauthAccount`（邮箱 `emailAddress`；`organizationType` 去掉 `claude_`
/// 是套餐）。这里只有资料，凭证在钥匙串里，confdeck 不读。
fn profile_of(account: &Value) -> Option<Profile> {
    account.as_object()?;
    let text = |key: &str| {
        account[key]
            .as_str()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
    };
    Some(Profile {
        email: text("emailAddress"),
        plan: text("organizationType").and_then(|kind| {
            kind.strip_prefix("claude_")
                .filter(|plan| !plan.is_empty())
                .map(str::to_string)
        }),
    })
}

/// 认出一个登录文件是谁的；没有登录（Codex 只有 API Key 的也算）是 `None`。
pub fn identify(kind: AgentKind, bytes: &[u8]) -> Option<Identity> {
    let json: Value = serde_json::from_slice(bytes).ok()?;
    let text = |value: &Value| {
        value
            .as_str()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    let (key, email, plan, fresh) = match kind {
        AgentKind::Codex => {
            let tokens = &json["tokens"];
            let claims = tokens["id_token"]
                .as_str()
                .and_then(jwt_claims)
                .unwrap_or(Value::Null);
            let auth = &claims["https://api.openai.com/auth"];
            let workspace =
                text(&auth["chatgpt_account_id"]).or_else(|| text(&tokens["account_id"]))?;
            let user = text(&auth["chatgpt_user_id"])
                .or_else(|| text(&auth["user_id"]))
                .or_else(|| text(&claims["sub"]))
                .unwrap_or_default();
            (
                format!("{user}:{workspace}"),
                text(&claims["email"]),
                text(&auth["chatgpt_plan_type"]),
                json["last_refresh"].as_str().and_then(millis),
            )
        }
        AgentKind::GrokBuild => {
            // 按登录的范围分条，同「订阅额度」：`https://auth.x.ai::` 开头的那条，没有再用老的 `…/sign-in`。
            let entries = json.as_object()?;
            let entry = entries
                .iter()
                .find(|(scope, _)| scope.starts_with("https://auth.x.ai::"))
                .or_else(|| entries.iter().find(|(scope, _)| scope.contains("/sign-in")))
                .or_else(|| entries.iter().next())?
                .1;
            let user = text(&entry["user_id"])?;
            let team = text(&entry["team_id"]).unwrap_or_default();
            (
                format!("{user}:{team}"),
                text(&entry["email"]),
                None,
                entry["expires_at"].as_str().and_then(millis),
            )
        }
        AgentKind::ClaudeCode => return claude::identify(&json),
        _ => return None,
    };
    let digest = Digest::of(format!("{}:{key}", kind.id()).as_bytes()).0;
    Some(Identity {
        id: digest[..16].to_string(),
        email,
        plan,
        fresh,
    })
}

fn millis(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|time| time.timestamp_millis())
}

/// JWT 中间那段（base64url）解出来的 JSON。不验签名：只用来认是谁。
fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    serde_json::from_slice(&base64url(payload)?).ok()
}

fn base64url(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' => break,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(out)
}

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

fn login_path(data: &DataDir, kind: AgentKind, id: &str) -> PathBuf {
    data.account_logins()
        .join(kind.id())
        .join(format!("{id}.json"))
}

/// 存下的一个账号的登录（和登录文件一个格式），查这个账号的额度用。
pub fn stored_login(data: &DataDir, kind: AgentKind, id: &str) -> Result<Vec<u8>, AccountError> {
    let path = login_path(data, kind, id);
    match fs::read(&path) {
        Ok(bytes) => Ok(bytes),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Err(AccountError::Missing),
        Err(err) => Err(io_error(&path)(err).into()),
    }
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

/// 存下的一个账号的登录：Codex、Grok 是 `accounts/<agent>/<id>.json`，Claude Code 的在钥匙串里（[`claude::STORE`]）。
fn read_stored(
    data: &DataDir,
    home: &Path,
    kind: AgentKind,
    id: &str,
) -> Result<Option<Vec<u8>>, EngineError> {
    let path = login_path(data, kind, id);
    if kind == AgentKind::ClaudeCode {
        return claude::read_stored(&Keychain::for_home(home), id).map_err(io_error(&path));
    }
    fsx::read_optional(&path).map_err(io_error(&path))
}

fn write_stored(
    data: &DataDir,
    home: &Path,
    kind: AgentKind,
    id: &str,
    bytes: &[u8],
) -> Result<(), EngineError> {
    let path = login_path(data, kind, id);
    if kind == AgentKind::ClaudeCode {
        return claude::write_stored(&Keychain::for_home(home), id, bytes).map_err(io_error(&path));
    }
    private_dir(&data.account_logins())?;
    let dir = path.parent().expect("login path has a parent");
    private_dir(dir)?;
    fsx::write_atomic(&path, bytes, Mode::Private).map_err(io_error(&path))
}

/// 把一份登录存进它的账号，返回写了没有（存着的登录、`accounts.json` 有一样变了）。调用的人拿着引擎的锁。
fn keep(
    data: &DataDir,
    home: &Path,
    kind: AgentKind,
    identity: &Identity,
    bytes: &[u8],
    how: Keep,
) -> Result<bool, EngineError> {
    let mut book = load(data)?;
    let mut changed = false;
    match book
        .accounts
        .iter_mut()
        .find(|account| account.agent == kind && account.id == identity.id)
    {
        None if how == Keep::Known => return Ok(false),
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
            changed = true;
        }
        Some(account) => {
            for (field, value) in [
                (&mut account.email, &identity.email),
                (&mut account.plan, &identity.plan),
            ] {
                if value.is_some() && field != value {
                    field.clone_from(value);
                    changed = true;
                }
            }
        }
    }
    let stored = read_stored(data, home, kind, &identity.id)?;
    let replace = replaces(kind, identity, stored.as_deref(), bytes, how);
    if replace {
        write_stored(data, home, kind, &identity.id, bytes)?;
    }
    if changed {
        save(data, &book)?;
    }
    Ok(replace || changed)
}

/// 存着的登录 `stored` 要不要换成 `bytes`（是 `identity` 的）：一样的不换，旧的不换（刚登录的除外）。
fn replaces(
    kind: AgentKind,
    identity: &Identity,
    stored: Option<&[u8]>,
    bytes: &[u8],
    how: Keep,
) -> bool {
    match stored {
        None => true,
        Some(old) if old == bytes => false,
        _ if how == Keep::Fresh => true,
        // 分不出新旧时信登录文件：那是 agent 最后写的。
        Some(old) => match (
            identity.fresh,
            identify(kind, old).and_then(|old| old.fresh),
        ) {
            (Some(new), Some(old)) => new >= old,
            (None, Some(_)) => false,
            _ => true,
        },
    }
}

/// 登录文件里认出来的登录：是谁，和文件原样的内容（Claude Code 的令牌在钥匙串里，没有内容）。
struct Found {
    identity: Identity,
    bytes: Option<Vec<u8>>,
}

/// Codex、Grok 的登录文件里是谁；没有文件、认不出是 `None`。
fn read_login(home: &Path, kind: AgentKind) -> Result<Option<Found>, AccountError> {
    let file = login_file(home, kind).ok_or(AccountError::Unsupported)?;
    let bytes = fsx::read_optional(&file.path).map_err(io_error(&file.path))?;
    Ok(found_in(kind, bytes))
}

/// [`read_login`]，文件已经读好了。
fn found_in(kind: AgentKind, bytes: Option<Vec<u8>>) -> Option<Found> {
    let bytes = bytes?;
    identify(kind, &bytes).map(|identity| Found {
        identity,
        bytes: Some(bytes),
    })
}

/// 登录文件里认出来的是存过的账号，就把文件里的登录存回去（比存着的新才存；没存过的不存）。`accounts` 是读好的存下的账号。
/// 没有要存的（一样，或者存着的更新）时不拿锁、不再读 `accounts.json`。第二项是存回去了没有（存了的话 `accounts.json` 里的邮箱、套餐可能变了）。
fn sync_found(
    engine: &Engine,
    home: &Path,
    kind: AgentKind,
    accounts: &[Account],
    found: Found,
) -> Result<(Live, bool), AccountError> {
    let Found { identity, bytes } = found;
    let Some(account) = accounts
        .iter()
        .find(|account| account.agent == kind && account.id == identity.id)
    else {
        return Ok((Live::Unknown(identity), false));
    };
    let Some(bytes) = bytes else {
        return Ok((Live::Known(identity.id), false));
    };
    let data = engine.data();
    let profile_changed = [
        (&account.email, &identity.email),
        (&account.plan, &identity.plan),
    ]
    .into_iter()
    .any(|(field, value)| value.is_some() && field != value);
    let stored = read_stored(data, home, kind, &identity.id)?;
    let differs =
        profile_changed || replaces(kind, &identity, stored.as_deref(), &bytes, Keep::Known);
    let mut kept = false;
    if differs {
        let _lock = engine.lock()?;
        kept = keep(data, home, kind, &identity, &bytes, Keep::Known)?;
    }
    Ok((Live::Known(identity.id), kept))
}

/// 一次快照里的官方账号（[`crate::status`]）。
#[derive(Debug, Default)]
pub struct Accounts {
    /// 存下的全部账号，先存的在前。
    pub list: Vec<Account>,
    /// 装了的、能存多个账号的 agent 的登录文件现在是谁。
    pub live: HashMap<AgentKind, Live>,
    /// Codex 的登录存在钥匙串里（`config.toml` 的 `cli_auth_credentials_store` 不是 `file`）：切不了。
    pub blocked: HashSet<AgentKind>,
    /// Claude Code 登着的官方账号的资料。存不了多个 Claude 账号（不是 macOS）时界面用它。
    pub claude: Option<Profile>,
    /// `accounts.json` 读不了、登录存不回去的原因（有好几个时是最后一个）。
    pub error: Option<AccountError>,
}

/// 读官方账号：每个装了的、能存多个账号的 agent 的登录文件现在是谁；是存过的账号，顺带把 agent 新换的令牌存回去。
/// `installed` 是装了的 agent；`codex` 是读好的 Codex `config.toml`（看登录是不是在钥匙串里）和 `auth.json`（登录文件）。
/// `accounts.json`、`~/.claude.json` 各读一次。Claude Code 的只看 `~/.claude.json` 是谁，不读钥匙串：
/// 它的令牌在切走的时候存（[`switch`]）。
pub(crate) fn read_all(
    engine: &Engine,
    home: &Path,
    installed: &[AgentKind],
    codex: Disk,
) -> Accounts {
    let mut accounts = Accounts::default();
    let claude = installed
        .contains(&AgentKind::ClaudeCode)
        .then(|| claude::read_account(home))
        .flatten();
    accounts.claude = claude.as_ref().and_then(profile_of);
    let book = list(engine.data());
    let mut saved = false;
    for kind in SUPPORTED {
        if !supported(kind) || !installed.contains(&kind) {
            continue;
        }
        if kind == AgentKind::Codex && codex_blocked(codex.config) {
            accounts.blocked.insert(kind);
            continue;
        }
        let Ok(book) = &book else {
            continue;
        };
        let found = match kind {
            AgentKind::ClaudeCode => {
                Ok(claude
                    .as_ref()
                    .and_then(claude::live_identity)
                    .map(|identity| Found {
                        identity,
                        bytes: None,
                    }))
            }
            AgentKind::Codex => Ok(found_in(kind, codex.login.map(<[u8]>::to_vec))),
            _ => read_login(home, kind),
        };
        let live = found.and_then(|found| match found {
            None => Ok((Live::LoggedOut, false)),
            Some(found) => sync_found(engine, home, kind, book, found),
        });
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

/// 切到一个存下的账号：先把登录文件现在的内容存好（没存过的账号存成新账号），再写入这个账号的登录。
pub fn switch(
    engine: &Engine,
    home: &Path,
    kind: AgentKind,
    id: &str,
) -> Result<Outcome, AccountError> {
    if kind == AgentKind::ClaudeCode && supported(kind) {
        return switch_claude(engine, home, id);
    }
    let file = login_file(home, kind).ok_or(AccountError::Unsupported)?;
    let data = engine.data().clone();
    let path = login_path(&data, kind, id);
    if !path.is_file() {
        return Err(AccountError::Missing);
    }
    let outcome = engine.write_login(&file, &mut |live| {
        if let Some(live) = live
            && let Some(identity) = identify(kind, live)
        {
            keep(&data, home, kind, &identity, live, Keep::Any)?;
        }
        // 要切的就是现在这个：上面可能刚把更新的登录存进去。
        fs::read(&path).map_err(io_error(&path))
    })?;
    Ok(outcome)
}

/// Claude Code 的切换：在改 `~/.claude.json` 的锁里读出现在登着的（钥匙串加 `oauthAccount`）存好，再换成要切的。
fn switch_claude(engine: &Engine, home: &Path, id: &str) -> Result<Outcome, AccountError> {
    let kind = AgentKind::ClaudeCode;
    let data = engine.data().clone();
    if read_stored(&data, home, kind, id)?.is_none() {
        return Err(AccountError::Missing);
    }
    let keychain = Keychain::for_home(home);
    let outcome = engine.write_login(&claude::profile_file(home), &mut |pre| {
        if let Some(live) = claude::read_live(pre, home, &keychain)?
            && let Some(identity) = identify(kind, &live)
        {
            keep(&data, home, kind, &identity, &live, Keep::Any)?;
        }
        let path = login_path(&data, kind, id);
        let login = read_stored(&data, home, kind, id)?
            .ok_or_else(|| io_error(&path)(io::ErrorKind::NotFound.into()))?;
        claude::put_live(home, &keychain, pre, &login)
    })?;
    Ok(outcome)
}

/// 存下一次登录（[`Login::wait`] 读到的）。重新登录存过的账号就换掉它存着的登录；
/// agent 现在没有登录，或者登着的就是这个账号，也写进登录文件。
pub fn add(
    engine: &Engine,
    home: &Path,
    kind: AgentKind,
    bytes: &[u8],
) -> Result<Account, AccountError> {
    if !supported(kind) {
        return Err(AccountError::Unsupported);
    }
    let identity = identify(kind, bytes).ok_or(AccountError::Unrecognized)?;
    let data = engine.data();
    {
        let _lock = engine.lock()?;
        keep(data, home, kind, &identity, bytes, Keep::Fresh)?;
    }
    if kind == AgentKind::ClaudeCode {
        let keychain = Keychain::for_home(home);
        engine.write_login(&claude::profile_file(home), &mut |pre| {
            let other = claude::read_live(pre, home, &keychain)?
                .and_then(|live| identify(kind, &live))
                .is_some_and(|live| live.id != identity.id);
            match pre.filter(|_| other) {
                Some(pre) => Ok(pre.to_vec()),
                None => claude::put_live(home, &keychain, pre, bytes),
            }
        })?;
        return load(data)?
            .accounts
            .into_iter()
            .find(|account| account.agent == kind && account.id == identity.id)
            .ok_or(AccountError::Missing);
    }
    let file = login_file(home, kind).ok_or(AccountError::Unsupported)?;
    engine.write_login(&file, &mut |live| match live
        .filter(|live| identify(kind, live).is_some_and(|live| live.id != identity.id))
    {
        Some(other) => Ok(other.to_vec()),
        None => Ok(bytes.to_vec()),
    })?;
    load(data)?
        .accounts
        .into_iter()
        .find(|account| account.agent == kind && account.id == identity.id)
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
    let data = engine.data();
    let _lock = engine.lock()?;
    let mut book = load(data)?;
    if !book
        .accounts
        .iter()
        .any(|account| account.agent == kind && account.id == id)
    {
        let bytes = if kind == AgentKind::ClaudeCode && supported(kind) {
            let profile = claude::profile_file(home).path;
            let pre = fsx::read_optional(&profile).map_err(io_error(&profile))?;
            claude::read_live(pre.as_deref(), home, &Keychain::for_home(home))?
        } else {
            let file = login_file(home, kind).ok_or(AccountError::Unsupported)?;
            fsx::read_optional(&file.path).map_err(io_error(&file.path))?
        }
        .ok_or(AccountError::Missing)?;
        let identity = identify(kind, &bytes).ok_or(AccountError::Missing)?;
        if identity.id != id {
            return Err(AccountError::Missing);
        }
        keep(data, home, kind, &identity, &bytes, Keep::Any)?;
        book = load(data)?;
    }
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
    let data = engine.data();
    let removed = {
        let _lock = engine.lock()?;
        let mut book = load(data)?;
        let before = book.accounts.len();
        book.accounts
            .retain(|account| !(account.agent == kind && account.id == id));
        if book.accounts.len() == before {
            return Err(AccountError::Missing);
        }
        save(data, &book)?;
        let path = login_path(data, kind, id);
        if kind == AgentKind::ClaudeCode {
            claude::delete_stored(&Keychain::for_home(home), id).map_err(io_error(&path))
        } else {
            match fs::remove_file(&path) {
                Err(err) if err.kind() != io::ErrorKind::NotFound => Err(io_error(&path)(err)),
                _ => Ok(()),
            }
        }
    };
    // 这个账号的额度查询设置不留（同 `apply::delete_provider`）。`set_check` 自己拿锁，放在锁外面。
    let _ = engine.set_check(&CheckKey::Account(kind, id.to_string()), Check::default());
    Ok(removed?)
}

/// agent 的命令行程序。
pub fn program(home: &Path, kind: AgentKind) -> Result<PathBuf, AccountError> {
    let name = match kind {
        AgentKind::Codex => "codex",
        AgentKind::GrokBuild => "grok",
        AgentKind::ClaudeCode if supported(kind) => "claude",
        _ => return Err(AccountError::Unsupported),
    };
    crate::program::find(home, name).ok_or(AccountError::NoProgram(name))
}

/// 输出最多留多少字。
const OUTPUT_LIMIT: usize = 16 * 1024;

/// 登录最多等多久。浏览器那边放着不管，登录命令会一直等下去；过了这么久就停掉它。
const LOGIN_DEADLINE: Duration = Duration::from_secs(15 * 60);

/// 一次在跑的登录：agent 的 `login` 命令，家目录指到 confdeck 数据目录下的一个临时目录。
/// 命令自己打开浏览器；打不开时它会把登录地址打出来（[`LoginHandle::url`]）。
///
/// 临时目录（和 Claude Code 存在钥匙串里的那条临时令牌）谁来收拾：登录完、取消、出错时 `Login` 丢掉时收拾；
/// 应用退出时等不到登录命令退出，调 [`LoginHandle::abandon`]；应用被强退、崩了的，下次启动时 [`clean_stale_logins`] 收拾。
pub struct Login {
    handle: LoginHandle,
    /// Claude Code 的登录命令也等标准输入（贴授权码），开着别关。
    _stdin: Option<ChildStdin>,
}

/// 在别的线程里看登录到哪了、取消它。
#[derive(Clone)]
pub struct LoginHandle {
    child: Arc<Mutex<Child>>,
    output: Arc<Mutex<String>>,
    scratch: Arc<Scratch>,
}

/// 登录用的临时家目录。
struct Scratch {
    kind: AgentKind,
    dir: PathBuf,
    /// Claude Code 登录时存令牌的钥匙串（见 [`claude`]）。
    keychain: Keychain,
}

impl Scratch {
    /// 登录命令存令牌用的钥匙串：目录里有 `.keychain` 的是假的（测试用的登录命令写在那），没有的是系统钥匙串
    /// （真的 `claude`，只在 macOS）。用假的时 [`Login::start`] 先建好 `.keychain`，启动时收拾残留
    /// （[`clean_stale_logins`]）按同一条规则就知道当初用的是哪个。
    fn keychain(dir: &Path) -> Keychain {
        let fake = dir.join(".keychain");
        if cfg!(target_os = "macos") && !fake.exists() {
            Keychain::System
        } else {
            Keychain::Fake(fake)
        }
    }

    /// 删掉临时目录；Claude Code 的连同登录命令可能已经存下的那条临时令牌。
    fn clean(&self) {
        if self.kind == AgentKind::ClaudeCode {
            claude::forget_login(&self.dir, &self.keychain);
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// 临时目录的名字：`.login-<agent>-<进程号>-<第几次>`。进程号用来认出哪些是已经不在的进程留下的（[`clean_stale_logins`]）。
fn scratch_name(kind: AgentKind, pid: u32, run: usize) -> String {
    format!(".login-{}-{pid}-{run}", kind.id())
}

/// [`scratch_name`] 反过来：哪个 agent、哪个进程的。
fn parse_scratch_name(name: &str) -> Option<(AgentKind, u32)> {
    let (rest, run) = name.strip_prefix(".login-")?.rsplit_once('-')?;
    run.parse::<usize>().ok()?;
    let (kind, pid) = rest.rsplit_once('-')?;
    let kind = AgentKind::ALL.into_iter().find(|k| k.id() == kind)?;
    Some((kind, pid.parse().ok()?))
}

/// 启动时收拾上次没收拾完的登录（应用被强退、崩了）：跑登录的那个进程已经不在的临时目录删掉，
/// Claude Code 的连同它可能存下的那条临时令牌。别的 confdeck 进程正在跑的登录不动。
pub fn clean_stale_logins(data: &DataDir) {
    let logins = data.account_logins();
    let Ok(entries) = fs::read_dir(&logins) else {
        return;
    };
    // 登录时用的是解开链接的路径（Claude Code 按它算钥匙串的服务名）。只解开上一层：
    // 临时目录本身是链接的不跟过去（删的会是链接指着的地方），也不是 confdeck 建的，不管。
    let logins = fs::canonicalize(&logins).unwrap_or(logins);
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Some((kind, pid)) = entry.file_name().to_str().and_then(parse_scratch_name) else {
            continue;
        };
        if pid == std::process::id() || process_alive(pid) {
            continue;
        }
        let dir = logins.join(entry.file_name());
        Scratch {
            kind,
            keychain: Scratch::keychain(&dir),
            dir,
        }
        .clean();
    }
}

/// 这个进程号还有进程在（`kill -0`，不发信号）。`kill` 起不来时当它还在：宁可留着目录，也不删别人正在用的。
fn process_alive(pid: u32) -> bool {
    Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_or(true, |status| status.success())
}

impl Login {
    /// 跑 `program`（[`program`] 找到的，测试时可以换成别的）的登录命令。
    pub fn start(program: &Path, data: &DataDir, kind: AgentKind) -> Result<Self, AccountError> {
        static RUNS: AtomicUsize = AtomicUsize::new(0);
        let run = RUNS.fetch_add(1, Ordering::Relaxed);
        let dir = data
            .account_logins()
            .join(scratch_name(kind, std::process::id(), run));
        let _ = fs::remove_dir_all(&dir);
        private_dir(&data.account_logins())?;
        private_dir(&dir)?;
        // Claude Code 按目录的写法算钥匙串的服务名，用解开链接的路径，两边算的一样。
        let dir = fs::canonicalize(&dir).unwrap_or(dir);
        // 真的 `claude` 把令牌存进系统钥匙串（临时的一条，读完就删）；测试用的假命令写文件，先建好它的位置。
        let real =
            cfg!(target_os = "macos") && program.file_name().is_some_and(|name| name == "claude");
        if !real {
            private_dir(&dir.join(".keychain"))?;
        }
        let keychain = Scratch::keychain(&dir);
        let mut command = Command::new(program);
        let mut stdin = Stdio::null();
        match kind {
            // 临时家目录里没有 config.toml，写明存文件，免得存进钥匙串。
            AgentKind::Codex => command
                .args(["login", "-c", "cli_auth_credentials_store=\"file\""])
                .env("CODEX_HOME", &dir),
            AgentKind::GrokBuild => command.args(["login", "--oauth"]).env("GROK_HOME", &dir),
            AgentKind::ClaudeCode if supported(kind) => {
                stdin = Stdio::piped();
                command
                    .args(["auth", "login", "--claudeai"])
                    .env("CLAUDE_CONFIG_DIR", &dir)
                    .env("CLAUDE_SECURESTORAGE_CONFIG_DIR", &dir)
            }
            _ => return Err(AccountError::Unsupported),
        };
        let mut child = command
            .stdin(stdin)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(io_error(program))?;
        let input = child.stdin.take();
        let output = Arc::new(Mutex::new(String::new()));
        let streams: [Option<Box<dyn Read + Send>>; 2] = [
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
        ];
        for stream in streams.into_iter().flatten() {
            let output = output.clone();
            thread::spawn(move || {
                for line in BufReader::new(stream).lines() {
                    let Ok(line) = line else {
                        break;
                    };
                    let mut output = output.lock().unwrap_or_else(PoisonError::into_inner);
                    if output.len() < OUTPUT_LIMIT {
                        output.push_str(&line);
                        output.push('\n');
                    }
                }
            });
        }
        Ok(Self {
            handle: LoginHandle {
                child: Arc::new(Mutex::new(child)),
                output,
                scratch: Arc::new(Scratch {
                    kind,
                    dir,
                    keychain,
                }),
            },
            _stdin: input,
        })
    }

    pub fn handle(&self) -> LoginHandle {
        self.handle.clone()
    }

    /// 等命令退出（会一直等到登录完、被取消，最多 15 分钟，放后台线程），读出临时目录里的登录文件，删掉临时目录。
    pub fn wait(self) -> Result<Vec<u8>, AccountError> {
        self.wait_for(LOGIN_DEADLINE)
    }

    fn wait_for(self, deadline: Duration) -> Result<Vec<u8>, AccountError> {
        let started = Instant::now();
        let scratch = &self.handle.scratch;
        let status = loop {
            let done = self
                .handle
                .child
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .try_wait()
                .map_err(io_error(&scratch.dir))?;
            if let Some(status) = done {
                break status;
            }
            if started.elapsed() >= deadline {
                self.handle.cancel();
                return Err(AccountError::LoginTimedOut);
            }
            thread::sleep(Duration::from_millis(200));
        };
        // 读输出的线程可能还差最后几行。
        thread::sleep(Duration::from_millis(50));
        let auth = if scratch.kind == AgentKind::ClaudeCode {
            claude::read_login(&scratch.dir, &scratch.keychain)
        } else {
            fs::read(scratch.dir.join("auth.json")).ok()
        };
        match auth.filter(|bytes| identify(scratch.kind, bytes).is_some()) {
            Some(bytes) => Ok(bytes),
            None => {
                let tail = self.handle.tail(4);
                Err(AccountError::Login(if tail.is_empty() {
                    status.to_string()
                } else {
                    tail
                }))
            }
        }
    }
}

impl Drop for Login {
    fn drop(&mut self) {
        self.handle.cancel();
        self.handle.scratch.clean();
    }
}

impl LoginHandle {
    /// 命令打出来的第一个网址：浏览器没打开时让人自己点。网址可能包在终端的超链接转义里（Claude Code），转义字符不要。
    pub fn url(&self) -> Option<String> {
        let output = self.output.lock().unwrap_or_else(PoisonError::into_inner);
        let start = output.find("https://")?;
        let url: String = output[start..]
            .chars()
            .take_while(|ch| !ch.is_whitespace() && !ch.is_control())
            .collect();
        Some(url)
    }

    /// 停掉登录命令；已经退出了就什么也不做。
    pub fn cancel(&self) {
        let mut child = self.child.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(child.try_wait(), Ok(None)) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// 应用要退出了：停掉登录命令，当场删掉临时目录（等 [`Login::wait`] 那边收拾就来不及了）。
    pub fn abandon(&self) {
        self.cancel();
        self.scratch.clean();
    }

    /// 输出的最后几行。
    fn tail(&self, lines: usize) -> String {
        let output = self.output.lock().unwrap_or_else(PoisonError::into_inner);
        let all: Vec<&str> = output
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        all[all.len().saturating_sub(lines)..].join("\n")
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use serde_json::json;

    use super::*;

    fn b64(bytes: &[u8]) -> String {
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

    struct Setup {
        _dir: tempfile::TempDir,
        home: PathBuf,
        engine: Engine,
    }

    fn setup() -> Setup {
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

    fn id_of(bytes: &[u8]) -> String {
        identify(AgentKind::Codex, bytes).unwrap().id
    }

    /// 快照读官方账号的那一步（`read_all`），Codex 的两个文件照快照那样先读好。
    fn read(s: &Setup, kind: AgentKind) -> Accounts {
        let config = fs::read(s.home.join(".codex/config.toml")).ok();
        let auth = s.auth();
        let codex = Disk {
            config: config.as_deref(),
            login: auth.as_deref(),
        };
        read_all(&s.engine, &s.home, &[kind], codex)
    }

    /// 登录文件现在是谁（[`read`]）。
    fn live(s: &Setup, kind: AgentKind) -> Live {
        let mut accounts = read(s, kind);
        assert!(accounts.error.is_none(), "{:?}", accounts.error);
        accounts.live.remove(&kind).expect("the agent is read")
    }

    #[test]
    fn base64url_decodes_without_padding() {
        assert_eq!(base64url("aGk").unwrap(), b"hi");
        assert_eq!(base64url("aGVsbG8_").unwrap(), b"hello?");
        assert_eq!(base64url(&b64(b"any bytes")).unwrap(), b"any bytes");
        assert_eq!(base64url("a!b"), None);
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
            "https://auth.x.ai::client": {"user_id": "u1", "team_id": "t1", "email": "g@example.com", "expires_at": "2026-10-13T00:00:00Z"},
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
        let found = found_in(AgentKind::Codex, s.auth()).unwrap();
        let (_, kept) = sync_found(&s.engine, &s.home, AgentKind::Codex, &book, found).unwrap();
        assert!(!kept);
        // 拿锁之前就看出不用存：和 `keep` 用的是同一个判断。
        let stored = s.stored(&id_of(&bob));
        let identity = identify(AgentKind::Codex, &bob).unwrap();
        assert!(!replaces(
            AgentKind::Codex,
            &identity,
            Some(&stored),
            &bob,
            Keep::Known
        ));
        assert!(replaces(
            AgentKind::Codex,
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

    #[cfg(unix)]
    #[test]
    fn login_runs_the_agent_command_in_a_scratch_home() {
        use std::os::unix::fs::PermissionsExt as _;
        let s = setup();
        let auth = String::from_utf8(codex_auth("alice", "ws", 0)).unwrap();
        let script = s.home.join("fake-codex");
        fs::create_dir_all(&s.home).unwrap();
        fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$1 $2 $3\" >&2\necho 'open https://auth.example/login?x=1 to continue'\nsleep 0.3\ncat > \"$CODEX_HOME/auth.json\" <<'EOF'\n{auth}\nEOF\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

        let login = Login::start(&script, s.engine.data(), AgentKind::Codex).unwrap();
        let handle = login.handle();
        let dir = login.handle.scratch.dir.clone();
        let bytes = login.wait().unwrap();
        assert_eq!(id_of(&bytes), id_of(auth.as_bytes()));
        assert_eq!(
            handle.url().as_deref(),
            Some("https://auth.example/login?x=1")
        );
        assert!(
            handle
                .tail(5)
                .contains("login -c cli_auth_credentials_store=\"file\"")
        );
        assert!(!dir.exists());

        // 取消：命令被停掉，没有登录文件。
        fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
        let login = Login::start(&script, s.engine.data(), AgentKind::Codex).unwrap();
        login.handle().cancel();
        assert!(matches!(login.wait(), Err(AccountError::Login(_))));

        // 等太久：停掉命令，报超时。
        let login = Login::start(&script, s.engine.data(), AgentKind::Codex).unwrap();
        let handle = login.handle();
        assert!(matches!(
            login.wait_for(Duration::from_millis(300)),
            Err(AccountError::LoginTimedOut)
        ));
        assert!(!matches!(handle.child.lock().unwrap().try_wait(), Ok(None)));
        assert!(!handle.scratch.dir.exists());

        // 应用退出：命令停掉，临时目录当场删掉（`wait` 那边还没轮到）。
        let login = Login::start(&script, s.engine.data(), AgentKind::Codex).unwrap();
        let handle = login.handle();
        handle.abandon();
        assert!(!matches!(handle.child.lock().unwrap().try_wait(), Ok(None)));
        assert!(!handle.scratch.dir.exists());
        drop(login);
    }

    #[cfg(unix)]
    #[test]
    fn stale_logins_are_cleaned_at_startup() {
        let s = setup();
        let logins = s.engine.data().account_logins();
        // 一个已经退出的进程号。
        let mut gone = Command::new("/usr/bin/true").spawn().unwrap();
        gone.wait().unwrap();
        // 一个还在跑的。
        let mut running = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let dirs = [
            scratch_name(AgentKind::Codex, gone.id(), 0),
            scratch_name(AgentKind::ClaudeCode, gone.id(), 1),
            scratch_name(AgentKind::GrokBuild, running.id(), 0),
            scratch_name(AgentKind::Codex, std::process::id(), 0),
            "codex".to_string(),
        ];
        for dir in &dirs {
            fs::create_dir_all(logins.join(dir).join(".keychain")).unwrap();
        }
        // 名字像临时目录的链接：不跟过去删它指着的目录。
        let elsewhere = s.home.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        let link = logins.join(scratch_name(AgentKind::Codex, gone.id(), 9));
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
        assert_eq!(
            parse_scratch_name(&dirs[1]),
            Some((AgentKind::ClaudeCode, gone.id()))
        );
        clean_stale_logins(s.engine.data());
        let left: Vec<bool> = dirs.iter().map(|dir| logins.join(dir).exists()).collect();
        assert_eq!(left, [false, false, true, true, true]);
        assert!(elsewhere.exists() && link.exists());
        running.kill().unwrap();
        running.wait().unwrap();
    }

    #[test]
    fn claude_profile_reads_the_account_not_the_login() {
        let s = setup();
        let profile = || read(&s, AgentKind::ClaudeCode).claude;
        assert_eq!(profile(), None);
        fs::create_dir_all(&s.home).unwrap();
        fs::write(
            s.home.join(".claude.json"),
            r#"{"numStartups": 3, "oauthAccount": {"emailAddress": " a@example.com ", "organizationType": "claude_max"}}"#,
        )
        .unwrap();
        assert_eq!(
            profile(),
            Some(Profile {
                email: Some("a@example.com".into()),
                plan: Some("max".into())
            })
        );
        fs::write(s.home.join(".claude.json"), r#"{"numStartups": 3}"#).unwrap();
        assert_eq!(profile(), None);
    }

    /// Claude Code 的一份登录（[`claude::bundle`] 拼的样子）：`user` 决定是谁，`refresh` 是第几次换的令牌。
    #[cfg(target_os = "macos")]
    fn claude_login(user: &str, refresh: i64) -> (Value, Value) {
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

    #[cfg(target_os = "macos")]
    #[test]
    fn claude_login_runs_in_a_scratch_config_dir() {
        use std::os::unix::fs::PermissionsExt as _;
        let s = setup();
        let (alice, alice_token) = claude_login("alice", 0);
        let profile = json!({"oauthAccount": alice}).to_string();
        let secure = json!({"claudeAiOauth": alice_token}).to_string();
        // 照 Claude Code：令牌存进服务名带目录 hash 的那一条（这里是假钥匙串的文件），`oauthAccount` 写进配置目录。
        let script = s.home.join("fake-claude");
        fs::create_dir_all(&s.home).unwrap();
        fs::write(
            &script,
            format!(
                "#!/bin/sh\n\
                 echo \"$1 $2 $3\" >&2\n\
                 [ \"$CLAUDE_SECURESTORAGE_CONFIG_DIR\" = \"$CLAUDE_CONFIG_DIR\" ] && echo same-dir >&2\n\
                 [ -p /dev/stdin ] && echo stdin-open >&2\n\
                 printf 'visit: \\033]8;;https://claude.example/oauth?code=1\\033\\\\https://claude.example/oauth?code=1\\033]8;;\\033\\\\\\n'\n\
                 sleep 0.3\n\
                 hash=$(printf %s \"$CLAUDE_SECURESTORAGE_CONFIG_DIR\" | shasum -a 256 | cut -c1-8)\n\
                 item=\"$CLAUDE_CONFIG_DIR/.keychain/Claude Code-credentials-$hash\"\n\
                 mkdir -p \"$item\"\n\
                 printf '%s' '{secure}' > \"$item/$USER\"\n\
                 printf '%s' '{profile}' > \"$CLAUDE_CONFIG_DIR/.claude.json\"\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

        let login = Login::start(&script, s.engine.data(), AgentKind::ClaudeCode).unwrap();
        let handle = login.handle();
        let dir = login.handle.scratch.dir.clone();
        let bytes = login.wait().unwrap();
        assert_eq!(bytes, claude::bundle(&alice, &alice_token).unwrap());
        assert_eq!(
            handle.url().as_deref(),
            Some("https://claude.example/oauth?code=1")
        );
        let tail = handle.tail(5);
        assert!(tail.contains("auth login --claudeai"), "{tail}");
        assert!(tail.contains("same-dir"), "{tail}");
        assert!(tail.contains("stdin-open"), "{tail}");
        assert!(!dir.exists());

        // 没写出登录就退出了：报错，带最后几行。
        fs::write(&script, "#!/bin/sh\necho 'Login failed' >&2\nexit 1\n").unwrap();
        let login = Login::start(&script, s.engine.data(), AgentKind::ClaudeCode).unwrap();
        assert!(
            matches!(login.wait(), Err(AccountError::Login(tail)) if tail.contains("Login failed"))
        );
    }
}
