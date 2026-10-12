//! models.dev（<https://models.dev/api.json>，OpenCode 用的就是它）：补模型目录里没填的上下文、输出上限、
//! 图像输入、推理档位、价格（decisions「功能范围」）。
//!
//! - **缓存**：下下来的原文存在 `~/.confdeck/cache/models.dev.json`，一天内不重下（[`load`]）；
//!   下不下来时用旧的，旧的也没有才报错。
//! - **查一个模型**（[`known`]）按 ccs-lite 的 `resolveModelMetadata`，逐个字段取，先到先得：
//!   1. 地址是某个预设的，用预设里这个模型（对着各家文档核过，同一个模型各家部署的窗口能差好几倍）；
//!   2. models.dev 里地址和这家一样的那一家（地址按「主机 + 路径，去掉版本段和请求路径」比）；
//!   3. models.dev 里的原厂条目（转售的条目用 `canonical_model_id` 指回原厂）。价格不从这里取：转售价不等于原厂价。
//! - **只补空着的字段**（[`fill`]），用户填过的不动。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::data::DataDir;
use crate::fsx::{self, Mode};
use crate::net::{Http, NetError};
use crate::preset;
use crate::provider::{Cost, Modality, Model, Provider};

pub const URL: &str = "https://models.dev/api.json";
/// 缓存多久以后重下。
pub const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const TIMEOUT: Duration = Duration::from_secs(45);

/// 缓存文件的位置。
pub fn cache_path(data: &DataDir) -> PathBuf {
    data.root().join("cache").join("models.dev.json")
}

/// 拿 models.dev 的数据：缓存不到一天就用缓存，否则重下（下不下来用旧缓存）。
pub fn load(data: &DataDir, http: &Http) -> Result<ModelsDev, NetError> {
    let path = cache_path(data);
    let cached = fsx::read_optional(&path).ok().flatten();
    let fresh = std::fs::metadata(&path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age < MAX_AGE);
    if fresh
        && let Some(bytes) = &cached
        && let Ok(index) = ModelsDev::parse(bytes)
    {
        return Ok(index);
    }
    match http.get(URL, &[], TIMEOUT, &[]) {
        Ok(bytes) => {
            let index = ModelsDev::parse(&bytes)?;
            // 存不了缓存不要紧，下次再下。
            std::fs::create_dir_all(path.parent().expect("cache has a parent")).ok();
            fsx::write_atomic(&path, &bytes, Mode::Keep).ok();
            Ok(index)
        }
        Err(err) => cached
            .and_then(|bytes| ModelsDev::parse(&bytes).ok())
            .ok_or(err),
    }
}

/// 解析好、建好索引的 models.dev。
#[derive(Debug, Default)]
pub struct ModelsDev {
    providers: Vec<DevProvider>,
    by_id: HashMap<String, usize>,
    /// 规范化的模型 ID → 各家名下的同名条目（供应商下标、条目在那一家的 ID）。原厂兜底用。
    by_normalized: HashMap<String, Vec<(usize, String)>>,
    /// 被 `canonical_model_id` 指过的供应商，就是原厂。
    vendors: HashSet<String>,
}

#[derive(Debug)]
struct DevProvider {
    id: String,
    /// 地址的比较用写法（[`endpoint_key`]）。
    endpoint: Option<String>,
    models: HashMap<String, Value>,
}

/// models.dev 不给官方三家写 `api`（用的是 SDK 的默认地址），这里补上。
const OFFICIAL: &[(&str, &str)] = &[
    ("anthropic", "https://api.anthropic.com"),
    ("openai", "https://api.openai.com/v1"),
    ("google", "https://generativelanguage.googleapis.com/v1beta"),
];

