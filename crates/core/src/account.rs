//! 官方账号多账号（10-13，阶段 4）：Codex、Grok Build 各存几个官方登录，切换时把选中的那个写回 agent 的登录文件。
//!
//! - **存在哪**：`~/.confdeck/accounts.json` 记有哪些账号（[`Account`]），每个账号的登录文件原样存在
//!   `~/.confdeck/accounts/<agent>/<id>.json`（只有自己能读）。不同步、不导出。
//! - **登录**（[`Login`]）：不自己做 OAuth，用 agent 自己的 `login` 命令，把它的家目录（`CODEX_HOME`、`GROK_HOME`）
//!   指到一个临时目录；登录完读那里的 `auth.json` 存成账号（[`add`]），删掉临时目录。
//! - **换下来的登录先存好**：agent 用着用着会换新令牌（刷新令牌用一次就作废），所以切换（[`switch`]）时先把登录文件
//!   现在的内容存回它所属的账号（比存着的新才存；不认识的账号存成新账号），再写入要切的账号。[`sync`] 平时也这样存。
//! - **认账号**（[`identify`]）：Codex 看 `id_token` 里的用户和工作区（同一个人的个人、团队工作区算两个），
//!   Grok 看 `user_id` 和 `team_id`。账号 ID 是这两样的 hash。
//! - **不换令牌**：confdeck 不用刷新令牌（同「订阅额度」）。存着不用的账号放久了令牌可能失效，到时候重新登录。
//! - **名字和备注**（[`label`]，10-13 提案 13）：用户自己起的，只存在 `accounts.json`，不碰登录文件。

use std::fs;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::AgentKind;
use crate::data::DataDir;
use crate::engine::{Engine, EngineError, LiveFile, Outcome};
use crate::fsx::{self, Digest, Mode};
use crate::patch::Format;

/// 能存多个官方账号的 agent。
pub const SUPPORTED: [AgentKind; 2] = [AgentKind::Codex, AgentKind::GrokBuild];

