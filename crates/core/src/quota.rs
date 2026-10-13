//! 订阅额度：官方订阅和各家套餐的限额窗口用了多少、什么时候重置（用量页顶上的卡片）。照 fork 的
//! `subscription.rs`、`subscription_grok.rs`、`coding_plan.rs`（ccs-lite）。
//!
//! | 哪个 | 凭证 | 怎么查 |
//! | --- | --- | --- |
//! | Claude 官方 | 不用凭证 | 跑 Claude Code 自己的 `/usage`（`claude -p /usage`），读它打出来的几行 |
//! | Codex 官方 | `~/.codex/auth.json` 的 ChatGPT 登录 | `GET chatgpt.com/backend-api/wham/usage` |
//! | Grok Build 官方 | `~/.grok/auth.json` | `GET cli-chat-proxy.grok.com/v1/billing?format=credits` |
//! | Kimi For Coding | 供应商的 Key | `GET api.kimi.com/coding/v1/usages`（海外 `api.kimi.ai`） |
//! | 智谱 / Z.AI Coding Plan | 供应商的 Key（不加 `Bearer`） | `GET open.bigmodel.cn/api/monitor/usage/quota/limit`（海外 `api.z.ai`） |
//! | MiniMax Token Plan（Key 不以 `sk-api-` 开头） | 供应商的 Key | `GET api.minimax.cn/v1/token_plan/remains`（海外 `api.minimax.io`） |
//! | OpenCode Go | 供应商的 Key | `GET opencode.ai/zen/go/v1/usage` |
//!
//! 官方的登录凭证只读不写：访问令牌过期了照样试一次，被拒就报登录过期，等 agent 自己下次运行时换新的。
//! Codex、Grok Build 也能拿存下的账号（`crate::account`，和登录文件一个格式）查：`codex_login`、`grok_login`。
//! 自己去换会让 agent 手里的刷新令牌作废。火山方舟的套餐要账号的 AccessKey（不是推理的 Key），ZenMux 要管理 Key，这里不查。

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::net::{Http, NetError};
use crate::provider::{self, Provider};

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
    /// 找不到 agent 的命令行程序（查 Claude 官方额度要跑 `claude`）。
    #[error("找不到 {0} 命令")]
    NoProgram(&'static str),
    /// Claude Code 的 `/usage` 跑不起来或者报了别的错，带它的原话。
    #[error("Claude Code：{0}")]
    Claude(String),
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

/// Codex 官方：用 `<home>/.codex/auth.json` 里的 ChatGPT 登录查。登录存在钥匙串里（`cli_auth_credentials_store`）的读不到，
/// 算没登录。
pub fn codex(http: &Http, home: &Path) -> Result<Quota, QuotaError> {
    let bytes =
        std::fs::read(home.join(".codex/auth.json")).map_err(|_| QuotaError::NotLoggedIn)?;
    codex_login(http, &bytes)
}

/// 用一份 Codex 登录（`auth.json` 的内容）查：`tokens.access_token`，有 `tokens.account_id` 时带上
/// `ChatGPT-Account-Id`（同一个人的几个工作区各算各的）。只有 API Key 的不算登录。
pub fn codex_login(http: &Http, login: &[u8]) -> Result<Quota, QuotaError> {
    let auth: Value = serde_json::from_slice(login).map_err(|_| QuotaError::NotLoggedIn)?;
    if auth["auth_mode"] == "apikey" {
        return Err(QuotaError::NotLoggedIn);
    }
    let tokens = &auth["tokens"];
    let token = tokens["access_token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .ok_or(QuotaError::NotLoggedIn)?;
    let bearer = format!("Bearer {token}");
    let mut headers = vec![
        ("Authorization", bearer.as_str()),
        ("Accept", "application/json"),
    ];
    if let Some(account) = tokens["account_id"].as_str().filter(|id| !id.is_empty()) {
        headers.push(("ChatGPT-Account-Id", account));
    }
    let bytes = http
        .get(
            "https://chatgpt.com/backend-api/wham/usage",
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
    parse_codex(&json(&bytes)?)
}

/// Codex 的用量：`rate_limit` 下 `primary_window`、`secondary_window` 各一个窗口，`used_percent`、`limit_window_seconds`
/// （18000 是 5 小时、604800 是每周、2592000 是免费账号的 30 天）、`reset_at`（秒）；`plan_type` 是套餐。
fn parse_codex(body: &Value) -> Result<Quota, QuotaError> {
    let limit = &body["rate_limit"];
    if !limit.is_object() && !body["plan_type"].is_string() {
        return Err(unknown(body));
    }
    let windows = ["primary_window", "secondary_window"]
        .iter()
        .filter_map(|key| {
            let window = limit.get(*key).filter(|w| w.is_object())?;
            let span = match window["limit_window_seconds"].as_u64() {
                Some(18_000) => Span::FiveHours,
                Some(604_800) => Span::Week,
                Some(2_592_000) => Span::Month,
                Some(seconds) => Span::Minutes((seconds / 60) as u32),
                None => Span::Period,
            };
            Some(Window {
                span,
                used: number(&window["used_percent"])?,
                resets_at: time(&window["reset_at"]),
            })
        })
        .collect();
    Ok(Quota {
        plan: body["plan_type"].as_str().map(str::to_string),
        windows,
    })
}

/// Claude 官方：跑 Claude Code 自己的 `/usage`（`claude -p /usage`，Claude Code 在本地回答，不调模型、不花钱，同 magpie），
/// 读它打出来的几行。confdeck 不碰 Claude Code 的登录（macOS 上在钥匙串里），也不拿它的令牌去调 Anthropic 的接口。
///
/// 不读用户的设置（`--setting-sources ""`）：settings.json 里切到别家的环境变量不影响，查的总是官方登录。所以代理也读不到，
/// 由 `proxy` 给（作为 `HTTPS_PROXY`）。在临时目录里跑，不留会话；`ANTHROPIC_*`、`CLAUDE*` 环境变量不传下去。
/// 套餐从 [`crate::account::claude_profile`] 拿。
pub fn claude(program: &Path, home: &Path, proxy: Option<&str>) -> Result<Quota, QuotaError> {
    let text = run_claude_usage(program, proxy)?;
    let now = chrono::Local::now();
    let mut quota = parse_claude_usage(&text, &now)?;
    quota.plan = crate::account::claude_profile(home).and_then(|profile| profile.plan);
    Ok(quota)
}

/// `/usage` 多久没回答就停掉。平时一秒以内。
const CLAUDE_TIMEOUT: Duration = Duration::from_secs(30);

/// 跑 `/usage`，交出 JSON 输出里的 `result`。
fn run_claude_usage(program: &Path, proxy: Option<&str>) -> Result<String, QuotaError> {
    use std::io::Read;
    use std::process::{Command, Stdio};

    let mut command = Command::new(program);
    command
        .args([
            "-p",
            "/usage",
            "--output-format",
            "json",
            "--tools",
            "",
            "--strict-mcp-config",
            "--setting-sources",
            "",
            "--no-session-persistence",
        ])
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, _) in std::env::vars_os() {
        let key_text = key.to_string_lossy();
        if key_text.starts_with("ANTHROPIC_") || key_text.starts_with("CLAUDE") {
            command.env_remove(&key);
        }
    }
    command
        .env("DISABLE_TELEMETRY", "1")
        .env("DISABLE_ERROR_REPORTING", "1")
        .env("DISABLE_AUTOUPDATER", "1");
    // nvm 这类装法的 `claude` 是个脚本，要在 PATH 里找得到旁边的 node。
    if let Some(dir) = program.parent() {
        let mut paths = vec![dir.to_path_buf()];
        paths.extend(
            std::env::var_os("PATH")
                .iter()
                .flat_map(std::env::split_paths),
        );
        if let Ok(path) = std::env::join_paths(paths) {
            command.env("PATH", path);
        }
    }
    if let Some(proxy) = proxy {
        command.env("HTTPS_PROXY", proxy).env("HTTP_PROXY", proxy);
    }
    let mut child = command
        .spawn()
        .map_err(|err| QuotaError::Claude(err.to_string()))?;
    // 读输出放在别的线程，免得输出多时管道塞满、子进程卡住。
    let read = |stream: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(mut stream) = stream {
                let _ = stream.read_to_string(&mut text);
            }
            text
        })
    };
    let stdout = read(
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );
    let stderr = read(
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < CLAUDE_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(100))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(QuotaError::Net(NetError::Timeout));
            }
        }
    }
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    let Ok(answer) = serde_json::from_str::<Value>(stdout.trim()) else {
        let said = if stderr.trim().is_empty() {
            &stdout
        } else {
            &stderr
        };
        return Err(QuotaError::Claude(truncate(said.trim())));
    };
    let result = answer["result"].as_str().unwrap_or_default().to_string();
    if answer["is_error"] == true {
        return Err(claude_refusal(&result).unwrap_or(QuotaError::Claude(truncate(&result))));
    }
    Ok(result)
}

