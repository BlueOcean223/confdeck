//! Claude Code 的官方账号（10-13 用户：「Claude 先做登录和切换吧，额度空着」）。
//!
//! Claude Code 的登录分两处，都要换：
//! - **令牌**在钥匙串 `Claude Code-credentials`（账户名见 [`user`]）那一条里，是一份 JSON，`claudeAiOauth` 是令牌，
//!   同一条里还有别的（MCP 服务器的登录等），换账号时只换 `claudeAiOauth`。钥匙串用不了时 Claude Code 存在
//!   `~/.claude/.credentials.json`，读的时候也先钥匙串后文件。
//! - **是谁**在 `~/.claude.json` 的 `oauthAccount`（邮箱、账号和组织的 UUID、组织类型），按键改这一个，文件里别的不动。
//!
//! confdeck 存、比、切的是两样拼成的一份 JSON（[`bundle`]）：`{"oauthAccount": {...}, "claudeAiOauth": {...}}`。
//! 存下的账号放在钥匙串里（服务名 [`STORE`]，账户名是账号 ID），不存成文件：Claude Code 自己的令牌就在钥匙串里。
//!
//! **登录**：`claude auth login --claudeai`，`CLAUDE_CONFIG_DIR` 和 `CLAUDE_SECURESTORAGE_CONFIG_DIR` 都指到一个临时目录。
//! 这时它把令牌存在钥匙串的另一条（[`service_for`]：服务名后面加 `-` 和目录 SHA-256 的前 8 位，2.1.296 实测的写法），
//! `oauthAccount` 写在临时目录的 `.claude.json`。读出来拼好以后删掉那一条和临时目录。
//!
//! 额度先不查（用户说先不做）。

use std::fs;
use std::path::Path;

use serde_json::{Map, Value};

use super::{Identity, io_error};
use crate::agent::AgentKind;
use crate::engine::{EngineError, LiveFile};
use crate::fsx::{self, Digest, Mode};
use crate::keychain::{self, Keychain};
use crate::patch::json::JsonPatch;
use crate::patch::{Format, KeyPath, LivePatch};

/// Claude Code 存令牌的服务名（没设 `CLAUDE_CONFIG_DIR` 时）。
pub const SERVICE: &str = "Claude Code-credentials";
/// confdeck 存下的 Claude 账号的服务名。
pub const STORE: &str = "confdeck · Claude Code";

/// Claude Code 用的钥匙串账户名：`$USER`（只含字母、数字和 `._-` 时），不然是 `claude-code-user`。
pub fn user() -> String {
    std::env::var("USER")
        .ok()
        .filter(|user| {
            !user.is_empty()
                && user
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        })
        .unwrap_or_else(|| "claude-code-user".to_string())
}

/// 配置目录指到 `dir` 时 Claude Code 存令牌用的服务名。
pub fn service_for(dir: &str) -> String {
    format!("{SERVICE}-{}", &Digest::of(dir.as_bytes()).0[..8])
}

/// 拼成一份：`oauthAccount` 和 `claudeAiOauth` 都有才算登录了。
pub fn bundle(account: &Value, oauth: &Value) -> Option<Vec<u8>> {
    if !account.is_object() || !oauth.is_object() {
        return None;
    }
    let mut map = Map::new();
    map.insert("oauthAccount".into(), account.clone());
    map.insert("claudeAiOauth".into(), oauth.clone());
    Some(keychain::ascii_json(&Value::Object(map)))
}

/// 认出一份登录是谁：`oauthAccount` 的账号 UUID 加组织 UUID（同一个人在两个组织算两个账号）。
pub(super) fn identify(json: &Value) -> Option<Identity> {
    let oauth = &json["claudeAiOauth"];
    oauth["accessToken"].as_str().filter(|t| !t.is_empty())?;
    let (key, email, plan) = profile_of(&json["oauthAccount"], oauth)?;
    Some(Identity {
        id: id_of(&key),
        email,
        plan,
        fresh: oauth["expiresAt"].as_i64(),
    })
}

/// `oauthAccount` 里的是谁、邮箱、套餐（组织类型去掉 `claude_`，没有再看令牌里的 `subscriptionType`）。
fn profile_of(account: &Value, oauth: &Value) -> Option<(String, Option<String>, Option<String>)> {
    let text = |value: &Value| {
        value
            .as_str()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
    };
    let uuid = text(&account["accountUuid"])?;
    let org = text(&account["organizationUuid"]).unwrap_or_default();
    let plan = text(&account["organizationType"])
        .and_then(|kind| {
            kind.strip_prefix("claude_")
                .filter(|plan| !plan.is_empty())
                .map(str::to_string)
        })
        .or_else(|| text(&oauth["subscriptionType"]));
    Some((
        format!("{uuid}:{org}"),
        text(&account["emailAddress"]),
        plan,
    ))
}

/// 账号 ID，同 Codex、Grok 的算法。
pub(super) fn id_of(key: &str) -> String {
    Digest::of(format!("{}:{key}", AgentKind::ClaudeCode.id()).as_bytes()).0[..16].to_string()
}

/// `~/.claude.json`，只按键改 `oauthAccount`。
pub(super) fn profile_file(home: &Path) -> LiveFile {
    LiveFile {
        owner: AgentKind::ClaudeCode.id(),
        path: home.join(".claude.json"),
        mode: Mode::Keep,
        format: Format::Json,
    }
}

