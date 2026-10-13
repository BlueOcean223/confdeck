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
//! 切换时先换令牌（钥匙串，在引擎的锁外），再在锁里换 `~/.claude.json` 的 `oauthAccount`（`Storage`）。
//!
//! 不是 macOS 时只看登着的是谁（`~/.claude.json`），不存、不切（令牌在系统的钥匙串里，没做）。
//!
//! **登录**：`claude auth login --claudeai`，`CLAUDE_CONFIG_DIR` 和 `CLAUDE_SECURESTORAGE_CONFIG_DIR` 都指到一个临时目录。
//! 这时它把令牌存在钥匙串的另一条（[`service_for`]：服务名后面加 `-` 和目录 SHA-256 的前 8 位，2.1.296 实测的写法），
//! `oauthAccount` 写在临时目录的 `.claude.json`。读出来拼好以后删掉那一条和临时目录。
//!
//! 额度先不查（用户说先不做）。

use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::{Map, Value};

use super::{Found, Identity, Storage, io_error, login_path};
use crate::agent::AgentKind;
use crate::data::DataDir;
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

/// Claude Code 的官方登录：令牌在钥匙串里，是谁在 `~/.claude.json`。
pub(super) struct Claude;

impl Storage for Claude {
    fn kind(&self) -> AgentKind {
        AgentKind::ClaudeCode
    }

    fn storable(&self) -> bool {
        cfg!(target_os = "macos")
    }

    fn identify(&self, login: &Value) -> Option<Identity> {
        identify(login)
    }

    fn live_file(&self, home: &Path) -> LiveFile {
        profile_file(home)
    }

    /// `~/.claude.json` 常有几百 KB，只解析 `oauthAccount`。
    fn in_file(&self, file: &[u8]) -> Option<Found> {
        #[derive(serde::Deserialize)]
        struct Doc {
            #[serde(rename = "oauthAccount", default)]
            account: Value,
        }
        let doc: Doc = serde_json::from_slice(file).ok()?;
        Some(Found {
            identity: live_identity(&doc.account)?,
            login: None,
        })
    }

    fn read_live(&self, home: &Path, file: Option<&[u8]>) -> Result<Option<Vec<u8>>, EngineError> {
        read_live(file, home, &Keychain::for_home(home))
    }

    fn secret(&self) -> bool {
        true
    }

    fn put_secret(&self, home: &Path, login: &[u8]) -> Result<(), EngineError> {
        put_token(home, &Keychain::for_home(home), login)
    }

    fn put_file(
        &self,
        home: &Path,
        file: Option<&[u8]>,
        login: &[u8],
    ) -> Result<Vec<u8>, EngineError> {
        put_profile(home, file, login)
    }

    fn read_stored(
        &self,
        data: &DataDir,
        home: &Path,
        id: &str,
    ) -> Result<Option<Vec<u8>>, EngineError> {
        read_stored(&Keychain::for_home(home), id).map_err(io_error(&login_path(
            data,
            self.kind(),
            id,
        )))
    }

    fn write_stored(
        &self,
        data: &DataDir,
        home: &Path,
        id: &str,
        login: &[u8],
    ) -> Result<(), EngineError> {
        write_stored(&Keychain::for_home(home), id, login).map_err(io_error(&login_path(
            data,
            self.kind(),
            id,
        )))
    }

    fn delete_stored(&self, data: &DataDir, home: &Path, id: &str) -> Result<(), EngineError> {
        delete_stored(&Keychain::for_home(home), id).map_err(io_error(&login_path(
            data,
            self.kind(),
            id,
        )))
    }

    fn program(&self) -> &'static str {
        "claude"
    }

    /// 配置目录和存令牌的目录都指到临时目录。它也等标准输入（贴授权码），开着。
    fn login_command(&self, command: &mut Command, dir: &Path) -> bool {
        command
            .args(["auth", "login", "--claudeai"])
            .env("CLAUDE_CONFIG_DIR", dir)
            .env("CLAUDE_SECURESTORAGE_CONFIG_DIR", dir);
        true
    }

    fn read_login(&self, dir: &Path, keychain: &Keychain) -> Option<Vec<u8>> {
        read_login(dir, keychain)
    }

    fn forget_login(&self, dir: &Path, keychain: &Keychain) {
        forget_login(dir, keychain);
    }
}

