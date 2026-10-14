//! 额度、余额的读数：单独一个 `Entity`，查回来只通知画读数的地方（供应商页、面板、官方卡片、「额度查询」卡片），
//! 不通知整个 Store。查谁、怎么查由 Store 按快照和查询设置列好交过来（[`Store::plan_readings`]），这里排、查、存结果；
//! 排的规则（到点、在查的不重复查、代数）在 core 的 `readings::Schedule`。
//!
//! - **每家供应商、每个官方账号**（core 的 `CheckKey`）各有一份设置（core 的 `quota::Check`，存在 `state.json`）：关掉的不查；
//!   在用的默认 5 分钟自动查一次，不在用的默认不自动查，那一格是个圆形箭头，点了查。
//! - **定时**：算出最早到点的时候再睡，没有自动查的不起定时器。第一次打开面板、切到供应商页以前一个也不查。
//! - 一个查询一个后台任务，共用一个 HTTP 客户端（core 的 `SharedHttp`，代理设置变了才重建）。
//! - 登着的账号查的时候登录文件里已经不是它了（别处切了、退出了），这次不算，发 [`LoginChanged`]，Store 重读一遍，
//!   计划跟着换成现在登着的。
//! - debug 构建里 `CONFDECK_QUOTAS` / `CONFDECK_BALANCES` 指着假数据时用假的查法（[`fake::Fake`]），文件里没有的照常查。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use confdeck_core::account::{self, Live};
use confdeck_core::agent::AgentKind;
use confdeck_core::data::DataDir;
use confdeck_core::net::{Proxy, SharedHttp};
use confdeck_core::quota::{Check, CheckKey};
use confdeck_core::readings::{
    self, Failed, Finished, Found, Net, Planned, Reader, Schedule, State, Subject, Web,
};
use confdeck_core::status::AgentStatus;
use gpui_kit::*;

use crate::data::Store;
use crate::errors::Describe as _;
use crate::quota_view::{Reading, Shown};

/// 登着的账号查的时候已经不是登录文件里那个了：Store 该重读。
pub struct LoginChanged;

pub struct Readings {
    schedule: Schedule,
    reader: Arc<dyn Reader>,
    http: SharedHttp,
    proxy: Proxy,
    /// 打开过面板或供应商页：之后才按到点的时候定时查。
    active: bool,
    /// 睡到下一个到点的时候。
    timer: Option<Task<()>>,
}

impl Readings {
    pub fn new(reader: Arc<dyn Reader>, http: SharedHttp) -> Self {
        Self {
            schedule: Schedule::default(),
            reader,
            http,
            proxy: Proxy::default(),
            active: false,
            timer: None,
        }
    }

    /// 换上 Store 算好的计划和现在的代理设置。计划变了才通知。
    pub(crate) fn set_plan(&mut self, planned: Vec<Planned>, proxy: Proxy, cx: &mut Context<Self>) {
        self.proxy = proxy;
        if self.schedule.plan(planned) {
            cx.notify();
        }
        self.arm(cx);
    }

    /// 到点的自动查（打开面板、切到供应商页、改了查询设置时调，定时器到点也调）。
    pub(crate) fn check_due(&mut self, cx: &mut Context<Self>) {
        self.active = true;
        let due = self.schedule.due(Instant::now());
        self.check(due, cx);
    }

    /// 供应商页点刷新：没关掉的全查。
    pub(crate) fn check_all(&mut self, cx: &mut Context<Self>) {
        let keys = self.schedule.keys();
        self.check(keys, cx);
    }

    /// 只查这一个（行里和「额度查询」卡片里的圆形箭头、切了账号、加了账号、改了查询设置以后）。
    pub(crate) fn check_one(&mut self, key: &CheckKey, cx: &mut Context<Self>) {
        self.check(vec![key.clone()], cx);
    }

