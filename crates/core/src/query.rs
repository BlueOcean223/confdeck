//! 自己配的余额查询（10-13 用户：照 cc-switch 做模板）：认不出来的站点（new-api 一类的中转、自建的网关）在「额度查询」卡片里
//! 选一个模板、填地址和凭证，查回来的是余额（[`Balance`]），和认得的几家（[`crate::balance`]）一样画。
//! 不跑脚本：cc-switch 的提取器是 JS，这里只填余额、已用在响应的哪个位置（`data.quota` 这样的路径）、除以多少。
//!
//! | 模板 | 请求 | 读什么 |
//! | --- | --- | --- |
//! | 通用 | `GET <地址>`（空着是 `<根地址>/user/balance`），`Authorization: Bearer <Key>` | `balance` |
//! | NewAPI，填了访问令牌和用户 ID | `GET <根地址>/api/user/self`，`Authorization: Bearer <令牌>`、`New-Api-User: <用户 ID>` | `data.quota`、`data.used_quota`，÷ 500000 |
//! | NewAPI，没填 | `GET <根地址>/v1/dashboard/billing/subscription` 和 `…/usage`，用供应商的 Key（one-api 也有） | `hard_limit_usd`、`total_usage` ÷ 100 |
//! | 自定义 | 自己写地址、GET 或 POST、请求头、请求体 | 自己写路径、除以多少 |
//!
//! - **根地址**：供应商地址的 `协议://主机[:端口]`（new-api 装在根上）。地址、请求头、请求体里的 `{{baseUrl}}` 换成它，
//!   `{{apiKey}}` 换成供应商的 Key（和 cc-switch 的写法一样）。
//! - **NewAPI 没填令牌时查的是这个 Key**：站点开着「令牌统计」（new-api 默认开）时是这个 Key 还剩多少，关着时是账户余额；
//!   不限额的 Key 回 1 亿，算「不限额」，只写已用。账户令牌在 new-api 的「个人设置 → 系统访问令牌」。
//! - **单位**：自己选（美元、人民币、不是钱）。new-api 的接口按站点的「额度展示类型」换算，换成人民币的站点要选人民币。

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::balance::{Balance, BalanceError, Currency, number, truncate};
use crate::net::{Http, NetError};
use crate::provider::{self, Provider};

const TIMEOUT: Duration = Duration::from_secs(15);

/// new-api 的额度单位：500000 是 1 美元（`QuotaPerUnit` 的默认值）。
pub const NEW_API_UNIT: f64 = 500_000.;

/// new-api 不限额的 Key 在 `hard_limit_usd` 里回这个数。
const UNLIMITED: f64 = 100_000_000.;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Template {
    #[default]
    General,
    NewApi,
    Custom,
}

/// 一家怎么查（存在 `state.json` 的 `checks` 里，见 [`crate::quota::Check`]）。哪个模板用哪几项见模块注释。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Query {
    #[serde(default)]
    pub template: Template,
    /// 通用、自定义：请求地址；NewAPI：站点的根地址。通用、NewAPI 空着用供应商的根地址。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// 通用：Key（空着用供应商的）；NewAPI：系统访问令牌。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub token: String,
    /// NewAPI：用户 ID。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub user: String,
    /// 自定义：用 POST（不然是 GET）。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub post: bool,
    /// 自定义：请求头，一行一个「名字: 值」。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub headers: String,
    /// 自定义：请求体。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
    /// 自定义：余额在响应的哪里（`data.balance`、`items.0.left`）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub left: String,
    /// 自定义：已用在哪里。可以只写这一项（不限额的）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub used: String,
    /// 读出来的数除以多少。没写是 1（NewAPI 的账户余额是 500000）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub divisor: Option<f64>,
    #[serde(default)]
    pub unit: Currency,
}

impl Query {
    /// 填够了能查：自定义要地址和余额、已用其中一项。通用、NewAPI 什么都能空着。
    pub fn complete(&self) -> bool {
        match self.template {
            Template::General | Template::NewApi => true,
            Template::Custom => {
                !self.url.trim().is_empty()
                    && !(self.left.trim().is_empty() && self.used.trim().is_empty())
            }
        }
    }
}

/// 供应商地址的根：`https://relay.example.com/v1` → `https://relay.example.com`。
pub fn root(provider: &Provider) -> Option<String> {
    let url = provider
        .endpoints
        .values()
        .find(|url| !url.trim().is_empty())?;
    root_of(url.trim())
}

