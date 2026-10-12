//! 供应商账户里还有多少钱（用量页顶上那排卡片）。照 fork 的 `balance.rs`（ccs-lite），加了 Kimi 开放平台。
//!
//! 按接口地址认是哪家，用这家供应商的 Key 查。套餐（Coding Plan、Token Plan）的额度不在这里，那是另一种接口。
//!
//! | 哪家 | 接口 | 币种 |
//! | --- | --- | --- |
//! | DeepSeek | `GET api.deepseek.com/user/balance` | 接口里写着 |
//! | Kimi 开放平台 | `GET api.moonshot.cn/v1/users/me/balance`（海外 `api.moonshot.ai`） | 国内元，海外美元 |
//! | MiniMax（按量，Key 以 `sk-api-` 开头） | `GET api.minimax.cn/account/query_balance`（海外 `api.minimax.io`），不在公开文档里，取自官方 CLI | 国内元，海外美元 |
//! | 硅基流动海外站 | `GET api.siliconflow.com/v1/user/info`；国内站 2026-08-14 停了这个接口，还没有替代 | 美元 |
//! | OpenRouter | `GET openrouter.ai/api/v1/key`：账户余额要管理 Key，只能查这个 Key 的上限还剩多少，没设上限只有用了多少 | 美元 |
//! | 阶跃星辰 | `GET api.stepfun.com/v1/accounts`（海外 `api.stepfun.ai`） | 国内元，海外美元 |
//! | Novita | `GET api.novita.ai/openapi/v1/billing/balance/detail`，单位 0.0001 美元 | 美元 |
//!
//! Kimi、MiniMax 鉴权失败时 HTTP 也是 200，要看业务码。

use std::time::Duration;

use serde_json::Value;

use crate::net::{Http, NetError};
use crate::provider::{self, Provider};

const TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Currency {
    Cny,
    Usd,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Balance {
    /// 还能用多少。OpenRouter 没设上限的 Key 是 `None`，只有 `used`。
    pub left: Option<f64>,
    /// 已经用了多少（OpenRouter 的 Key）。
    pub used: Option<f64>,
    pub currency: Currency,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum BalanceError {
    #[error("没有 Key")]
    NoKey,
    #[error("Key 不对")]
    Auth,
    #[error("{0}")]
    Net(NetError),
    /// 回的不是认得的样子，或者业务码不是成功。
    #[error("返回的内容认不出：{0}")]
    Parse(String),
}

/// 能查余额的几家。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Site {
    DeepSeek,
    Kimi { intl: bool },
    MiniMax { intl: bool },
    SiliconFlow,
    OpenRouter,
    StepFun { intl: bool },
    Novita,
}

fn site_of(url: &str) -> Option<Site> {
    let host = provider::host(url)?.to_ascii_lowercase();
    Some(match host.as_str() {
        "api.deepseek.com" => Site::DeepSeek,
        "api.moonshot.cn" => Site::Kimi { intl: false },
        "api.moonshot.ai" => Site::Kimi { intl: true },
        "api.minimax.cn" | "api.minimaxi.com" => Site::MiniMax { intl: false },
        "api.minimax.io" => Site::MiniMax { intl: true },
        "api.siliconflow.com" => Site::SiliconFlow,
        "openrouter.ai" => Site::OpenRouter,
        "api.stepfun.com" => Site::StepFun { intl: false },
        "api.stepfun.ai" => Site::StepFun { intl: true },
        "api.novita.ai" => Site::Novita,
        _ => return None,
    })
}

fn site(provider: &Provider) -> Option<Site> {
    let site = provider.endpoints.values().find_map(|url| site_of(url))?;
    // MiniMax 按量计费的 Key 以 `sk-api-` 开头；别的是 Token Plan，查的是套餐额度（官方 CLI 的 `selectUsageEndpoint`）。
    if matches!(site, Site::MiniMax { .. }) && !provider.key.trim().starts_with("sk-api-") {
        return None;
    }
    Some(site)
}

/// 这家能查余额（地址认得出，填了 Key）。
pub fn supported(provider: &Provider) -> bool {
    site(provider).is_some() && !provider.key.trim().is_empty()
}

/// 查这家的余额。先用 [`supported`] 筛：查不了的这里报「认不出」。
pub fn query(http: &Http, provider: &Provider) -> Result<Balance, BalanceError> {
    let key = provider.key.trim();
    if key.is_empty() {
        return Err(BalanceError::NoKey);
    }
    let Some(site) = site(provider) else {
        return Err(BalanceError::Parse("unsupported".into()));
    };
    let url = match site {
        Site::DeepSeek => "https://api.deepseek.com/user/balance",
        Site::Kimi { intl: false } => "https://api.moonshot.cn/v1/users/me/balance",
        Site::Kimi { intl: true } => "https://api.moonshot.ai/v1/users/me/balance",
        Site::MiniMax { intl: false } => "https://api.minimax.cn/account/query_balance",
        Site::MiniMax { intl: true } => "https://api.minimax.io/account/query_balance",
        Site::SiliconFlow => "https://api.siliconflow.com/v1/user/info",
        Site::OpenRouter => "https://openrouter.ai/api/v1/key",
        Site::StepFun { intl: false } => "https://api.stepfun.com/v1/accounts",
        Site::StepFun { intl: true } => "https://api.stepfun.ai/v1/accounts",
        Site::Novita => "https://api.novita.ai/openapi/v1/billing/balance/detail",
    };
    let auth = format!("Bearer {key}");
    let headers = [
        ("Authorization", auth.as_str()),
        ("Accept", "application/json"),
    ];
    let bytes = http
        .get(url, &headers, TIMEOUT, &[key])
        .map_err(|err| match err {
            NetError::Status {
                status: 401 | 403, ..
            } => BalanceError::Auth,
            err => BalanceError::Net(err),
        })?;
    let body: Value =
        serde_json::from_slice(&bytes).map_err(|err| BalanceError::Parse(err.to_string()))?;
    parse(site, &body)
}

fn parse(site: Site, body: &Value) -> Result<Balance, BalanceError> {
    let unknown = || BalanceError::Parse(truncate(&body.to_string()));
    let local = |intl: bool| if intl { Currency::Usd } else { Currency::Cny };
    let left = |amount: Option<f64>, currency| {
        amount.map(|left| Balance {
            left: Some(left),
            used: None,
            currency,
        })
    };
    match site {
        Site::DeepSeek => {
            // 可能有人民币、美元两条，取第一条（充的是哪种就是哪种）。
            let info = body["balance_infos"].get(0).ok_or_else(unknown)?;
            let currency = match info["currency"].as_str() {
                Some("USD") => Currency::Usd,
                _ => Currency::Cny,
            };
            left(number(&info["total_balance"]), currency).ok_or_else(unknown)
        }
        Site::Kimi { intl } => {
            if body["code"].as_i64().is_some_and(|code| code != 0) {
                return Err(code_error(body, &body["code"]));
            }
            left(number(&body["data"]["available_balance"]), local(intl)).ok_or_else(unknown)
        }
        Site::MiniMax { intl } => {
            let code = &body["base_resp"]["status_code"];
            if code.as_i64().is_some_and(|code| code != 0) {
                return Err(code_error(body, code));
            }
            left(number(&body["available_amount"]), local(intl)).ok_or_else(unknown)
        }
        Site::SiliconFlow => {
            left(number(&body["data"]["totalBalance"]), Currency::Usd).ok_or_else(unknown)
        }
        Site::OpenRouter => {
            let key = body.get("data").unwrap_or(body);
            match (number(&key["limit"]), number(&key["limit_remaining"])) {
                (Some(_), Some(remaining)) => Ok(Balance {
                    left: Some(remaining),
                    used: number(&key["usage"]),
                    currency: Currency::Usd,
                }),
                // 不限额的 Key：没有余额可说，不能当成 0。
                _ => Ok(Balance {
                    left: None,
                    used: Some(number(&key["usage"]).ok_or_else(unknown)?),
                    currency: Currency::Usd,
                }),
            }
        }
        Site::StepFun { intl } => left(number(&body["balance"]), local(intl)).ok_or_else(unknown),
        Site::Novita => left(
            number(&body["availableBalance"]).map(|ticks| ticks / 10_000.),
            Currency::Usd,
        )
        .ok_or_else(unknown),
    }
}

/// 业务码不是成功：常见的鉴权失败码算「Key 不对」，别的带原话。
fn code_error(body: &Value, code: &Value) -> BalanceError {
    // MiniMax：1004 未授权、2049 Key 无效；Kimi 的鉴权失败直接回 401，走不到这里。
    if matches!(code.as_i64(), Some(1004 | 2049)) {
        return BalanceError::Auth;
    }
    BalanceError::Parse(truncate(&body.to_string()))
}

/// 数字或写成字符串的数字。
fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
}

