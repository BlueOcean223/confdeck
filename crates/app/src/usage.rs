//! 用量的数据：后台扫各 agent 的会话记录（core 的 `usage`），算好四个时间段的汇总放在 `Store` 里，用量页只管画。
//!
//! - 打开用量页、点顶栏的刷新时扫一次；扫的时候再点不重复扫。第一次要读全部历史（本机 release 约 6 秒），之后几十毫秒。
//! - 汇总一次算好四个时间段和热力图，切时间段不用等。算完把请求丢掉，只留汇总。
//! - 会话页的会话列表也从这里来：扫的时候顺带记下的（core 的 `session`），文件已经不在的不要。
//! - 价格用 models.dev 时只读磁盘上的缓存，不联网（供应商页拉列表、补全时会下）。
//! - 余额和订阅额度（10-13 起不在用量页）见 `readings.rs`。

use std::collections::HashMap;
use std::sync::Arc;

use confdeck_core::agent::AgentKind;
use confdeck_core::models_dev::{self, ModelsDev};
use confdeck_core::session::Session;
use confdeck_core::usage::history;
use confdeck_core::usage::summary::{self, Context as Pricing, Heatmap, Period, Source, Summary};
use confdeck_core::usage::{Db, UsageError};
use gpui_kit::*;

use crate::data::Store;
use crate::errors::Describe as _;

pub const PERIODS: [Period; 4] = [Period::Today, Period::Week, Period::Month, Period::All];

#[derive(Default)]
pub struct UsageState {
    /// 第一次扫完之前是 `None`。
    pub loaded: Option<Arc<Loaded>>,
    pub scanning: bool,
    /// 用量库打不开、读不了。
    pub error: Option<String>,
}

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
        let cached = self.net.models_dev();
        let task = cx.background_executor().spawn(async move {
            let (dev, fresh) = crate::net::models_dev_or_load(cached, &data, || {
                ModelsDev::read(&models_dev::cache_path(&data))
            });
            let dev = dev.ok();
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
                fresh,
            ))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |store, cx| {
                store.usage.scanning = false;
                match result {
                    Ok((loaded, fresh)) => {
                        store.usage.loaded = Some(Arc::new(loaded));
                        store.usage.error = None;
                        store.net.keep_models_dev(fresh);
                    }
                    Err(err) => store.usage.error = Some(err.describe()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

/// 官方登录是哪家的预设（名字、图标用）。
pub fn vendor(agent: AgentKind) -> Option<&'static str> {
    crate::official::vendor(agent).map(|(preset, _)| preset)
}