impl ModelsDev {
    pub fn parse(bytes: &[u8]) -> Result<Self, NetError> {
        let value: Value = serde_json::from_slice(bytes).map_err(|err| NetError::Parse {
            detail: err.to_string(),
        })?;
        let Value::Object(entries) = value else {
            return Err(NetError::Parse {
                detail: "不是 JSON 对象".into(),
            });
        };
        let mut index = Self::default();
        for (provider_id, provider) in entries {
            let Value::Object(mut provider) = provider else {
                continue;
            };
            let api = provider
                .get("api")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    OFFICIAL
                        .iter()
                        .find(|(id, _)| *id == provider_id)
                        .map(|(_, url)| url.to_string())
                });
            let models: HashMap<String, Value> = match provider.remove("models") {
                Some(Value::Object(models)) => models
                    .into_iter()
                    .filter(|(_, model)| model.is_object())
                    .collect(),
                _ => HashMap::new(),
            };
            let n = index.providers.len();
            for (model_id, model) in &models {
                index
                    .by_normalized
                    .entry(normalize_id(model_id))
                    .or_default()
                    .push((n, model_id.clone()));
                if let Some(canonical) = model.get("canonical_model_id").and_then(Value::as_str)
                    && let Some((vendor, _)) = canonical.split_once('/')
                {
                    index.vendors.insert(vendor.to_string());
                }
            }
            index.by_id.insert(provider_id.clone(), n);
            index.providers.push(DevProvider {
                id: provider_id,
                endpoint: api.as_deref().and_then(endpoint_key),
                models,
            });
        }
        Ok(index)
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// 地址是这几个之一的那家 models.dev 供应商名下的这个模型（带价格）。
    fn by_endpoint(&self, urls: &[&str], id: &str) -> Option<Model> {
        let keys: Vec<String> = urls.iter().filter_map(|url| endpoint_key(url)).collect();
        self.providers
            .iter()
            .filter(|provider| provider.endpoint.as_ref().is_some_and(|e| keys.contains(e)))
            .find_map(|provider| find_in(&provider.models, id))
            .map(|value| to_model(id, value, true))
    }

    /// 原厂兜底：同名条目指回的原厂条目；没人指回时才用原厂自己名下的同名条目。
    /// 指向不止一家原厂时说不清是哪个模型，不猜。不带价格。
    fn by_vendor(&self, id: &str) -> Option<Model> {
        let normalized = normalize_id(id);
        let mut pointed: HashMap<String, &Value> = HashMap::new();
        let mut owned: HashMap<String, &Value> = HashMap::new();
        for (n, model_id) in self.by_normalized.get(&normalized).into_iter().flatten() {
            let provider = &self.providers[*n];
            let Some(model) = provider.models.get(model_id) else {
                continue;
            };
            if let Some(canonical) = model.get("canonical_model_id").and_then(Value::as_str)
                && let Some((vendor, rest)) = canonical.split_once('/')
                && let Some(vendor_model) = self
                    .by_id
                    .get(vendor)
                    .and_then(|&v| self.providers[v].models.get(rest))
            {
                pointed.insert(canonical.to_string(), vendor_model);
                continue;
            }
            if self.vendors.contains(&provider.id) {
                owned.insert(format!("{}/{model_id}", provider.id), model);
            }
        }
        let found = if pointed.is_empty() { owned } else { pointed };
        (found.len() == 1)
            .then(|| found.into_values().next())
            .flatten()
            .map(|value| to_model(id, value, false))
    }
}

/// 这家供应商的这个模型已知的参数（只有 ID 和查到的字段）；哪儿都查不到是 `None`。
pub fn known(provider: &Provider, id: &str, dev: Option<&ModelsDev>) -> Option<Model> {
    let id = id.trim();
    if id.is_empty() {
        return None;
    }
    let mut model = Model {
        id: id.to_string(),
        ..Model::default()
    };
    let mut found = false;
    let presets = provider
        .preset
        .as_deref()
        .and_then(preset::get)
        .into_iter()
        .chain(
            provider
                .endpoints
                .iter()
                .filter_map(|(&protocol, url)| preset::matching(protocol, url)),
        );
    for preset in presets {
        if let Some(known) = preset
            .models
            .iter()
            .find(|m| m.id == id || m.id.eq_ignore_ascii_case(id))
        {
            found |= fill(&mut model, known);
            break;
        }
    }
    if let Some(dev) = dev {
        let urls: Vec<&str> = provider.endpoints.values().map(String::as_str).collect();
        if let Some(known) = dev.by_endpoint(&urls, id) {
            found |= fill(&mut model, &known);
        }
        if let Some(known) = dev.by_vendor(id) {
            found |= fill(&mut model, &known);
        }
    }
    found.then_some(model)
}