fn root_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    (!authority.is_empty()).then(|| format!("{scheme}://{authority}"))
}

/// 按 `query` 查这家的余额。
pub fn run(http: &Http, provider: &Provider, query: &Query) -> Result<Balance, BalanceError> {
    let key = provider.key.trim();
    let base = match query.url.trim() {
        url if !url.is_empty() && query.template == Template::NewApi => {
            url.trim_end_matches('/').to_string()
        }
        _ => root(provider).unwrap_or_default(),
    };
    let fill = |text: &str| {
        text.replace("{{apiKey}}", key)
            .replace("{{baseUrl}}", base.trim_end_matches('/'))
    };
    let divisor = query.divisor.filter(|d| *d != 0.);
    match query.template {
        Template::General => {
            let token = match query.token.trim() {
                "" => key,
                token => token,
            };
            if token.is_empty() {
                return Err(BalanceError::NoKey);
            }
            let url = match query.url.trim() {
                "" => format!("{base}/user/balance"),
                url => fill(url),
            };
            let auth = format!("Bearer {token}");
            let body = fetch(
                http,
                false,
                &url,
                &[("Authorization", &auth)],
                None,
                &[token],
            )?;
            let left = number(&body["balance"]).ok_or_else(|| missing(&body, "balance"))?;
            Ok(Balance {
                left: Some(left / divisor.unwrap_or(1.)),
                used: None,
                currency: query.unit,
            })
        }
        Template::NewApi if !query.token.trim().is_empty() && !query.user.trim().is_empty() => {
            let (token, user) = (query.token.trim(), query.user.trim());
            let auth = format!("Bearer {token}");
            let body = fetch(
                http,
                false,
                &format!("{base}/api/user/self"),
                &[("Authorization", &auth), ("New-Api-User", user)],
                None,
                &[token],
            )?;
            if body["success"].as_bool() == Some(false) {
                return Err(BalanceError::Parse(text_of(&body["message"], &body)));
            }
            let data = &body["data"];
            let unit = divisor.unwrap_or(NEW_API_UNIT);
            let left = number(&data["quota"]).ok_or_else(|| missing(&body, "data.quota"))?;
            Ok(Balance {
                left: Some(left / unit),
                used: number(&data["used_quota"]).map(|used| used / unit),
                currency: query.unit,
            })
        }
        Template::NewApi => {
            if key.is_empty() {
                return Err(BalanceError::NoKey);
            }
            let auth = format!("Bearer {key}");
            let headers = [("Authorization", auth.as_str())];
            let limit = fetch(
                http,
                false,
                &format!("{base}/v1/dashboard/billing/subscription"),
                &headers,
                None,
                &[key],
            )?;
            let limit = number(&limit["hard_limit_usd"])
                .ok_or_else(|| missing(&limit, "hard_limit_usd"))?;
            let usage = fetch(
                http,
                false,
                &format!("{base}/v1/dashboard/billing/usage"),
                &headers,
                None,
                &[key],
            )?;
            let used =
                number(&usage["total_usage"]).ok_or_else(|| missing(&usage, "total_usage"))? / 100.;
            Ok(Balance {
                left: (limit < UNLIMITED).then_some(limit - used),
                used: Some(used),
                currency: query.unit,
            })
        }
        Template::Custom => {
            let url = fill(query.url.trim());
            let headers: Vec<(String, String)> = query
                .headers
                .lines()
                .filter_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    let name = name.trim();
                    (!name.is_empty()).then(|| (name.to_string(), fill(value.trim())))
                })
                .collect();
            let headers: Vec<(&str, &str)> = headers
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str()))
                .collect();
            let body = (query.post && !query.body.trim().is_empty()).then(|| fill(&query.body));
            let secrets = [key, query.token.trim()];
            let response = fetch(http, query.post, &url, &headers, body.as_deref(), &secrets)?;
            let read = |path: &str| -> Result<Option<f64>, BalanceError> {
                let path = path.trim();
                if path.is_empty() {
                    return Ok(None);
                }
                pick(&response, path)
                    .and_then(number)
                    .map(|n| Some(n / divisor.unwrap_or(1.)))
                    .ok_or_else(|| missing(&response, path))
            };
            let (left, used) = (read(&query.left)?, read(&query.used)?);
            if left.is_none() && used.is_none() {
                return Err(BalanceError::Parse(truncate(&response.to_string())));
            }
            Ok(Balance {
                left,
                used,
                currency: query.unit,
            })
        }
    }
}

