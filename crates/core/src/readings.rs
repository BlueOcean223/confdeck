//! 额度、余额的读数（arch-perf R3）：查谁（[`CheckKey`]）、用哪种查法（[`reader_for`]）、怎么查（[`Reader`]，正式的是 [`Web`]），
//! 什么时候查、查回来的收不收（[`Schedule`]：只记状态、不碰网络和文件，界面拿它排）。
//!
//! - **查法**按这个顺序挑：供应商自己配的（余额，[`crate::query`]）> 认得的套餐额度（[`quota::plan`]）> 认得的余额
//!   （[`balance::query`]）；官方账号用登录查，登着的用登录文件里的，存下的用存着的。Claude 官方的额度先不查（10-13 用户说先不做）。
//! - **登着的账号的读数按账号存**（和存下的一样是 `CheckKey::Account`）：切了账号，新账号的读数不会是上一个的。
//!   登录文件查的时候已经换了人，查到的不算（[`Reader::read`] 返回 `None`）。
//! - **代数**：一个查询对象的查法变了（改了自己配的查法、地址、Key，账号从登着的变成存下的），代数加一。之前开始的查询回来不收，
//!   新的查询不被「已经在查」挡掉。读数的意思变了的（不是同一个账号换个登录来源）旧读数也丢掉，马上再查。
//!   供应商改个名字、改模型列表不算：查的时候用不到。
//! - 键由查询对象定（[`Subject::key`]），计划里的一项由 [`planned`] 一处算：登着的官方账号的键一定是 `Account` 或 `Login`，
//!   查的时候按键核对登录文件里是不是这个账号。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::account;
use crate::agent::AgentKind;
use crate::balance::{self, Balance, BalanceError};
use crate::data::DataDir;
use crate::net::{Http, NetError, Proxy, SharedHttp};
use crate::provider::Provider;
use crate::query::{self, Query};
use crate::quota::{self, Check, CheckKey, Quota, QuotaError};

/// 查到的。
#[derive(Debug, Clone, PartialEq)]
pub enum Found {
    Quota(Quota),
    Balance(Balance),
}

/// 查不到的原因：查额度的、查余额的各是各的。
#[derive(Debug, Clone, PartialEq)]
pub enum Failed {
    Quota(QuotaError),
    Balance(BalanceError),
}

pub type Outcome = Result<Found, Failed>;

/// 查法。
#[derive(Debug, Clone, PartialEq)]
pub enum Method {
    /// 供应商自己配的查法：查回来的是余额。
    Custom(Box<Provider>, Query),
    /// 认得的套餐额度。
    Plan(Box<Provider>),
    /// 认得的余额。
    Balance(Box<Provider>),
    /// 登着的官方账号：用登录文件里的登录。
    Live(AgentKind),
    /// 存下的官方账号：用存着的登录。
    Stored(AgentKind, String),
}

impl Method {
    /// 查回来的是额度（不然是余额）。
    fn quota(&self) -> bool {
        !matches!(self, Self::Custom(..) | Self::Balance(_))
    }

    /// 和 `other` 查的是同一样、查法也一样：供应商比查的时候用得到的（地址、Key、预设），名字、模型列表不比。
    fn reads_same(&self, other: &Self) -> bool {
        let same = |a: &Provider, b: &Provider| {
            a.key == b.key && a.endpoints == b.endpoints && a.preset == b.preset
        };
        match (self, other) {
            (Self::Custom(a, x), Self::Custom(b, y)) => x == y && same(a, b),
            (Self::Plan(a), Self::Plan(b)) | (Self::Balance(a), Self::Balance(b)) => same(a, b),
            _ => self == other,
        }
    }

    /// 换成 `other` 以后，之前的读数还是这个对象的：同一个账号换了登录来源（登着的、存下的）。
    fn keeps_reading(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (
                Self::Live(_) | Self::Stored(..),
                Self::Live(_) | Self::Stored(..)
            )
        )
    }

    /// 连不上（建不了客户端）时报的错。
    fn unreachable(&self, err: NetError) -> Failed {
        if self.quota() {
            Failed::Quota(QuotaError::Net(err))
        } else {
            Failed::Balance(BalanceError::Net(err))
        }
    }
}

