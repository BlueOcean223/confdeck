//! 查的时候认供应商、算花费，汇总成用量页要的数。
//!
//! **认供应商**，依次：
//! 1. 记录里写着的：Pi、OpenCode 的 `confdeck-<id>` 是 confdeck 的供应商，别的名字是 agent 自己配的；
//!    Codex 的 `model_provider` 是 `openai` 算官方，`custom` 往下认，别的名字照写；Grok Build 不是 confdeck 写的模型表算官方。
//! 2. 切换记录（[`super::history`]）：请求那一刻这个 agent 连的是哪家。
//! 3. 模型名：这个 agent 自家的模型（`claude-*`、`gpt-*`、`grok-*`）算官方（Codex `custom`、Grok confdeck 表除外）；
//!    恰好一家供应商的模型目录里有它，算那家。都不是算「未知」。
//!
//! **花费**，依次：供应商模型目录里的价格 → agent 自己算的（Pi、Grok Build）→ 内置预设和 models.dev 查到的价格
//! （官方和认不出的按 agent 自家的官方标价）。都没有的请求不计花费，单独数出来（`unpriced`）。

use std::collections::HashMap;

use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone};

use super::history::{self, Switch, Target};
use super::{Request, Tokens};
use crate::agent::AgentKind;
use crate::models_dev::{self, ModelsDev};
use crate::preset;
use crate::provider::{Cost, Provider};

/// 一次请求算谁的。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Source {
    /// confdeck 的一家供应商（ID；可能已经删了）。
    Provider(String),
    /// agent 自带的登录（Claude Code 是 Anthropic，Codex 是 OpenAI，Grok Build 是 xAI）。
    Official(AgentKind),
    /// 记录里写着、但不是 confdeck 写的配置（Pi、OpenCode 自己配的供应商，Codex 别的 `model_provider`）。
    Other(AgentKind, String),
    Unknown,
}

/// 认供应商、算价格要用的东西。
pub struct Context<'a> {
    pub providers: &'a [Provider],
    pub history: &'a [Switch],
    pub dev: Option<&'a ModelsDev>,
}

impl Context<'_> {
    /// 供应商的名字：现在的，删了的用切换记录里的；都没有是 `None`。
    pub fn provider_name(&self, id: &str) -> Option<String> {
        if let Some(provider) = self.providers.iter().find(|p| p.id == id) {
            return Some(provider.name.clone());
        }
        self.history
            .iter()
            .rev()
            .find_map(|switch| match &switch.to {
                Target::Provider { id: found, name } if found == id => Some(name.clone()),
                _ => None,
            })
    }

    pub fn source(&self, request: &Request) -> Source {
        let hint = request.provider.as_deref();
        match request.agent {
            // 多选 agent 的记录写的是配置里那一项的名字（`confdeck-<供应商 ID>`）。
            AgentKind::Pi | AgentKind::Opencode => match hint {
                Some(name) => match provider_of(request.agent, name) {
                    Some(id) => Source::Provider(id.to_string()),
                    None => Source::Other(request.agent, name.to_string()),
                },
                None => self.by_model(request, false),
            },
            AgentKind::Codex => match hint {
                Some("openai") => Source::Official(request.agent),
                Some("custom") | None => self.by_history(request, hint.is_none()),
                Some(name) => Source::Other(request.agent, name.to_string()),
            },
            AgentKind::ClaudeCode => self.by_history(request, true),
            AgentKind::GrokBuild => match hint {
                Some(_) => self.by_history(request, false),
                None => Source::Official(request.agent),
            },
        }
    }

    fn by_history(&self, request: &Request, official: bool) -> Source {
        match history::at(self.history, request.agent, request.at) {
            Some(Target::Provider { id, .. }) => Source::Provider(id.clone()),
            Some(Target::Official) if official => Source::Official(request.agent),
            _ => self.by_model(request, official),
        }
    }

    fn by_model(&self, request: &Request, official: bool) -> Source {
        if official && is_official_model(request.agent, &request.model) {
            return Source::Official(request.agent);
        }
        let mut found = self.providers.iter().filter(|provider| {
            provider
                .models
                .iter()
                .any(|model| model.id.eq_ignore_ascii_case(&request.model))
        });
        match (found.next(), found.next()) {
            (Some(provider), None) => Source::Provider(provider.id.clone()),
            _ => Source::Unknown,
        }
    }

    /// 这个来源这个模型的价格。
    fn price(&self, source: &Source, agent: AgentKind, model: &str) -> Price {
        let mut price = Price::default();
        match source {
            Source::Provider(id) => {
                if let Some(provider) = self.providers.iter().find(|p| &p.id == id) {
                    price.listed = provider
                        .models
                        .iter()
                        .find(|m| m.id.eq_ignore_ascii_case(model))
                        .and_then(|m| m.cost);
                    price.known = models_dev::known(provider, model, self.dev).and_then(|m| m.cost);
                }
            }
            // 认不出是哪家的，也先按官方标价估。
            Source::Official(_) | Source::Unknown => {
                let id = match agent {
                    AgentKind::ClaudeCode => "anthropic",
                    AgentKind::Codex => "openai",
                    AgentKind::GrokBuild => "xai",
                    AgentKind::Pi | AgentKind::Opencode => return price,
                };
                price.known = official(id, model, self.dev);
            }
            // Pi 自带的供应商用的是常见的 ID（`anthropic`、`deepseek`），预设里有就按预设的价格。
            Source::Other(_, name) => price.known = official(name, model, self.dev),
        }
        price
    }

    /// 一次请求的花费（美元），没有价格是 `None`。同一来源同一模型的价格查一次记在 `prices` 里。
    fn cost(&self, prices: &mut Prices, source: &Source, request: &Request) -> Option<f64> {
        prices
            .entry((source.clone(), request.agent, request.model.clone()))
            .or_insert_with(|| self.price(source, request.agent, &request.model))
            .of(request)
    }
}