/// 发请求、解析 JSON。401、403 算「Key 不对」；回了 `{"error": {...}}` 的（new-api 出错也回 200）带上原话。
fn fetch(
    http: &Http,
    post: bool,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
    secrets: &[&str],
) -> Result<Value, BalanceError> {
    if provider::host(url).is_none_or(str::is_empty) {
        return Err(BalanceError::Net(NetError::BadUrl {
            url: url.to_string(),
        }));
    }
    let mut all: Vec<(&str, &str)> = vec![("Accept", "application/json")];
    if body.is_some()
        && !headers
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case("content-type"))
    {
        all.push(("Content-Type", "application/json"));
    }
    all.extend_from_slice(headers);
    let bytes = http
        .send(post, url, &all, body, TIMEOUT, secrets)
        .map_err(|err| match err {
            NetError::Status {
                status: 401 | 403, ..
            } => BalanceError::Auth,
            err => BalanceError::Net(err),
        })?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| BalanceError::Parse(truncate(&String::from_utf8_lossy(&bytes))))?;
    if let Some(error) = value
        .get("error")
        .filter(|e| e.is_object() || e.is_string())
    {
        let message = error.get("message").unwrap_or(error);
        return Err(BalanceError::Parse(text_of(message, &value)));
    }
    Ok(value)
}

/// 按路径取值：`data.balance`、`items.0.left`（数组用下标）。前面的 `$.`、`response.` 不算（cc-switch 的提取器里这么写）。
pub fn pick<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let path = path.trim();
    let path = path
        .strip_prefix("$.")
        .or_else(|| path.strip_prefix("response."))
        .unwrap_or(path);
    path.split('.')
        .filter(|part| !part.is_empty())
        .try_fold(value, |value, part| match value {
            Value::Array(items) => items.get(part.parse::<usize>().ok()?),
            _ => value.get(part),
        })
}

fn missing(body: &Value, path: &str) -> BalanceError {
    BalanceError::Parse(format!("{path}: {}", truncate(&body.to_string())))
}