/// `/usage` 说的是没登录、登录过期。
fn claude_refusal(text: &str) -> Option<QuotaError> {
    let text = text.to_lowercase();
    if [
        "not logged in",
        "/login",
        "logged out",
        "signed out",
        "not signed in",
    ]
    .iter()
    .any(|said| text.contains(said))
    {
        return Some(QuotaError::NotLoggedIn);
    }
    if [
        "401",
        "expired",
        "unauthorized",
        "invalid token",
        "authentication",
    ]
    .iter()
    .any(|said| text.contains(said))
    {
        return Some(QuotaError::Expired);
    }
    None
}

/// `/usage` 打出来的几行（Claude Code 2.1.296）：
///
/// ```text
/// Current session: 12% used · resets Oct 13 at 2:49pm (Asia/Shanghai)
/// Current week (all models): 2% used · resets Oct 20 at 4:59am (Asia/Shanghai)
/// Current week (Fable): 0% used · resets Oct 20 at 5am (Asia/Shanghai)
/// ```
///
/// 同一个窗口写了两次用第一次。一个窗口都没有的：说了没登录、过期按那个报，说加载失败的原样报，别的是没有套餐
/// （用 API Key 的）。
fn parse_claude_usage(
    text: &str,
    now: &chrono::DateTime<chrono::Local>,
) -> Result<Quota, QuotaError> {
    let mut windows: Vec<Window> = Vec::new();
    for line in text.lines() {
        let Some(window) = claude_usage_line(line.trim(), now) else {
            continue;
        };
        if windows.iter().all(|w| w.span != window.span) {
            windows.push(window);
        }
    }
    if windows.is_empty() {
        if let Some(refusal) = claude_refusal(text) {
            return Err(refusal);
        }
        let failed = text
            .lines()
            .find(|line| line.to_lowercase().contains("failed"));
        return Err(match failed {
            Some(line) => QuotaError::Claude(truncate(line.trim())),
            None => QuotaError::NoPlan,
        });
    }
    Ok(Quota {
        plan: None,
        windows,
    })
}

