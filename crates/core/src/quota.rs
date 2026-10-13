//! 订阅额度：官方订阅和各家套餐的限额窗口用了多少、什么时候重置（用量页顶上的卡片）。照 fork 的
//! `subscription.rs`、`subscription_grok.rs`、`coding_plan.rs`（ccs-lite）。
//!
//! | 哪个 | 凭证 | 怎么查 |
//! | --- | --- | --- |
//! | Claude 官方 | Claude Code 的登录：macOS 钥匙串 `Claude Code-credentials`，没有再读 `~/.claude/.credentials.json` | `GET api.anthropic.com/api/oauth/usage` |
//! | Codex 官方 | 不用凭证 | 会话记录里最新的 `rate_limits`（[`crate::usage`] 扫的时候存下），不联网 |
//! | Grok Build 官方 | `~/.grok/auth.json` | `GET cli-chat-proxy.grok.com/v1/billing?format=credits` |
//! | Kimi For Coding | 供应商的 Key | `GET api.kimi.com/coding/v1/usages`（海外 `api.kimi.ai`） |
//! | 智谱 / Z.AI Coding Plan | 供应商的 Key（不加 `Bearer`） | `GET open.bigmodel.cn/api/monitor/usage/quota/limit`（海外 `api.z.ai`） |
//! | MiniMax Token Plan（Key 不以 `sk-api-` 开头） | 供应商的 Key | `GET api.minimax.cn/v1/token_plan/remains`（海外 `api.minimax.io`） |
//! | OpenCode Go | 供应商的 Key | `GET opencode.ai/zen/go/v1/usage` |
//!
//! 官方的登录凭证只读不写：访问令牌过期了照样试一次，被拒就报登录过期，等 agent 自己下次运行时换新的。
//! 自己去换会让 agent 手里的刷新令牌作废。火山方舟的套餐要账号的 AccessKey（不是推理的 Key），ZenMux 要管理 Key，这里不查。

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::net::{Http, NetError};
use crate::provider::{self, Provider};
use crate::usage::Limit;

const TIMEOUT: Duration = Duration::from_secs(15);

/// 一个账号或一个套餐的额度。
#[derive(Debug, Clone, PartialEq)]
pub struct Quota {
    /// 套餐名（Claude 的 `subscriptionType`、Codex 的 `plan_type`、智谱的 `level`），原样，没有是 `None`。
    pub plan: Option<String>,
    pub windows: Vec<Window>,
}

/// 一个限额窗口。
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub span: Span,
    /// 用了百分之几，0–100（超额时可能更大）。
    pub used: f64,
    /// 什么时候重置（毫秒）。
    pub resets_at: Option<i64>,
}

/// 窗口多长。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Span {
    FiveHours,
    Week,
    Month,
    /// 只算一个模型的周限额（Claude 的 Fable，老账号还有 Opus、Sonnet）。
    WeekOf(String),
    /// 别的长度（Codex 的 `window_minutes`）。
    Minutes(u32),
    /// 不知道多长（Grok 的账单周期没写类型）。
    Period,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum QuotaError {
    /// 没有登录凭证（这个 agent 没用官方登录），界面不画这张卡。
    #[error("没有登录")]
    NotLoggedIn,
    /// 官方登录被拒：访问令牌过期，agent 下次运行时会换新的。
    #[error("登录过期")]
    Expired,
    #[error("Key 不对")]
    Auth,
    /// Key 是对的，但没有套餐（OpenCode Go 回 403、智谱一个窗口都没有）。
    #[error("没有套餐")]
    NoPlan,
    #[error("{0}")]
    Net(NetError),
    /// 回的不是认得的样子，或者业务码不是成功。
    #[error("返回的内容认不出：{0}")]
    Parse(String),
}

// ── 套餐 ──────────────────────────────────────────────

/// 能查套餐额度的几家。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    Kimi { intl: bool },
    Zhipu { intl: bool },
    MiniMax { intl: bool },
    OpencodeGo,
}

fn plan_of(provider: &Provider) -> Option<Plan> {
    let key = provider.key.trim();
    provider.endpoints.values().find_map(|url| {
        let host = provider::host(url)?.to_ascii_lowercase();
        let path = url.to_ascii_lowercase();
        Some(match host.as_str() {
            "api.kimi.com" => Plan::Kimi { intl: false },
            "api.kimi.ai" => Plan::Kimi { intl: true },
            // 按量和套餐是同一个地址：用套餐预设建的，或者地址里有 `/coding` 的才算套餐。
            "open.bigmodel.cn" | "bigmodel.cn" | "api.z.ai" => {
                let coding = matches!(
                    provider.preset.as_deref(),
                    Some("zhipu-coding" | "zai-coding")
                ) || path.contains("/coding");
                if !coding {
                    return None;
                }
                Plan::Zhipu {
                    intl: host == "api.z.ai",
                }
            }
            // 按量的 Key 以 `sk-api-` 开头，查的是余额（[`crate::balance`]）。
            "api.minimax.cn" | "api.minimaxi.com" if !key.starts_with("sk-api-") => {
                Plan::MiniMax { intl: false }
            }
            "api.minimax.io" if !key.starts_with("sk-api-") => Plan::MiniMax { intl: true },
            "opencode.ai" if path.contains("/zen/go") => Plan::OpencodeGo,
            _ => return None,
        })
    })
}