/// 把 `known` 里有、`target` 里空着的字段填上。填了任何一项返回 `true`。
pub fn fill(target: &mut Model, known: &Model) -> bool {
    let mut changed = false;
    if target.name.is_none() && known.name.is_some() {
        target.name = known.name.clone();
        changed = true;
    }
    if target.context.is_none() && known.context.is_some() {
        target.context = known.context;
        changed = true;
    }
    if target.output.is_none() && known.output.is_some() {
        target.output = known.output;
        changed = true;
    }
    if target.input.is_empty() && known.input.contains(&Modality::Image) {
        target.input = known.input.clone();
        changed = true;
    }
    if target.reasoning.is_empty() && !known.reasoning.is_empty() {
        target.reasoning = known.reasoning.clone();
        changed = true;
    }
    if target.cost.is_none() && known.cost.is_some() {
        target.cost = known.cost;
        changed = true;
    }
    changed
}

/// 一条 models.dev 的模型条目换成目录里的写法。
fn to_model(id: &str, value: &Value, with_cost: bool) -> Model {
    let positive = |value: Option<&Value>| {
        value
            .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|n| n as u64)))
            .filter(|&n| n > 0)
    };
    let strings = |value: Option<&Value>| -> Vec<String> {
        value
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect()
    };
    let limit = value.get("limit");
    let image = strings(value.get("modalities").and_then(|m| m.get("input")))
        .iter()
        .any(|m| m == "image");
    let mut reasoning: Vec<String> = Vec::new();
    for option in value
        .get("reasoning_options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if option.get("type").and_then(Value::as_str) == Some("effort") {
            for level in strings(option.get("values")) {
                if !reasoning.contains(&level) {
                    reasoning.push(level);
                }
            }
        }
    }
    let cost = value.get("cost").filter(|_| with_cost).and_then(|cost| {
        let price = |key: &str| cost.get(key).and_then(Value::as_f64).filter(|n| *n >= 0.0);
        Some(Cost {
            input: price("input")?,
            output: price("output")?,
            cache_read: price("cache_read"),
            cache_write: price("cache_write"),
        })
    });
    Model {
        id: id.to_string(),
        name: value
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(String::from),
        context: positive(limit.and_then(|l| l.get("context"))),
        output: positive(limit.and_then(|l| l.get("output"))),
        input: if image {
            vec![Modality::Text, Modality::Image]
        } else {
            Vec::new()
        },
        reasoning,
        cost,
    }
}

/// 按 ID 找：原样、不分大小写、规范化以后（去掉 `vendor/` 前缀、`:变体`、`[1m]`）。
fn find_in<'a>(models: &'a HashMap<String, Value>, id: &str) -> Option<&'a Value> {
    if let Some(found) = models.get(id) {
        return Some(found);
    }
    let normalized = normalize_id(id);
    let mut by_normalized = None;
    for (model_id, value) in models {
        if model_id.eq_ignore_ascii_case(id) {
            return Some(value);
        }
        if by_normalized.is_none() && normalize_id(model_id) == normalized {
            by_normalized = Some(value);
        }
    }
    by_normalized
}

/// 同一个模型在各家的写法落到同一个 ID 上：去掉命名空间（`vendor/`）、`:变体`、`[1m]`，`@` 换成 `-`，小写。
pub fn normalize_id(id: &str) -> String {
    let after_slash = id.rsplit('/').next().unwrap_or(id);
    let before_colon = after_slash.split(':').next().unwrap_or(after_slash);
    let mut normalized = before_colon.trim().replace('@', "-").to_ascii_lowercase();
    if let Some(stripped) = normalized.strip_suffix("[1m]") {
        normalized = stripped.trim().to_string();
    }
    normalized
}

/// 地址的比较用写法：主机 + 路径，去掉末尾的版本段（`v1`、`v4`、`v1beta`）和请求路径
/// （`chat/completions`、`responses`、`messages`）——写不写都是同一个接口。其余路径要完全一样：
/// `api.z.ai/api/paas/v4` 和 `/api/coding/paas/v4` 是同一个域名下的两个套餐，窗口和价格都不同。
fn endpoint_key(url: &str) -> Option<String> {
    let rest = url
        .trim()
        .split_once("://")
        .map_or(url.trim(), |(_, rest)| rest);
    let mut parts = rest.split(['?', '#']).next()?.split('/');
    let authority = parts.next()?;
    let host = authority.rsplit('@').next()?.to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    let mut segments: Vec<String> = parts
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    loop {
        let len = segments.len();
        let last = segments.last().map(String::as_str);
        if last == Some("completions") && len >= 2 && segments[len - 2] == "chat" {
            segments.truncate(len - 2);
        } else if matches!(last, Some("responses" | "messages")) || last.is_some_and(is_version) {
            segments.pop();
        } else {
            break;
        }
    }
    let mut key = host;
    for segment in segments {
        key.push('/');
        key.push_str(&segment);
    }
    Some(key)
}