/// 「Current session: 12% used · resets Oct 13 at 2:49pm (Asia/Shanghai)」这样的一行。
fn claude_usage_line(line: &str, now: &chrono::DateTime<chrono::Local>) -> Option<Window> {
    let (name, rest) = line.strip_prefix("Current ")?.split_once(':')?;
    let (used, rest) = rest.trim().split_once("% used")?;
    let span = match name.trim() {
        "session" => Span::FiveHours,
        week => {
            let scope = week.strip_prefix("week")?.trim();
            let scope = scope
                .strip_prefix('(')
                .and_then(|s| s.strip_suffix(')'))
                .unwrap_or(scope)
                .trim();
            let scope = scope.strip_suffix(" only").unwrap_or(scope).trim();
            if scope.is_empty() || scope.eq_ignore_ascii_case("all models") {
                Span::Week
            } else {
                Span::WeekOf(scope.to_string())
            }
        }
    };
    let resets_at = rest
        .trim()
        .trim_start_matches('·')
        .trim()
        .strip_prefix("resets ")
        .and_then(|when| claude_reset(when.trim(), now));
    Some(Window {
        span,
        used: used.trim().parse().ok()?,
        resets_at,
    })
}

/// 重置时间：「Oct 13 at 2:49pm (Asia/Shanghai)」「Oct 9, 2:59pm (UTC)」「3pm (Asia/Shanghai)」。括号里是时区：
/// `UTC` 按 UTC，别的按本机时区（Claude Code 和 confdeck 在同一台机器上）。没写年份的取最近的、还没过的那一年；
/// 只有钟点的取最近的、还没过的那一天。
fn claude_reset(text: &str, now: &chrono::DateTime<chrono::Local>) -> Option<i64> {
    use chrono::{Datelike, NaiveDate, NaiveTime, TimeZone};

    let (when, zone) = match text.rsplit_once(" (") {
        Some((when, zone)) => (when.trim(), zone.trim_end_matches(')')),
        None => (text, ""),
    };
    let (date, time) = match when.split_once(" at ").or_else(|| when.split_once(", ")) {
        Some((date, time)) => (Some(date.trim()), time.trim()),
        None => (None, when),
    };
    let time = time.to_lowercase();
    let (clock, pm) = match (time.strip_suffix("am"), time.strip_suffix("pm")) {
        (Some(clock), _) => (clock.trim(), Some(false)),
        (_, Some(clock)) => (clock.trim(), Some(true)),
        _ => (time.trim(), None),
    };
    let (hour, minute) = match clock.split_once(':') {
        Some((h, m)) => (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?),
        None => (clock.parse::<u32>().ok()?, 0),
    };
    let hour = match pm {
        Some(true) if hour < 12 => hour + 12,
        Some(false) if hour == 12 => 0,
        _ => hour,
    };
    let time = NaiveTime::from_hms_opt(hour, minute, 0)?;
    let utc = zone.eq_ignore_ascii_case("utc");
    let at = |date: NaiveDate| -> Option<i64> {
        let naive = date.and_time(time);
        Some(if utc {
            naive.and_utc().timestamp_millis()
        } else {
            chrono::Local
                .from_local_datetime(&naive)
                .earliest()?
                .timestamp_millis()
        })
    };
    let floor = now.timestamp_millis() - 60 * 60 * 1000;
    let today = now.date_naive();
    match date {
        Some(date) => {
            let (month, day) = date.split_once(' ')?;
            let month = [
                "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
            ]
            .iter()
            .position(|m| month.to_lowercase().starts_with(m))? as u32
                + 1;
            let day: u32 = day.trim().parse().ok()?;
            (today.year()..=today.year() + 1)
                .filter_map(|year| at(NaiveDate::from_ymd_opt(year, month, day)?))
                .find(|at| *at >= floor)
        }
        None => [today, today.succ_opt()?]
            .into_iter()
            .filter_map(at)
            .find(|at| *at >= floor),
    }
}