    fn check(&mut self, keys: Vec<CheckKey>, cx: &mut Context<Self>) {
        let now = Instant::now();
        let mut started = false;
        for key in keys {
            let Some(ticket) = self.schedule.start(&key, now) else {
                continue;
            };
            started = true;
            let reader = self.reader.clone();
            let net = Net {
                http: self.http.clone(),
                proxy: self.proxy.clone(),
            };
            let task = cx.background_executor().spawn(async move {
                let outcome = reader.read(&ticket, &net);
                (ticket, outcome)
            });
            cx.spawn(async move |this, cx| {
                let (ticket, outcome) = task.await;
                this.update(cx, |this, cx| {
                    match this.schedule.finish(&ticket, outcome) {
                        Finished::Kept => cx.notify(),
                        Finished::LoginChanged => {
                            cx.emit(LoginChanged);
                            cx.notify();
                        }
                        // 作废的那次：界面上没变，计划也已经换过了。
                        Finished::Stale => {}
                    }
                    this.arm(cx);
                })
                .ok();
            })
            .detach();
        }
        if started {
            cx.notify();
        }
        self.arm(cx);
    }

    /// 定时器睡到最早到点的时候；没有自动查的（或者还没打开过面板、供应商页）不起。
    fn arm(&mut self, cx: &mut Context<Self>) {
        let next = if self.active {
            self.schedule.next_due(Instant::now())
        } else {
            None
        };
        self.timer = next.map(|at| {
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(at.saturating_duration_since(Instant::now()))
                    .await;
                this.update(cx, |this, cx| this.check_due(cx)).ok();
            })
        });
    }

    /// 这一个的读数；不在计划里（查不了，比如 Claude 官方）的是 `None`。
    pub fn of(&self, key: &CheckKey) -> Option<Reading<'_>> {
        let state = self.schedule.get(key)?;
        Some(Reading {
            of: key.clone(),
            checking: state.running,
            shown: shown(&state),
        })
    }

    /// 有没有在查的（供应商页的刷新按钮转）。
    pub fn checking_any(&self) -> bool {
        self.schedule.running()
    }
}

impl EventEmitter<LoginChanged> for Readings {}

/// 一个查询对象现在画成什么。
fn shown<'a>(state: &State<'a>) -> Shown<'a> {
    if state.planned.off {
        return Shown::Off;
    }
    match state.last {
        Some(Ok(Found::Quota(found))) => Shown::Quota(found),
        Some(Ok(Found::Balance(found))) => Shown::Balance(found),
        Some(Err(Failed::Quota(err))) => Shown::QuotaFailed(err),
        Some(Err(Failed::Balance(err))) => Shown::BalanceFailed(err),
        // 还没有读数：到点会自动查的是占位条，不自动查的只有箭头。
        None if state.planned.every > 0 => Shown::Waiting,
        None => Shown::Unchecked,
    }
}

impl Store {
    /// 一家供应商、一个账号的额度查询设置；没设过是默认值。
    pub fn check_of(&self, target: &CheckKey) -> Check {
        self.local.check(target)
    }

    /// 改一家供应商、一个账号的额度查询设置，改完按新的设置看一次要不要查。
    pub fn set_check(&mut self, target: &CheckKey, check: Check, cx: &mut Context<Self>) {
        let old = self.check_of(target);
        // 刚打开的、换了查法的：马上查一次，不等间隔（换了查法的旧读数在换计划时丢掉）。
        let now = !check.off && (old.off || old.query != check.query);
        match self.engine.set_check(target, check) {
            Ok(local) => self.set_local(local),
            Err(err) => {
                let notice = crate::data::Notice::err(err.describe());
                self.announce(&[notice], cx);
                return;
            }
        }
        self.plan_readings(cx);
        self.readings.update(cx, |readings, cx| {
            if now {
                readings.check_one(target, cx);
            }
            readings.check_due(cx);
        });
        cx.notify();
    }

    /// 要查的每一个（core 的 `readings::planned`：键、查法、多久查一次）交给 [`Readings`]。
    /// 快照换上、查询设置和代理改了以后调。
    pub(crate) fn plan_readings(&mut self, cx: &mut Context<Self>) {
        let planned = self.reading_plan();
        let proxy = self.proxy();
        self.readings
            .update(cx, |readings, cx| readings.set_plan(planned, proxy, cx));
    }