fn read_json(path: &Path) -> Result<Option<Value>, EngineError> {
    Ok(fsx::read_optional(path)
        .map_err(io_error(path))?
        .and_then(|bytes| serde_json::from_slice(&bytes).ok()))
}

/// 登着的是谁：只看 `~/.claude.json`，不读钥匙串。
pub(super) fn live_identity(home: &Path) -> Option<Identity> {
    let doc = read_json(&home.join(".claude.json")).ok()??;
    let (key, email, plan) = profile_of(&doc["oauthAccount"], &Value::Null)?;
    Some(Identity {
        id: id_of(&key),
        email,
        plan,
        fresh: None,
    })
}

/// 令牌放在哪：钥匙串那一条，或者钥匙串用不了时的文件。
enum Secure {
    Keychain,
    File,
}

/// 钥匙串（没有再看文件）里那份 JSON。
fn read_secure(home: &Path, keychain: &Keychain) -> Result<Option<(Secure, Value)>, EngineError> {
    let file = home.join(".claude").join(".credentials.json");
    match keychain.read(SERVICE, &user()) {
        Ok(Some(bytes)) => {
            if let Ok(json) = serde_json::from_slice(&bytes) {
                return Ok(Some((Secure::Keychain, json)));
            }
        }
        Ok(None) => {}
        Err(err) => return Err(io_error(&file)(err)),
    }
    Ok(read_json(&file)?.map(|json| (Secure::File, json)))
}

/// 现在登着的那份（拼好的）；没登录、只有一半的是 `None`。`profile` 是 `~/.claude.json` 的内容。
pub(super) fn read_live(
    profile: Option<&[u8]>,
    home: &Path,
    keychain: &Keychain,
) -> Result<Option<Vec<u8>>, EngineError> {
    let Some(profile) = profile.and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok())
    else {
        return Ok(None);
    };
    let Some((_, secure)) = read_secure(home, keychain)? else {
        return Ok(None);
    };
    Ok(bundle(&profile["oauthAccount"], &secure["claudeAiOauth"]))
}

/// 换成 `login`（拼好的那份）：令牌那份 JSON 只换 `claudeAiOauth`，写回原来在的地方（都没有就写钥匙串）；
/// 返回 `~/.claude.json` 只换了 `oauthAccount` 的内容。在引擎的 `write_login` 里调，`pre` 是 `~/.claude.json` 现在的内容。
pub(super) fn put_live(
    home: &Path,
    keychain: &Keychain,
    pre: Option<&[u8]>,
    login: &[u8],
) -> Result<Vec<u8>, EngineError> {
    let file = home.join(".claude").join(".credentials.json");
    let login: Value = serde_json::from_slice(login).map_err(|err| {
        io_error(&file)(std::io::Error::new(std::io::ErrorKind::InvalidData, err))
    })?;
    let (place, mut secure) = match read_secure(home, keychain)? {
        Some((place, Value::Object(map))) => (place, map),
        Some((place, _)) => (place, Map::new()),
        None => (Secure::Keychain, Map::new()),
    };
    secure.insert("claudeAiOauth".into(), login["claudeAiOauth"].clone());
    let secure = Value::Object(secure);
    match place {
        Secure::Keychain => keychain
            .write(SERVICE, &user(), &keychain::ascii_json(&secure))
            .map_err(io_error(&file))?,
        Secure::File => {
            let bytes = serde_json::to_vec(&secure).expect("json serializes");
            fsx::write_atomic(&file, &bytes, Mode::Private).map_err(io_error(&file))?
        }
    }
    let profile = profile_file(home);
    let patch = JsonPatch {
        set: vec![(
            KeyPath::new(&["oauthAccount"]),
            login["oauthAccount"].clone(),
        )],
        ..Default::default()
    };
    Ok(patch.apply(&profile.path, pre)?)
}

/// 存下的账号的登录。
pub(super) fn read_stored(
    keychain: &Keychain,
    id: &str,
) -> Result<Option<Vec<u8>>, std::io::Error> {
    keychain.read(STORE, id)
}

pub(super) fn write_stored(
    keychain: &Keychain,
    id: &str,
    login: &[u8],
) -> Result<(), std::io::Error> {
    keychain.write(STORE, id, login)
}

pub(super) fn delete_stored(keychain: &Keychain, id: &str) -> Result<(), std::io::Error> {
    keychain.delete(STORE, id)
}

/// 登录命令跑完后读临时目录：令牌在钥匙串 [`service_for`] 那一条（没有再看目录里的 `.credentials.json`），
/// `oauthAccount` 在目录里的 `.claude.json`。读完删掉那一条。
pub(super) fn read_login(dir: &Path, keychain: &Keychain) -> Option<Vec<u8>> {
    let service = service_for(&dir.to_string_lossy());
    let secure = keychain
        .read(&service, &user())
        .ok()
        .flatten()
        .or_else(|| fs::read(dir.join(".credentials.json")).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    let _ = keychain.delete(&service, &user());
    let profile: Value = serde_json::from_slice(&fs::read(dir.join(".claude.json")).ok()?).ok()?;
    bundle(&profile["oauthAccount"], &secure?["claudeAiOauth"])
}

/// 取消、出错时删掉登录命令可能已经存下的那一条。
pub(super) fn forget_login(dir: &Path, keychain: &Keychain) {
    let _ = keychain.delete(&service_for(&dir.to_string_lossy()), &user());
}