/// 查的是谁。
pub enum Subject<'a> {
    Provider(&'a Provider),
    /// 登录文件里那个官方账号：认得出是谁时带账号 ID。
    Live(AgentKind, Option<&'a str>),
    /// 存下的一个官方账号（账号 ID）。
    Stored(AgentKind, &'a str),
}

impl Subject<'_> {
    /// 它的设置、读数存在哪个键下：登着的账号认得出是谁时和存下的一样按账号，认不出是 `Login`。
    pub fn key(&self) -> CheckKey {
        match *self {
            Self::Provider(provider) => CheckKey::Provider(provider.id.clone()),
            Self::Live(kind, Some(id)) | Self::Stored(kind, id) => {
                CheckKey::Account(kind, id.into())
            }
            Self::Live(kind, None) => CheckKey::Login(kind),
        }
    }
}

/// 计划里的一项：键、查法、多久自动查一次（`in_use`：在用的默认自动查）、关没关。查不了是 `None`。
pub fn planned(subject: Subject, check: &Check, in_use: bool) -> Option<Planned> {
    let key = subject.key();
    Some(Planned {
        method: reader_for(subject, check)?,
        key,
        every: check.every(in_use),
        off: check.off,
    })
}

/// 用哪种查法；查不了是 `None`。`check` 是这一个的查询设置（自己配的查法在里面）。
pub fn reader_for(subject: Subject, check: &Check) -> Option<Method> {
    match subject {
        Subject::Provider(provider) => {
            if let Some(query) = check.query.clone().filter(Query::complete) {
                Some(Method::Custom(Box::new(provider.clone()), query))
            } else if quota::plan_supported(provider) {
                Some(Method::Plan(Box::new(provider.clone())))
            } else if balance::supported(provider) {
                Some(Method::Balance(Box::new(provider.clone())))
            } else {
                None
            }
        }
        Subject::Live(kind, _) => official(kind).then_some(Method::Live(kind)),
        Subject::Stored(kind, id) => official(kind).then(|| Method::Stored(kind, id.to_string())),
    }
}

/// 这个 agent 的官方账号查不查额度：Codex、Grok Build 查；Claude 官方先不查（10-13 用户说先不做）。
pub fn official(kind: AgentKind) -> bool {
    matches!(kind, AgentKind::Codex | AgentKind::GrokBuild)
}

/// 「⋯」里有没有「额度查询」：供应商都有（认不出的可以自己配），官方账号要能查（Claude 官方没有）。
pub fn checkable(key: &CheckKey) -> bool {
    match key {
        CheckKey::Provider(_) => true,
        CheckKey::Account(kind, _) | CheckKey::Login(kind) => official(*kind),
    }
}

/// 认得这家的查法（套餐额度、余额）：「额度查询」卡片里有没有「内置」。
pub fn built_in(provider: &Provider) -> bool {
    quota::plan_supported(provider) || balance::supported(provider)
}

// ── 查 ──────────────────────────────────────────────

/// 联网用的：共用的客户端和现在的代理设置。
pub struct Net {
    pub http: SharedHttp,
    pub proxy: Proxy,
}

impl Net {
    pub fn http(&self) -> Result<Arc<Http>, NetError> {
        self.http.get(&self.proxy)
    }
}

/// 查法的实现。正式的走 HTTP（[`Web`]）；界面截图用的假数据是另一个（在 app 里）。
pub trait Reader: Send + Sync {
    /// 查 `ticket` 那一个。`None`：登着的账号查的时候登录文件里已经不是它了（换了人、退出了），查到的不算，
    /// 界面该重读一遍看现在登着的是谁。
    fn read(&self, ticket: &Ticket, net: &Net) -> Option<Outcome>;
}

/// 正式的查法：官方的登录从家目录的登录文件、数据目录里存下的登录读。
pub struct Web {
    pub home: PathBuf,
    pub data: DataDir,
}

impl Reader for Web {
    fn read(&self, ticket: &Ticket, net: &Net) -> Option<Outcome> {
        let login = match &ticket.method {
            Method::Live(kind) => {
                let login = account::live_login(&self.home, *kind);
                if !still_logged_in(&ticket.key, *kind, login.as_deref()) {
                    return None;
                }
                login
            }
            Method::Stored(kind, id) => {
                account::stored_login(&self.data, &self.home, *kind, id).ok()
            }
            _ => None,
        };
        Some(read_with(&ticket.method, login, net))
    }
}

/// 查一次。`login` 是官方账号用的登录，读不到报没登录，不联网。
fn read_with(method: &Method, login: Option<Vec<u8>>, net: &Net) -> Outcome {
    let http = || net.http().map_err(|err| method.unreachable(err));
    let quota = |found: Result<Quota, QuotaError>| found.map(Found::Quota).map_err(Failed::Quota);
    let balance =
        |found: Result<Balance, BalanceError>| found.map(Found::Balance).map_err(Failed::Balance);
    match method {
        Method::Custom(provider, query) => balance(query::run(&*http()?, provider, query)),
        Method::Plan(provider) => quota(quota::plan(&*http()?, provider)),
        Method::Balance(provider) => balance(balance::query(&*http()?, provider)),
        Method::Live(kind) | Method::Stored(kind, _) => {
            let Some(login) = login else {
                return Err(Failed::Quota(QuotaError::NotLoggedIn));
            };
            let http = http()?;
            quota(match kind {
                AgentKind::Codex => quota::codex_login(&http, &login),
                AgentKind::GrokBuild => quota::grok_login(&http, &login),
                _ => Err(QuotaError::NotLoggedIn),
            })
        }
    }
}

/// 登录文件里的（`login`）还是 `key` 那个账号。认不出是谁的（`CheckKey::Login`）不核。
fn still_logged_in(key: &CheckKey, kind: AgentKind, login: Option<&[u8]>) -> bool {
    match key {
        CheckKey::Account(_, id) => login
            .and_then(|bytes| account::identify(kind, bytes))
            .is_some_and(|identity| identity.id == *id),
        _ => true,
    }
}

// ── 排 ──────────────────────────────────────────────

/// 计划里的一个：查谁、怎么查、多久自动查一次。
#[derive(Debug, Clone, PartialEq)]
pub struct Planned {
    pub key: CheckKey,
    pub method: Method,
    /// 自动查的间隔（分钟），0 是不自动查（[`Check::every`]）。
    pub every: u32,
    /// 关掉了：不查，那一格空着。
    pub off: bool,
}

/// 开始查的一次：回来的时候凭它认还算不算数。
#[derive(Debug, Clone)]
pub struct Ticket {
    pub key: CheckKey,
    pub method: Method,
    generation: u64,
}

struct Slot {
    planned: Planned,
    generation: u64,
    /// 在查的那一次的代数。
    running: Option<u64>,
    last: Option<Outcome>,
    /// 上次开始查的时候。
    started: Option<Instant>,
}

impl Slot {
    fn running(&self) -> bool {
        self.running == Some(self.generation)
    }

    /// 到点自动查的时候；不自动查、关掉的、在查的是 `None`。
    fn due_at(&self, now: Instant) -> Option<Instant> {
        if self.planned.off || self.planned.every == 0 || self.running() {
            return None;
        }
        let every = Duration::from_secs(u64::from(self.planned.every) * 60);
        Some(self.started.map_or(now, |at| at + every))
    }
}

/// 每个查询对象查到哪了：查法、在不在查、上次的读数、上次什么时候查的。
#[derive(Default)]
pub struct Schedule {
    slots: HashMap<CheckKey, Slot>,
    /// 发出去的最后一个代数。全局递增：去掉又加回来的查询对象不会和之前发出的撞上。
    generation: u64,
}

/// 查回来以后（[`Schedule::finish`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finished {
    /// 收下了。
    Kept,
    /// 这一代的，但登录文件里已经不是这个账号了：没收，在查的标记消掉了。
    LoginChanged,
    /// 不是这一代的（查法换了、从计划里去掉了）：没收，界面上什么也没变。
    Stale,
}