pub fn supported(kind: AgentKind) -> bool {
    SUPPORTED.contains(&kind)
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
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> EngineError + '_ {
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
/// confdeck 不读钥匙串，这时不让切。
pub fn blocked(home: &Path, kind: AgentKind) -> bool {
    if kind != AgentKind::Codex {
        return false;
    }
    let Ok(text) = fs::read_to_string(home.join(".codex").join("config.toml")) else {
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

/// Claude Code 登着的官方账号：`~/.claude.json` 的 `oauthAccount`（邮箱 `emailAddress`；`organizationType` 去掉 `claude_`
/// 是套餐）。这里只有资料，凭证在钥匙串里，confdeck 不读。没登录官方的是 `None`。Claude Code 不能存多个账号。
pub fn claude_profile(home: &Path) -> Option<Profile> {
    let bytes = fs::read(home.join(".claude.json")).ok()?;
    let json: Value = serde_json::from_slice(&bytes).ok()?;
    let account = json.get("oauthAccount").filter(|a| a.is_object())?;
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

/// 把一份登录存进它的账号。调用的人拿着引擎的锁。
fn keep(
    data: &DataDir,
    kind: AgentKind,
    identity: &Identity,
    bytes: &[u8],
    how: Keep,
) -> Result<(), EngineError> {
    let mut book = load(data)?;
    let mut changed = false;
    match book
        .accounts
        .iter_mut()
        .find(|account| account.agent == kind && account.id == identity.id)
    {
        None if how == Keep::Known => return Ok(()),
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
    let path = login_path(data, kind, &identity.id);
    let stored = fsx::read_optional(&path).map_err(io_error(&path))?;
    let replace = match stored.as_deref() {
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
    };
    if replace {
        private_dir(&data.account_logins())?;
        let dir = path.parent().expect("login path has a parent");
        private_dir(dir)?;
        fsx::write_atomic(&path, bytes, Mode::Private).map_err(io_error(&path))?;
    }
    if changed {
        save(data, &book)?;
    }
    Ok(())
}

/// 看登录文件现在是谁；是存过的账号，就把文件里的登录存回去（比存着的新才存）。没存过的不存。
pub fn sync(engine: &Engine, home: &Path, kind: AgentKind) -> Result<Live, AccountError> {
    let file = login_file(home, kind).ok_or(AccountError::Unsupported)?;
    let Some(bytes) = fsx::read_optional(&file.path).map_err(io_error(&file.path))? else {
        return Ok(Live::LoggedOut);
    };
    let Some(identity) = identify(kind, &bytes) else {
        return Ok(Live::LoggedOut);
    };
    let data = engine.data();
    let known = load(data)?
        .accounts
        .iter()
        .any(|account| account.agent == kind && account.id == identity.id);
    if !known {
        return Ok(Live::Unknown(identity));
    }
    let _lock = engine.lock()?;
    keep(data, kind, &identity, &bytes, Keep::Known)?;
    Ok(Live::Known(identity.id))
}

/// 切到一个存下的账号：先把登录文件现在的内容存好（没存过的账号存成新账号），再写入这个账号的登录。
pub fn switch(
    engine: &Engine,
    home: &Path,
    kind: AgentKind,
    id: &str,
) -> Result<Outcome, AccountError> {
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
            keep(&data, kind, &identity, live, Keep::Any)?;
        }
        // 要切的就是现在这个：上面可能刚把更新的登录存进去。
        fs::read(&path).map_err(io_error(&path))
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
    let file = login_file(home, kind).ok_or(AccountError::Unsupported)?;
    let identity = identify(kind, bytes).ok_or(AccountError::Unrecognized)?;
    let data = engine.data();
    {
        let _lock = engine.lock()?;
        keep(data, kind, &identity, bytes, Keep::Fresh)?;
    }
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
        let file = login_file(home, kind).ok_or(AccountError::Unsupported)?;
        let bytes = fsx::read_optional(&file.path)
            .map_err(io_error(&file.path))?
            .ok_or(AccountError::Missing)?;
        let identity = identify(kind, &bytes).ok_or(AccountError::Missing)?;
        if identity.id != id {
            return Err(AccountError::Missing);
        }
        keep(data, kind, &identity, &bytes, Keep::Any)?;
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
pub fn remove(engine: &Engine, kind: AgentKind, id: &str) -> Result<(), AccountError> {
    let data = engine.data();
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
    match fs::remove_file(&path) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => Err(io_error(&path)(err).into()),
        _ => Ok(()),
    }
}

/// agent 的命令行程序。
pub fn program(home: &Path, kind: AgentKind) -> Result<PathBuf, AccountError> {
    let name = match kind {
        AgentKind::Codex => "codex",
        AgentKind::GrokBuild => "grok",
        _ => return Err(AccountError::Unsupported),
    };
    crate::program::find(home, name).ok_or(AccountError::NoProgram(name))
}

/// 输出最多留多少字。
const OUTPUT_LIMIT: usize = 16 * 1024;

/// 一次在跑的登录：agent 的 `login` 命令，家目录指到 confdeck 数据目录下的一个临时目录。
/// 命令自己打开浏览器；打不开时它会把登录地址打出来（[`LoginHandle::url`]）。
pub struct Login {
    kind: AgentKind,
    dir: PathBuf,
    handle: LoginHandle,
}

/// 在别的线程里看登录到哪了、取消它。
#[derive(Clone)]
pub struct LoginHandle {
    child: Arc<Mutex<Child>>,
    output: Arc<Mutex<String>>,
}

impl Login {
    /// 跑 `program`（[`program`] 找到的，测试时可以换成别的）的登录命令。
    pub fn start(program: &Path, data: &DataDir, kind: AgentKind) -> Result<Self, AccountError> {
        static RUNS: AtomicUsize = AtomicUsize::new(0);
        let run = RUNS.fetch_add(1, Ordering::Relaxed);
        let dir = data.account_logins().join(format!(
            ".login-{}-{}-{run}",
            kind.id(),
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        private_dir(&data.account_logins())?;
        private_dir(&dir)?;
        let mut command = Command::new(program);
        match kind {
            // 临时家目录里没有 config.toml，写明存文件，免得存进钥匙串。
            AgentKind::Codex => command
                .args(["login", "-c", "cli_auth_credentials_store=\"file\""])
                .env("CODEX_HOME", &dir),
            AgentKind::GrokBuild => command.args(["login", "--oauth"]).env("GROK_HOME", &dir),
            _ => return Err(AccountError::Unsupported),
        };
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(io_error(program))?;
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
            kind,
            dir,
            handle: LoginHandle {
                child: Arc::new(Mutex::new(child)),
                output,
            },
        })
    }

    pub fn handle(&self) -> LoginHandle {
        self.handle.clone()
    }

    /// 等命令退出（会一直等到登录完或者被取消，放后台线程），读出临时目录里的登录文件，删掉临时目录。
    pub fn wait(self) -> Result<Vec<u8>, AccountError> {
        let status = loop {
            let done = self
                .handle
                .child
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .try_wait()
                .map_err(io_error(&self.dir))?;
            if let Some(status) = done {
                break status;
            }
            thread::sleep(Duration::from_millis(200));
        };
        // 读输出的线程可能还差最后几行。
        thread::sleep(Duration::from_millis(50));
        let auth = fs::read(self.dir.join("auth.json")).ok();
        match auth.filter(|bytes| identify(self.kind, bytes).is_some()) {
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
        let _ = fs::remove_dir_all(&self.dir);
    }
}

impl LoginHandle {
    /// 命令打出来的第一个网址：浏览器没打开时让人自己点。
    pub fn url(&self) -> Option<String> {
        let output = self.output.lock().unwrap_or_else(PoisonError::into_inner);
        output
            .split_whitespace()
            .find(|word| word.starts_with("https://"))
            .map(str::to_string)
    }

    /// 停掉登录命令；已经退出了就什么也不做。
    pub fn cancel(&self) {
        let mut child = self.child.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(child.try_wait(), Ok(None)) {
            let _ = child.kill();
            let _ = child.wait();
        }
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
mod tests {
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
    fn codex_auth(user: &str, workspace: &str, refresh: u32) -> Vec<u8> {
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
            sync(&s.engine, &s.home, AgentKind::Codex).unwrap(),
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
        assert_eq!(
            sync(&s.engine, &s.home, AgentKind::Codex).unwrap(),
            Live::Known(id_of(&bob))
        );
        assert_eq!(s.stored(&id_of(&bob)), bob_latest);
        // 登录文件比存着的旧（比如从备份恢复了）：不往回存。
        s.set_auth(&bob);
        sync(&s.engine, &s.home, AgentKind::Codex).unwrap();
        assert_eq!(s.stored(&id_of(&bob)), bob_latest);
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
        remove(&s.engine, AgentKind::Codex, &id_of(&alice)).unwrap();
        assert!(list(s.engine.data()).unwrap().is_empty());
        assert_eq!(s.auth().unwrap(), alice);
        assert!(matches!(
            remove(&s.engine, AgentKind::Codex, &id_of(&alice)),
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
        assert!(!blocked(&s.home, AgentKind::Codex));
        fs::create_dir_all(s.home.join(".codex")).unwrap();
        fs::write(
            s.home.join(".codex/config.toml"),
            "cli_auth_credentials_store = \"file\"\n",
        )
        .unwrap();
        assert!(!blocked(&s.home, AgentKind::Codex));
        fs::write(
            s.home.join(".codex/config.toml"),
            "cli_auth_credentials_store = \"keyring\"\n",
        )
        .unwrap();
        assert!(blocked(&s.home, AgentKind::Codex));
        assert!(!blocked(&s.home, AgentKind::GrokBuild));
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
        let dir = login.dir.clone();
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
    }

    #[test]
    fn claude_profile_reads_the_account_not_the_login() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(claude_profile(dir.path()), None);
        fs::write(
            dir.path().join(".claude.json"),
            r#"{"numStartups": 3, "oauthAccount": {"emailAddress": " a@example.com ", "organizationType": "claude_max"}}"#,
        )
        .unwrap();
        assert_eq!(
            claude_profile(dir.path()),
            Some(Profile {
                email: Some("a@example.com".into()),
                plan: Some("max".into())
            })
        );
        fs::write(dir.path().join(".claude.json"), r#"{"numStartups": 3}"#).unwrap();
        assert_eq!(claude_profile(dir.path()), None);
    }
}
