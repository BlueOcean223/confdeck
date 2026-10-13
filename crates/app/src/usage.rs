//! 用量的数据：后台扫各 agent 的会话记录（core 的 `usage`），算好四个时间段的汇总放在 `Store` 里，用量页只管画。
//!
//! - 打开用量页、点顶栏的刷新时扫一次；扫的时候再点不重复扫。第一次要读全部历史（本机 release 约 6 秒），之后几十毫秒。
//! - 汇总一次算好四个时间段和热力图，切时间段不用等。算完把请求丢掉，只留汇总。
//! - 会话页的会话列表也从这里来：扫的时候顺带记下的（core 的 `session`），文件已经不在的不要。
//! - 价格用 models.dev 时只读磁盘上的缓存，不联网（供应商页拉列表、补全时会下）。
//! - 余额（core 的 `balance`）和订阅额度（core 的 `quota`）画在供应商页、菜单栏面板（10-13 起不在用量页）。
//!   每家供应商、每个官方账号（[`CheckOf`]）各有一份设置（core 的 `quota::Check`，存在 `state.json`）：关掉的不查；
//!   自动查的到点就查（[`Store::check_due`]：每 30 秒看一次，打开面板、切到供应商页时也看）；在用的默认 5 分钟，
//!   不在用的默认不自动查，那一格是个圆形箭头，点了查。供应商页点刷新是没关掉的全查（[`Store::check_all`]）。
//!   一家一个后台任务，重查时先留着上次的数，查回来再换。存下的官方账号也各查各的（`QuotaOf::Account`）。
//! - Claude 官方的额度先不查（10-13 用户说先不做），官方那一行的额度一栏空着。

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use confdeck_core::account::{self, Live};
use confdeck_core::agent::AgentKind;
use confdeck_core::balance::{self, Balance, BalanceError};
use confdeck_core::current::Current;
use confdeck_core::fsx;
use confdeck_core::models_dev::{self, ModelsDev};
use confdeck_core::net::Http;
use confdeck_core::provider::Provider;
use confdeck_core::query::{self, Query};
use confdeck_core::quota::{self, Check, Quota, QuotaError};
use confdeck_core::session::Session;
use confdeck_core::usage::history::{self, Target};
use confdeck_core::usage::summary::{self, Context as Pricing, Heatmap, Period, Source, Summary};
use confdeck_core::usage::{Db, UsageError};
use gpui_kit::*;

use crate::data::{Selection, Store};

pub const PERIODS: [Period; 4] = [Period::Today, Period::Week, Period::Month, Period::All];

#[derive(Default)]
pub struct UsageState {
    /// 第一次扫完之前是 `None`。
    pub loaded: Option<Arc<Loaded>>,
    pub scanning: bool,
    /// 用量库打不开、读不了。
    pub error: Option<String>,
    /// 供应商 ID → 余额。还在查、没查过的没有。
    pub balances: HashMap<String, Result<Balance, BalanceError>>,
    /// 正在查余额的供应商 ID。
    pub checking: Vec<String>,
    /// Claude、Codex、Grok Build 官方和各家套餐的额度。还在查、没查过的没有。
    pub quotas: HashMap<QuotaOf, Result<Quota, QuotaError>>,
    pub quota_checking: Vec<QuotaOf>,
    /// 每家、每个账号上次开始查的时候。
    checked_at: HashMap<CheckOf, Instant>,
    /// 额度查询的设置（`state.json` 的 `checks`），`Store::reload` 时重读。
    pub checks: BTreeMap<String, Check>,
    /// 定时查的循环起了没有。
    ticking: bool,
}

/// 额度查询的设置管的是谁：一家供应商，或一个官方账号。
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum CheckOf {
    Provider(String),
    /// 官方账号（账号 ID，登着的和存下的都是）。
    Account(AgentKind, String),
    /// 登录文件里认不出是谁（没登录、Codex 的登录在钥匙串里）：照样查一次，好写「未登录」。
    Login(AgentKind),
}

impl CheckOf {
    /// `state.json` 里 `checks` 的键。
    pub fn key(&self) -> String {
        match self {
            CheckOf::Provider(id) => format!("provider:{id}"),
            CheckOf::Account(kind, id) => format!("account:{}:{id}", kind.id()),
            CheckOf::Login(kind) => format!("login:{}", kind.id()),
        }
    }
}