/// 这家能查套餐额度（地址认得出，填了 Key）。
pub fn plan_supported(provider: &Provider) -> bool {
    plan_of(provider).is_some() && !provider.key.trim().is_empty()
}

/// 查这家套餐的额度。先用 [`plan_supported`] 筛：查不了的这里报「认不出」。
pub fn plan(http: &Http, provider: &Provider) -> Result<Quota, QuotaError> {
    let key = provider.key.trim();
    let Some(plan) = plan_of(provider).filter(|_| !key.is_empty()) else {
        return Err(QuotaError::Parse("unsupported".into()));
    };
    let url = match plan {
        Plan::Kimi { intl: false } => "https://api.kimi.com/coding/v1/usages",
        Plan::Kimi { intl: true } => "https://api.kimi.ai/coding/v1/usages",
        Plan::Zhipu { intl: false } => "https://open.bigmodel.cn/api/monitor/usage/quota/limit",
        Plan::Zhipu { intl: true } => "https://api.z.ai/api/monitor/usage/quota/limit",
        Plan::MiniMax { intl: false } => "https://api.minimax.cn/v1/token_plan/remains",
        Plan::MiniMax { intl: true } => "https://api.minimax.io/v1/token_plan/remains",
        Plan::OpencodeGo => "https://opencode.ai/zen/go/v1/usage",
    };
    let bearer = format!("Bearer {key}");
    // 智谱的 Authorization 不加 `Bearer`。
    let auth = if matches!(plan, Plan::Zhipu { .. }) {
        key
    } else {
        bearer.as_str()
    };
    let headers = [("Authorization", auth), ("Accept", "application/json")];
    let bytes = http
        .get(url, &headers, TIMEOUT, &[key])
        .map_err(|err| match err {
            // OpenCode Go：Key 对，但这个工作区没订 Go。
            NetError::Status { status: 403, .. } if plan == Plan::OpencodeGo => QuotaError::NoPlan,
            NetError::Status {
                status: 401 | 403, ..
            } => QuotaError::Auth,
            err => QuotaError::Net(err),
        })?;
    let body = json(&bytes)?;
    match plan {
        Plan::Kimi { .. } => parse_kimi(&body),
        Plan::Zhipu { .. } => parse_zhipu(&body),
        Plan::MiniMax { .. } => parse_minimax(&body),
        Plan::OpencodeGo => parse_opencode_go(&body),
    }
}

/// Kimi：现在是 `usages.{limit_5h, limit_7d, limit_month_total}`，各带 0–1 的 `used_ratio` 和 `reset_time`；
/// 旧的是 `limits[].detail`（5 小时）和 `usage`（每周），各带 `limit`、`used` 或 `remaining`、`resetTime`。两种都认。
fn parse_kimi(body: &Value) -> Result<Quota, QuotaError> {
    if let Some(usages) = body.get("usages").and_then(Value::as_object) {
        let windows: Vec<Window> = [
            ("limit_5h", Span::FiveHours),
            ("limit_7d", Span::Week),
            ("limit_month_total", Span::Month),
        ]
        .into_iter()
        .filter_map(|(key, span)| {
            let window = usages.get(key)?;
            Some(Window {
                span,
                used: number(&window["used_ratio"]).filter(|r| r.is_finite())? * 100.,
                resets_at: time(&window["reset_time"]),
            })
        })
        .collect();
        if windows.is_empty() {
            return Err(unknown(body));
        }
        return Ok(Quota {
            plan: None,
            windows,
        });
    }
    let old = |detail: &Value, span: Span| {
        let limit = number(&detail["limit"]).unwrap_or(1.);
        let used = number(&detail["used"])
            .unwrap_or_else(|| (limit - number(&detail["remaining"]).unwrap_or(0.)).max(0.));
        Window {
            span,
            used: if limit > 0. { used / limit * 100. } else { 0. },
            resets_at: time(&detail["resetTime"]),
        }
    };
    let limits = body.get("limits").and_then(Value::as_array);
    let usage = body.get("usage").filter(|v| v.is_object());
    if limits.is_none() && usage.is_none() {
        return Err(unknown(body));
    }
    let mut windows: Vec<Window> = limits
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("detail"))
        .map(|detail| old(detail, Span::FiveHours))
        .collect();
    windows.extend(usage.map(|usage| old(usage, Span::Week)));
    Ok(Quota {
        plan: None,
        windows,
    })
}