type Prices = HashMap<(Source, AgentKind, String), Price>;

fn official(preset_id: &str, model: &str, dev: Option<&ModelsDev>) -> Option<Cost> {
    let preset = preset::get(preset_id)?;
    models_dev::known(&preset.provider(preset_id, ""), model, dev).and_then(|m| m.cost)
}

/// 多选 agent 配置里一项的名字是哪家供应商的（适配器的 `provider_of`）。
fn provider_of(agent: AgentKind, name: &str) -> Option<&str> {
    agent.writes().multi().ok()?.provider_of(name)
}

/// agent 自家的模型。
fn is_official_model(agent: AgentKind, model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    match agent {
        AgentKind::ClaudeCode => model.starts_with("claude-"),
        AgentKind::Codex => {
            model.starts_with("gpt-")
                || model.starts_with("codex-")
                || (model.starts_with('o') && model[1..].starts_with(|c: char| c.is_ascii_digit()))
        }
        AgentKind::GrokBuild => model.starts_with("grok-"),
        AgentKind::Pi | AgentKind::Opencode => false,
    }
}

#[derive(Default, Clone)]
struct Price {
    /// 供应商模型目录里写的。
    listed: Option<Cost>,
    /// 预设和 models.dev 查到的。
    known: Option<Cost>,
}

impl Price {
    fn of(&self, request: &Request) -> Option<f64> {
        if let Some(cost) = &self.listed {
            return Some(charge(cost, &request.tokens));
        }
        request.cost.or_else(|| {
            self.known
                .as_ref()
                .map(|cost| charge(cost, &request.tokens))
        })
    }
}

/// 按每百万 token 的价格算美元；没写缓存价的按输入价算。
fn charge(cost: &Cost, tokens: &Tokens) -> f64 {
    let per = |count: u64, price: f64| count as f64 * price / 1_000_000.;
    per(tokens.input, cost.input)
        + per(tokens.cache_read, cost.cache_read.unwrap_or(cost.input))
        + per(tokens.cache_write, cost.cache_write.unwrap_or(cost.input))
        + per(tokens.output, cost.output)
}

/// 一组请求的合计。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Totals {
    pub requests: u64,
    pub tokens: Tokens,
    /// 美元，只算了有价格的请求。
    pub cost: f64,
    /// 没有价格、没算进 `cost` 的请求数。
    pub unpriced: u64,
}

impl Totals {
    fn add(&mut self, tokens: &Tokens, cost: Option<f64>) {
        self.requests += 1;
        self.tokens.add(tokens);
        match cost {
            Some(cost) => self.cost += cost,
            None => self.unpriced += 1,
        }
    }
}