/// 一个查询对象现在的样子（界面画读数用）。
pub struct State<'a> {
    pub planned: &'a Planned,
    /// 这一代在查。
    pub running: bool,
    pub last: Option<&'a Outcome>,
}

impl Schedule {
    /// 换上新的计划。不在里面的去掉（读数一起）；查法变了的代数加一，不是同一个账号换登录来源的丢掉旧读数，
    /// 在查的那次不收了或者旧读数丢了的，自动查的马上到点。返回有没有变。
    pub fn plan(&mut self, planned: Vec<Planned>) -> bool {
        let mut changed = planned.len() != self.slots.len();
        let mut slots = HashMap::with_capacity(planned.len());
        for new in planned {
            let slot = match self.slots.remove(&new.key) {
                Some(mut slot) => {
                    if !slot.planned.method.reads_same(&new.method) {
                        let keeps = slot.planned.method.keeps_reading(&new.method);
                        if !keeps {
                            slot.last = None;
                        }
                        // 在查的那次不收了、旧读数丢了：自动查的马上到点。
                        if slot.running() || !keeps {
                            slot.started = None;
                        }
                        self.generation += 1;
                        slot.generation = self.generation;
                    }
                    changed |= slot.planned != new;
                    slot.planned = new;
                    slot
                }
                None => {
                    changed = true;
                    self.generation += 1;
                    Slot {
                        planned: new,
                        generation: self.generation,
                        running: None,
                        last: None,
                        started: None,
                    }
                }
            };
            slots.insert(slot.planned.key.clone(), slot);
        }
        self.slots = slots;
        changed
    }