/// 智谱、Z.AI：`data.limits[]` 里 `type` 是 `TOKENS_LIMIT`（或 `CREDIT_LIMIT`）的，`unit` 3 是 5 小时、6 是每周，
/// `percentage` 已用，`nextResetTime` 毫秒；`data.level` 是套餐。没有 `unit` 的按重置时间先后补空位
/// （2026-02-12 以前订的老套餐只有 5 小时那一条）。
fn parse_zhipu(body: &Value) -> Result<Quota, QuotaError> {
    if body["success"] == false {
        return Err(QuotaError::Parse(truncate(&body.to_string())));
    }
    let data = body.get("data").ok_or_else(|| unknown(body))?;
    let mut five = None;
    let mut week = None;
    let mut loose = Vec::new();
    for item in data["limits"].as_array().into_iter().flatten() {
        let kind = item["type"].as_str().unwrap_or_default();
        if !kind.eq_ignore_ascii_case("TOKENS_LIMIT") && !kind.eq_ignore_ascii_case("CREDIT_LIMIT")
        {
            continue;
        }
        let entry = (
            number(&item["percentage"]).unwrap_or(0.),
            time(&item["nextResetTime"]),
        );
        match item["unit"].as_i64() {
            Some(3) if five.is_none() => five = Some(entry),
            Some(6) if week.is_none() => week = Some(entry),
            _ => loose.push(entry),
        }
    }
    // 没有重置时间的先补 5 小时（用量 0 时 5 小时那条可能没有重置时间）。
    loose.sort_by_key(|(_, reset)| (reset.is_some(), reset.unwrap_or(i64::MIN)));
    for entry in loose {
        if five.is_none() {
            five = Some(entry);
        } else if week.is_none() {
            week = Some(entry);
        }
    }
    let windows: Vec<Window> = [(Span::FiveHours, five), (Span::Week, week)]
        .into_iter()
        .filter_map(|(span, entry)| {
            let (used, resets_at) = entry?;
            Some(Window {
                span,
                used,
                resets_at,
            })
        })
        .collect();
    if windows.is_empty() {
        return Err(QuotaError::NoPlan);
    }
    Ok(Quota {
        plan: data["level"].as_str().map(str::to_string),
        windows,
    })
}

/// MiniMax：鉴权失败时 HTTP 也是 200，看 `base_resp.status_code`（1004、2049 是 Key 不对）。
/// `model_remains[]` 里 `model_name` 是 `general` 的那条：`current_interval_*` 是 5 小时，`current_weekly_*` 是每周，
/// 写的是剩下的百分比；`*_status` 3 是不限额（不画），2 是用完了。
fn parse_minimax(body: &Value) -> Result<Quota, QuotaError> {
    let code = &body["base_resp"]["status_code"];
    match code.as_i64() {
        Some(0) | None => {}
        Some(1004 | 2049) => return Err(QuotaError::Auth),
        Some(_) => return Err(QuotaError::Parse(truncate(&body.to_string()))),
    }
    let item = body["model_remains"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["model_name"] == "general"))
        .ok_or_else(|| unknown(body))?;
    let window = |status: &str, left: &str, end: &str, span: Span| {
        let status = item[status].as_i64();
        if status == Some(3) {
            return None;
        }
        let left = number(&item[left]).or((status == Some(2)).then_some(0.))?;
        Some(Window {
            span,
            used: 100. - left,
            resets_at: time(&item[end]),
        })
    };
    let windows: Vec<Window> = [
        window(
            "current_interval_status",
            "current_interval_remaining_percent",
            "end_time",
            Span::FiveHours,
        ),
        window(
            "current_weekly_status",
            "current_weekly_remaining_percent",
            "weekly_end_time",
            Span::Week,
        ),
    ]
    .into_iter()
    .flatten()
    .collect();
    Ok(Quota {
        plan: None,
        windows,
    })
}

