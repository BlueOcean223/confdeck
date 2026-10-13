//! `Store` 的官方账号部分（10-13，阶段 4）：Codex、Grok Build 存下的几个官方登录、登录文件现在是谁、正在跑的登录。
//! core 那边见 `confdeck_core::account`。
//!
//! - 每次 `Store::reload` 都 `sync` 一遍：登录文件是存过的账号，就把 agent 新换的令牌存回去。
//! - 加账号：在后台跑 agent 自己的 `login` 命令（家目录指到临时目录），它自己开浏览器；登录完存下来。
//! - 选择器或供应商页上选一个账号：这个 agent 不在「默认」就先切回默认，再换登录文件。
//! - 账号的名字和备注（提案 13）只存在 confdeck 的 `accounts.json`。

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use confdeck_core::account::{self, Account, AccountError, Live, Login, LoginHandle};
use confdeck_core::agent::AgentKind;
use confdeck_core::apply;
use gpui_kit::*;
use rust_i18n::t;

use crate::data::{Notice, Selection, Store};

#[derive(Default)]
pub(crate) struct AccountState {
    /// 存下的全部账号，先存的在前。
    pub list: Vec<Account>,
    /// 装了的、能切账号的 agent 的登录文件现在是谁。
    pub live: HashMap<AgentKind, Live>,
    /// Codex 的登录存在钥匙串里：切不了。
    pub blocked: HashSet<AgentKind>,
    /// 正在登录的。
    pub logins: HashMap<AgentKind, Pending>,
    /// `accounts.json` 读不了的原因。
    pub error: Option<String>,
    /// Claude Code 登着的官方账号的资料（邮箱、套餐）。它存不了多个账号。
    pub claude: Option<account::Profile>,
}

/// 一次正在跑的登录。
pub(crate) struct Pending {
    handle: LoginHandle,
    /// 登录命令打出来的地址：浏览器没打开时让人自己点。
    pub url: Option<String>,
    cancelled: bool,
}

impl Store {
    /// 重读存下的账号，看登录文件现在是谁（顺带把 agent 新换的令牌存回去）。`reload` 末尾调。
    pub(crate) fn reload_accounts(&mut self) {
        let mut live = HashMap::new();
        let mut blocked = HashSet::new();
        let mut error = None;
        for agent in self
            .agents
            .iter()
            .filter(|a| a.installed && account::supported(a.kind))
        {
            if account::blocked(&self.home, agent.kind) {
                blocked.insert(agent.kind);
                continue;
            }
            match account::sync(&self.engine, &self.home, agent.kind) {
                Ok(found) => {
                    live.insert(agent.kind, found);
                }
                Err(err) => error = Some(err.to_string()),
            }
        }
        match account::list(self.engine.data()) {
            Ok(list) => self.accounts.list = list,
            Err(err) => {
                self.accounts.list = Vec::new();
                error = Some(err.to_string());
            }
        }
        self.accounts.live = live;
        self.accounts.blocked = blocked;
        self.accounts.error = error;
        let claude = self
            .agents
            .iter()
            .any(|a| a.kind == AgentKind::ClaudeCode && a.installed);
        self.accounts.claude = claude
            .then(|| account::claude_profile(&self.home))
            .flatten();
    }

    /// 这个 agent 存下的账号。
    pub fn accounts_of(&self, kind: AgentKind) -> Vec<&Account> {
        self.accounts
            .list
            .iter()
            .filter(|account| account.agent == kind)
            .collect()
    }

    /// 登录文件现在是谁；没装、切不了的是 `None`。
    pub fn live_account(&self, kind: AgentKind) -> Option<&Live> {
        self.accounts.live.get(&kind)
    }

    pub fn account_blocked(&self, kind: AgentKind) -> bool {
        self.accounts.blocked.contains(&kind)
    }

    pub fn login_pending(&self, kind: AgentKind) -> Option<&Pending> {
        self.accounts.logins.get(&kind)
    }

    /// 胶囊上写的账号：这个 agent 用着官方登录、存过账号时，写登录文件里那个账号的名字，没起名字写邮箱。
    pub fn live_label(&self, agent: usize) -> Option<String> {
        let kind = self.agents[agent].kind;
        if self.accounts_of(kind).is_empty() {
            return None;
        }
        match self.live_account(kind)? {
            Live::Known(id) => {
                let account = self
                    .accounts
                    .list
                    .iter()
                    .find(|account| account.agent == kind && &account.id == id)?;
                account.name.clone().or_else(|| account.email.clone())
            }
            Live::Unknown(identity) => identity.email.clone(),
            Live::LoggedOut => None,
        }
    }

    /// 给账号起名字、写备注（空的就是去掉）。登录文件里还没存的那个先存下。
    pub fn label_account(
        &mut self,
        kind: AgentKind,
        id: &str,
        name: &str,
        note: &str,
        cx: &mut Context<Self>,
    ) {
        let result = account::label(&self.engine, &self.home, kind, id, name, note);
        self.reload();
        if let Err(err) = result {
            let notice = Notice::err(describe(&err));
            self.announce(&[notice], cx);
        } else {
            cx.notify();
        }
    }