    /// 计划里的全部（没关掉的）。
    pub fn keys(&self) -> Vec<CheckKey> {
        self.slots
            .values()
            .filter(|slot| !slot.planned.off)
            .map(|slot| slot.planned.key.clone())
            .collect()
    }

    /// 到点该自动查的。
    pub fn due(&self, now: Instant) -> Vec<CheckKey> {
        self.slots
            .values()
            .filter(|slot| slot.due_at(now).is_some_and(|at| at <= now))
            .map(|slot| slot.planned.key.clone())
            .collect()
    }

    /// 最早什么时候有到点的；没有自动查的是 `None`。
    pub fn next_due(&self, now: Instant) -> Option<Instant> {
        self.slots
            .values()
            .filter_map(|slot| slot.due_at(now))
            .min()
    }

    /// 开始查一个：不在计划里的、关掉的、这一代已经在查的不查。
    pub fn start(&mut self, key: &CheckKey, now: Instant) -> Option<Ticket> {
        let slot = self.slots.get_mut(key)?;
        if slot.planned.off || slot.running() {
            return None;
        }
        slot.running = Some(slot.generation);
        slot.started = Some(now);
        Some(Ticket {
            key: key.clone(),
            method: slot.planned.method.clone(),
            generation: slot.generation,
        })
    }

    /// 查回来了：还是这一代的收下。`outcome` 是 `None` 是查到的不算（登录换了人）。
    pub fn finish(&mut self, ticket: &Ticket, outcome: Option<Outcome>) -> Finished {
        let Some(slot) = self.slots.get_mut(&ticket.key) else {
            return Finished::Stale;
        };
        if slot.running == Some(ticket.generation) {
            slot.running = None;
        }
        if slot.generation != ticket.generation {
            return Finished::Stale;
        }
        match outcome {
            Some(outcome) => {
                slot.last = Some(outcome);
                Finished::Kept
            }
            None => Finished::LoginChanged,
        }
    }