/// OpenCode Go：`usage.{rolling, weekly, monthly}`，各带已用的 `percent` 和 `resetsAt`。没文档的接口，
/// 一个窗口读不出就跳过；`percent` 是 0 时 `resetsAt` 是占位的，不要。
fn parse_opencode_go(body: &Value) -> Result<Quota, QuotaError> {
    let usage = &body["usage"];
    let windows: Vec<Window> = [
        ("rolling", Span::FiveHours),
        ("weekly", Span::Week),
        ("monthly", Span::Month),
    ]
    .into_iter()
    .filter_map(|(key, span)| {
        let window = &usage[key];
        let used = number(&window["percent"])?;
        Some(Window {
            span,
            used,
            resets_at: (used > 0.).then(|| time(&window["resetsAt"])).flatten(),
        })
    })
    .collect();
    if windows.is_empty() {
        return Err(unknown(body));
    }
    Ok(Quota {
        plan: None,
        windows,
    })
}

// ── 官方订阅 ──────────────────────────────────────────

/// Codex 官方：会话记录里最新的 `rate_limits`。`primary`、`secondary` 各带 `used_percent`、`window_minutes`、
/// `resets_at`（秒；老版本是相对记录时间的 `resets_in_seconds`）。记下以后已经重置过的窗口算 0、不写重置时间。
pub fn codex(limit: &Limit, now: i64) -> Quota {
    let data = &limit.data;
    let windows = ["primary", "secondary"]
        .iter()
        .filter_map(|key| {
            let window = data.get(*key).filter(|w| w.is_object())?;
            let used = number(&window["used_percent"])?;
            let resets_at = window["resets_at"]
                .as_i64()
                .map(|secs| secs * 1000)
                .or_else(|| {
                    window["resets_in_seconds"]
                        .as_i64()
                        .map(|secs| limit.at + secs * 1000)
                });
            let span = match window["window_minutes"].as_u64() {
                Some(300) => Span::FiveHours,
                Some(10080) => Span::Week,
                Some(43200) => Span::Month,
                Some(minutes) => Span::Minutes(minutes as u32),
                None => Span::Period,
            };
            Some(match resets_at {
                Some(at) if at <= now => Window {
                    span,
                    used: 0.,
                    resets_at: None,
                },
                _ => Window {
                    span,
                    used,
                    resets_at,
                },
            })
        })
        .collect();
    Quota {
        plan: data["plan_type"].as_str().map(str::to_string),
        windows,
    }
}

/// Claude 官方：用 Claude Code 的登录查。`keychain` 时先读 macOS 钥匙串，没有再读 `<home>/.claude/.credentials.json`；
/// 家目录不是真的（测试）时不要读钥匙串，读到的是本机用户的登录。
pub fn claude(http: &Http, home: &Path, keychain: bool) -> Result<Quota, QuotaError> {
    let text = keychain
        .then(claude_keychain)
        .flatten()
        .or_else(|| std::fs::read_to_string(home.join(".claude/.credentials.json")).ok())
        .ok_or(QuotaError::NotLoggedIn)?;
    let creds: Value = serde_json::from_str(&text).map_err(|_| QuotaError::NotLoggedIn)?;
    let entry = creds
        .get("claudeAiOauth")
        .or_else(|| creds.get("claude.ai_oauth"))
        .ok_or(QuotaError::NotLoggedIn)?;
    let token = entry["accessToken"]
        .as_str()
        .filter(|t| !t.is_empty())
        .ok_or(QuotaError::NotLoggedIn)?;
    let auth = format!("Bearer {token}");
    let headers = [
        ("Authorization", auth.as_str()),
        ("anthropic-beta", "oauth-2025-04-20"),
        ("Accept", "application/json"),
    ];
    let bytes = http
        .get(
            "https://api.anthropic.com/api/oauth/usage",
            &headers,
            TIMEOUT,
            &[token],
        )
        .map_err(|err| match err {
            // 401、带 Anthropic 错误体的 403 是令牌被拒；别的 403 和 429 是限流（Claude Code 2.1.284 的分法）。
            NetError::Status { status: 401, .. } => QuotaError::Expired,
            NetError::Status { status: 403, body } if body.contains("\"type\"") => {
                QuotaError::Expired
            }
            err => QuotaError::Net(err),
        })?;
    let mut quota = parse_claude(&json(&bytes)?)?;
    quota.plan = entry["subscriptionType"].as_str().map(str::to_string);
    Ok(quota)
}

#[cfg(target_os = "macos")]
fn claude_keychain() -> Option<String> {
    // Claude Code 自己用 `security` 写进去，`security` 在这一条的信任列表里，读的时候不弹窗。
    // 按 Claude Code 写的账户名找（同 CCometixLine），名字下别的条目不会混进来。
    let output = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-a",
            &keychain_user(
                std::env::var("USER")
                    .or_else(|_| std::env::var("LOGNAME"))
                    .ok(),
            ),
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

#[cfg(not(target_os = "macos"))]
fn claude_keychain() -> Option<String> {
    None
}

/// Claude Code 写钥匙串用的账户名：`USER`，只含字母、数字和 `._-` 才用，不然是 `claude-code-user`。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn keychain_user(user: Option<String>) -> String {
    user.filter(|user| {
        !user.is_empty()
            && user
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    })
    .unwrap_or_else(|| "claude-code-user".to_string())
}