/// Grok Build 官方：用 `<home>/.grok/auth.json` 里的登录查。这个文件按登录的范围分条，用 `https://auth.x.ai::` 开头那条，
/// 没有再用老的 `…/sign-in` 那条。账单只回一个额度窗口：`config.creditUsagePercent` 和 `config.currentPeriod`（类型、结束时间）。
pub fn grok(http: &Http, home: &Path) -> Result<Quota, QuotaError> {
    let bytes = std::fs::read(home.join(".grok/auth.json")).map_err(|_| QuotaError::NotLoggedIn)?;
    grok_login(http, &bytes)
}

/// 用一份 Grok Build 登录（`auth.json` 的内容）查。
pub fn grok_login(http: &Http, login: &[u8]) -> Result<Quota, QuotaError> {
    let auth: Value = serde_json::from_slice(login).map_err(|_| QuotaError::NotLoggedIn)?;
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
        Plan, QuotaError, Span, Window, claude_reset, claude_usage_line, parse_claude_usage,
        parse_codex, parse_grok, parse_kimi, parse_minimax, parse_opencode_go, parse_zhipu,
        plan_of,
    };
    use crate::provider::{Protocol, Provider};

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
    fn codex_names_windows_by_length() {
        let body = json!({"plan_type": "pro",
            "rate_limit": {"allowed": true, "limit_reached": false,
                "primary_window": {"used_percent": 30, "limit_window_seconds": 18000, "reset_at": 1_791_468_000},
                "secondary_window": {"used_percent": 12.5, "limit_window_seconds": 604800, "reset_after_seconds": 3600}},
            "credits": {"has_credits": false, "unlimited": false, "balance": "0"}});
        let quota = parse_codex(&body).unwrap();
        assert_eq!(quota.plan.as_deref(), Some("pro"));
        assert_eq!(
            quota.windows,
            [
                Window {
                    span: Span::FiveHours,
                    used: 30.,
                    resets_at: Some(1_791_468_000_000)
                },
                Window {
                    span: Span::Week,
                    used: 12.5,
                    resets_at: None
                }
            ]
        );
        // 免费账号只有一个 30 天的窗口；没有 `rate_limit` 的是别的回答。
        let free = json!({"plan_type": "free", "rate_limit": {"primary_window":
            {"used_percent": 4, "limit_window_seconds": 2592000}, "secondary_window": null}});
        assert_eq!(parse_codex(&free).unwrap().windows[0].span, Span::Month);
        assert!(parse_codex(&json!({"detail": "Unauthorized"})).is_err());
    }

    #[test]
    fn claude_usage_lines() {
        use chrono::TimeZone;
        let now = chrono::Local
            .with_ymd_and_hms(2026, 10, 13, 12, 0, 0)
            .unwrap();
        let at = |y, mo, d, h, mi| {
            chrono::Local
                .with_ymd_and_hms(y, mo, d, h, mi, 0)
                .unwrap()
                .timestamp_millis()
        };
        let text = "You are currently using your subscription to power your Claude Code usage\n\n\
            Current session: 12% used · resets Oct 13 at 2:49pm (Asia/Shanghai)\n\
            Current week (all models): 2% used · resets Oct 20 at 4:59am (Asia/Shanghai)\n\
            Current week (Fable): 0% used · resets Oct 20 at 5am (Asia/Shanghai)\n\
            Current week (Fable): 9% used\n\n\
            What's contributing to your limits usage?\n  83% of your usage was at >150k context";
        let quota = parse_claude_usage(text, &now).unwrap();
        assert_eq!(
            quota.windows,
            [
                Window {
                    span: Span::FiveHours,
                    used: 12.,
                    resets_at: Some(at(2026, 10, 13, 14, 49))
                },
                Window {
                    span: Span::Week,
                    used: 2.,
                    resets_at: Some(at(2026, 10, 20, 4, 59))
                },
                Window {
                    span: Span::WeekOf("Fable".into()),
                    used: 0.,
                    resets_at: Some(at(2026, 10, 20, 5, 0))
                }
            ]
        );
        // 新版的写法、只有钟点的、跨年的、老账号的「Sonnet only」。
        let utc = chrono::Utc
            .with_ymd_and_hms(2026, 10, 14, 14, 59, 0)
            .unwrap()
            .timestamp_millis();
        assert_eq!(claude_reset("Oct 14, 2:59pm (UTC)", &now), Some(utc));
        assert_eq!(
            claude_reset("3pm (Asia/Shanghai)", &now),
            Some(at(2026, 10, 13, 15, 0))
        );
        assert_eq!(
            claude_reset("9am (Asia/Shanghai)", &now),
            Some(at(2026, 10, 14, 9, 0))
        );
        assert_eq!(claude_reset("12am", &now), Some(at(2026, 10, 14, 0, 0)));
        let december = chrono::Local
            .with_ymd_and_hms(2026, 12, 30, 12, 0, 0)
            .unwrap();
        assert_eq!(
            claude_reset("Jan 2 at 1am", &december),
            Some(at(2027, 1, 2, 1, 0))
        );
        let old = claude_usage_line("Current week (Sonnet only): 4% used", &now).unwrap();
        assert_eq!(old.span, Span::WeekOf("Sonnet".into()));
        assert_eq!(old.resets_at, None);
    }

    /// 假的 `claude`：照 Claude Code 2.1.296 的 JSON 输出，`result` 里是额度；顺带看 `ANTHROPIC_*` 有没有传下去。
    #[cfg(unix)]
    #[test]
    fn claude_runs_usage_and_reads_its_answer() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("claude");
        std::fs::write(
            &program,
            "#!/bin/sh\n\
             [ \"$1 $2\" = \"-p /usage\" ] || exit 2\n\
             printf '{\"type\":\"result\",\"is_error\":false,\"local_command\":\"usage\",\"result\":\"Current session: %s%% used\\\\nCurrent week (all models): 2%% used\"}' \"${ANTHROPIC_BASE_URL:-12}\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(
            dir.path().join(".claude.json"),
            r#"{"oauthAccount": {"organizationType": "claude_max"}}"#,
        )
        .unwrap();
        // SAFETY: 测试里只有这里改这个变量。
        unsafe { std::env::set_var("ANTHROPIC_BASE_URL", "99") };
        let quota = super::claude(&program, dir.path(), None).unwrap();
        unsafe { std::env::remove_var("ANTHROPIC_BASE_URL") };
        assert_eq!(quota.plan.as_deref(), Some("max"));
        assert_eq!(quota.windows[0].used, 12.);
        assert_eq!(quota.windows[1].span, Span::Week);

        std::fs::write(&program, "#!/bin/sh\necho 'command not found: node' >&2\nexit 127\n").unwrap();
        assert_eq!(
            super::claude(&program, dir.path(), None),
            Err(QuotaError::Claude("command not found: node".into()))
        );
    }

    #[test]
    fn claude_usage_without_windows() {
        let now = chrono::Local::now();
        assert_eq!(
            parse_claude_usage("Not logged in · Please run /login", &now),
            Err(QuotaError::NotLoggedIn)
        );
        assert_eq!(
            parse_claude_usage("OAuth token has expired", &now),
            Err(QuotaError::Expired)
        );
        assert!(matches!(
            parse_claude_usage("Failed to load usage data", &now),
            Err(QuotaError::Claude(_))
        ));
        assert_eq!(
            parse_claude_usage("/usage is only available for subscription plans.", &now),
            Err(QuotaError::NoPlan)
        );
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