    /// 加一个账号：跑 agent 的登录命令，它自己开浏览器。登录完存下来，提示「已添加」。
    pub fn add_account(&mut self, kind: AgentKind, cx: &mut Context<Self>) {
        if self.accounts.logins.contains_key(&kind) {
            return;
        }
        let program = match login_program(&self.home, kind) {
            Ok(program) => program,
            Err(err) => {
                let notice = Notice::err(describe(&err));
                self.announce(&[notice], cx);
                return;
            }
        };
        let login = match Login::start(&program, self.engine.data(), kind) {
            Ok(login) => login,
            Err(err) => {
                let notice = Notice::err(describe(&err));
                self.announce(&[notice], cx);
                return;
            }
        };
        self.accounts.logins.insert(
            kind,
            Pending {
                handle: login.handle(),
                url: None,
                cancelled: false,
            },
        );
        cx.notify();

        // 登录命令过一会儿才打出地址：在等的时候隔一会儿看一眼。
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                let going = this
                    .update(cx, |store, cx| {
                        let Some(pending) = store.accounts.logins.get_mut(&kind) else {
                            return false;
                        };
                        let url = pending.handle.url();
                        if url != pending.url {
                            pending.url = url;
                            cx.notify();
                        }
                        true
                    })
                    .unwrap_or(false);
                if !going {
                    break;
                }
            }
        })
        .detach();

        let engine = self.engine.clone();
        let home = self.home.clone();
        let task = cx.background_executor().spawn(async move {
            let bytes = login.wait()?;
            account::add(&engine, &home, kind, &bytes)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |store, cx| {
                let cancelled = store
                    .accounts
                    .logins
                    .remove(&kind)
                    .is_some_and(|pending| pending.cancelled);
                store.reload();
                let notice = match result {
                    Ok(account) => Notice::ok(t!(
                        "accounts.added",
                        account = account.email.unwrap_or(account.id)
                    )),
                    Err(_) if cancelled => {
                        cx.notify();
                        return;
                    }
                    Err(err) => Notice::err(describe(&err)),
                };
                store.announce(&[notice], cx);
                // 新加的账号还没有额度读数，不查的话一直是占位条，要等下一次刷新（10-13 用户截图里看到的）。
                store.refresh_quotas(cx);
            })
            .ok();
        })
        .detach();
    }

    /// 停掉正在跑的登录。
    pub fn cancel_login(&mut self, kind: AgentKind, cx: &mut Context<Self>) {
        if let Some(pending) = self.accounts.logins.get_mut(&kind) {
            pending.cancelled = true;
            pending.handle.cancel();
            cx.notify();
        }
    }

    /// 选择器里选了一个存下的账号：不在「默认」就先切回默认，再换登录文件。胶囊先改过来，不等写完。
    pub fn use_account(
        &mut self,
        agent: usize,
        id: String,
        cx: &mut Context<Self>,
    ) -> Task<Vec<Notice>> {
        let kind = self.agents[agent].kind;
        let to_default = !matches!(
            (&self.agents[agent].selection, &self.agents[agent].current),
            (
                Selection::Single(None),
                Some(confdeck_core::current::Current::Default)
            )
        );
        let email = self
            .accounts
            .list
            .iter()
            .find(|account| account.agent == kind && account.id == id)
            .and_then(|account| account.name.clone().or_else(|| account.email.clone()))
            .unwrap_or_else(|| id.clone());
        let unchanged = !to_default
            && matches!(self.live_account(kind), Some(Live::Known(live)) if *live == id);
        if unchanged {
            return Task::ready(Vec::new());
        }
        self.agents[agent].selection = Selection::Single(None);
        self.agents[agent].current = Some(confdeck_core::current::Current::Default);
        self.accounts.live.insert(kind, Live::Known(id.clone()));
        cx.notify();

        let engine = self.engine.clone();
        let home = self.home.clone();
        let write = cx.background_executor().spawn(async move {
            if to_default {
                apply::switch(&engine, &home, kind, None).map_err(|err| err.to_string())?;
            }
            account::switch(&engine, &home, kind, &id)
                .map(|_| ())
                .map_err(|err| describe(&err))
        });
        cx.spawn(async move |this, cx| {
            let result = write.await;
            this.update(cx, |store, cx| {
                store.reload();
                let notices = match result {
                    Ok(()) => vec![Notice::ok(t!(
                        "write.switched",
                        agent = kind.to_string(),
                        provider = email
                    ))],
                    Err(error) => vec![Notice::err(t!(
                        "toast.sync_failed",
                        agent = kind.to_string(),
                        error = error
                    ))],
                };
                store.announce(&notices, cx);
                // 额度跟着账号走（Grok 联网查的是登录文件里的账号）。
                store.refresh_remote(true, cx);
                notices
            })
            .unwrap_or_default()
        })
    }

    /// 删掉一个存下的账号。登录文件不动。
    pub fn remove_account(&mut self, kind: AgentKind, id: &str, cx: &mut Context<Self>) {
        let result = account::remove(&self.engine, kind, id);
        self.reload();
        if let Err(err) = result {
            let notice = Notice::err(describe(&err));
            self.announce(&[notice], cx);
        } else {
            cx.notify();
        }
    }
}

/// 登录命令。debug 构建里 `CONFDECK_LOGIN` 可以换成一个假的（截图、测试用，`/tmp/confdeck-tools/fake-login.sh`）。
fn login_program(
    home: &std::path::Path,
    kind: AgentKind,
) -> Result<std::path::PathBuf, AccountError> {
    #[cfg(debug_assertions)]
    if let Some(fake) = std::env::var_os("CONFDECK_LOGIN") {
        return Ok(fake.into());
    }
    account::program(home, kind)
}

fn describe(err: &AccountError) -> String {
    match err {
        AccountError::NoProgram(name) => t!("accounts.no_program", program = name).to_string(),
        AccountError::Login(output) => t!("accounts.login_failed", error = output).to_string(),
        AccountError::Unrecognized => t!("accounts.unrecognized").to_string(),
        other => other.to_string(),
    }
}