    /// 能查的每一个：装了的 agent 登着的官方账号、存下的官方账号（登着的那个上面已经有了）、每家供应商。
    fn reading_plan(&self) -> Vec<Planned> {
        let mut subjects: Vec<Subject> = Vec::new();
        for agent in self.agents.iter().filter(|a| a.installed()) {
            let account = match self.live_account(agent.kind) {
                Some(Live::Known(id)) => Some(id.as_str()),
                Some(Live::Unknown(identity)) => Some(identity.id.as_str()),
                _ => None,
            };
            subjects.push(Subject::Live(agent.kind, account));
        }
        for account in &self.accounts.read.list {
            let kind = account.agent;
            let installed = self.agents.iter().any(|a| a.kind == kind && a.installed());
            let live =
                matches!(self.live_account(kind), Some(Live::Known(id)) if *id == account.id);
            if !installed || live || self.account_blocked(kind) || !account::supported(kind) {
                continue;
            }
            subjects.push(Subject::Stored(kind, &account.id));
        }
        subjects.extend(self.providers.iter().map(Subject::Provider));
        subjects
            .into_iter()
            .filter_map(|subject| {
                let key = subject.key();
                readings::planned(subject, &self.check_of(&key), self.in_use(&key))
            })
            .collect()
    }

    /// 这家、这个账号算不算在用：有 agent 选着的供应商，登录文件里登着的官方账号（agent 暂时在用第三方也算）。
    pub fn in_use(&self, target: &CheckKey) -> bool {
        match target {
            CheckKey::Provider(id) => self
                .providers
                .iter()
                .position(|p| &p.id == id)
                .is_some_and(|i| !self.users_of(i).is_empty()),
            CheckKey::Account(kind, id) => match self.live_account(*kind) {
                Some(Live::Known(live)) => live == id,
                Some(Live::Unknown(identity)) => &identity.id == id,
                _ => false,
            },
            CheckKey::Login(_) => true,
        }
    }

    /// 登录文件里那个官方账号的设置、读数在哪个键下：认得出是谁就是那个账号，认不出是 `Login`。
    pub fn live_target(&self, kind: AgentKind) -> CheckKey {
        match self.live_account(kind) {
            Some(Live::Known(id)) => CheckKey::Account(kind, id.clone()),
            Some(Live::Unknown(identity)) => CheckKey::Account(kind, identity.id.clone()),
            _ => CheckKey::Login(kind),
        }
    }

    /// 这个 agent 现在选的那家的键：单选的、选的是列表里的哪家或者官方登录时才有。
    pub fn selection_key(&self, agent: usize) -> Option<CheckKey> {
        let row = &self.agents[agent];
        match &row.status {
            AgentStatus::Provider { .. } => {
                let provider = &self.providers[self.picked(agent)?];
                Some(CheckKey::Provider(provider.id.clone()))
            }
            AgentStatus::Official => Some(self.live_target(row.kind)),
            _ => None,
        }
    }
}

/// 用哪种查法：平时是正式的；debug 构建里指了假数据的是 [`Fake`]。
pub(crate) fn reader(home: PathBuf, data: DataDir) -> Arc<dyn Reader> {
    let web = Web { home, data };
    #[cfg(debug_assertions)]
    if std::env::var_os("CONFDECK_QUOTAS").is_some()
        || std::env::var_os("CONFDECK_BALANCES").is_some()
    {
        return Arc::new(fake::Fake { web });
    }
    Arc::new(web)
}

#[cfg(debug_assertions)]
mod fake {
    use std::time::Duration;

    use confdeck_core::balance::{Balance, BalanceError, Currency};
    use confdeck_core::net::{NetError, Unreachable};
    use confdeck_core::quota::{Quota, QuotaError, Span, Window};
    use confdeck_core::readings::{Failed, Found, Method, Net, Outcome, Reader, Ticket, Web};

    /// 截图用的假查法：文件里有的给假的（等 300 毫秒，像在查），没有的照常查。
    pub(super) struct Fake {
        pub web: Web,
    }

    impl Reader for Fake {
        fn read(&self, ticket: &Ticket, net: &Net) -> Option<Outcome> {
            let faked = match &ticket.method {
                Method::Custom(provider, _) | Method::Balance(provider) => {
                    fake_balance(&provider.id)
                        .map(|found| found.map(Found::Balance).map_err(Failed::Balance))
                }
                Method::Plan(provider) => fake_quota(&provider.id),
                Method::Live(kind) => fake_quota(kind.id()),
                Method::Stored(_, id) => fake_quota(&format!("account:{id}")),
            };
            faked.or_else(|| self.web.read(ticket, net))
        }
    }