/// `v1`、`v4`、`v1beta`、`v2alpha1`。
fn is_version(segment: &str) -> bool {
    let Some(rest) = segment.strip_prefix('v') else {
        return false;
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return false;
    }
    let tail = &rest[digits..];
    let tail = tail
        .strip_prefix("alpha")
        .or_else(|| tail.strip_prefix("beta"))
        .unwrap_or(tail);
    tail.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::provider::Protocol;
    use crate::provider::tests::relay;

    const SAMPLE: &str = r#"{
        "zhipuai": {"id": "zhipuai", "api": "https://open.bigmodel.cn/api/paas/v4", "models": {
            "glm-5.2": {"id": "glm-5.2", "name": "GLM-5.2", "limit": {"context": 204800, "output": 131072},
                "modalities": {"input": ["text"]}, "reasoning": true,
                "reasoning_options": [{"type": "toggle"}],
                "cost": {"input": 0.6, "output": 2.2, "cache_read": 0.11}}
        }},
        "zhipuai-coding-plan": {"api": "https://open.bigmodel.cn/api/coding/paas/v4", "models": {
            "glm-5.2": {"name": "GLM-5.2", "limit": {"context": 1000000, "output": 131072},
                "cost": {"input": 0, "output": 0}}
        }},
        "openrouter": {"api": "https://openrouter.ai/api/v1", "models": {
            "z-ai/glm-5.2": {"name": "Z.ai: GLM 5.2", "canonical_model_id": "zhipuai/glm-5.2",
                "limit": {"context": 202752, "output": 131072}, "cost": {"input": 0.65, "output": 2.4}},
            "deepseek/deepseek-v4-pro": {"canonical_model_id": "deepseek/deepseek-v4-pro"}
        }},
        "deepseek": {"api": "https://api.deepseek.com", "models": {
            "deepseek-v4-pro": {"name": "DeepSeek V4 Pro", "limit": {"context": 1048576, "output": 393216},
                "modalities": {"input": ["text", "image"]},
                "reasoning_options": [{"type": "toggle"}, {"type": "effort", "values": ["low", "High", "max"]}],
                "cost": {"input": 0.28, "output": 0.42, "cache_read": 0.028}}
        }},
        "anthropic": {"models": {
            "claude-fable-5-1": {"name": "Claude Fable 5.1", "limit": {"context": 1000000, "output": 128000},
                "cost": {"input": 3, "output": 15, "cache_read": 0.3, "cache_write": 3.75}},
            "claude-sonnet-4-5": {"name": "Claude Sonnet 4.5", "limit": {"context": 1000000, "output": 64000},
                "cost": {"input": 3, "output": 15, "cache_read": 0.3, "cache_write": 3.75}}
        }},
        "broken": "not an object"
    }"#;

    fn dev() -> ModelsDev {
        ModelsDev::parse(SAMPLE.as_bytes()).unwrap()
    }

    fn at(endpoints: &[(Protocol, &str)]) -> Provider {
        Provider {
            preset: None,
            endpoints: endpoints
                .iter()
                .map(|(p, u)| (*p, u.to_string()))
                .collect::<BTreeMap<_, _>>(),
            ..relay()
        }
    }

    #[test]
    fn endpoints_compare_without_versions_and_request_paths() {
        assert_eq!(
            endpoint_key("https://open.bigmodel.cn/api/paas/v4/").as_deref(),
            Some("open.bigmodel.cn/api/paas")
        );
        assert_eq!(
            endpoint_key("https://api.minimax.io/anthropic/v1/messages").as_deref(),
            Some("api.minimax.io/anthropic")
        );
        assert_eq!(
            endpoint_key("https://API.deepseek.com/v1/chat/completions").as_deref(),
            Some("api.deepseek.com")
        );
        assert_eq!(
            endpoint_key("https://generativelanguage.googleapis.com/v1beta").as_deref(),
            Some("generativelanguage.googleapis.com")
        );
        assert_ne!(
            endpoint_key("https://open.bigmodel.cn/api/coding/paas/v4"),
            endpoint_key("https://open.bigmodel.cn/api/paas/v4")
        );
        assert_eq!(normalize_id("z-ai/GLM-5.2:free"), "glm-5.2");
        assert_eq!(normalize_id("glm-5.3[1m]"), "glm-5.3");
    }

    #[test]
    fn the_same_address_wins_and_brings_its_price() {
        let dev = dev();
        // Coding Plan 的地址：1M 窗口，价格 0（套餐）。
        let plan = at(&[(
            Protocol::OpenaiChat,
            "https://open.bigmodel.cn/api/coding/paas/v4",
        )]);
        let found = super::known(&plan, "glm-5.2", Some(&dev)).unwrap();
        assert_eq!(found.context, Some(1_000_000));
        assert_eq!(found.cost.unwrap().input, 0.0);
        // 按量的地址：200K，有价格；推理档位只有开关、没有 effort，不猜。
        let paas = at(&[(Protocol::OpenaiChat, "https://open.bigmodel.cn/api/paas/v4")]);
        let found = super::known(&paas, "glm-5.2", Some(&dev)).unwrap();
        assert_eq!(found.context, Some(204_800));
        assert_eq!(found.cost.unwrap().cache_read, Some(0.11));
        assert!(found.reasoning.is_empty());
    }

    #[test]
    fn unknown_addresses_fall_back_to_the_vendor_without_a_price() {
        let dev = dev();
        let relay = at(&[(Protocol::OpenaiChat, "https://relay.example/v1")]);
        // OpenRouter 的条目指回智谱。
        let found = super::known(&relay, "z-ai/glm-5.2", Some(&dev)).unwrap();
        assert_eq!(found.id, "z-ai/glm-5.2");
        assert_eq!(found.context, Some(204_800));
        assert_eq!(found.cost, None);
        let found = super::known(&relay, "deepseek-v4-pro", Some(&dev)).unwrap();
        assert_eq!(found.reasoning, ["low", "high", "max"]);
        assert_eq!(found.input, [Modality::Text, Modality::Image]);
        // 官方三家没写 `api`，按内置地址认（Anthropic 的地址也是预设，挑一个预设里没有的模型）。
        let anthropic = at(&[(Protocol::Anthropic, "https://api.anthropic.com")]);
        let found = super::known(&anthropic, "claude-sonnet-4-5", Some(&dev)).unwrap();
        assert_eq!(found.cost.unwrap().cache_write, Some(3.75));
        assert_eq!(super::known(&relay, "no-such-model", Some(&dev)), None);
    }

    #[test]
    fn presets_come_first() {
        // DeepSeek 的预设地址：预设里的值优先，models.dev 补预设没写的（价格）。
        let preset = preset::get("deepseek").unwrap();
        let provider = preset.provider("deepseek", "sk");
        let first = &preset.models[0];
        let found = super::known(&provider, &first.id, Some(&dev())).unwrap();
        assert_eq!(found.context, first.context);
        assert_eq!(found.output, first.output);
        assert_eq!(found.reasoning, first.reasoning);
    }

    #[test]
    fn fill_only_touches_empty_fields() {
        let mut model = Model {
            id: "deepseek-v4-pro".into(),
            context: Some(128_000),
            ..Model::default()
        };
        let found = super::known(
            &at(&[(Protocol::OpenaiChat, "https://api.deepseek.com/v1")]),
            "deepseek-v4-pro",
            Some(&dev()),
        )
        .unwrap();
        assert!(fill(&mut model, &found));
        assert_eq!(model.context, Some(128_000));
        // 地址是 DeepSeek 预设的：输出上限取预设的，价格预设没有，取 models.dev 的。
        assert_eq!(model.output, Some(384_000));
        assert_eq!(model.cost.unwrap().output, 0.42);
        assert!(!fill(&mut model, &found));
    }

    #[test]
    fn load_uses_a_fresh_cache_and_falls_back_to_a_stale_one() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::new(dir.path());
        // 没缓存、下不下来：报错。
        let dead = Http::new(&crate::net::Proxy {
            mode: crate::net::ProxyMode::Custom,
            url: "http://127.0.0.1:1".into(),
        })
        .unwrap();
        assert!(load(&data, &dead).is_err());
        // 有缓存（刚写的，算新鲜）：不联网。
        let path = cache_path(&data);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, SAMPLE).unwrap();
        assert!(!load(&data, &dead).unwrap().is_empty());
        // 缓存过期、下不下来：用旧的。
        let old = SystemTime::now() - MAX_AGE - Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert!(!load(&data, &dead).unwrap().is_empty());
    }
}