/// Claude 的用量：顶层的 `five_hour`、`seven_day`、`seven_day_fable`、`seven_day_opus`、`seven_day_sonnet` 带 0–100 的
/// `utilization` 和 `resets_at`；新版另有 `limits[]`，`kind` 是 `weekly_scoped` 的按 `scope.model.display_name` 是哪个模型的
/// 周限额（现在是 Fable），用 `percent`，同一个模型的盖掉顶层那条，同一个模型写了两条的用第一条（同 fork）。
/// 别的键这里不认识，不画。`five_hour`、`seven_day` 一个都没有的回答不算数（中转回的错误体）；只缺一个的，
/// 缺的那个是刚重置过、还没用（Claude Code 重置后就不报那个窗口），写 0%（同 CCometixLine）。
fn parse_claude(body: &Value) -> Result<Quota, QuotaError> {
    const KEYS: [&str; 9] = [
        "five_hour",
        "seven_day",
        "seven_day_fable",
        "seven_day_opus",
        "seven_day_sonnet",
        "seven_day_oauth_apps",
        "cinder_cove",
        "extra_usage",
        "limits",
    ];
    if !body
        .as_object()
        .is_some_and(|o| KEYS.iter().any(|k| o.contains_key(*k)))
    {
        return Err(unknown(body));
    }
    let used = |key: &str| number(&body[key]["utilization"]);
    if used("five_hour").is_none() && used("seven_day").is_none() {
        return Err(unknown(body));
    }
    let mut windows: Vec<Window> = [("five_hour", Span::FiveHours), ("seven_day", Span::Week)]
        .into_iter()
        .map(|(key, span)| Window {
            span,
            used: used(key).unwrap_or(0.),
            resets_at: used(key).and_then(|_| time(&body[key]["resets_at"])),
        })
        .collect();
    windows.extend(
        [
            ("seven_day_fable", Span::WeekOf("Fable".into())),
            ("seven_day_opus", Span::WeekOf("Opus".into())),
            ("seven_day_sonnet", Span::WeekOf("Sonnet".into())),
        ]
        .into_iter()
        .filter_map(|(key, span)| {
            Some(Window {
                span,
                used: used(key)?,
                resets_at: time(&body[key]["resets_at"]),
            })
        }),
    );
    let mut scoped: Vec<Span> = Vec::new();
    for limit in body["limits"].as_array().into_iter().flatten() {
        // CCometixLine 见过的回答没有 `group`。
        let weekly = limit["kind"] == "weekly_scoped"
            && limit["group"]
                .as_str()
                .is_none_or(|group| group == "weekly");
        // 某个使用场景的子限额不算整个模型的。
        if !weekly || !limit["scope"]["surface"].is_null() {
            continue;
        }
        let Some(model) = limit["scope"]["model"]["display_name"].as_str() else {
            continue;
        };
        let Some(used) = number(&limit["percent"]).filter(|p| p.is_finite() && *p >= 0.) else {
            continue;
        };
        // 「 fAbLe 」也是 Fable。
        let model = model.trim().to_lowercase();
        let mut chars = model.chars();
        let Some(first) = chars.next() else {
            continue;
        };
        let span = Span::WeekOf(first.to_uppercase().chain(chars).collect());
        if scoped.contains(&span) {
            continue;
        }
        scoped.push(span.clone());
        let window = Window {
            span,
            used,
            resets_at: time(&limit["resets_at"]),
        };
        match windows.iter_mut().find(|w| w.span == window.span) {
            Some(old) => *old = window,
            None => windows.push(window),
        }
    }
    Ok(Quota {
        plan: None,
        windows,
    })
}