    pub fn get(&self, key: &CheckKey) -> Option<State<'_>> {
        let slot = self.slots.get(key)?;
        Some(State {
            planned: &slot.planned,
            running: slot.running(),
            last: slot.last.as_ref(),
        })
    }

    /// 有没有在查的。
    pub fn running(&self) -> bool {
        self.slots.values().any(Slot::running)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::provider::Protocol;
    use crate::query::Template;

    fn provider(id: &str, url: &str, key: &str) -> Provider {
        Provider {
            id: id.into(),
            name: id.into(),
            endpoints: BTreeMap::from([(Protocol::Anthropic, url.to_string())]),
            key: key.into(),
            ..Default::default()
        }
    }

    fn custom(url: &str) -> Query {
        Query {
            template: Template::Custom,
            url: url.into(),
            left: "data.left".into(),
            ..Default::default()
        }
    }

    fn planned(key: CheckKey, method: Method, every: u32) -> Planned {
        Planned {
            key,
            method,
            every,
            off: false,
        }
    }

    fn account(id: &str) -> CheckKey {
        CheckKey::Account(AgentKind::Codex, id.into())
    }

    fn left(amount: f64) -> Outcome {
        Ok(Found::Balance(Balance {
            left: Some(amount),
            used: None,
            currency: balance::Currency::Usd,
        }))
    }

    #[test]
    fn the_method_is_custom_then_plan_then_balance() {
        let deepseek = provider("deepseek", "https://api.deepseek.com/anthropic", "sk-1");
        let kimi = provider("kimi", "https://api.kimi.com/coding/", "sk-kimi");
        let relay = provider("relay", "https://relay.example.com", "sk-2");
        let none = Check::default();
        assert!(matches!(
            reader_for(Subject::Provider(&deepseek), &none),
            Some(Method::Balance(_))
        ));
        assert!(matches!(
            reader_for(Subject::Provider(&kimi), &none),
            Some(Method::Plan(_))
        ));
        assert_eq!(reader_for(Subject::Provider(&relay), &none), None);
        // 自己配的排在最前；没填够的不算。
        let mine = Check {
            query: Some(custom("https://relay.example.com/balance")),
            ..Default::default()
        };
        assert!(matches!(
            reader_for(Subject::Provider(&kimi), &mine),
            Some(Method::Custom(..))
        ));
        let half = Check {
            query: Some(Query {
                template: Template::Custom,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(matches!(
            reader_for(Subject::Provider(&kimi), &half),
            Some(Method::Plan(_))
        ));
        // 官方：Claude 先不查。
        assert_eq!(
            reader_for(Subject::Live(AgentKind::Codex, None), &none),
            Some(Method::Live(AgentKind::Codex))
        );
        assert_eq!(
            reader_for(Subject::Stored(AgentKind::GrokBuild, "a"), &none),
            Some(Method::Stored(AgentKind::GrokBuild, "a".into()))
        );
        assert_eq!(
            reader_for(Subject::Live(AgentKind::ClaudeCode, Some("a")), &none),
            None
        );
        assert_eq!(
            reader_for(Subject::Stored(AgentKind::ClaudeCode, "a"), &none),
            None
        );
        assert!(built_in(&deepseek) && built_in(&kimi) && !built_in(&relay));
    }

    #[test]
    fn the_key_follows_the_subject() {
        let kimi = provider("kimi", "https://api.kimi.com/coding/", "sk-kimi");
        let codex = AgentKind::Codex;
        assert_eq!(
            Subject::Provider(&kimi).key(),
            CheckKey::Provider("kimi".into())
        );
        // 登着的认得出是谁：和存下的同一个键（切账号时读数跟着账号走）。
        assert_eq!(Subject::Live(codex, Some("a")).key(), account("a"));
        assert_eq!(Subject::Stored(codex, "a").key(), account("a"));
        assert_eq!(Subject::Live(codex, None).key(), CheckKey::Login(codex));

        let every = Check {
            every: Some(15),
            ..Default::default()
        };
        assert_eq!(
            super::planned(Subject::Live(codex, Some("a")), &every, true),
            Some(Planned {
                key: account("a"),
                method: Method::Live(codex),
                every: 15,
                off: false,
            })
        );
        let off = Check { off: true, ..every };
        let planned = super::planned(Subject::Live(codex, Some("a")), &off, true).unwrap();
        assert!(planned.off && planned.every == 0);
        // 不在用的默认不自动查；查不了的不进计划。
        let none = Check::default();
        let stored = super::planned(Subject::Stored(codex, "b"), &none, false);
        assert_eq!(stored.unwrap().every, 0);
        let claude = super::planned(Subject::Live(AgentKind::ClaudeCode, None), &none, true);
        assert_eq!(claude, None);
        assert!(checkable(&CheckKey::Provider("relay".into())));
        assert!(checkable(&CheckKey::Login(AgentKind::GrokBuild)));
        assert!(!checkable(&CheckKey::Login(AgentKind::ClaudeCode)));
    }

    #[test]
    fn what_is_due_and_when() {
        let now = Instant::now();
        let mut schedule = Schedule::default();
        schedule.plan(vec![
            planned(account("a"), Method::Live(AgentKind::Codex), 5),
            planned(
                account("b"),
                Method::Stored(AgentKind::Codex, "b".into()),
                0,
            ),
            Planned {
                off: true,
                ..planned(
                    account("c"),
                    Method::Stored(AgentKind::Codex, "c".into()),
                    5,
                )
            },
        ]);
        // 自动查的、没查过的马上到点；不自动查的、关掉的不到点。
        assert_eq!(schedule.due(now), vec![account("a")]);
        assert_eq!(schedule.next_due(now), Some(now));
        let ticket = schedule.start(&account("a"), now).unwrap();
        // 在查的不到点，也不再开始。
        assert!(schedule.due(now).is_empty());
        assert_eq!(schedule.next_due(now), None);
        assert!(schedule.start(&account("a"), now).is_none());
        assert_eq!(schedule.finish(&ticket, Some(left(1.))), Finished::Kept);
        // 下一次在开始查以后 5 分钟。
        let later = now + Duration::from_secs(5 * 60);
        assert_eq!(schedule.next_due(now), Some(later));
        assert!(schedule.due(later - Duration::from_secs(1)).is_empty());
        assert_eq!(schedule.due(later), vec![account("a")]);
        // 不自动查的也能点了查；关掉的不行。
        assert!(schedule.start(&account("b"), now).is_some());
        assert!(schedule.start(&account("c"), now).is_none());
        assert!(schedule.start(&account("x"), now).is_none());
    }

    #[test]
    fn the_earliest_due_wins_and_settings_count_as_changes() {
        let now = Instant::now();
        let mut schedule = Schedule::default();
        let plan = |every_a, every_b| {
            vec![
                planned(account("a"), Method::Live(AgentKind::Codex), every_a),
                planned(
                    account("b"),
                    Method::Stored(AgentKind::Codex, "b".into()),
                    every_b,
                ),
            ]
        };
        schedule.plan(plan(5, 15));
        for key in [account("a"), account("b")] {
            let ticket = schedule.start(&key, now).unwrap();
            schedule.finish(&ticket, Some(left(1.)));
        }
        assert_eq!(
            schedule.next_due(now),
            Some(now + Duration::from_secs(5 * 60))
        );
        // 只改间隔、只关掉也是变了（要通知界面），读数和代数不动。
        assert!(schedule.plan(plan(30, 15)));
        assert_eq!(
            schedule.next_due(now),
            Some(now + Duration::from_secs(15 * 60))
        );
        let mut off = plan(30, 15);
        off[1].off = true;
        assert!(schedule.plan(off.clone()));
        assert!(!schedule.plan(off));
        assert_eq!(schedule.get(&account("b")).unwrap().last, Some(&left(1.)));
    }

    #[test]
    fn a_reading_that_does_not_count_keeps_the_last_one() {
        let now = Instant::now();
        let mut schedule = Schedule::default();
        schedule.plan(vec![planned(
            account("a"),
            Method::Live(AgentKind::Codex),
            5,
        )]);
        let ticket = schedule.start(&account("a"), now).unwrap();
        assert_eq!(schedule.finish(&ticket, Some(left(1.))), Finished::Kept);
        // 登录文件换了人：这次不算，在查的标记照样消掉，上次的读数留着。
        let ticket = schedule.start(&account("a"), now).unwrap();
        assert_eq!(schedule.finish(&ticket, None), Finished::LoginChanged);
        let state = schedule.get(&account("a")).unwrap();
        assert!(!state.running);
        assert_eq!(state.last, Some(&left(1.)));
    }

    /// B5：查询在飞时切了账号。
    #[test]
    fn switching_accounts_while_a_check_is_out() {
        let now = Instant::now();
        let mut schedule = Schedule::default();
        let live = |key| planned(key, Method::Live(AgentKind::Codex), 5);
        let stored =
            |key: CheckKey, id: &str| planned(key, Method::Stored(AgentKind::Codex, id.into()), 0);
        schedule.plan(vec![live(account("a")), stored(account("b"), "b")]);
        let a_live = schedule.start(&account("a"), now).unwrap();
        let b_stored = schedule.start(&account("b"), now).unwrap();
        // 切到 b：a 变成存下的、b 变成登着的。
        assert!(schedule.plan(vec![stored(account("a"), "a"), live(account("b"))]));
        // b 马上到点，能用登录文件再查，不被存着的那次挡掉。
        assert_eq!(schedule.due(now), vec![account("b")]);
        let b_live = schedule.start(&account("b"), now).unwrap();
        assert_eq!(b_live.method, Method::Live(AgentKind::Codex));
        assert!(schedule.get(&account("b")).unwrap().running);
        // 切之前开始的两个回来都不收。a 的那次读到的已经是 b 的登录、查到的不算，计划已经换过，不用再重读。
        assert_eq!(schedule.finish(&a_live, None), Finished::Stale);
        assert_eq!(schedule.finish(&b_stored, Some(left(2.))), Finished::Stale);
        assert!(schedule.get(&account("a")).unwrap().last.is_none());
        assert!(
            schedule.get(&account("b")).unwrap().running,
            "b is still out"
        );
        // 新的那次回来，读数记在 b 下。
        assert_eq!(schedule.finish(&b_live, Some(left(3.))), Finished::Kept);
        assert_eq!(schedule.get(&account("b")).unwrap().last, Some(&left(3.)));
        assert!(!schedule.running());
    }

    #[test]
    fn a_new_method_drops_the_old_reading_but_a_new_login_source_does_not() {
        let now = Instant::now();
        let relay = provider("relay", "https://relay.example.com", "sk");
        let key = CheckKey::Provider("relay".into());
        let mut schedule = Schedule::default();
        let with = |query: Query| {
            planned(
                CheckKey::Provider("relay".into()),
                Method::Custom(Box::new(relay.clone()), query),
                5,
            )
        };
        schedule.plan(vec![with(custom("https://relay.example.com/a"))]);
        let ticket = schedule.start(&key, now).unwrap();
        schedule.finish(&ticket, Some(left(1.)));
        // 同样的计划再换上一次：没变，读数留着。
        assert!(!schedule.plan(vec![with(custom("https://relay.example.com/a"))]));
        assert!(schedule.get(&key).unwrap().last.is_some());
        assert!(schedule.due(now).is_empty());
        // 改了查法：读数丢掉，马上到点。
        assert!(schedule.plan(vec![with(custom("https://relay.example.com/b"))]));
        assert!(schedule.get(&key).unwrap().last.is_none());
        assert_eq!(schedule.due(now), vec![key.clone()]);

        // 同一个账号从登着的变成存下的：读数还是它的。
        schedule.plan(vec![planned(
            account("a"),
            Method::Live(AgentKind::Codex),
            5,
        )]);
        let ticket = schedule.start(&account("a"), now).unwrap();
        schedule.finish(&ticket, Some(left(2.)));
        schedule.plan(vec![planned(
            account("a"),
            Method::Stored(AgentKind::Codex, "a".into()),
            0,
        )]);
        assert_eq!(schedule.get(&account("a")).unwrap().last, Some(&left(2.)));
        // 不在计划里的去掉了。
        assert!(schedule.get(&key).is_none());
    }

    #[test]
    fn a_new_key_drops_the_old_reading_but_a_new_name_does_not() {
        let now = Instant::now();
        let key = CheckKey::Provider("kimi".into());
        let with = |provider: Provider| planned(key.clone(), Method::Plan(Box::new(provider)), 5);
        let kimi = provider("kimi", "https://api.kimi.com/coding/", "sk-wrong");
        let mut schedule = Schedule::default();
        schedule.plan(vec![with(kimi.clone())]);
        let ticket = schedule.start(&key, now).unwrap();
        // 查着的时候改了名字：查的时候用不到，这次照收。
        let mut renamed = kimi.clone();
        renamed.name = "Kimi 2".into();
        schedule.plan(vec![with(renamed.clone())]);
        assert_eq!(schedule.finish(&ticket, Some(left(1.))), Finished::Kept);
        assert!(schedule.due(now).is_empty());
        // 改对了 Key：旧读数（比如「Key 不对」）丢掉，马上再查。
        let mut fixed = renamed;
        fixed.key = "sk-right".into();
        assert!(schedule.plan(vec![with(fixed)]));
        assert!(schedule.get(&key).unwrap().last.is_none());
        assert_eq!(schedule.due(now), vec![key]);
    }

    #[test]
    fn a_target_that_comes_back_does_not_take_old_results() {
        let now = Instant::now();
        let mut schedule = Schedule::default();
        let plan = vec![planned(account("a"), Method::Live(AgentKind::Codex), 5)];
        schedule.plan(plan.clone());
        let ticket = schedule.start(&account("a"), now).unwrap();
        schedule.plan(Vec::new());
        assert_eq!(schedule.finish(&ticket, Some(left(1.))), Finished::Stale);
        schedule.plan(plan);
        assert_eq!(schedule.finish(&ticket, Some(left(1.))), Finished::Stale);
        assert!(schedule.start(&account("a"), now).is_some());
    }

    #[test]
    fn a_login_that_changed_hands_is_not_counted() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let grok = home.join(".grok");
        std::fs::create_dir_all(&grok).unwrap();
        std::fs::write(
            grok.join("auth.json"),
            r#"{"https://auth.x.ai::client": {"user_id": "u2", "team_id": "t", "key": "k"}}"#,
        )
        .unwrap();
        let web = Web {
            home: home.clone(),
            data: DataDir::new(dir.path().join("data")),
        };
        let net = Net {
            http: SharedHttp::default(),
            proxy: Proxy::default(),
        };
        let ticket = |key: CheckKey, method: Method| Ticket {
            key,
            method,
            generation: 1,
        };
        let live = Method::Live(AgentKind::GrokBuild);
        // 登录文件里已经是 u2 了，查 u1 的不算。
        let u1 = CheckKey::Account(AgentKind::GrokBuild, "u1-id".into());
        assert_eq!(web.read(&ticket(u1.clone(), live.clone()), &net), None);
        let login = std::fs::read(grok.join("auth.json")).unwrap();
        let u2 = account::identify(AgentKind::GrokBuild, &login).unwrap().id;
        let u2 = CheckKey::Account(AgentKind::GrokBuild, u2);
        assert!(still_logged_in(&u2, AgentKind::GrokBuild, Some(&login)));
        assert!(!still_logged_in(&u1, AgentKind::GrokBuild, Some(&login)));
        // 退出了：键是账号的也不算（界面重读以后换成 `Login`）；认不出是谁的照查，写「没登录」（不联网）。
        std::fs::remove_file(grok.join("auth.json")).unwrap();
        assert_eq!(web.read(&ticket(u2, live.clone()), &net), None);
        assert_eq!(
            web.read(&ticket(CheckKey::Login(AgentKind::GrokBuild), live), &net),
            Some(Err(Failed::Quota(QuotaError::NotLoggedIn)))
        );
        // 存下的登录没了：同样不联网。
        assert_eq!(
            web.read(
                &ticket(
                    CheckKey::Account(AgentKind::Codex, "gone".into()),
                    Method::Stored(AgentKind::Codex, "gone".into())
                ),
                &net
            ),
            Some(Err(Failed::Quota(QuotaError::NotLoggedIn)))
        );
    }

    #[test]
    fn a_custom_query_goes_through_the_shared_client() {
        // 代理设置建不了客户端：查的时候报连不上，说明走的是 `Net` 给的客户端。
        let relay = provider("relay", "https://relay.example.com", "sk");
        let web = Web {
            home: PathBuf::from("/nonexistent"),
            data: DataDir::new("/nonexistent"),
        };
        let broken = Net {
            http: SharedHttp::default(),
            proxy: Proxy {
                mode: crate::net::ProxyMode::Custom,
                url: String::new(),
            },
        };
        let ticket = Ticket {
            key: CheckKey::Provider("relay".into()),
            method: Method::Custom(Box::new(relay), custom("https://relay.example.com/balance")),
            generation: 1,
        };
        assert!(matches!(
            web.read(&ticket, &broken),
            Some(Err(Failed::Balance(BalanceError::Net(_))))
        ));
    }

    #[test]
    fn a_custom_query_reads_the_balance() {
        let server = crate::net::tests::serve(|_| (200, r#"{"data": {"left": 12.5}}"#.into()));
        let relay = provider("relay", &server.url, "sk");
        let web = Web {
            home: PathBuf::from("/nonexistent"),
            data: DataDir::new("/nonexistent"),
        };
        let net = Net {
            http: SharedHttp::default(),
            proxy: Proxy {
                mode: crate::net::ProxyMode::Direct,
                url: String::new(),
            },
        };
        let ticket = Ticket {
            key: CheckKey::Provider("relay".into()),
            method: Method::Custom(Box::new(relay), custom(&format!("{}/balance", server.url))),
            generation: 1,
        };
        let Some(Ok(Found::Balance(found))) = web.read(&ticket, &net) else {
            panic!("expected a balance");
        };
        assert_eq!(found.left, Some(12.5));
    }
}