/// 用量页的时间段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    Today,
    Week,
    Month,
    All,
}

impl Period {
    /// 起点（毫秒）：今天零点，或者往前 6 天、29 天的零点；`All` 是 `None`。
    pub fn since<Tz: TimeZone>(self, now: &DateTime<Tz>) -> Option<i64> {
        let days = match self {
            Self::Today => 0,
            Self::Week => 6,
            Self::Month => 29,
            Self::All => return None,
        };
        let today = now.date_naive();
        Some(start_of(&now.timezone(), today - Duration::days(days)))
    }
}

fn start_of<Tz: TimeZone>(zone: &Tz, day: NaiveDate) -> i64 {
    let midnight = day.and_hms_opt(0, 0, 0).expect("零点总是合法的");
    zone.from_local_datetime(&midnight).earliest().map_or_else(
        || midnight.and_utc().timestamp_millis(),
        |time| time.timestamp_millis(),
    )
}

/// 柱状图一根柱子多长时间。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Hour,
    #[default]
    Day,
    Week,
}

/// 「全部」超过这么多天就按周画。
const MAX_DAY_BARS: i64 = 92;

/// 一根柱子。
#[derive(Debug, Clone, PartialEq)]
pub struct Bucket {
    /// 起点（毫秒）。
    pub start: i64,
    pub totals: Totals,
}

/// 一个时间段的用量。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Summary {
    pub total: Totals,
    /// 柱状图：今天是 24 个小时，其余按天；`All` 从第一条请求那天起，超过三个月按周。
    pub buckets: Vec<Bucket>,
    pub step: Step,
    /// 下面三组按输入加输出（不算缓存）从多到少。
    pub sources: Vec<(Source, Totals)>,
    pub agents: Vec<(AgentKind, Totals)>,
    pub models: Vec<(String, Totals)>,
}

/// 热力图有多少周。
pub const HEATMAP_WEEKS: i64 = 53;

/// 热力图：最近 [`HEATMAP_WEEKS`] 周每天的合计。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Heatmap {
    /// 第一天：52 周前那一周的周一。
    pub first: NaiveDate,
    /// 从 `first` 到今天，一天一个。
    pub days: Vec<Totals>,
}

/// 用量页要的汇总：每个时间段一份，加上热力图。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Overview {
    /// 和传进来的时间段一一对应。
    pub summaries: Vec<Summary>,
    pub heatmap: Heatmap,
}

/// 汇总 `requests` 里落在各个 `periods` 的部分，再按天汇总最近 [`HEATMAP_WEEKS`] 周。`now` 定「今天」和时区。
/// 一遍算完：每个请求只认一次供应商、算一次花费。`requests` 按时间先后排（[`super::Db::requests`] 就是）。
pub fn overview<Tz: TimeZone>(
    requests: &[Request],
    context: &Context,
    periods: &[Period],
    now: &DateTime<Tz>,
) -> Overview {
    let mut tallies: Vec<Tally> = periods
        .iter()
        .map(|period| Tally::new(requests.first(), *period, now))
        .collect();
    let mut days = Days::new(now);
    let since = tallies
        .iter()
        .map(|tally| tally.since)
        .fold(days.since(), i64::min);
    let mut prices = Prices::new();
    for request in requests.iter().filter(|r| r.at >= since) {
        let source = context.source(request);
        let cost = context.cost(&mut prices, &source, request);
        for tally in &mut tallies {
            tally.add(request, &source, cost);
        }
        days.add(request, cost);
    }
    Overview {
        summaries: tallies.into_iter().map(Tally::finish).collect(),
        heatmap: days.finish(),
    }
}

/// 一个时间段的汇总，边加边算。
struct Tally {
    since: i64,
    summary: Summary,
    sources: HashMap<Source, Totals>,
    agents: HashMap<AgentKind, Totals>,
    models: HashMap<String, Totals>,
}

impl Tally {
    /// `earliest` 是最早的一条请求（「全部」的柱子从那天起）。
    fn new<Tz: TimeZone>(earliest: Option<&Request>, period: Period, now: &DateTime<Tz>) -> Self {
        let (step, buckets) = buckets(earliest, period, now);
        Self {
            since: period.since(now).unwrap_or(i64::MIN),
            summary: Summary {
                buckets,
                step,
                ..Summary::default()
            },
            sources: HashMap::new(),
            agents: HashMap::new(),
            models: HashMap::new(),
        }
    }

