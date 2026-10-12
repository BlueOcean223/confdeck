//! 用量的数据：后台扫各 agent 的会话记录（core 的 `usage`），算好四个时间段的汇总放在 `Store` 里，用量页只管画。
//!
//! - 打开用量页、点顶栏的刷新时扫一次；扫的时候再点不重复扫。第一次要读全部历史（本机 release 约 6 秒），之后几十毫秒。
//! - 汇总一次算好四个时间段和热力图，切时间段不用等。算完把请求丢掉，只留汇总。
//! - 价格用 models.dev 时只读磁盘上的缓存，不联网（供应商页拉列表、补全时会下）。
//! - 余额（core 的 `balance`）：能查的每家一个后台任务，同时查；5 分钟内不重查，点刷新按钮才重查。
//!   重查时先留着上次的数，查回来再换。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use confdeck_core::agent::AgentKind;
use confdeck_core::balance::{self, Balance, BalanceError};
use confdeck_core::current::Current;
use confdeck_core::fsx;
use confdeck_core::models_dev::{self, ModelsDev};
use confdeck_core::net::Http;
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
    balances_at: Option<Instant>,
}

/// 余额多久以内不重查（点刷新按钮除外）。
const BALANCE_MAX_AGE: Duration = Duration::from_secs(5 * 60);

pub struct Loaded {
    /// 和 [`PERIODS`] 一一对应。
    pub summaries: Vec<Summary>,
    /// 最近 53 周每天的合计（「全部」的热力图）。
    pub heatmap: Heatmap,
    /// 供应商 ID → 名字：现在的，删了的从切换记录里找。
    pub names: HashMap<String, String>,
}

impl Loaded {
    pub fn summary(&self, period: Period) -> &Summary {
        let index = PERIODS.iter().position(|p| *p == period).unwrap_or(0);
        &self.summaries[index]
    }
}

impl Store {
    /// 扫新加的会话记录、重算汇总，查余额。正在扫就不再扫；`force` 时余额不管上次是什么时候查的都重查。
    pub fn refresh_usage(&mut self, force: bool, cx: &mut Context<Self>) {
        self.refresh_balances(force, cx);
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

    fn refresh_balances(&mut self, force: bool, cx: &mut Context<Self>) {
        let fresh = self
            .usage
            .balances_at
            .is_some_and(|at| at.elapsed() < BALANCE_MAX_AGE);
        if fresh && !force {
            return;
        }
        self.usage.balances_at = Some(Instant::now());
        let proxy = self.proxy();
        for provider in self.providers.iter().filter(|p| balance::supported(p)) {
            if self.usage.checking.contains(&provider.id) {
                continue;
            }
            self.usage.checking.push(provider.id.clone());
            let provider = provider.clone();
            let proxy = proxy.clone();
            let task = cx.background_executor().spawn(async move {
                #[cfg(debug_assertions)]
                if let Some(result) = fake_balance(&provider.id) {
                    return (provider.id, result);
                }
                let result = Http::new(&proxy)
                    .map_err(BalanceError::Net)
                    .and_then(|http| balance::query(&http, &provider));
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
        cx.notify();
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
    match agent {
        AgentKind::ClaudeCode => Some("anthropic"),
        AgentKind::Codex => Some("openai"),
        AgentKind::GrokBuild => Some("xai"),
        AgentKind::Pi | AgentKind::Opencode => None,
    }
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
