//! 加账号时的登录（[`Login`]）：跑 agent 自己的登录命令，家目录指到 confdeck 数据目录下的一个临时目录，
//! 登录完读出那里的登录，删掉临时目录。各 agent 的命令、读法在 [`Storage`]。

use std::ffi::OsStr;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use super::{AccountError, Storage, identify_with, io_error, private_dir, storable, storage};
use crate::agent::AgentKind;
use crate::data::DataDir;
use crate::keychain::Keychain;

/// agent 的命令行程序。
pub fn program(home: &Path, kind: AgentKind) -> Result<PathBuf, AccountError> {
    let name = storable(kind)?.program();
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
    store: &'static dyn Storage,
    dir: PathBuf,
    /// 登录命令存令牌的钥匙串（Claude Code 用，见 [`super::claude`]）。
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

    /// 删掉临时目录，连同登录命令可能已经存进钥匙串的那条临时令牌。
    fn clean(&self) {
        self.store.forget_login(&self.dir, &self.keychain);
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
/// 连同它可能存进钥匙串的那条临时令牌。别的 confdeck 进程正在跑的登录不动。
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
        let Some((store, pid)) = entry
            .file_name()
            .to_str()
            .and_then(parse_scratch_name)
            .and_then(|(kind, pid)| Some((storage(kind)?, pid)))
        else {
            continue;
        };
        if pid == std::process::id() || process_alive(pid) {
            continue;
        }
        let dir = logins.join(entry.file_name());
        Scratch {
            store,
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
        let store = storable(kind)?;
        let run = RUNS.fetch_add(1, Ordering::Relaxed);
        let dir = data
            .account_logins()
            .join(scratch_name(kind, std::process::id(), run));
        let _ = fs::remove_dir_all(&dir);
        private_dir(&data.account_logins())?;
        private_dir(&dir)?;
        // Claude Code 按目录的写法算钥匙串的服务名，用解开链接的路径，两边算的一样。
        let dir = fs::canonicalize(&dir).unwrap_or(dir);
        // 真的登录命令把令牌存进系统钥匙串（Claude Code 的临时一条，读完就删）；测试用的假命令写文件，先建好它的位置。
        let real =
            cfg!(target_os = "macos") && program.file_name() == Some(OsStr::new(store.program()));
        if !real {
            private_dir(&dir.join(".keychain"))?;
        }
        let keychain = Scratch::keychain(&dir);
        let mut command = Command::new(program);
        let stdin = if store.login_command(&mut command, &dir) {
            Stdio::piped()
        } else {
            Stdio::null()
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
                    store,
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

    /// 等命令退出（会一直等到登录完、被取消，最多 15 分钟，放后台线程），读出临时目录里的登录，删掉临时目录。
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
        let login = scratch
            .store
            .read_login(&scratch.dir, &scratch.keychain)
            .filter(|login| identify_with(scratch.store, login).is_some());
        match login {
            Some(login) => Ok(login),
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
mod tests {
    use std::process::Command;
    use std::time::Duration;

    use super::*;
    use crate::account::tests::{codex_auth, id_of, setup};
    #[cfg(target_os = "macos")]
    use crate::account::{claude, tests::claude_login};
    #[cfg(target_os = "macos")]
    use serde_json::json;

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