    fn add(&mut self, request: &Request, source: &Source, cost: Option<f64>) {
        if request.at < self.since {
            return;
        }
        let tokens = &request.tokens;
        self.summary.total.add(tokens, cost);
        let index = self
            .summary
            .buckets
            .partition_point(|bucket| bucket.start <= request.at)
            .saturating_sub(1);
        if let Some(bucket) = self.summary.buckets.get_mut(index) {
            bucket.totals.add(tokens, cost);
        }
        self.sources
            .entry(source.clone())
            .or_default()
            .add(tokens, cost);
        self.agents
            .entry(request.agent)
            .or_default()
            .add(tokens, cost);
        self.models
            .entry(request.model.clone())
            .or_default()
            .add(tokens, cost);
    }

    fn finish(self) -> Summary {
        Summary {
            sources: ranked(self.sources),
            agents: ranked(self.agents),
            models: ranked(self.models),
            ..self.summary
        }
    }
}

/// 热力图，边加边算。
struct Days {
    first: NaiveDate,
    /// 每天零点（毫秒）。
    starts: Vec<i64>,
    days: Vec<Totals>,
}

impl Days {
    fn new<Tz: TimeZone>(now: &DateTime<Tz>) -> Self {
        let zone = now.timezone();
        let today = now.date_naive();
        let back = i64::from(today.weekday().num_days_from_monday()) + 7 * (HEATMAP_WEEKS - 1);
        let first = today - Duration::days(back);
        let starts: Vec<i64> = first
            .iter_days()
            .take_while(|day| *day <= today)
            .map(|day| start_of(&zone, day))
            .collect();
        Self {
            first,
            days: vec![Totals::default(); starts.len()],
            starts,
        }
    }

    fn since(&self) -> i64 {
        self.starts[0]
    }

    fn add(&mut self, request: &Request, cost: Option<f64>) {
        if request.at < self.since() {
            return;
        }
        let index = self.starts.partition_point(|start| *start <= request.at) - 1;
        self.days[index].add(&request.tokens, cost);
    }

    fn finish(self) -> Heatmap {
        Heatmap {
            first: self.first,
            days: self.days,
        }
    }
}

fn ranked<K: Ord>(groups: HashMap<K, Totals>) -> Vec<(K, Totals)> {
    let mut groups: Vec<_> = groups.into_iter().collect();
    groups.sort_by(|(a, x), (b, y)| {
        y.tokens
            .in_out()
            .cmp(&x.tokens.in_out())
            .then(y.tokens.total().cmp(&x.tokens.total()))
            .then(a.cmp(b))
    });
    groups
}

/// 柱子的起点。`earliest` 是最早的一条请求（「全部」从那天画起）。
fn buckets<Tz: TimeZone>(
    earliest: Option<&Request>,
    period: Period,
    now: &DateTime<Tz>,
) -> (Step, Vec<Bucket>) {
    let zone = now.timezone();
    let today = now.date_naive();
    let bucket = |start| Bucket {
        start,
        totals: Totals::default(),
    };
    if period == Period::Today {
        let midnight = start_of(&zone, today);
        let hours = (0..24).map(|hour| bucket(midnight + hour * 3_600_000));
        return (Step::Hour, hours.collect());
    }
    let first = match period.since(now) {
        Some(since) => zone.timestamp_millis_opt(since).earliest(),
        None => earliest.and_then(|r| zone.timestamp_millis_opt(r.at).earliest()),
    }
    .map_or(today, |time| time.date_naive())
    .min(today);
    let (step, days) = if (today - first).num_days() >= MAX_DAY_BARS {
        (Step::Week, 7)
    } else {
        (Step::Day, 1)
    };
    let starts = first
        .iter_days()
        .step_by(days)
        .take_while(|day| *day <= today)
        .map(|day| bucket(start_of(&zone, day)));
    (step, starts.collect())
}

#[cfg(test)]
mod tests {
    use chrono::{FixedOffset, TimeZone};

    use super::{Context, Period, Source, Step, Summary, overview};
    use crate::agent::AgentKind;
    use crate::provider::{Cost, Model, Provider};
    use crate::usage::history::{Switch, Target};
    use crate::usage::{Request, Tokens};