fn truncate(text: &str) -> String {
    text.chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Balance, BalanceError, Currency, Site, parse, site_of};

    fn left(amount: f64, currency: Currency) -> Result<Balance, BalanceError> {
        Ok(Balance {
            left: Some(amount),
            used: None,
            currency,
        })
    }

    #[test]
    fn sites_are_told_by_host() {
        assert_eq!(
            site_of("https://api.deepseek.com/anthropic"),
            Some(Site::DeepSeek)
        );
        assert_eq!(
            site_of("https://api.moonshot.ai/v1"),
            Some(Site::Kimi { intl: true })
        );
        assert_eq!(
            site_of("https://api.minimaxi.com/v1"),
            Some(Site::MiniMax { intl: false })
        );
        // 套餐的地址、国内的硅基流动查不了。
        assert_eq!(site_of("https://api.kimi.com/coding"), None);
        assert_eq!(site_of("https://api.siliconflow.cn/v1"), None);
        assert_eq!(site_of("https://api.minimax.cn.example.com/v1"), None);
    }

    #[test]
    fn each_site_reads_its_own_shape() {
        let deepseek = json!({"is_available": true, "balance_infos": [
            {"currency": "CNY", "total_balance": "86.40", "granted_balance": "0", "topped_up_balance": "86.40"}]});
        assert_eq!(parse(Site::DeepSeek, &deepseek), left(86.4, Currency::Cny));

        // Kimi 文档的例子。
        let kimi = json!({"code": 0, "data": {"available_balance": 49.58894, "voucher_balance": 46.58893,
            "cash_balance": 3.00001}, "scode": "0x0", "status": true});
        assert_eq!(
            parse(Site::Kimi { intl: false }, &kimi),
            left(49.58894, Currency::Cny)
        );

        let minimax = json!({"available_amount": "12.50", "cash_balance": "12.50",
            "base_resp": {"status_code": 0, "status_msg": "success"}});
        assert_eq!(
            parse(Site::MiniMax { intl: true }, &minimax),
            left(12.5, Currency::Usd)
        );
        let denied = json!({"base_resp": {"status_code": 1004, "status_msg": "not authorized"}});
        assert_eq!(
            parse(Site::MiniMax { intl: false }, &denied),
            Err(BalanceError::Auth)
        );

        let novita = json!({"availableBalance": "1000000", "cashBalance": "800000", "creditLimit": "200000"});
        assert_eq!(parse(Site::Novita, &novita), left(100., Currency::Usd));

        let silicon = json!({"code": 20000, "data": {"balance": "1", "totalBalance": "23.75"}});
        assert_eq!(
            parse(Site::SiliconFlow, &silicon),
            left(23.75, Currency::Usd)
        );

        assert!(matches!(
            parse(Site::StepFun { intl: false }, &json!({"object": "account"})),
            Err(BalanceError::Parse(_))
        ));
    }

    #[test]
    fn openrouter_keys_with_and_without_a_limit() {
        let limited = json!({"data": {"limit": 100, "limit_remaining": 74.5, "usage": 25.5}});
        assert_eq!(
            parse(Site::OpenRouter, &limited),
            Ok(Balance {
                left: Some(74.5),
                used: Some(25.5),
                currency: Currency::Usd
            })
        );
        let open = json!({"data": {"limit": null, "limit_remaining": null, "usage": 12.34}});
        assert_eq!(
            parse(Site::OpenRouter, &open),
            Ok(Balance {
                left: None,
                used: Some(12.34),
                currency: Currency::Usd
            })
        );
    }
}
