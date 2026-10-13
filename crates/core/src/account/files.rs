//! 登录是一个文件的 agent（Codex、Grok Build）：登录文件是 `auth.json`，存下的账号是 `accounts/<agent>/<id>.json`，
//! 原样存。两个只差路径、登录命令和认人的办法。

use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

use super::{Found, Identity, Storage, io_error, login_path, private_dir};
use crate::agent::AgentKind;
use crate::data::DataDir;
use crate::engine::{EngineError, LiveFile};
use crate::fsx::{self, Mode};
use crate::keychain::Keychain;
use crate::patch::Format;

pub(super) struct Files {
    kind: AgentKind,
    /// 家目录下的配置目录。
    dir: &'static str,
    program: &'static str,
    /// 登录命令的参数。
    args: &'static [&'static str],
    /// 指定配置目录的环境变量：登录时指到临时目录。
    home_env: &'static str,
    identify: fn(&Value) -> Option<Identity>,
    blocked: fn(Option<&[u8]>) -> bool,
}

pub(super) static CODEX: Files = Files {
    kind: AgentKind::Codex,
    dir: ".codex",
    program: "codex",
    // 临时家目录里没有 config.toml，写明存文件，免得存进钥匙串。
    args: &["login", "-c", "cli_auth_credentials_store=\"file\""],
    home_env: "CODEX_HOME",
    identify: codex_identity,
    blocked: codex_blocked,
};

pub(super) static GROK: Files = Files {
    kind: AgentKind::GrokBuild,
    dir: ".grok",
    program: "grok",
    args: &["login", "--oauth"],
    home_env: "GROK_HOME",
    identify: grok_identity,
    blocked: |_| false,
};

impl Storage for Files {
    fn kind(&self) -> AgentKind {
        self.kind
    }

    fn identify(&self, login: &Value) -> Option<Identity> {
        (self.identify)(login)
    }

    fn live_file(&self, home: &Path) -> LiveFile {
        LiveFile {
            owner: self.kind.id(),
            path: home.join(self.dir).join("auth.json"),
            mode: Mode::Private,
            format: Format::Json,
        }
    }

    fn in_file(&self, file: &[u8]) -> Option<Found> {
        Some(Found {
            identity: (self.identify)(&serde_json::from_slice(file).ok()?)?,
            login: Some(file.to_vec()),
        })
    }

    fn read_live(&self, _home: &Path, file: Option<&[u8]>) -> Result<Option<Vec<u8>>, EngineError> {
        Ok(file.map(<[u8]>::to_vec))
    }

    fn put_file(
        &self,
        _home: &Path,
        _file: Option<&[u8]>,
        login: &[u8],
    ) -> Result<Vec<u8>, EngineError> {
        Ok(login.to_vec())
    }

    fn read_stored(
        &self,
        data: &DataDir,
        _home: &Path,
        id: &str,
    ) -> Result<Option<Vec<u8>>, EngineError> {
        let path = login_path(data, self.kind, id);
        fsx::read_optional(&path).map_err(io_error(&path))
    }

    fn write_stored(
        &self,
        data: &DataDir,
        _home: &Path,
        id: &str,
        login: &[u8],
    ) -> Result<(), EngineError> {
        let path = login_path(data, self.kind, id);
        private_dir(&data.account_logins())?;
        private_dir(path.parent().expect("login path has a parent"))?;
        fsx::write_atomic(&path, login, Mode::Private).map_err(io_error(&path))
    }

    fn delete_stored(&self, data: &DataDir, _home: &Path, id: &str) -> Result<(), EngineError> {
        let path = login_path(data, self.kind, id);
        match fs::remove_file(&path) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => Err(io_error(&path)(err)),
            _ => Ok(()),
        }
    }

    fn blocked(&self, config: Option<&[u8]>) -> bool {
        (self.blocked)(config)
    }

    fn program(&self) -> &'static str {
        self.program
    }

    fn login_command(&self, command: &mut Command, dir: &Path) -> bool {
        command.args(self.args).env(self.home_env, dir);
        false
    }

    fn read_login(&self, dir: &Path, _keychain: &Keychain) -> Option<Vec<u8>> {
        fs::read(dir.join("auth.json")).ok()
    }
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

fn text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// Codex：`id_token` 里的用户和工作区（同一个人的个人、团队工作区算两个）。只有 API Key 的不算登录。
fn codex_identity(login: &Value) -> Option<Identity> {
    let tokens = &login["tokens"];
    let claims = tokens["id_token"]
        .as_str()
        .and_then(jwt_claims)
        .unwrap_or(Value::Null);
    let auth = &claims["https://api.openai.com/auth"];
    let workspace = text(&auth["chatgpt_account_id"]).or_else(|| text(&tokens["account_id"]))?;
    let user = text(&auth["chatgpt_user_id"])
        .or_else(|| text(&auth["user_id"]))
        .or_else(|| text(&claims["sub"]))
        .unwrap_or_default();
    Some(Identity::new(
        AgentKind::Codex,
        &format!("{user}:{workspace}"),
        text(&claims["email"]),
        text(&auth["chatgpt_plan_type"]),
        login["last_refresh"].as_str().and_then(millis),
    ))
}

/// Grok Build：用的那一条（[`grok_entry`]）的 `user_id` 和 `team_id`。
fn grok_identity(login: &Value) -> Option<Identity> {
    let entry = grok_entry(login)?;
    let user = text(&entry["user_id"])?;
    let team = text(&entry["team_id"]).unwrap_or_default();
    Some(Identity::new(
        AgentKind::GrokBuild,
        &format!("{user}:{team}"),
        text(&entry["email"]),
        None,
        entry["expires_at"].as_str().and_then(millis),
    ))
}

/// Grok Build 的登录用哪一条：文件按登录的范围分条，有令牌（`key`）的 `https://auth.x.ai::` 开头那条，没有再用老的 `…/sign-in`。
/// 认账号和查额度（[`crate::quota::grok_login`]）都用它（R5 之前认账号不看有没有令牌、最后还会随便拿一条，和查额度用的可能不是同一条）。
pub(crate) fn grok_entry(login: &Value) -> Option<&Value> {
    let entries = login.as_object()?;
    let usable = |entry: &Value| entry["key"].as_str().is_some_and(|key| !key.is_empty());
    entries
        .iter()
        .find(|(scope, entry)| scope.starts_with("https://auth.x.ai::") && usable(entry))
        .or_else(|| {
            entries
                .iter()
                .find(|(scope, entry)| scope.contains("/sign-in") && usable(entry))
        })
        .map(|(_, entry)| entry)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_decodes_without_padding() {
        assert_eq!(base64url("aGk").unwrap(), b"hi");
        assert_eq!(base64url("aGVsbG8_").unwrap(), b"hello?");
        assert_eq!(
            base64url(&crate::account::tests::b64(b"any bytes")).unwrap(),
            b"any bytes"
        );
        assert_eq!(base64url("a!b"), None);
    }

    #[test]
    fn grok_uses_the_entry_with_a_token() {
        let login = serde_json::json!({
            "https://auth.x.ai::old": {"user_id": "u0"},
            "https://accounts.x.ai/sign-in": {"user_id": "u1", "key": "k1"},
            "other": {"user_id": "u2", "key": "k2"},
        });
        assert_eq!(grok_entry(&login).unwrap()["user_id"], "u1");
        let only_other = serde_json::json!({"other": {"user_id": "u2", "key": "k2"}});
        assert_eq!(grok_entry(&only_other), None);
    }
}