    fn summarize(
        requests: &[Request],
        context: &Context,
        period: Period,
        now: &chrono::DateTime<FixedOffset>,
    ) -> Summary {
        overview(requests, context, &[period], now)
            .summaries
            .remove(0)
    }

    fn request(agent: AgentKind, at: i64, model: &str, provider: Option<&str>) -> Request {
        Request {
            key: format!("{at}"),
            agent,
            at,
            session: "s".into(),
            model: model.into(),
            provider: provider.map(str::to_string),
            tokens: Tokens {
                input: 1_000_000,
                cache_read: 0,
                cache_write: 0,
                output: 0,
                reasoning: 0,
            },
            cost: None,
        }
    }

    fn provider(id: &str, model: &str, price: Option<f64>) -> Provider {
        Provider {
            id: id.into(),
            name: id.to_uppercase(),
            models: vec![Model {
                id: model.into(),
                cost: price.map(|input| Cost {
                    input,
                    output: input,
                    cache_read: None,
                    cache_write: None,
                }),
                ..Model::default()
            }],
            ..Provider::default()
        }
    }

    #[test]
    fn requests_are_attributed_in_order() {
        let providers = [
            provider("ds", "deepseek-v4", Some(2.)),
            provider("glm", "glm-5", None),
        ];
        let history = [Switch {
            at: 100,
            agent: AgentKind::ClaudeCode,
            to: Target::Provider {
                id: "ds".into(),
                name: "DS".into(),
            },
        }];
        let context = Context {
            providers: &providers,
            history: &history,
            dev: None,
        };
        let source = |r: Request| context.source(&r);
        // 记录里写着的。
        assert_eq!(
            source(request(AgentKind::Pi, 0, "x", Some("confdeck-glm"))),
            Source::Provider("glm".into())
        );
        assert_eq!(
            source(request(AgentKind::Pi, 0, "x", Some("anthropic"))),
            Source::Other(AgentKind::Pi, "anthropic".into())
        );
        assert_eq!(
            source(request(AgentKind::Codex, 0, "glm-5", Some("openai"))),
            Source::Official(AgentKind::Codex)
        );
        // 切换记录。
        assert_eq!(
            source(request(AgentKind::ClaudeCode, 150, "anything", None)),
            Source::Provider("ds".into())
        );
        // 切换记录之前：按模型认。
        assert_eq!(
            source(request(AgentKind::ClaudeCode, 50, "claude-opus-5-5", None)),
            Source::Official(AgentKind::ClaudeCode)
        );
        assert_eq!(
            source(request(AgentKind::ClaudeCode, 50, "GLM-5", None)),
            Source::Provider("glm".into())
        );
        assert_eq!(
            source(request(AgentKind::Codex, 50, "gpt-6", Some("custom"))),
            Source::Unknown
        );
        assert_eq!(
            source(request(AgentKind::GrokBuild, 50, "grok-4.6", None)),
            Source::Official(AgentKind::GrokBuild)
        );
        assert_eq!(context.provider_name("ds").as_deref(), Some("DS"));
    }

