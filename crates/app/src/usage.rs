//! 用量的数据：后台扫各 agent 的会话记录（core 的 `usage`），算好四个时间段的汇总放在 `Store` 里，用量页只管画。
//!
//! - 打开用量页、点顶栏的刷新时扫一次；扫的时候再点不重复扫。第一次要读全部历史（本机 release 约 6 秒），之后几十毫秒。
//!   用量库第一次扫时打开，之后一直开着（文件被删了重新打开）。
//! - 汇总一遍算好四个时间段和热力图，切时间段不用等。请求从用量库一条条读、边读边算，不攒起来，只留汇总。
//!   没有还没算进汇总的请求、供应商、切换记录、models.dev、日期和时区都没变时，沿用上次的汇总，不再读请求。
//! - 会话页的会话列表也从这里来：扫的时候顺带记下的（core 的 `session`），文件已经不在的不要。
//! - 价格用 models.dev 时只读磁盘上的缓存，不联网（供应商页拉列表、补全时会下）。
//! - 余额和订阅额度不在用量页，见 `readings.rs`。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use chrono::NaiveDate;
use confdeck_core::agent::AgentKind;
use confdeck_core::models_dev::{self, ModelsDev};
use confdeck_core::provider::Provider;
use confdeck_core::session::Session;
use confdeck_core::usage::history::{self, Switch};
use confdeck_core::usage::summary::{
    Context as Pricing, Heatmap, Overviewing, Period, Source, Summary,
};
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
    /// 用量库，第一次扫时打开（打不开下次再试）。只在后台线程上用，一次一个。
    db: Arc<Mutex<Option<Opened>>>,
}

struct Opened {
    db: Db,
    /// 扫进去的请求还没算进汇总（扫完以后没算成），下次扫没有新增也要重算。
    unsummarized: bool,
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
    /// 汇总是按什么算的。
    basis: Basis,
}

/// 除了请求以外，汇总还取决于这些。
struct Basis {
    /// 「今天」是哪天，本地时间比 UTC 快几秒：时间段和热力图按本地的零点切。
    day: NaiveDate,
    offset: i32,
    providers: Vec<Provider>,
    switches: Vec<Switch>,
    dev: Option<Arc<ModelsDev>>,
    /// 用的时候 models.dev 磁盘缓存的修改时间和大小。
    dev_file: Option<(SystemTime, u64)>,
}

impl Basis {
    /// models.dev 比的是不是同一份：进程里留着的、缓存文件没变时沿用的都是同一个 `Arc`，换了就是重读过。
    fn same(&self, other: &Basis) -> bool {
        let dev = match (&self.dev, &other.dev) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (a, b) => a.is_none() && b.is_none(),
        };
        dev && self.day == other.day
            && self.offset == other.offset
            && self.providers == other.providers
            && self.switches == other.switches
    }
}

/// 进程里那份 models.dev 放了超过一天就不给了（`NetState::models_dev`），用量只读磁盘缓存，
/// 缓存文件没变时用上次那份，不再读一遍。
fn reusable_dev(
    previous: Option<&Loaded>,
    file: Option<(SystemTime, u64)>,
) -> Option<Arc<ModelsDev>> {
    let previous = previous?;
    (file.is_some() && previous.basis.dev_file == file)
        .then(|| previous.basis.dev.clone())
        .flatten()
}

/// 扫完以后，上次的能沿用多少。
#[derive(Debug, PartialEq)]
enum Reuse {
    /// 重算汇总。
    Nothing,
    /// 沿用汇总，会话列表换新的。
    Summaries,
    /// 整份沿用，不换 `Loaded`。
    Everything,
}

