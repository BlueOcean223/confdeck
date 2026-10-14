//! 对一家供应商发请求：测各个接口地址连不连得上（[`check`]），从它的接口拉模型列表（[`list_models`]）。
//! 都不发模型请求、不花钱。请求走 [`Http`]，出站代理在那里生效。

use std::time::Duration;

use serde_json::Value;

use crate::net::{Http, NetError};
use crate::provider::{Modality, Model, Protocol, Provider};

/// 一个接口地址的连通结果。几个协议用同一个地址时只测一次，`protocols` 里列着它们。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reach {
    pub protocols: Vec<Protocol>,
    pub url: String,
    /// 往返时间（连接建好以后的第二次请求），或者连不上的原因。
    pub result: Result<Duration, NetError>,
}

/// 测这家每个接口地址连不连得上（只测可达和延迟，不发模型请求）。
/// 收到任何 HTTP 响应都算连得上，404、401 也算：这一步不管 Key 对不对。
pub fn check(http: &Http, provider: &Provider) -> Vec<Reach> {
    let mut reaches: Vec<Reach> = Vec::new();
    for (&protocol, url) in &provider.endpoints {
        if let Some(seen) = reaches.iter_mut().find(|reach| reach.url == *url) {
            seen.protocols.push(protocol);
            continue;
        }
        reaches.push(Reach {
            protocols: vec![protocol],
            url: url.clone(),
            result: Err(NetError::Timeout),
        });
    }
    for reach in &mut reaches {
        reach.result = http.reach(&reach.url);
    }
    reaches
}

const LIST_TIMEOUT: Duration = Duration::from_secs(20);

/// Anthropic 兼容地址常挂在这些子路径下；拉列表时去掉它们，再按 OpenAI 的写法试一次根地址。
/// 按长度从长到短，长的先认（不然 `/anthropic` 会先吃掉 `/api/anthropic`）。来自 ccs-lite 的 `model_fetch`。
const COMPAT_SUFFIXES: &[&str] = &[
    "/api/claudecode",
    "/api/anthropic",
    "/apps/anthropic",
    "/api/coding",
    "/claudecode",
    "/anthropic",
    "/step_plan",
    "/coding",
    "/claude",
];

/// 从这家的接口拉模型列表：`GET …/models`，按 ID 排好。
///
/// 按 OpenAI（Chat、Responses）、Anthropic、Gemini 的顺序挨个试这家提供的地址，头一个回了列表的算数。
/// 每种协议按它的写法带 Key（Bearer、`x-api-key`、`x-goog-api-key`）。返回里带了上下文、输出上限、
/// 图像输入的（OpenRouter、Gemini 等）顺手记上，其余的留给 models.dev 补。
///
/// 都没拉到时返回最有用的那个错误：Key 不对、服务出错排在「返回的不是模型列表」前面，再后面是「这个地址没有模型列表」（404）。
/// 返回的不是模型列表（中转对不认识的路径回一个网页）也接着试下一个地址。
pub fn list_models(http: &Http, provider: &Provider) -> Result<Vec<Model>, NetError> {
    let key = provider.key.as_str();
    let mut error: Option<NetError> = None;
    for attempt in attempts(provider) {
        let headers: Vec<(&str, &str)> = attempt
            .headers
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        let result = http
            .get(&attempt.url, &headers, LIST_TIMEOUT, &[key])
            .and_then(|body| parse_models(&body));
        match result {
            Ok(mut models) if !models.is_empty() => {
                models.sort_by(|a, b| a.id.cmp(&b.id));
                models.dedup_by(|a, b| a.id == b.id);
                return Ok(models);
            }
            Ok(_) => {}
            Err(err) => {
                if error
                    .as_ref()
                    .is_none_or(|old| usefulness(&err) > usefulness(old))
                {
                    error = Some(err);
                }
            }
        }
    }
    Err(error.unwrap_or(NetError::Status {
        status: 404,
        body: String::new(),
    }))
}

/// 拉列表的几个错误里哪个更能说明问题。
fn usefulness(err: &NetError) -> u8 {
    match err {
        NetError::Status {
            status: 404 | 405, ..
        } => 0,
        NetError::Parse { .. } => 1,
        _ => 2,
    }
}