    #[test]
    fn summary_groups_and_prices() {
        let providers = [provider("ds", "deepseek-v4", Some(2.))];
        let context = Context {
            providers: &providers,
            history: &[],
            dev: None,
        };
        let zone = FixedOffset::east_opt(8 * 3600).unwrap();
        let now = zone.with_ymd_and_hms(2026, 10, 12, 15, 0, 0).unwrap();
        let today = zone
            .with_ymd_and_hms(2026, 10, 12, 0, 0, 0)
            .unwrap()
            .timestamp_millis();
        let hour = 3_600_000;
        let mut reported = request(AgentKind::Pi, today + 9 * hour, "kimi-k3", Some("moonshot"));
        reported.cost = Some(0.5);
        let requests = [
            request(AgentKind::ClaudeCode, today - 6 * hour, "deepseek-v4", None),
            request(AgentKind::ClaudeCode, today + 9 * hour, "deepseek-v4", None),
            request(AgentKind::ClaudeCode, today + 10 * hour, "mystery", None),
            reported,
        ];

        let day = summarize(&requests, &context, Period::Today, &now);
        assert_eq!(day.total.requests, 3);
        assert_eq!(day.total.cost, 2.5);
        assert_eq!(day.total.unpriced, 1);
        assert_eq!(day.buckets.len(), 24);
        assert_eq!(day.buckets[9].totals.requests, 2);
        assert_eq!(day.buckets[10].totals.requests, 1);
        assert_eq!(day.sources.len(), 3);
        assert_eq!(day.agents[0].0, AgentKind::ClaudeCode);

        let week = summarize(&requests, &context, Period::Week, &now);
        assert_eq!(week.buckets.len(), 7);
        assert_eq!(week.buckets[5].totals.requests, 1);
        assert_eq!(week.buckets[6].totals.requests, 3);
        assert_eq!(
            week.models[0],
            ("deepseek-v4".to_string(), week.models[0].1.clone())
        );
        assert_eq!(week.models[0].1.cost, 4.);

        let all = summarize(&requests, &context, Period::All, &now);
        assert_eq!(all.buckets.len(), 2);
        assert_eq!(all.step, Step::Day);
        assert_eq!(all.total.requests, 4);

        // 半年前的一条：按周画，最后一根含今天。
        let mut old = requests.to_vec();
        old.insert(
            0,
            request(AgentKind::Codex, today - 180 * 24 * hour, "gpt-6", None),
        );
        let all = summarize(&old, &context, Period::All, &now);
        assert_eq!(all.step, Step::Week);
        assert_eq!(all.buckets.len(), 26);
        assert_eq!(all.buckets[0].totals.requests, 1);
        assert_eq!(all.buckets[25].totals.requests, 4);

        // 热力图：今天（周一）是第 53 周的第一天，一年多以前的不算。
        old.insert(
            0,
            request(AgentKind::Codex, today - 400 * 24 * hour, "gpt-6", None),
        );
        let map = overview(&old, &context, &[Period::Today], &now).heatmap;
        assert_eq!(map.first, now.date_naive() - chrono::Duration::days(52 * 7));
        assert_eq!(map.days.len(), 52 * 7 + 1);
        assert_eq!(map.days[364].requests, 3);
        assert_eq!(map.days[364].cost, 2.5);
        assert_eq!(map.days[363].requests, 1);
        assert_eq!(map.days.iter().map(|d| d.requests).sum::<u64>(), 5);
    }

    /// 一遍算几个时间段，和一个一个算的一样；热力图不受时间段影响。
    #[test]
    fn periods_together_are_each_period_alone() {
        let providers = [provider("ds", "deepseek-v4", Some(2.))];
        let history = [Switch {
            at: 0,
            agent: AgentKind::ClaudeCode,
            to: Target::Provider {
                id: "ds".into(),
                name: "DS".into(),
            },
        }];
        let context = Context {
            providers: &providers,
            history: &history,
            dev: None,
        };
        let zone = FixedOffset::east_opt(8 * 3600).unwrap();
        let now = zone.with_ymd_and_hms(2026, 10, 14, 15, 0, 0).unwrap();
        let day = 24 * 3_600_000;
        let today = zone
            .with_ymd_and_hms(2026, 10, 14, 0, 0, 0)
            .unwrap()
            .timestamp_millis();
        let models = ["deepseek-v4", "claude-opus-5-5", "mystery"];
        let mut requests: Vec<Request> = [500, 200, 40, 20, 6, 3, 1, 0]
            .into_iter()
            .enumerate()
            .map(|(i, back)| {
                request(
                    AgentKind::ClaudeCode,
                    today - back * day + i as i64 * 3_600_000,
                    models[i % 3],
                    None,
                )
            })
            .collect();
        requests[3].cost = Some(0.25);

        let periods = [Period::Today, Period::Week, Period::Month, Period::All];
        let together = overview(&requests, &context, &periods, &now);
        for (period, summary) in periods.iter().zip(&together.summaries) {
            assert_eq!(
                *summary,
                summarize(&requests, &context, *period, &now),
                "{period:?}"
            );
        }
        assert_eq!(together.summaries[3].total.requests, 8);
        assert_eq!(together.summaries[2].total.requests, 5);
        // 只要今天的时候，一年以内的也照样进热力图。
        let today_only = overview(&requests, &context, &[Period::Today], &now);
        assert_eq!(today_only.heatmap, together.heatmap);
        assert_eq!(
            together
                .heatmap
                .days
                .iter()
                .map(|d| d.requests)
                .sum::<u64>(),
            7
        );
    }
}