/// Grok Build 官方：用 `<home>/.grok/auth.json` 里的登录查。这个文件按登录的范围分条，用 `https://auth.x.ai::` 开头那条，
/// 没有再用老的 `…/sign-in` 那条。账单只回一个额度窗口：`config.creditUsagePercent` 和 `config.currentPeriod`（类型、结束时间）。
pub fn grok(http: &Http, home: &Path) -> Result<Quota, QuotaError> {
    let text = std::fs::read_to_string(home.join(".grok/auth.json"))
        .map_err(|_| QuotaError::NotLoggedIn)?;
    let auth: Value = serde_json::from_str(&text).map_err(|_| QuotaError::NotLoggedIn)?;
    let entries = auth.as_object().ok_or(QuotaError::NotLoggedIn)?;
    let usable = |entry: &Value| entry["key"].as_str().is_some_and(|k| !k.is_empty());
    let entry = entries
        .iter()
        .find(|(scope, entry)| scope.starts_with("https://auth.x.ai::") && usable(entry))
        .or_else(|| {
            entries
                .iter()
                .find(|(scope, entry)| scope.contains("/sign-in") && usable(entry))
        })
        .map(|(_, entry)| entry)
        .ok_or(QuotaError::NotLoggedIn)?;
    let token = entry["key"].as_str().unwrap_or_default();
    let bearer = format!("Bearer {token}");
    let mut headers = vec![
        ("Authorization", bearer.as_str()),
        ("X-XAI-Token-Auth", "xai-grok-cli"),
        ("x-grok-client-mode", "headless"),
        ("Accept", "application/json"),
    ];
    if let Some(user) = entry["user_id"].as_str().filter(|id| !id.is_empty()) {
        headers.push(("x-userid", user));
    }
    let bytes = http
        .get(
            "https://cli-chat-proxy.grok.com/v1/billing?format=credits",
            &headers,
            TIMEOUT,
            &[token],
        )
        .map_err(|err| match err {
            NetError::Status {
                status: 401 | 403, ..
            } => QuotaError::Expired,
            err => QuotaError::Net(err),
        })?;
    parse_grok(&json(&bytes)?)
}

fn parse_grok(body: &Value) -> Result<Quota, QuotaError> {
    let config = body
        .get("config")
        .filter(|c| c.is_object())
        .ok_or_else(|| unknown(body))?;
    let period = &config["currentPeriod"];
    let span = match period["type"].as_str() {
        Some("USAGE_PERIOD_TYPE_WEEKLY") => Span::Week,
        Some("USAGE_PERIOD_TYPE_MONTHLY") => Span::Month,
        _ => Span::Period,
    };
    Ok(Quota {
        plan: None,
        windows: vec![Window {
            span,
            used: number(&config["creditUsagePercent"])
                .unwrap_or(0.)
                .clamp(0., 100.),
            resets_at: time(&period["end"]),
        }],
    })
}

// ── 小工具 ────────────────────────────────────────────

fn json(bytes: &[u8]) -> Result<Value, QuotaError> {
    serde_json::from_slice(bytes).map_err(|err| QuotaError::Parse(err.to_string()))
}

fn unknown(body: &Value) -> QuotaError {
    QuotaError::Parse(truncate(&body.to_string()))
}

/// 数字或写成字符串的数字。
fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
}

/// 时间（毫秒）：ISO 8601 字符串，或者秒、毫秒的数字（大于 1e12 的是毫秒）；0 和负数是没有。
fn time(value: &Value) -> Option<i64> {
    if let Some(text) = value.as_str().filter(|t| !t.is_empty()) {
        if let Ok(time) = chrono::DateTime::parse_from_rfc3339(text) {
            return Some(time.timestamp_millis());
        }
        let naive = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f").ok()?;
        return Some(naive.and_utc().timestamp_millis());
    }
    let n = value.as_i64().filter(|n| *n > 0)?;
    Some(if n < 1_000_000_000_000 { n * 1000 } else { n })
}