/// 没有还没算进汇总的请求、汇总靠的东西都没变时沿用汇总；会话列表也一样时整份沿用。
fn reuse(
    previous: Option<&Loaded>,
    unsummarized: bool,
    basis: &Basis,
    sessions: &[Session],
) -> Reuse {
    match previous {
        Some(previous) if !unsummarized && previous.basis.same(basis) => {
            if previous.sessions == sessions {
                Reuse::Everything
            } else {
                Reuse::Summaries
            }
        }
        _ => Reuse::Nothing,
    }
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
        let db = self.usage.db.clone();
        let previous = self.usage.loaded.clone();
        let task = cx.background_executor().spawn(async move {
            let cache = models_dev::cache_path(&data);
            let dev_file = std::fs::metadata(&cache)
                .ok()
                .and_then(|meta| Some((meta.modified().ok()?, meta.len())));
            let cached = cached.or_else(|| reusable_dev(previous.as_deref(), dev_file));
            let (dev, fresh) =
                crate::net::models_dev_or_load(cached, &data, || ModelsDev::read(&cache));
            let mut opened = db.lock().unwrap_or_else(PoisonError::into_inner);
            let path = data.usage_db();
            let opened = match &mut *opened {
                Some(opened) if path.exists() => opened,
                slot => slot.insert(Opened {
                    db: Db::open(&path)?,
                    unsummarized: false,
                }),
            };
            let report = confdeck_core::usage::scan(&home, &mut opened.db);
            opened.unsummarized |= report.added > 0;
            let mut sessions = opened.db.sessions()?;
            sessions.retain(|session| session.path.exists());
            let now = chrono::Local::now();
            let basis = Basis {
                day: now.date_naive(),
                offset: now.offset().local_minus_utc(),
                providers,
                switches: history::load(&data)?,
                dev: dev.ok(),
                dev_file,
            };
            let reused = reuse(previous.as_deref(), opened.unsummarized, &basis, &sessions);
            let loaded = match (reused, previous) {
                (Reuse::Everything, _) => None,
                (Reuse::Summaries, Some(previous)) => Some(Loaded {
                    summaries: previous.summaries.clone(),
                    heatmap: previous.heatmap.clone(),
                    names: previous.names.clone(),
                    sessions,
                    basis,
                }),
                _ => {
                    let pricing = Pricing {
                        providers: &basis.providers,
                        history: &basis.switches,
                        dev: basis.dev.as_deref(),
                    };
                    let earliest = opened.db.first_request_at()?;
                    let mut overviewing = Overviewing::new(&pricing, earliest, &PERIODS, &now);
                    opened
                        .db
                        .each_request(Some(overviewing.since()), |request| {
                            overviewing.add(request)
                        })?;
                    let overview = overviewing.finish();
                    let names = overview
                        .summaries
                        .iter()
                        .flat_map(|summary| &summary.sources)
                        .filter_map(|(source, _)| match source {
                            Source::Provider(id) => Some((id.clone(), pricing.provider_name(id)?)),
                            _ => None,
                        })
                        .collect();
                    opened.unsummarized = false;
                    Some(Loaded {
                        summaries: overview.summaries,
                        heatmap: overview.heatmap,
                        names,
                        sessions,
                        basis,
                    })
                }
            };
            Ok::<_, UsageError>((loaded, fresh))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |store, cx| {
                store.usage.scanning = false;
                match result {
                    Ok((loaded, fresh)) => {
                        // `None` 是和上次一样，留着原来那份（用量页、会话页按它缓存的东西接着用）。
                        if let Some(loaded) = loaded {
                            store.usage.loaded = Some(Arc::new(loaded));
                        }
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

#[cfg(test)]
impl Loaded {
    /// 只有会话列表的一份（会话页的测试用）。
    pub(crate) fn of_sessions(sessions: Vec<Session>) -> Self {
        Self {
            summaries: Vec::new(),
            heatmap: Heatmap::default(),
            names: HashMap::new(),
            sessions,
            basis: Basis {
                day: NaiveDate::default(),
                offset: 0,
                providers: Vec::new(),
                switches: Vec::new(),
                dev: None,
                dev_file: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    use confdeck_core::agent::AgentKind;
    use confdeck_core::models_dev::ModelsDev;
    use confdeck_core::provider::Provider;
    use confdeck_core::session::Session;
    use confdeck_core::usage::history::{Switch, Target};

    use super::{Basis, Loaded, Reuse, reusable_dev, reuse};

    fn basis(dev: Option<Arc<ModelsDev>>) -> Basis {
        Basis {
            day: chrono::NaiveDate::from_ymd_opt(2026, 10, 14).unwrap(),
            offset: 8 * 3600,
            providers: vec![Provider {
                id: "relay".into(),
                ..Provider::default()
            }],
            switches: vec![Switch {
                at: 1,
                agent: AgentKind::ClaudeCode,
                to: Target::Official,
            }],
            dev,
            dev_file: None,
        }
    }

    /// 汇总沿用上次的条件：日期、时区、供应商、切换记录一样，models.dev 是同一份（重读过的不算）。
    #[test]
    fn a_summary_is_reused_only_on_the_same_basis() {
        let dev = Arc::new(ModelsDev::default());
        let same = basis(Some(dev.clone()));
        assert!(same.same(&basis(Some(dev.clone()))));
        assert!(basis(None).same(&basis(None)));
        assert!(!same.same(&basis(Some(Arc::new(ModelsDev::default())))));
        assert!(!same.same(&basis(None)));

        let mut next_day = basis(Some(dev.clone()));
        next_day.day = next_day.day.succ_opt().unwrap();
        assert!(!same.same(&next_day));
        let mut moved = basis(Some(dev.clone()));
        moved.offset = 9 * 3600;
        assert!(!same.same(&moved));
        let mut renamed = basis(Some(dev.clone()));
        renamed.providers[0].name = "Relay".into();
        assert!(!same.same(&renamed));
        let mut switched = basis(Some(dev));
        switched.switches[0].at = 2;
        assert!(!same.same(&switched));
    }

    /// 有还没算进汇总的请求、汇总靠的东西变了都重算；都没变时会话列表一样整份沿用，不一样只沿用汇总。
    #[test]
    fn a_scan_reuses_what_has_not_changed() {
        let dev = Arc::new(ModelsDev::default());
        let sessions = vec![Session {
            agent: AgentKind::Codex,
            id: "s1".into(),
            path: "/s/s1.jsonl".into(),
            cwd: None,
            prompt: None,
            name: None,
            title: None,
            started: None,
            active: 1,
        }];
        let mut previous = Loaded::of_sessions(sessions.clone());
        previous.basis = basis(Some(dev.clone()));
        let same = basis(Some(dev.clone()));

        assert_eq!(
            reuse(Some(&previous), false, &same, &sessions),
            Reuse::Everything
        );
        assert_eq!(reuse(Some(&previous), false, &same, &[]), Reuse::Summaries);
        assert_eq!(
            reuse(Some(&previous), true, &same, &sessions),
            Reuse::Nothing
        );
        assert_eq!(
            reuse(Some(&previous), false, &basis(None), &sessions),
            Reuse::Nothing
        );
        assert_eq!(reuse(None, false, &same, &sessions), Reuse::Nothing);
    }

    /// 进程里的 models.dev 过期了不给时，缓存文件没变就用上次那份；文件变了、没有文件、上次没有都重读。
    #[test]
    fn models_dev_is_reused_while_its_file_is_unchanged() {
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let dev = Arc::new(ModelsDev::default());
        let mut previous = Loaded::of_sessions(Vec::new());
        previous.basis.dev = Some(dev.clone());
        previous.basis.dev_file = Some((at, 100));

        let reused = reusable_dev(Some(&previous), Some((at, 100))).unwrap();
        assert!(Arc::ptr_eq(&reused, &dev));
        assert!(reusable_dev(Some(&previous), Some((at + Duration::from_secs(1), 100))).is_none());
        assert!(reusable_dev(Some(&previous), Some((at, 101))).is_none());
        assert!(reusable_dev(Some(&previous), None).is_none());
        assert!(reusable_dev(None, Some((at, 100))).is_none());

        previous.basis.dev_file = None;
        assert!(reusable_dev(Some(&previous), None).is_none());
    }
}