/// 认出一份登录是谁：`oauthAccount` 的账号 UUID 加组织 UUID（同一个人在两个组织算两个账号）。
fn identify(json: &Value) -> Option<Identity> {
    let oauth = &json["claudeAiOauth"];
    oauth["accessToken"].as_str().filter(|t| !t.is_empty())?;
    let (key, email, plan) = profile_of(&json["oauthAccount"], oauth)?;
    Some(Identity::new(
        AgentKind::ClaudeCode,
        &key,
        email,
        plan,
        oauth["expiresAt"].as_i64(),
    ))
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

/// `~/.claude.json`，只按键改 `oauthAccount`。
fn profile_file(home: &Path) -> LiveFile {
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

/// 登着的是谁（`~/.claude.json` 的 `oauthAccount`）。不读钥匙串。
fn live_identity(account: &Value) -> Option<Identity> {
    let (key, email, plan) = profile_of(account, &Value::Null)?;
    Some(Identity::new(
        AgentKind::ClaudeCode,
        &key,
        email,
        plan,
        None,
    ))
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
fn read_live(
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

/// 换成 `login`（拼好的那份）的令牌：令牌那份 JSON 只换 `claudeAiOauth`，写回原来在的地方（都没有就写钥匙串）。
fn put_token(home: &Path, keychain: &Keychain, login: &[u8]) -> Result<(), EngineError> {
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
            .map_err(io_error(&file)),
        Secure::File => {
            let bytes = serde_json::to_vec(&secure).expect("json serializes");
            fsx::write_atomic(&file, &bytes, Mode::Private).map_err(io_error(&file))
        }
    }
}

/// 换成 `login`（拼好的那份）以后 `~/.claude.json` 的内容：只换 `oauthAccount`。`pre` 是文件现在的内容。
fn put_profile(home: &Path, pre: Option<&[u8]>, login: &[u8]) -> Result<Vec<u8>, EngineError> {
    let profile = profile_file(home);
    let login: Value = serde_json::from_slice(login).map_err(|err| {
        io_error(&profile.path)(std::io::Error::new(std::io::ErrorKind::InvalidData, err))
    })?;
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

fn write_stored(keychain: &Keychain, id: &str, login: &[u8]) -> Result<(), std::io::Error> {
    keychain.write(STORE, id, login)
}

fn delete_stored(keychain: &Keychain, id: &str) -> Result<(), std::io::Error> {
    keychain.delete(STORE, id)
}

/// 登录命令跑完后读临时目录：令牌在钥匙串 [`service_for`] 那一条（没有再看目录里的 `.credentials.json`），
/// `oauthAccount` 在目录里的 `.claude.json`。读完删掉那一条。
fn read_login(dir: &Path, keychain: &Keychain) -> Option<Vec<u8>> {
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
fn forget_login(dir: &Path, keychain: &Keychain) {
    let _ = keychain.delete(&service_for(&dir.to_string_lossy()), &user());
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// 临时家目录：钥匙串是目录里的文件（`Keychain::Fake`），不碰真的。
    fn home() -> (tempfile::TempDir, std::path::PathBuf, Keychain) {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let keychain = Keychain::for_home(&home);
        assert!(matches!(keychain, Keychain::Fake(_)));
        (dir, home, keychain)
    }

    fn account(user: &str) -> Value {
        json!({"accountUuid": format!("uuid-{user}"), "organizationUuid": "org", "emailAddress": format!("{user}@example.com"), "organizationType": "claude_pro"})
    }

    fn token(user: &str, expires: i64) -> Value {
        json!({"accessToken": format!("at-{user}"), "refreshToken": format!("rt-{user}"), "expiresAt": expires})
    }

    fn secure(keychain: &Keychain) -> Option<Value> {
        let bytes = keychain.read(SERVICE, &user()).unwrap()?;
        Some(serde_json::from_slice(&bytes).unwrap())
    }

    #[test]
    fn a_login_needs_the_token_and_the_account() {
        let login: Value =
            serde_json::from_slice(&bundle(&account("a"), &token("a", 5)).unwrap()).unwrap();
        let identity = identify(&login).unwrap();
        assert_eq!(identity.email.as_deref(), Some("a@example.com"));
        assert_eq!(identity.plan.as_deref(), Some("pro"));
        assert_eq!(identity.fresh, Some(5));
        // 只看 `~/.claude.json` 认出来的是同一个人。
        assert_eq!(live_identity(&account("a")).unwrap().id, identity.id);
        // 同一个人在另一个组织是另一个账号。
        let mut other = account("a");
        other["organizationUuid"] = json!("org2");
        assert_ne!(live_identity(&other).unwrap().id, identity.id);
        // 少一半不算登录。
        assert_eq!(bundle(&account("a"), &Value::Null), None);
        assert_eq!(
            identify(&json!({"oauthAccount": account("a"), "claudeAiOauth": {"accessToken": ""}})),
            None
        );
    }

    #[test]
    fn the_profile_is_read_without_the_token() {
        let file = json!({"numStartups": 3, "projects": {"x": 1}, "oauthAccount": account("a")});
        let found = Claude.in_file(file.to_string().as_bytes()).unwrap();
        assert_eq!(found.identity.email.as_deref(), Some("a@example.com"));
        assert_eq!(found.login, None);
        assert_eq!(Claude.in_file(br#"{"numStartups": 3}"#), None);
        assert_eq!(Claude.in_file(b"not json"), None);
    }

    #[test]
    fn the_live_login_is_the_token_and_the_profile() {
        let (_dir, home, keychain) = home();
        let profile = json!({"oauthAccount": account("a")}).to_string();
        assert_eq!(
            Claude.read_live(&home, Some(profile.as_bytes())).unwrap(),
            None
        );
        // 钥匙串用不了时 Claude Code 存在 `.credentials.json`。
        let file = home.join(".claude/.credentials.json");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, json!({"claudeAiOauth": token("a", 1)}).to_string()).unwrap();
        let login = Claude
            .read_live(&home, Some(profile.as_bytes()))
            .unwrap()
            .unwrap();
        assert_eq!(login, bundle(&account("a"), &token("a", 1)).unwrap());
        // 钥匙串里有就用钥匙串的。
        keychain
            .write(
                SERVICE,
                &user(),
                json!({"claudeAiOauth": token("a", 2)})
                    .to_string()
                    .as_bytes(),
            )
            .unwrap();
        let login = Claude
            .read_live(&home, Some(profile.as_bytes()))
            .unwrap()
            .unwrap();
        assert_eq!(login, bundle(&account("a"), &token("a", 2)).unwrap());
        // 没有 `~/.claude.json` 不算登录。
        assert_eq!(Claude.read_live(&home, None).unwrap(), None);
    }

    #[test]
    fn switching_changes_only_the_token_and_the_account() {
        let (_dir, home, keychain) = home();
        let b = bundle(&account("b"), &token("b", 1)).unwrap();
        // 什么都没有：写钥匙串。
        Claude.put_secret(&home, &b).unwrap();
        assert_eq!(
            secure(&keychain).unwrap(),
            json!({"claudeAiOauth": token("b", 1)})
        );
        // 同一条里的别的（MCP 的登录）留着。
        keychain
            .write(
                SERVICE,
                &user(),
                json!({"claudeAiOauth": token("a", 1), "mcpOAuth": {"x": 1}})
                    .to_string()
                    .as_bytes(),
            )
            .unwrap();
        Claude.put_secret(&home, &b).unwrap();
        assert_eq!(
            secure(&keychain).unwrap(),
            json!({"claudeAiOauth": token("b", 1), "mcpOAuth": {"x": 1}})
        );
        // `~/.claude.json` 只换 `oauthAccount`。
        let pre =
            json!({"numStartups": 3, "oauthAccount": account("a"), "projects": {}}).to_string();
        let next: Value =
            serde_json::from_slice(&Claude.put_file(&home, Some(pre.as_bytes()), &b).unwrap())
                .unwrap();
        assert_eq!(
            next,
            json!({"numStartups": 3, "oauthAccount": account("b"), "projects": {}})
        );
    }

    #[test]
    fn a_token_in_the_file_stays_in_the_file() {
        let (_dir, home, keychain) = home();
        let file = home.join(".claude/.credentials.json");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, json!({"claudeAiOauth": token("a", 1)}).to_string()).unwrap();
        Claude
            .put_secret(&home, &bundle(&account("b"), &token("b", 1)).unwrap())
            .unwrap();
        let written: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        assert_eq!(written, json!({"claudeAiOauth": token("b", 1)}));
        assert_eq!(secure(&keychain), None);
    }

    #[test]
    fn stored_logins_are_in_the_keychain() {
        let (dir, home, keychain) = home();
        let data = DataDir::new(dir.path().join("data"));
        let a = bundle(&account("a"), &token("a", 1)).unwrap();
        assert_eq!(Claude.read_stored(&data, &home, "id-a").unwrap(), None);
        Claude.write_stored(&data, &home, "id-a", &a).unwrap();
        assert_eq!(
            Claude.read_stored(&data, &home, "id-a").unwrap(),
            Some(a.clone())
        );
        assert_eq!(keychain.read(STORE, "id-a").unwrap(), Some(a));
        assert!(!data.account_logins().exists());
        Claude.delete_stored(&data, &home, "id-a").unwrap();
        assert_eq!(Claude.read_stored(&data, &home, "id-a").unwrap(), None);
        Claude.delete_stored(&data, &home, "id-a").unwrap();
    }

    #[test]
    fn a_finished_login_is_read_from_the_scratch_and_forgotten() {
        let (dir, _home, _) = home();
        let scratch = dir.path().join("scratch");
        fs::create_dir_all(&scratch).unwrap();
        let keychain = Keychain::Fake(scratch.join(".keychain"));
        let service = service_for(&scratch.to_string_lossy());
        assert!(
            service.starts_with("Claude Code-credentials-") && service.len() == SERVICE.len() + 9
        );
        keychain
            .write(
                &service,
                &user(),
                json!({"claudeAiOauth": token("a", 1)})
                    .to_string()
                    .as_bytes(),
            )
            .unwrap();
        fs::write(
            scratch.join(".claude.json"),
            json!({"oauthAccount": account("a")}).to_string(),
        )
        .unwrap();
        assert_eq!(
            Claude.read_login(&scratch, &keychain),
            Some(bundle(&account("a"), &token("a", 1)).unwrap())
        );
        // 读完那一条就删了。
        assert_eq!(keychain.read(&service, &user()).unwrap(), None);
        // 取消时也删。
        keychain.write(&service, &user(), b"{}").unwrap();
        Claude.forget_login(&scratch, &keychain);
        assert_eq!(keychain.read(&service, &user()).unwrap(), None);
    }
}