fn truncate(text: &str) -> String {
    text.chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        Plan, QuotaError, Span, Window, codex, keychain_user, parse_claude, parse_grok, parse_kimi,
        parse_minimax, parse_opencode_go, parse_zhipu, plan_of,
    };
    use crate::provider::{Protocol, Provider};
    use crate::usage::Limit;

    fn provider(url: &str, key: &str, preset: Option<&str>) -> Provider {
        Provider {
            id: "p".into(),
            name: "P".into(),
            preset: preset.map(str::to_string),
            key: key.into(),
            endpoints: [(Protocol::Anthropic, url.to_string())].into(),
            models: vec![],
            default_model: None,
            overrides: Default::default(),
        }
    }

    #[test]
    fn plans_are_told_by_address_and_key() {
        let of = |url, key, preset| plan_of(&provider(url, key, preset));
        assert_eq!(
            of("https://api.kimi.com/coding", "k", None),
            Some(Plan::Kimi { intl: false })
        );
        assert_eq!(
            of(
                "https://open.bigmodel.cn/api/anthropic",
                "k",
                Some("zhipu-coding")
            ),
            Some(Plan::Zhipu { intl: false })
        );
        // 按量的智谱和 MiniMax 不查套餐。
        assert_eq!(
            of("https://open.bigmodel.cn/api/anthropic", "k", Some("zhipu")),
            None
        );
        assert_eq!(
            of("https://api.minimax.io/anthropic", "sk-api-1", None),
            None
        );
        assert_eq!(
            of("https://api.minimax.io/anthropic", "sk-cp-1", None),
            Some(Plan::MiniMax { intl: true })
        );
        assert_eq!(
            of("https://opencode.ai/zen/go/v1", "k", None),
            Some(Plan::OpencodeGo)
        );
        assert_eq!(of("https://opencode.ai/zen/v1", "k", None), None);
    }

    #[test]
    fn kimi_reads_both_shapes() {
        let now = json!({"usages": {
            "limit_5h": {"used_ratio": "0.25", "reset_time": "2026-10-12T10:00:00Z"},
            "limit_7d": {"used_ratio": 0.5, "reset_time": "2026-10-15T00:00:00Z"},
            "limit_month_code": {"used_ratio": 0.1}}});
        let quota = parse_kimi(&now).unwrap();
        assert_eq!(quota.windows.len(), 2);
        assert_eq!(quota.windows[0].used, 25.);
        assert_eq!(quota.windows[1].span, Span::Week);
        assert_eq!(quota.windows[0].resets_at, Some(1_791_799_200_000));

        let old = json!({"limits": [{"detail": {"limit": 100, "remaining": 40, "resetTime": 1_791_799_200}}],
            "usage": {"limit": 1000, "used": 100}});
        let quota = parse_kimi(&old).unwrap();
        assert_eq!(quota.windows[0].used, 60.);
        assert_eq!(quota.windows[0].resets_at, Some(1_791_799_200_000));
        assert_eq!(quota.windows[1].used, 10.);
        assert!(parse_kimi(&json!({"usages": {}})).is_err());
    }

    #[test]
    fn zhipu_sorts_windows_by_unit_then_by_reset() {
        let body = json!({"success": true, "data": {"level": "pro", "limits": [
            {"type": "TOKENS_LIMIT", "unit": 6, "number": 7, "percentage": 40, "nextResetTime": 1_791_799_200_000_i64},
            {"type": "TOKENS_LIMIT", "unit": 3, "number": 5, "percentage": 12, "nextResetTime": 1_791_900_000_000_i64},
            {"type": "TIME_LIMIT", "percentage": 99}]}});
        let quota = parse_zhipu(&body).unwrap();
        assert_eq!(quota.plan.as_deref(), Some("pro"));
        assert_eq!(quota.windows[0].span, Span::FiveHours);
        assert_eq!(quota.windows[0].used, 12.);
        assert_eq!(quota.windows[1].used, 40.);

        let none = json!({"success": true, "data": {"limits": []}});
        assert_eq!(parse_zhipu(&none), Err(QuotaError::NoPlan));
        let failed = json!({"success": false, "msg": "nope"});
        assert!(matches!(parse_zhipu(&failed), Err(QuotaError::Parse(_))));
    }

    #[test]
    fn minimax_reads_what_is_left() {
        let body = json!({"base_resp": {"status_code": 0}, "model_remains": [
            {"model_name": "video", "current_interval_remaining_percent": 1},
            {"model_name": "general", "current_interval_status": 1, "current_interval_remaining_percent": 70,
             "end_time": 1_791_799_200_000_i64, "current_weekly_status": 3}]});
        let quota = parse_minimax(&body).unwrap();
        assert_eq!(
            quota.windows,
            vec![Window {
                span: Span::FiveHours,
                used: 30.,
                resets_at: Some(1_791_799_200_000)
            }]
        );
        let bad = json!({"base_resp": {"status_code": 2049, "status_msg": "invalid api key"}});
        assert_eq!(parse_minimax(&bad), Err(QuotaError::Auth));
    }

    #[test]
    fn opencode_go_drops_placeholder_resets() {
        let body = json!({"usage": {
            "rolling": {"status": "ok", "percent": 0, "resetsAt": "2026-10-12T10:00:00Z"},
            "weekly": {"status": "ok", "percent": 35, "resetsAt": "2026-10-15T00:00:00Z"}}});
        let quota = parse_opencode_go(&body).unwrap();
        assert_eq!(quota.windows[0].resets_at, None);
        assert_eq!(quota.windows[1].used, 35.);
        assert!(quota.windows[1].resets_at.is_some());
    }

    #[test]
    fn codex_windows_that_reset_since_count_as_unused() {
        let limit = Limit {
            at: 1_000_000,
            data: json!({"plan_type": "pro",
                "primary": {"used_percent": 30.0, "window_minutes": 300, "resets_at": 1_500},
                "secondary": {"used_percent": 12.0, "window_minutes": 10080, "resets_at": 9_000}}),
        };
        let quota = codex(&limit, 2_000_000);
        assert_eq!(quota.plan.as_deref(), Some("pro"));
        assert_eq!(
            quota.windows[0],
            Window {
                span: Span::FiveHours,
                used: 0.,
                resets_at: None
            }
        );
        assert_eq!(quota.windows[1].used, 12.);
        assert_eq!(quota.windows[1].resets_at, Some(9_000_000));
    }

    #[test]
    fn claude_scoped_limits_replace_top_level_ones() {
        let body = json!({
            "five_hour": {"utilization": 20.0, "resets_at": "2026-10-12T10:00:00+00:00"},
            "seven_day": {"utilization": 41.0, "resets_at": "2026-10-15T00:00:00+00:00"},
            "seven_day_opus": {"utilization": 5.0, "resets_at": null},
            "seven_day_sonnet": null,
            "limits": [
                {"kind": "weekly_scoped", "group": "weekly", "percent": 9.0,
                 "scope": {"model": {"display_name": "opus"}}, "resets_at": "2026-10-15T00:00:00+00:00"},
                {"kind": "weekly_scoped", "group": "weekly", "percent": 70.0,
                 "scope": {"model": {"display_name": "Fable"}, "surface": "web"}},
                {"kind": "weekly_scoped", "group": "weekly", "percent": 37.5,
                 "scope": {"model": {"display_name": "  fAbLe "}}, "resets_at": null},
                {"kind": "weekly_scoped", "group": "weekly", "percent": 90.0,
                 "scope": {"model": {"display_name": "Fable"}}}]});
        let quota = parse_claude(&body).unwrap();
        let spans: Vec<&Span> = quota.windows.iter().map(|w| &w.span).collect();
        assert_eq!(
            spans,
            [
                &Span::FiveHours,
                &Span::Week,
                &Span::WeekOf("Opus".into()),
                &Span::WeekOf("Fable".into())
            ]
        );
        assert_eq!(quota.windows[2].used, 9.);
        assert_eq!(quota.windows[3].used, 37.5);
        // 顶层的 seven_day_fable 也认。
        let quota = parse_claude(&json!({"seven_day": {"utilization": 41.0},
            "seven_day_fable": {"utilization": 12.0}}))
        .unwrap();
        assert_eq!(quota.windows[2].span, Span::WeekOf("Fable".into()));
    }

    /// CCometixLine 测试里的回答：`five_hour` 是 null，`limits[]` 没有 `group`，混着别的形状。
    #[test]
    fn claude_missing_window_counts_as_unused() {
        let body = json!({"five_hour": null,
            "seven_day": {"utilization": 41.0, "resets_at": "2026-10-08T14:00:00+00:00"},
            "limits": [
                {"kind": "weekly_scoped", "scope": {"model": null}, "percent": 5.0},
                "unexpected",
                {"kind": "daily", "scope": {"model": {"display_name": "Fable"}}, "percent": 80.0},
                {"kind": "weekly_scoped", "scope": {"model": {"display_name": "Fable"}},
                 "percent": 12.4, "resets_at": 1_791_468_000}]});
        let quota = parse_claude(&body).unwrap();
        assert_eq!(
            quota.windows,
            [
                Window {
                    span: Span::FiveHours,
                    used: 0.,
                    resets_at: None
                },
                Window {
                    span: Span::Week,
                    used: 41.,
                    resets_at: Some(1_791_468_000_000)
                },
                Window {
                    span: Span::WeekOf("Fable".into()),
                    used: 12.4,
                    resets_at: Some(1_791_468_000_000)
                }
            ]
        );
        // 两个窗口都没有的不算数。
        for body in [
            json!({}),
            json!({"error": {"type": "rate_limit_error"}}),
            json!({"five_hour": null, "seven_day": {"utilization": null}}),
        ] {
            assert!(parse_claude(&body).is_err(), "{body}");
        }
    }

    #[test]
    fn keychain_user_follows_claude_code() {
        assert_eq!(keychain_user(Some("alice".into())), "alice");
        assert_eq!(keychain_user(Some("a.b_c-1".into())), "a.b_c-1");
        assert_eq!(keychain_user(Some("张三".into())), "claude-code-user");
        assert_eq!(keychain_user(Some(String::new())), "claude-code-user");
        assert_eq!(keychain_user(None), "claude-code-user");
    }

    #[test]
    fn grok_names_the_period() {
        let body = json!({"config": {"creditUsagePercent": 140.0,
            "currentPeriod": {"type": "USAGE_PERIOD_TYPE_WEEKLY", "end": "2026-10-15T00:00:00Z"}}});
        let quota = parse_grok(&body).unwrap();
        assert_eq!(quota.windows[0].span, Span::Week);
        assert_eq!(quota.windows[0].used, 100.);
    }
}