/// 查什么。
enum Job {
    Quota(QuotaOf, QuotaJob),
    /// 余额：认得的几家（`None`），或者按自己配的查法。
    Balance(Box<Provider>, Option<Query>),
}

/// 每隔多久看一次有没有到点该查的。
const TICK: Duration = Duration::from_secs(30);

/// 谁的额度。
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum QuotaOf {
    /// 官方订阅（Claude Code、Codex、Grok Build 的登录）。
    Agent(AgentKind),
    /// 套餐（供应商 ID）。
    Provider(String),
    /// 存下的一个官方账号（不是登着的那个，登着的算 `Agent`）。
    Account(AgentKind, String),
}

/// 后台查一家额度。
type QuotaJob = Box<dyn FnOnce(&Http) -> Result<Quota, QuotaError> + Send>;

pub struct Loaded {
    /// 和 [`PERIODS`] 一一对应。
    pub summaries: Vec<Summary>,
    /// 最近 53 周每天的合计（「全部」的热力图）。
    pub heatmap: Heatmap,
    /// 供应商 ID → 名字：现在的，删了的从切换记录里找。
    pub names: HashMap<String, String>,
    /// 全部会话，最近活动的在前。
    pub sessions: Vec<Session>,
}

impl Loaded {
    pub fn summary(&self, period: Period) -> &Summary {
        let index = PERIODS.iter().position(|p| *p == period).unwrap_or(0);
        &self.summaries[index]
    }
}

