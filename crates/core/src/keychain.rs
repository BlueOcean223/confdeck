//! macOS 钥匙串里的通用密码（Claude Code 的登录在这里）。
//!
//! - **用系统的 `security` 命令**，不链 Security.framework：Claude Code 自己就是这么读写的，它建的条目的访问控制里有 `security`，
//!   读写都不弹窗。confdeck 用 `security` 建的条目也一样。
//! - **写**：`security -i` 从标准输入读命令，密码用十六进制（`-X`）传，不出现在命令行参数里（`ps` 看得到参数），照 Claude Code。
//! - **读**：`find-generic-password -w`。密码里有非 ASCII 字符时它打十六进制，两种都认。
//! - **不碰真的钥匙串的时候**：家目录不是真的家目录（测试、试用版指到临时目录）时换成一个目录里的文件（[`Keychain::Fake`]），
//!   见 [`Keychain::for_home`]。

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::fsx::{self, Mode};

const SECURITY: &str = "/usr/bin/security";
/// `security` 找不到条目时的退出码。
const NOT_FOUND: i32 = 44;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Keychain {
    /// 登录钥匙串。
    System,
    /// 目录里的文件：`<目录>/<服务名>/<账户名>`。测试、试用版用。
    Fake(PathBuf),
}

impl Keychain {
    /// `home` 是真的家目录（macOS）时是系统钥匙串；别的（测试、`CONFDECK_HOME` 指到临时目录的试用版）是
    /// `home/.confdeck-keychain` 下的文件，不会动到真的登录。
    pub fn for_home(home: &Path) -> Self {
        let real = std::env::home_dir().is_some_and(|real| same_dir(&real, home));
        if cfg!(target_os = "macos") && real {
            Self::System
        } else {
            Self::Fake(home.join(".confdeck-keychain"))
        }
    }

    /// 读一条；没有是 `None`。
    pub fn read(&self, service: &str, account: &str) -> io::Result<Option<Vec<u8>>> {
        match self {
            Self::System => {
                let output = Command::new(SECURITY)
                    .args(["find-generic-password", "-a", account, "-s", service, "-w"])
                    .stdin(Stdio::null())
                    .output()?;
                match output.status.code() {
                    Some(0) => Ok(Some(decode(&output.stdout))),
                    Some(NOT_FOUND) => Ok(None),
                    _ => Err(failed("find-generic-password", &output.stderr)),
                }
            }
            Self::Fake(dir) => fsx::read_optional(&fake_path(dir, service, account)),
        }
    }

    /// 写一条（有就换掉）。
    pub fn write(&self, service: &str, account: &str, secret: &[u8]) -> io::Result<()> {
        match self {
            Self::System => {
                for text in [service, account] {
                    if text.contains(['"', '\\', '\n']) {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("钥匙串的名字里不能有引号、反斜杠：{text}"),
                        ));
                    }
                }
                let hex: String = secret.iter().map(|b| format!("{b:02x}")).collect();
                let mut child = Command::new(SECURITY)
                    .arg("-i")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()?;
                {
                    let mut stdin = child.stdin.take().expect("stdin is piped");
                    writeln!(
                        stdin,
                        "add-generic-password -U -a \"{account}\" -s \"{service}\" -X \"{hex}\""
                    )?;
                }
                let output = child.wait_with_output()?;
                // `-i` 模式下命令失败退出码也可能是 0，错误打在标准错误里。
                if output.status.success() && output.stderr.iter().all(u8::is_ascii_whitespace) {
                    Ok(())
                } else {
                    Err(failed("add-generic-password", &output.stderr))
                }
            }
            Self::Fake(dir) => {
                let path = fake_path(dir, service, account);
                fs::create_dir_all(path.parent().expect("fake path has a parent"))?;
                fsx::write_atomic(&path, secret, Mode::Private)
            }
        }
    }

    /// 删掉一条；本来就没有也算删了。
    pub fn delete(&self, service: &str, account: &str) -> io::Result<()> {
        match self {
            Self::System => {
                let output = Command::new(SECURITY)
                    .args(["delete-generic-password", "-a", account, "-s", service])
                    .stdin(Stdio::null())
                    .output()?;
                match output.status.code() {
                    Some(0 | NOT_FOUND) => Ok(()),
                    _ => Err(failed("delete-generic-password", &output.stderr)),
                }
            }
            Self::Fake(dir) => match fs::remove_file(fake_path(dir, service, account)) {
                Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
                _ => Ok(()),
            },
        }
    }
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

fn fake_path(dir: &Path, service: &str, account: &str) -> PathBuf {
    let clean = |text: &str| text.replace(['/', '\\'], "_");
    dir.join(clean(service)).join(clean(account))
}

/// `-w` 打出来的密码：去掉末尾换行；整串是十六进制（密码里有非 ASCII 时 `security` 这么打）就解开。
fn decode(stdout: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(stdout);
    let text = text.trim_end_matches(['\n', '\r']);
    let hex = text.len().is_multiple_of(2)
        && !text.is_empty()
        && text.bytes().all(|b| b.is_ascii_hexdigit());
    if hex && !text.starts_with('{') {
        let bytes: Option<Vec<u8>> = (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
            .collect();
        if let Some(bytes) = bytes {
            return bytes;
        }
    }
    text.as_bytes().to_vec()
}

fn failed(command: &str, stderr: &[u8]) -> io::Error {
    let detail = String::from_utf8_lossy(stderr).trim().to_string();
    io::Error::other(format!("security {command}: {detail}"))
}

/// JSON 写成纯 ASCII（非 ASCII 字符写成 `\uXXXX`）：存进钥匙串再用 `-w` 读出来还是原样，不会变成十六进制。
pub fn ascii_json(value: &serde_json::Value) -> Vec<u8> {
    let text = serde_json::to_string(value).expect("json serializes");
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_ascii() {
            out.push(ch);
        } else {
            let mut units = [0u16; 2];
            for unit in ch.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn temp_homes_get_a_fake_keychain() {
        let dir = tempfile::tempdir().unwrap();
        let keychain = Keychain::for_home(dir.path());
        assert_eq!(
            keychain,
            Keychain::Fake(dir.path().join(".confdeck-keychain"))
        );
        assert_eq!(
            keychain.read("Claude Code-credentials", "me").unwrap(),
            None
        );
        keychain
            .write("Claude Code-credentials", "me", b"{}")
            .unwrap();
        assert_eq!(
            keychain.read("Claude Code-credentials", "me").unwrap(),
            Some(b"{}".to_vec())
        );
        keychain.delete("Claude Code-credentials", "me").unwrap();
        keychain.delete("Claude Code-credentials", "me").unwrap();
        assert_eq!(
            keychain.read("Claude Code-credentials", "me").unwrap(),
            None
        );
    }

    #[test]
    fn security_output_may_be_hex() {
        assert_eq!(decode(b"{\"a\":1}\n"), b"{\"a\":1}");
        assert_eq!(decode(b"7b7d\n"), b"{}");
        assert_eq!(decode(b"abc\n"), b"abc");
    }

    #[test]
    fn ascii_json_escapes_everything_else() {
        let value = json!({"name": "张三 🙂"});
        let bytes = ascii_json(&value);
        assert!(bytes.is_ascii());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
            value
        );
    }
}