fn text_of(message: &Value, body: &Value) -> String {
    match message.as_str() {
        Some(text) if !text.trim().is_empty() => truncate(text.trim()),
        _ => truncate(&body.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;
    use crate::net::tests::{direct, serve};
    use crate::provider::Protocol;

    fn relay(url: &str, key: &str) -> Provider {
        let mut endpoints = BTreeMap::new();
        endpoints.insert(Protocol::Anthropic, format!("{url}/claude"));
        Provider {
            id: "relay".into(),
            name: "Relay".into(),
            key: key.into(),
            endpoints,
            ..Default::default()
        }
    }

    #[test]
    fn root_drops_the_path() {
        assert_eq!(
            root_of("https://relay.example.com:8443/v1/"),
            Some("https://relay.example.com:8443".into())
        );
        assert_eq!(root_of("relay.example.com"), None);
    }

    #[test]
    fn paths_walk_objects_and_arrays() {
        let body = json!({"data": {"items": [{"left": "12.5"}]}, "balance": 3});
        assert_eq!(pick(&body, "data.items.0.left"), Some(&json!("12.5")));
        assert_eq!(pick(&body, "$.balance"), Some(&json!(3)));
        assert_eq!(pick(&body, "response.balance"), Some(&json!(3)));
        assert_eq!(pick(&body, "data.items.1.left"), None);
    }

    #[test]
    fn new_api_with_the_key_reads_billing() {
        let server = serve(|request| {
            if request.starts_with("GET /v1/dashboard/billing/subscription") {
                (200, json!({"hard_limit_usd": 30.0}).to_string())
            } else {
                (200, json!({"total_usage": 1234.0}).to_string())
            }
        });
        let query = Query {
            template: Template::NewApi,
            ..Default::default()
        };
        let found = run(&direct(), &relay(&server.url, "sk-test"), &query).unwrap();
        assert_eq!(found.left, Some(30.0 - 12.34));
        assert_eq!(found.used, Some(12.34));
        assert_eq!(found.currency, Currency::Usd);
        let request = server.requests.recv().unwrap().to_ascii_lowercase();
        assert!(
            request.contains("authorization: bearer sk-test"),
            "{request}"
        );
    }

    #[test]
    fn new_api_unlimited_key_has_no_balance() {
        let server = serve(|request| {
            if request.contains("subscription") {
                (200, json!({"hard_limit_usd": 100000000.0}).to_string())
            } else {
                (200, json!({"total_usage": 500.0}).to_string())
            }
        });
        let query = Query {
            template: Template::NewApi,
            ..Default::default()
        };
        let found = run(&direct(), &relay(&server.url, "sk-test"), &query).unwrap();
        assert_eq!((found.left, found.used), (None, Some(5.0)));
    }

    #[test]
    fn new_api_with_a_token_reads_the_account() {
        let server = serve(|_| {
            (
                200,
                json!({"success": true, "data": {"quota": 2_500_000, "used_quota": 500_000, "group": "default"}})
                    .to_string(),
            )
        });
        let query = Query {
            template: Template::NewApi,
            token: "access".into(),
            user: "42".into(),
            unit: Currency::Cny,
            ..Default::default()
        };
        let found = run(&direct(), &relay(&server.url, ""), &query).unwrap();
        assert_eq!((found.left, found.used), (Some(5.0), Some(1.0)));
        assert_eq!(found.currency, Currency::Cny);
        let request = server.requests.recv().unwrap().to_ascii_lowercase();
        assert!(request.starts_with("get /api/user/self"), "{request}");
        assert!(request.contains("new-api-user: 42"), "{request}");
    }

    #[test]
    fn errors_say_what_went_wrong() {
        let server = serve(|request| {
            if request.contains("/api/user/self") {
                (
                    200,
                    json!({"success": false, "message": "无权进行此操作"}).to_string(),
                )
            } else if request.contains("subscription") {
                (401, json!({"error": {"message": "无效的令牌"}}).to_string())
            } else {
                (
                    200,
                    json!({"error": {"message": "upstream", "type": "x"}}).to_string(),
                )
            }
        });
        let provider = relay(&server.url, "sk-test");
        let new_api = Query {
            template: Template::NewApi,
            ..Default::default()
        };
        assert_eq!(run(&direct(), &provider, &new_api), Err(BalanceError::Auth));
        let account = Query {
            token: "t".into(),
            user: "1".into(),
            ..new_api
        };
        assert_eq!(
            run(&direct(), &provider, &account),
            Err(BalanceError::Parse("无权进行此操作".into()))
        );
        let general = Query::default();
        assert_eq!(
            run(&direct(), &provider, &general),
            Err(BalanceError::Parse("upstream".into()))
        );
        assert_eq!(
            run(&direct(), &relay(&server.url, ""), &general),
            Err(BalanceError::NoKey)
        );
    }

    #[test]
    fn custom_fills_placeholders_and_reads_paths() {
        let server = serve(|request| {
            assert!(request.starts_with("POST /api/balance"), "{request}");
            assert!(request.contains("x-key: sk-test"), "{request}");
            assert!(request.ends_with("{\"key\":\"sk-test\"}"), "{request}");
            (
                200,
                json!({"data": {"left": 860, "spent": "120"}}).to_string(),
            )
        });
        let query = Query {
            template: Template::Custom,
            url: "{{baseUrl}}/api/balance".into(),
            post: true,
            headers: "X-Key: {{apiKey}}\n\n bad line\n".into(),
            body: "{\"key\":\"{{apiKey}}\"}".into(),
            left: "data.left".into(),
            used: "data.spent".into(),
            divisor: Some(10.),
            unit: Currency::Plain,
            ..Default::default()
        };
        assert!(query.complete());
        let found = run(&direct(), &relay(&server.url, "sk-test"), &query).unwrap();
        assert_eq!((found.left, found.used), (Some(86.), Some(12.)));
        let wrong = Query {
            left: "data.balance".into(),
            ..query
        };
        assert!(matches!(
            run(&direct(), &relay(&server.url, "sk-test"), &wrong),
            Err(BalanceError::Parse(text)) if text.starts_with("data.balance")
        ));
    }

    #[test]
    fn saves_only_what_was_filled() {
        let query = Query {
            template: Template::NewApi,
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&query).unwrap(),
            json!({"template": "newapi", "unit": "usd"})
        );
        assert!(
            !Query {
                template: Template::Custom,
                url: "https://x".into(),
                ..Default::default()
            }
            .complete()
        );
    }
}