impl Store {
    /// 扫新加的会话记录、重算汇总。正在扫就不再扫。
    pub fn refresh_usage(&mut self, cx: &mut Context<Self>) {
        if self.usage.scanning {
            return;
        }
        self.usage.scanning = true;
        cx.notify();
        let data = self.engine.data().clone();
        let home = self.home.clone();
        let providers = self.providers.clone();
        let cached = self.net.models_dev.clone();
        let task = cx.background_executor().spawn(async move {
            let dev = cached.or_else(|| {
                let bytes = fsx::read_optional(&models_dev::cache_path(&data)).ok()??;
                ModelsDev::parse(&bytes).ok().map(Arc::new)
            });
            let mut db = Db::open(&data.usage_db())?;
            confdeck_core::usage::scan(&home, &mut db);
            let requests = db.requests(None)?;
            let switches = history::load(&data)?;
            let mut sessions = db.sessions()?;
            sessions.retain(|session| session.path.exists());
            let pricing = Pricing {
                providers: &providers,
                history: &switches,
                dev: dev.as_deref(),
            };
            let now = chrono::Local::now();
            let summaries: Vec<Summary> = PERIODS
                .iter()
                .map(|period| summary::summarize(&requests, &pricing, *period, &now))
                .collect();
            let heatmap = summary::heatmap(&requests, &pricing, &now);
            let names = summaries
                .iter()
                .flat_map(|summary| &summary.sources)
                .filter_map(|(source, _)| match source {
                    Source::Provider(id) => Some((id.clone(), pricing.provider_name(id)?)),
                    _ => None,
                })
                .collect();
            Ok::<_, UsageError>((
                Loaded {
                    summaries,
                    heatmap,
                    names,
                    sessions,
                },
                dev,
            ))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |store, cx| {
                store.usage.scanning = false;
                match result {
                    Ok((loaded, dev)) => {
                        store.usage.loaded = Some(Arc::new(loaded));
                        store.usage.error = None;
                        if store.net.models_dev.is_none() {
                            store.net.models_dev = dev;
                        }
                    }
                    Err(err) => store.usage.error = Some(err.to_string()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 一家供应商、一个账号的额度查询设置；没设过是默认值。
    pub fn check_of(&self, target: &CheckOf) -> Check {
        self.usage
            .checks
            .get(&target.key())
            .cloned()
            .unwrap_or_default()
    }

    /// 改一家供应商、一个账号的额度查询设置，改完按新的设置看一次要不要查。
    pub fn set_check(&mut self, target: &CheckOf, check: Check, cx: &mut Context<Self>) {
        let old = self.check_of(target);
        // 刚打开的、换了查法的：马上查一次，不等间隔。换了查法的，旧的读数不留。
        let now = !check.off && (old.off || old.query != check.query);
        if old.query != check.query
            && let CheckOf::Provider(id) = target
        {
            self.usage.balances.remove(id);
            self.usage.quotas.remove(&QuotaOf::Provider(id.clone()));
        }
        if let Err(err) = self.engine.set_check(&target.key(), check) {
            let notice = crate::data::Notice::err(err.to_string());
            self.announce(&[notice], cx);
            return;
        }
        self.usage.checks = self.engine.checks();
        if now {
            self.check_one(target, cx);
        }
        self.check_due(cx);
        cx.notify();
    }

    /// 这家、这个账号算不算在用：有 agent 选着的供应商，登录文件里登着的官方账号（agent 暂时在用第三方也算）。
    pub fn in_use(&self, target: &CheckOf) -> bool {
        match target {
            CheckOf::Provider(id) => self
                .providers
                .iter()
                .position(|p| &p.id == id)
                .is_some_and(|i| !self.users_of(i).is_empty()),
            CheckOf::Account(kind, id) => match self.live_account(*kind) {
                Some(Live::Known(live)) => live == id,
                Some(Live::Unknown(identity)) => &identity.id == id,
                _ => false,
            },
            CheckOf::Login(_) => true,
        }
    }

    /// 登录文件里那个官方账号的设置管它：认得出是谁就是那个账号，认不出是 `Login`。
    pub fn live_target(&self, kind: AgentKind) -> CheckOf {
        match self.live_account(kind) {
            Some(Live::Known(id)) => CheckOf::Account(kind, id.clone()),
            Some(Live::Unknown(identity)) => CheckOf::Account(kind, identity.id.clone()),
            _ => CheckOf::Login(kind),
        }
    }

    /// 起定时查的循环（第一次打开面板或主窗口时调，之后调了也只起一次）。
    pub(crate) fn start_checks(&mut self, cx: &mut Context<Self>) {
        if self.usage.ticking {
            return;
        }
        self.usage.ticking = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                if this.update(cx, |store, cx| store.check_due(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// 到点的自动查：自动查的间隔不是 0、离上次查过了这么久的。
    pub(crate) fn check_due(&mut self, cx: &mut Context<Self>) {
        self.start_checks(cx);
        let now = Instant::now();
        self.run_checks(cx, |store, target| {
            let every = store.check_of(target).every(store.in_use(target));
            every > 0
                && store.usage.checked_at.get(target).is_none_or(|at| {
                    now.duration_since(*at) >= Duration::from_secs(u64::from(every) * 60)
                })
        });
    }

    /// 供应商页点刷新：没关掉的全查。
    pub(crate) fn check_all(&mut self, cx: &mut Context<Self>) {
        self.run_checks(cx, |_, _| true);
    }

    /// 只查这一个（行里和「额度查询」卡片里的圆形箭头、切了账号、加了账号以后）。
    pub(crate) fn check_one(&mut self, target: &CheckOf, cx: &mut Context<Self>) {
        self.run_checks(cx, |_, t| t == target);
    }

    /// 能查的每一个：供应商的套餐额度或余额、官方登录、存下的账号。Claude 官方的先不查（10-13 用户说先不做）。
    fn jobs(&self) -> Vec<(CheckOf, Job)> {
        let mut jobs: Vec<(CheckOf, Job)> = Vec::new();
        for agent in self.agents.iter().filter(|a| a.installed) {
            let home = self.home.clone();
            let job: QuotaJob = match agent.kind {
                AgentKind::Codex => Box::new(move |http| quota::codex(http, &home)),
                AgentKind::GrokBuild => Box::new(move |http| quota::grok(http, &home)),
                _ => continue,
            };
            jobs.push((
                self.live_target(agent.kind),
                Job::Quota(QuotaOf::Agent(agent.kind), job),
            ));
        }
        // 存下的官方账号各查各的：用存着的登录。登着的那个上面已经查了。
        for account in &self.accounts.list {
            let kind = account.agent;
            let installed = self.agents.iter().any(|a| a.kind == kind && a.installed);
            let live =
                matches!(self.live_account(kind), Some(Live::Known(id)) if *id == account.id);
            if !installed || live || self.account_blocked(kind) || !account::supported(kind) {
                continue;
            }
            let data = self.engine.data().clone();
            let id = account.id.clone();
            let job: QuotaJob = Box::new(move |http| {
                let login =
                    account::stored_login(&data, kind, &id).map_err(|_| QuotaError::NotLoggedIn)?;
                match kind {
                    AgentKind::Codex => quota::codex_login(http, &login),
                    AgentKind::GrokBuild => quota::grok_login(http, &login),
                    _ => Err(QuotaError::NotLoggedIn),
                }
            });
            jobs.push((
                CheckOf::Account(kind, account.id.clone()),
                Job::Quota(QuotaOf::Account(kind, account.id.clone()), job),
            ));
        }
        for provider in &self.providers {
            let target = CheckOf::Provider(provider.id.clone());
            if let Some(query) = self.custom_query(provider) {
                jobs.push((
                    target,
                    Job::Balance(Box::new(provider.clone()), Some(query)),
                ));
            } else if quota::plan_supported(provider) {
                let found = provider.clone();
                jobs.push((
                    target,
                    Job::Quota(
                        QuotaOf::Provider(provider.id.clone()),
                        Box::new(move |http| quota::plan(http, &found)),
                    ),
                ));
            } else if balance::supported(provider) {
                jobs.push((target, Job::Balance(Box::new(provider.clone()), None)));
            }
        }
        jobs
    }

    /// 查 `pick` 选中的、没关掉的、没在查的。
    fn run_checks(&mut self, cx: &mut Context<Self>, pick: impl Fn(&Self, &CheckOf) -> bool) {
        let proxy = self.proxy();
        let now = Instant::now();
        for (target, job) in self.jobs() {
            if self.check_of(&target).off || !pick(self, &target) {
                continue;
            }
            match job {
                Job::Quota(of, job) => {
                    if self.usage.quota_checking.contains(&of) {
                        continue;
                    }
                    self.usage.checked_at.insert(target, now);
                    self.usage.quota_checking.push(of.clone());
                    let proxy = proxy.clone();
                    let task = cx.background_executor().spawn(async move {
                        #[cfg(debug_assertions)]
                        if let Some(result) = fake_quota(&of) {
                            return (of, result);
                        }
                        let result = Http::new(&proxy)
                            .map_err(QuotaError::Net)
                            .and_then(|http| job(&http));
                        (of, result)
                    });
                    cx.spawn(async move |this, cx| {
                        let (of, result) = task.await;
                        this.update(cx, |store, cx| {
                            store
                                .usage
                                .quota_checking
                                .retain(|checking| checking != &of);
                            store.usage.quotas.insert(of, result);
                            cx.notify();
                        })
                        .ok();
                    })
                    .detach();
                }
                Job::Balance(provider, custom) => {
                    if self.usage.checking.contains(&provider.id) {
                        continue;
                    }
                    self.usage.checked_at.insert(target, now);
                    self.usage.checking.push(provider.id.clone());
                    let proxy = proxy.clone();
                    let task = cx.background_executor().spawn(async move {
                        #[cfg(debug_assertions)]
                        if let Some(result) = fake_balance(&provider.id) {
                            return (provider.id, result);
                        }
                        let result =
                            Http::new(&proxy)
                                .map_err(BalanceError::Net)
                                .and_then(|http| match &custom {
                                    Some(custom) => query::run(&http, &provider, custom),
                                    None => balance::query(&http, &provider),
                                });
                        (provider.id, result)
                    });
                    cx.spawn(async move |this, cx| {
                        let (id, result) = task.await;
                        this.update(cx, |store, cx| {
                            store.usage.checking.retain(|checking| checking != &id);
                            store.usage.balances.insert(id, result);
                            cx.notify();
                        })
                        .ok();
                    })
                    .detach();
                }
            }
        }
        cx.notify();
    }

    /// 这家供应商自己配的查法（填够了的）。
    pub fn custom_query(&self, provider: &Provider) -> Option<Query> {
        self.check_of(&CheckOf::Provider(provider.id.clone()))
            .query
            .filter(Query::complete)
    }

    /// 有没有哪家、哪个账号在查（供应商页的刷新按钮转）。
    pub fn checking_any(&self) -> bool {
        !self.usage.checking.is_empty() || !self.usage.quota_checking.is_empty()
    }

    /// 这一个现在是不是在查。
    pub fn checking_target(&self, target: &CheckOf) -> bool {
        match target {
            CheckOf::Provider(id) => {
                self.usage.checking.contains(id)
                    || self
                        .usage
                        .quota_checking
                        .contains(&QuotaOf::Provider(id.clone()))
            }
            CheckOf::Account(kind, _) if self.in_use(target) => {
                self.usage.quota_checking.contains(&QuotaOf::Agent(*kind))
            }
            CheckOf::Account(kind, id) => self
                .usage
                .quota_checking
                .contains(&QuotaOf::Account(*kind, id.clone())),
            CheckOf::Login(kind) => self.usage.quota_checking.contains(&QuotaOf::Agent(*kind)),
        }
    }

    /// 单选 agent 现在连的是哪家，记进切换记录（和上一笔一样不记）。在别处改了配置，用量也能对上是哪家。
    pub(crate) fn note_selections(&self) {
        for agent in &self.agents {
            let (Selection::Single(_), true) = (&agent.selection, agent.installed) else {
                continue;
            };
            let target = match &agent.current {
                Some(Current::Default) => Target::Official,
                Some(Current::Provider(id)) | Some(Current::KeyMismatch { provider: id }) => {
                    match self.providers.iter().find(|p| &p.id == id) {
                        Some(provider) => Target::of(Some(provider)),
                        None => continue,
                    }
                }
                Some(Current::Custom { .. }) => Target::Other,
                // 配置读不了：不知道连的是哪家，不记。
                None => continue,
            };
            // 记不下来只是用量页少一条线索。
            let _ = history::note(self.engine.data(), agent.kind, target);
        }
    }
}

/// 官方登录是哪家的预设（名字、图标用）。
pub fn vendor(agent: AgentKind) -> Option<&'static str> {
    crate::official::vendor(agent).map(|(preset, _)| preset)
}

/// 截图用的假余额（debug 构建）：`CONFDECK_BALANCES` 指一个 JSON 文件，
/// `{"供应商 ID": {"left": 86.4, "currency": "cny"}}`，`{"used": 12.3}` 是 OpenRouter 不限额的 Key，
/// `{"error": "auth"}` 是 Key 不对，`{"error": "net"}` 是连不上。文件里没有的照常联网查。
#[cfg(debug_assertions)]
fn fake_balance(id: &str) -> Option<Result<Balance, BalanceError>> {
    use confdeck_core::balance::Currency;
    use confdeck_core::net::{NetError, Unreachable};
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

/// 截图用的假额度（debug 构建）：`CONFDECK_QUOTAS` 指一个 JSON 文件，键是 agent 的 ID（`claude-code`、`grok-build`）或供应商 ID，
/// `{"plan": "max", "windows": [{"span": "5h", "used": 23, "resets_in": 95}]}`（`span` 是 `5h`、`week`、`month`、
/// 别的字写成「每周 · 那个模型」；`resets_in` 是分钟），`{"error": "expired" | "auth" | "none" | "net" | "login"}`。
/// 文件里没有的照常查。
#[cfg(debug_assertions)]
fn fake_quota(of: &QuotaOf) -> Option<Result<Quota, QuotaError>> {
    use confdeck_core::net::{NetError, Unreachable};
    use confdeck_core::quota::{Span, Window};
    let path = std::env::var_os("CONFDECK_QUOTAS")?;
    let all: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let account;
    let key = match of {
        QuotaOf::Agent(agent) => agent.id(),
        QuotaOf::Provider(id) => id.as_str(),
        QuotaOf::Account(_, id) => {
            account = format!("account:{id}");
            account.as_str()
        }
    };
    let fake = all.get(key)?;
    std::thread::sleep(Duration::from_millis(300));
    let now = chrono::Local::now().timestamp_millis();
    Some(match fake["error"].as_str() {
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
    })
}