    /// 假余额：`CONFDECK_BALANCES` 指一个 JSON 文件，`{"供应商 ID": {"left": 86.4, "currency": "cny"}}`，
    /// `{"used": 12.3}` 是 OpenRouter 不限额的 Key，`{"error": "auth"}` 是 Key 不对，`{"error": "net"}` 是连不上。
    fn fake_balance(id: &str) -> Option<Result<Balance, BalanceError>> {
        let path = std::env::var_os("CONFDECK_BALANCES")?;
        let all: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
        let fake = all.get(id)?;
        std::thread::sleep(Duration::from_millis(300));
        Some(match fake["error"].as_str() {
            Some("auth") => Err(BalanceError::Auth),
            Some(_) => Err(BalanceError::Net(NetError::Unreachable(Unreachable::Dns))),
            None => Ok(Balance {
                left: fake["left"].as_f64(),
                used: fake["used"].as_f64(),
                currency: if fake["currency"] == "cny" {
                    Currency::Cny
                } else {
                    Currency::Usd
                },
            }),
        })
    }

    /// 假额度：`CONFDECK_QUOTAS` 指一个 JSON 文件，键是 agent 的 ID（`codex`、`grok-build`，登着的账号）、
    /// `account:<账号 ID>`（存下的账号）或供应商 ID，`{"plan": "max", "windows": [{"span": "5h", "used": 23, "resets_in": 95}]}`
    /// （`span` 是 `5h`、`week`、`month`，别的字写成「每周 · 那个模型」；`resets_in` 是分钟），
    /// `{"error": "expired" | "auth" | "none" | "net" | "login"}`。
    fn fake_quota(key: &str) -> Option<Outcome> {
        let path = std::env::var_os("CONFDECK_QUOTAS")?;
        let all: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
        let fake = all.get(key)?;
        std::thread::sleep(Duration::from_millis(300));
        let now = chrono::Local::now().timestamp_millis();
        let found = match fake["error"].as_str() {
            Some("expired") => Err(QuotaError::Expired),
            Some("auth") => Err(QuotaError::Auth),
            Some("none") => Err(QuotaError::NoPlan),
            Some("login") => Err(QuotaError::NotLoggedIn),
            Some(_) => Err(QuotaError::Net(NetError::Unreachable(Unreachable::Dns))),
            None => Ok(Quota {
                plan: fake["plan"].as_str().map(str::to_string),
                windows: fake["windows"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|w| Window {
                        span: match w["span"].as_str().unwrap_or_default() {
                            "5h" => Span::FiveHours,
                            "week" => Span::Week,
                            "month" => Span::Month,
                            model => Span::WeekOf(model.to_string()),
                        },
                        used: w["used"].as_f64().unwrap_or(0.),
                        resets_at: w["resets_in"].as_i64().map(|m| now + m * 60_000),
                    })
                    .collect(),
            }),
        };
        Some(found.map(Found::Quota).map_err(Failed::Quota))
    }
}

#[cfg(test)]
mod tests {
    use confdeck_core::agent::AgentKind;
    use confdeck_core::balance::{Balance, BalanceError, Currency};
    use confdeck_core::quota::CheckKey;
    use confdeck_core::readings::{Failed, Found, Method, Outcome, Planned, State};

    use super::shown;
    use crate::quota_view::Shown;

    fn planned(every: u32, off: bool) -> Planned {
        Planned {
            key: CheckKey::Login(AgentKind::Codex),
            method: Method::Live(AgentKind::Codex),
            every,
            off,
        }
    }

    #[test]
    fn what_a_reading_is_drawn_as() {
        let state = |planned, last| State {
            planned,
            running: false,
            last,
        };
        let auto = planned(5, false);
        let manual = planned(0, false);
        let off = planned(5, true);
        // 没有读数：自动查的是占位条，不自动查的只有箭头。
        assert!(matches!(shown(&state(&auto, None)), Shown::Waiting));
        assert!(matches!(shown(&state(&manual, None)), Shown::Unchecked));
        let balance: Outcome = Ok(Found::Balance(Balance {
            left: Some(1.),
            used: None,
            currency: Currency::Usd,
        }));
        let failed: Outcome = Err(Failed::Balance(BalanceError::Auth));
        assert!(matches!(
            shown(&state(&manual, Some(&balance))),
            Shown::Balance(_)
        ));
        assert!(matches!(
            shown(&state(&auto, Some(&failed))),
            Shown::BalanceFailed(BalanceError::Auth)
        ));
        // 关掉的：读数留着也不画。
        assert!(matches!(shown(&state(&off, Some(&balance))), Shown::Off));
    }
}