struct Attempt {
    url: String,
    headers: Vec<(&'static str, String)>,
}

/// 要试的地址和头，按顺序，去掉重复的地址。
fn attempts(provider: &Provider) -> Vec<Attempt> {
    let key = provider.key.as_str();
    let bearer = || ("authorization", format!("Bearer {key}"));
    let mut list: Vec<Attempt> = Vec::new();
    let mut push = |url: String, headers: Vec<(&'static str, String)>| {
        if !list.iter().any(|attempt| attempt.url == url) {
            list.push(Attempt { url, headers });
        }
    };
    let with_key = |headers: Vec<(&'static str, String)>| {
        if key.is_empty() { Vec::new() } else { headers }
    };

    for protocol in [Protocol::OpenaiChat, Protocol::OpenaiResponses] {
        if let Some(base) = provider.endpoint(protocol) {
            for url in openai_models_urls(base) {
                push(url, with_key(vec![bearer()]));
            }
        }
    }
    if let Some(base) = provider.endpoint(Protocol::Anthropic) {
        let mut headers = vec![
            ("x-api-key", key.to_string()),
            ("anthropic-version", "2023-06-01".to_string()),
        ];
        // 第三方的 Anthropic 兼容接口有的只认 Bearer；官方只认 `x-api-key`，两个都带它不一定收。
        if !provider.is_anthropic_official() {
            headers.push(bearer());
        }
        push(format!("{base}/v1/models?limit=1000"), with_key(headers));
        if let Some(root) = strip_compat_suffix(base) {
            for url in openai_models_urls(root) {
                push(url, with_key(vec![bearer()]));
            }
        }
    }
    if let Some(base) = provider.endpoint(Protocol::Gemini) {
        push(
            format!("{base}/v1beta/models?pageSize=1000"),
            with_key(vec![("x-goog-api-key", key.to_string())]),
        );
    }
    list
}

/// OpenAI 风格的模型列表在哪：地址已经以版本段结尾（`/v1`、智谱的 `/api/paas/v4`）就是 `{base}/models`，
/// 否则先试 `{base}/v1/models` 再试 `{base}/models`。
fn openai_models_urls(base: &str) -> Vec<String> {
    let base = base.trim_end_matches('/');
    if ends_with_version(base) {
        vec![format!("{base}/models")]
    } else {
        vec![format!("{base}/v1/models"), format!("{base}/models")]
    }
}

fn ends_with_version(url: &str) -> bool {
    let last = url.rsplit('/').next().unwrap_or_default();
    last.strip_prefix('v')
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

fn strip_compat_suffix(base: &str) -> Option<&str> {
    COMPAT_SUFFIXES
        .iter()
        .find_map(|suffix| base.strip_suffix(suffix))
        .filter(|root| root.contains("://") && crate::provider::host(root).is_some())
}

/// 各家的列表长得不一样：OpenAI、Anthropic、OpenRouter 是 `{"data": [...]}`，Gemini 和 Codex 的目录是
/// `{"models": [...]}`，Together 直接是数组。每一项至少要有 ID（`id`、`slug`，或 Gemini 的 `name: "models/…"`）。
fn parse_models(body: &[u8]) -> Result<Vec<Model>, NetError> {
    let value: Value = serde_json::from_slice(body).map_err(|err| NetError::Parse {
        detail: err.to_string(),
    })?;
    let entries = match &value {
        Value::Array(entries) => entries,
        Value::Object(object) => match object.get("data").or_else(|| object.get("models")) {
            Some(Value::Array(entries)) => entries,
            _ => {
                return Err(NetError::Parse {
                    detail: "没有 data / models 列表".into(),
                });
            }
        },
        _ => {
            return Err(NetError::Parse {
                detail: "不是 JSON 对象".into(),
            });
        }
    };
    Ok(entries.iter().filter_map(entry_model).collect())
}

fn entry_model(entry: &Value) -> Option<Model> {
    let text = |path: &[&str]| -> Option<String> {
        let mut value = entry;
        for key in path {
            value = value.get(key)?;
        }
        value
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    let number = |paths: &[&[&str]]| -> Option<u64> {
        paths.iter().find_map(|path| {
            let mut value = entry;
            for key in *path {
                value = value.get(key)?;
            }
            value
                .as_u64()
                .or_else(|| value.as_f64().map(|n| n as u64))
                .filter(|&n| n > 0)
        })
    };

    // Gemini 只要能生成内容的（列表里还有嵌入模型）。
    if let Some(Value::Array(methods)) = entry.get("supportedGenerationMethods")
        && !methods
            .iter()
            .any(|m| m.as_str() == Some("generateContent"))
    {
        return None;
    }
    let id = text(&["id"])
        .or_else(|| text(&["slug"]))
        .or_else(|| text(&["name"]).map(|name| name.trim_start_matches("models/").to_string()))?;
    let name = text(&["display_name"])
        .or_else(|| text(&["displayName"]))
        .or_else(|| text(&["name"]))
        .filter(|name| *name != id && !name.starts_with("models/"));
    let context = number(&[
        &["context_length"],
        &["context_window"],
        &["max_context_length"],
        &["max_input_tokens"],
        &["inputTokenLimit"],
        &["max_model_len"],
        &["top_provider", "context_length"],
    ]);
    let output = number(&[
        &["max_output_tokens"],
        &["outputTokenLimit"],
        &["max_completion_tokens"],
        &["top_provider", "max_completion_tokens"],
    ]);
    let modalities = ["architecture", "input_modalities"];
    let image = [
        &modalities[..],
        &["input_modalities"],
        &["modalities", "input"],
    ]
    .iter()
    .any(|path| {
        let mut value = entry;
        for key in *path {
            match value.get(key) {
                Some(next) => value = next,
                None => return false,
            }
        }
        value
            .as_array()
            .is_some_and(|list| list.iter().any(|m| m.as_str() == Some("image")))
    });
    Some(Model {
        id,
        name,
        context,
        output,
        input: if image {
            vec![Modality::Text, Modality::Image]
        } else {
            Vec::new()
        },
        ..Model::default()
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::net::tests::{direct, serve};
    use crate::provider::tests::relay;

    fn provider(endpoints: &[(Protocol, String)]) -> Provider {
        Provider {
            endpoints: endpoints.iter().cloned().collect::<BTreeMap<_, _>>(),
            ..relay()
        }
    }

    #[test]
    fn openai_lists_come_from_models_with_a_bearer_key() {
        let server = serve(|request| {
            if request.starts_with("GET /v1/models ") && request.contains("Bearer sk-relay") {
                (
                    200,
                    r#"{"object":"list","data":[{"id":"glm-4.6","object":"model"},{"id":"glm-4.5-air"}]}"#
                        .into(),
                )
            } else {
                (404, "{}".into())
            }
        });
        let provider = provider(&[(Protocol::OpenaiChat, format!("{}/v1", server.url))]);
        let models = list_models(&direct(), &provider).unwrap();
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["glm-4.5-air", "glm-4.6"]);
    }

    #[test]
    fn anthropic_compat_paths_fall_back_to_the_root() {
        // `/anthropic/v1/models` 没有，根上的 `/v1/models` 有（DeepSeek、Kimi 这类）。
        let server = serve(|request| {
            if request.starts_with("GET /v1/models ") {
                (200, r#"{"data":[{"id":"deepseek-v4-pro"}]}"#.into())
            } else {
                (404, "not found".into())
            }
        });
        let provider = provider(&[(Protocol::Anthropic, format!("{}/anthropic", server.url))]);
        let models = list_models(&direct(), &provider).unwrap();
        assert_eq!(models[0].id, "deepseek-v4-pro");
        let first = server.requests.recv().unwrap();
        assert!(
            first.starts_with("GET /anthropic/v1/models?limit=1000 "),
            "{first}"
        );
        assert!(first.contains("x-api-key: sk-relay"), "{first}");
        assert!(first.contains("anthropic-version: 2023-06-01"), "{first}");
    }

    #[test]
    fn gemini_lists_keep_only_generating_models() {
        let server = serve(|request| {
            assert!(request.contains("x-goog-api-key: sk-relay"), "{request}");
            (
                200,
                r#"{"models":[
                    {"name":"models/gemini-3-pro","displayName":"Gemini 3 Pro","inputTokenLimit":1048576,
                     "outputTokenLimit":65536,"supportedGenerationMethods":["generateContent","countTokens"]},
                    {"name":"models/text-embedding-004","supportedGenerationMethods":["embedContent"]}
                ]}"#
                .into(),
            )
        });
        let provider = provider(&[(Protocol::Gemini, server.url.clone())]);
        let models = list_models(&direct(), &provider).unwrap();
        assert_eq!(
            models,
            [Model {
                id: "gemini-3-pro".into(),
                name: Some("Gemini 3 Pro".into()),
                context: Some(1_048_576),
                output: Some(65_536),
                ..Model::default()
            }]
        );
    }

    #[test]
    fn openrouter_style_entries_carry_limits_and_images() {
        let body =
            br#"{"data":[{"id":"z-ai/glm-5.2","name":"Z.ai: GLM 5.2","context_length":202752,
            "architecture":{"input_modalities":["text","image"]},
            "top_provider":{"context_length":202752,"max_completion_tokens":131072}}]}"#;
        let models = parse_models(body).unwrap();
        assert_eq!(
            models,
            [Model {
                id: "z-ai/glm-5.2".into(),
                name: Some("Z.ai: GLM 5.2".into()),
                context: Some(202_752),
                output: Some(131_072),
                input: vec![Modality::Text, Modality::Image],
                ..Model::default()
            }]
        );
        assert!(parse_models(b"[]").unwrap().is_empty());
        assert!(matches!(
            parse_models(b"<html>"),
            Err(NetError::Parse { .. })
        ));
    }

    #[test]
    fn a_wrong_key_beats_not_found() {
        let server = serve(|request| {
            if request.starts_with("GET /v1/models ") {
                (401, r#"{"error":"invalid key sk-relay"}"#.into())
            } else {
                (404, "{}".into())
            }
        });
        let provider = provider(&[(Protocol::OpenaiResponses, server.url.clone())]);
        assert_eq!(
            list_models(&direct(), &provider),
            Err(NetError::Status {
                status: 401,
                body: r#"{"error":"invalid key ***"}"#.into()
            })
        );
    }

    #[test]
    fn a_page_that_is_not_a_model_list_does_not_stop_the_search() {
        // 中转对不认识的 `/v1/models` 回了个网页（200），`/models` 才是列表。
        let server = serve(|request| {
            if request.starts_with("GET /v1/models ") {
                (200, "<!doctype html><html></html>".into())
            } else if request.starts_with("GET /models ") {
                (200, r#"{"data":[{"id":"kimi-k3"}]}"#.into())
            } else {
                (404, "{}".into())
            }
        });
        let relay = provider(&[(Protocol::OpenaiChat, server.url.clone())]);
        let models = list_models(&direct(), &relay).unwrap();
        assert_eq!(models[0].id, "kimi-k3");

        // 都不是列表时报「返回的不是模型列表」，不报 404。
        let server = serve(|request| {
            if request.starts_with("GET /v1/models ") {
                (200, "<!doctype html>".into())
            } else {
                (404, "{}".into())
            }
        });
        let relay = provider(&[(Protocol::OpenaiChat, server.url.clone())]);
        assert!(matches!(
            list_models(&direct(), &relay),
            Err(NetError::Parse { .. })
        ));
    }

    #[test]
    fn version_paths_and_compat_suffixes() {
        assert_eq!(
            openai_models_urls("https://open.bigmodel.cn/api/paas/v4"),
            ["https://open.bigmodel.cn/api/paas/v4/models"]
        );
        assert_eq!(
            openai_models_urls("https://api.deepseek.com"),
            [
                "https://api.deepseek.com/v1/models",
                "https://api.deepseek.com/models"
            ]
        );
        assert_eq!(
            strip_compat_suffix("https://open.bigmodel.cn/api/anthropic"),
            Some("https://open.bigmodel.cn")
        );
        assert_eq!(strip_compat_suffix("https://relay.example"), None);
    }

    #[test]
    fn check_tests_each_address_once() {
        let server = serve(|_| (404, String::new()));
        let provider = provider(&[
            (Protocol::OpenaiChat, format!("{}/v1", server.url)),
            (Protocol::OpenaiResponses, format!("{}/v1", server.url)),
            (Protocol::Anthropic, "http://127.0.0.1:1".into()),
        ]);
        let reaches = check(&direct(), &provider);
        assert_eq!(reaches.len(), 2);
        let shared = reaches.iter().find(|r| r.protocols.len() == 2).unwrap();
        assert!(shared.result.is_ok());
        let dead = reaches
            .iter()
            .find(|r| r.protocols == [Protocol::Anthropic])
            .unwrap();
        assert!(dead.result.is_err());
    }
}
