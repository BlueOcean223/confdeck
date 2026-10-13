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
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::de::{self, Deserialize, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
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
    let fresh = std::fs::metadata(&path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age < MAX_AGE);
    if fresh && let Ok(index) = ModelsDev::read(&path) {
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
        Err(err) => ModelsDev::read(&path).map_err(|_| err),
    }
}

/// 解析好、建好索引的 models.dev。
///
/// 常驻内存要小（10-13 arch-perf R0）：文件 5 MB 多，整份解析成 `Value` 树要 60 MB，drop 以后进程的常驻内存也不降
/// （macOS 的分配器不还页）；8 千多个模型各存一个 [`Model`]、再建几张以字符串为键的表，也要 6、7 MB。所以：
/// - 边读边取（[`Builder`]），每个模型只读用得到的几个键，描述这类长字段读过去就丢；
/// - 模型的 ID、名字、`canonical_model_id` 首尾相接存在 `text` 里，条目只记位置（[`Span`]）；推理档位只有几种写法，记下标；
/// - 按 ID 找用排好序的下标数组二分查找，不另存一份字符串当键。
#[derive(Debug, Default)]
pub struct ModelsDev {
    providers: Vec<DevProvider>,
    /// 供应商 ID → 下标。
    by_id: HashMap<String, u32>,
    models: Vec<DevModel>,
    text: String,
    /// 推理档位的写法（`low`、`high`……）。
    levels: Vec<String>,
    /// 各模型的推理档位（`levels` 的下标），条目里记位置。
    level_refs: Vec<u8>,
    /// 规范化 ID 的 hash 和模型下标，按这两样排好（原厂兜底、按规范化的 ID 找用）。hash 撞了用条目的 ID 再核一遍。
    by_normalized: Vec<(u64, u32)>,
    /// 被 `canonical_model_id` 指过的供应商，就是原厂。
    vendors: HashSet<String>,
}

/// `text` 或 `level_refs` 里的一段；长度 0 是没有。
#[derive(Debug, Clone, Copy, Default)]
struct Span {
    start: u32,
    len: u32,
}

impl Span {
    fn range(self) -> std::ops::Range<usize> {
        self.start as usize..(self.start + self.len) as usize
    }
}

#[derive(Debug)]
struct DevProvider {
    id: String,
    /// 地址的比较用写法（[`endpoint_key`]）。
    endpoint: Option<String>,
    /// 这家的模型下标，按 ID 排好；同一个 ID 有几条时文件里靠前的在前。
    by_id: Vec<u32>,
    /// 同上，按小写的 ID 排。
    by_lower: Vec<u32>,
}

#[derive(Debug)]
struct DevModel {
    provider: u32,
    id: Span,
    name: Span,
    canonical: Span,
    /// 上下文、输出上限；0 是没有（models.dev 写 0 或不写，都当没有）。
    context: u64,
    output: u64,
    image: bool,
    levels: Span,
    cost: Option<Cost>,
}

/// models.dev 不给官方三家写 `api`（用的是 SDK 的默认地址），这里补上。
const OFFICIAL: &[(&str, &str)] = &[
    ("anthropic", "https://api.anthropic.com"),
    ("openai", "https://api.openai.com/v1"),
    ("google", "https://generativelanguage.googleapis.com/v1beta"),
];

impl ModelsDev {
    pub fn parse(bytes: &[u8]) -> Result<Self, NetError> {
        Self::build(&mut serde_json::Deserializer::from_slice(bytes))
    }

    /// 从文件读（缓存）：边读边解析，不先把 5 MB 读进内存。解析不了的是 [`std::io::ErrorKind::InvalidData`]。
    pub fn read(path: &Path) -> std::io::Result<Self> {
        let file = std::fs::File::open(path)?;
        Self::build(&mut serde_json::Deserializer::from_reader(
            std::io::BufReader::new(file),
        ))
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))
    }

    fn build<'de, R: serde_json::de::Read<'de>>(
        de: &mut serde_json::Deserializer<R>,
    ) -> Result<Self, NetError> {
        let parse_error = |err: serde_json::Error| NetError::Parse {
            detail: err.to_string(),
        };
        let mut dev = Self::default();
        de.deserialize_map(Builder(&mut dev)).map_err(parse_error)?;
        de.end().map_err(parse_error)?;
        dev.index();
        Ok(dev)
    }

    /// 读完以后建索引。
    fn index(&mut self) {
        let mut by_provider: Vec<Vec<u32>> = vec![Vec::new(); self.providers.len()];
        for (m, model) in self.models.iter().enumerate() {
            by_provider[model.provider as usize].push(m as u32);
        }
        let sorted: Vec<(Vec<u32>, Vec<u32>)> = by_provider
            .into_iter()
            .map(|models| {
                // 都从文件里的顺序稳定排序：同一个 ID（同一个小写写法）文件里靠前的在前。
                let mut exact = models.clone();
                exact.sort_by(|&a, &b| self.id_of(a).cmp(self.id_of(b)));
                let mut lower = models;
                lower.sort_by(|&a, &b| cmp_lower(self.id_of(a), self.id_of(b)));
                (exact, lower)
            })
            .collect();
        for (provider, (by_id, by_lower)) in self.providers.iter_mut().zip(sorted) {
            provider.by_id = by_id;
            provider.by_lower = by_lower;
        }
        let mut by_normalized: Vec<(u64, u32)> = (0..self.models.len() as u32)
            .map(|m| (hash(&normalize_id(self.id_of(m))), m))
            .collect();
        by_normalized.sort_unstable();
        self.by_normalized = by_normalized;
    }

    fn text(&self, span: Span) -> &str {
        &self.text[span.range()]
    }

    /// 模型 `m` 的 ID。
    fn id_of(&self, m: u32) -> &str {
        self.text(self.models[m as usize].id)
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// 规范化以后是 `normalized` 的模型（按文件里的顺序）。
    fn normalized(&self, normalized: &str) -> impl Iterator<Item = u32> + '_ {
        let key = hash(normalized);
        let from = self.by_normalized.partition_point(|&(h, _)| h < key);
        let normalized = normalized.to_string();
        self.by_normalized[from..]
            .iter()
            .take_while(move |&&(h, _)| h == key)
            .map(|&(_, m)| m)
            .filter(move |&m| normalize_id(self.id_of(m)) == normalized)
    }

    /// 在这一家里按 ID 找：原样、不分大小写、规范化以后（去掉 `vendor/` 前缀、`:变体`、`[1m]`）。
    fn find(&self, n: u32, id: &str) -> Option<u32> {
        if let Some(m) = self.find_exact(n, id) {
            return Some(m);
        }
        let provider = &self.providers[n as usize];
        let lower = provider
            .by_lower
            .partition_point(|&m| cmp_lower(self.id_of(m), id).is_lt());
        if let Some(&m) = provider.by_lower.get(lower)
            && self.id_of(m).eq_ignore_ascii_case(id)
        {
            return Some(m);
        }
        self.normalized(&normalize_id(id))
            .find(|&m| self.models[m as usize].provider == n)
    }

    /// 地址是这几个之一的那家 models.dev 供应商名下的这个模型（带价格）。
    fn by_endpoint(&self, urls: &[&str], id: &str) -> Option<Model> {
        let keys: Vec<String> = urls.iter().filter_map(|url| endpoint_key(url)).collect();
        (0..self.providers.len() as u32)
            .filter(|&n| {
                self.providers[n as usize]
                    .endpoint
                    .as_ref()
                    .is_some_and(|e| keys.contains(e))
            })
            .find_map(|n| self.find(n, id))
            .map(|m| self.model(m, id, true))
    }

    /// 原厂兜底：同名条目指回的原厂条目；没人指回时才用原厂自己名下的同名条目。
    /// 指向不止一家原厂时说不清是哪个模型，不猜。不带价格。
    fn by_vendor(&self, id: &str) -> Option<Model> {
        let mut pointed: HashMap<&str, u32> = HashMap::new();
        let mut owned: HashMap<(u32, &str), u32> = HashMap::new();
        for m in self.normalized(&normalize_id(id)) {
            let model = &self.models[m as usize];
            let canonical = self.text(model.canonical);
            if let Some((vendor, rest)) = canonical.split_once('/')
                && let Some(&v) = self.by_id.get(vendor)
                && let Some(found) = self.find_exact(v, rest)
            {
                pointed.insert(canonical, found);
                continue;
            }
            if self
                .vendors
                .contains(&self.providers[model.provider as usize].id)
            {
                // 同一家里同一个 ID 写了两次：和 `find_exact` 一样取文件里靠前的。
                owned
                    .entry((model.provider, self.text(model.id)))
                    .or_insert(m);
            }
        }
        let found: Vec<u32> = if pointed.is_empty() {
            owned.into_values().collect()
        } else {
            pointed.into_values().collect()
        };
        match found[..] {
            [only] => Some(self.model(only, id, false)),
            _ => None,
        }
    }

    /// 这一家里 ID 原样是 `id` 的那条。
    fn find_exact(&self, n: u32, id: &str) -> Option<u32> {
        let provider = &self.providers[n as usize];
        let at = provider.by_id.partition_point(|&m| self.id_of(m) < id);
        provider
            .by_id
            .get(at)
            .copied()
            .filter(|&m| self.id_of(m) == id)
    }

    /// 查 `id` 查到了模型 `m`：换成目录里的写法，ID 写查的那个，`with_cost` 为假时不带价格。
    fn model(&self, m: u32, id: &str, with_cost: bool) -> Model {
        let model = &self.models[m as usize];
        let positive = |n: u64| (n > 0).then_some(n);
        Model {
            id: id.to_string(),
            name: (model.name.len > 0).then(|| self.text(model.name).to_string()),
            context: positive(model.context),
            output: positive(model.output),
            input: if model.image {
                vec![Modality::Text, Modality::Image]
            } else {
                Vec::new()
            },
            reasoning: self.level_refs[model.levels.range()]
                .iter()
                .map(|&level| self.levels[level as usize].clone())
                .collect(),
            cost: model.cost.filter(|_| with_cost),
        }
    }

    /// 读到一家（`api` 可能在 `models` 后面，所以先记下来，读完这家再填地址）。
    fn push_provider(&mut self, id: String) -> u32 {
        let n = self.providers.len() as u32;
        self.by_id.insert(id.clone(), n);
        self.providers.push(DevProvider {
            id,
            endpoint: None,
            by_id: Vec::new(),
            by_lower: Vec::new(),
        });
        n
    }

    /// 读到一个模型：用得到的几个键在 `entry` 里。
    fn push_model(&mut self, provider: u32, id: &str, entry: &serde_json::Map<String, Value>) {
        let positive = |value: Option<&Value>| {
            value
                .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|n| n as u64)))
                .unwrap_or(0)
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
        let limit = entry.get("limit");
        let image = strings(entry.get("modalities").and_then(|m| m.get("input")))
            .iter()
            .any(|m| m == "image");
        let levels_start = self.level_refs.len() as u32;
        for option in entry
            .get("reasoning_options")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if option.get("type").and_then(Value::as_str) != Some("effort") {
                continue;
            }
            for level in strings(option.get("values")) {
                // 下标是 u8，最多记 256 种写法（实际只有几种）；再多的不记。
                let index = match self.levels.iter().position(|l| *l == level) {
                    Some(index) => index,
                    None if self.levels.len() <= usize::from(u8::MAX) => {
                        self.levels.push(level);
                        self.levels.len() - 1
                    }
                    None => continue,
                };
                let index = index as u8;
                if !self.level_refs[levels_start as usize..].contains(&index) {
                    self.level_refs.push(index);
                }
            }
        }
        let cost = entry.get("cost").and_then(|cost| {
            let price = |key: &str| cost.get(key).and_then(Value::as_f64).filter(|n| *n >= 0.0);
            Some(Cost {
                input: price("input")?,
                output: price("output")?,
                cache_read: price("cache_read"),
                cache_write: price("cache_write"),
            })
        });
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        let canonical = entry
            .get("canonical_model_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if let Some((vendor, _)) = canonical.split_once('/') {
            self.vendors.insert(vendor.to_string());
        }
        let model = DevModel {
            provider,
            id: self.push_text(id),
            name: self.push_text(name),
            canonical: self.push_text(canonical),
            context: positive(limit.and_then(|l| l.get("context"))),
            output: positive(limit.and_then(|l| l.get("output"))),
            image,
            levels: Span {
                start: levels_start,
                len: self.level_refs.len() as u32 - levels_start,
            },
            cost,
        };
        self.models.push(model);
    }

    fn push_text(&mut self, text: &str) -> Span {
        let start = self.text.len() as u32;
        self.text.push_str(text);
        Span {
            start,
            len: text.len() as u32,
        }
    }
}

/// 不分大小写比（ASCII）。
fn cmp_lower(a: &str, b: &str) -> std::cmp::Ordering {
    a.bytes()
        .map(|b| b.to_ascii_lowercase())
        .cmp(b.bytes().map(|b| b.to_ascii_lowercase()))
}

fn hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

// 下面几个 visitor 边读边往 [`ModelsDev`] 里放：只认用到的键，别的键和不是对象的条目读过去就丢，不建整份的 `Value` 树。
// 和原来按 `Value` 取一样宽松：哪一家、哪个模型的形状不对，跳过那一项，整份照样能用。

/// 不是对象的值（字符串、数字、数组、`null`）：读过去，什么都不做。
macro_rules! skip_non_objects {
    () => {
        fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
            Ok(Default::default())
        }
        fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
            Ok(Default::default())
        }
        fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
            Ok(Default::default())
        }
        fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
            Ok(Default::default())
        }
        fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
            Ok(Default::default())
        }
        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Default::default())
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            while seq.next_element::<IgnoredAny>()?.is_some() {}
            Ok(Default::default())
        }
    };
}

/// 顶层：供应商 ID → 一家。
struct Builder<'a>(&'a mut ModelsDev);

impl<'de> Visitor<'de> for Builder<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a JSON object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(id) = map.next_key::<String>()? {
            map.next_value_seed(ProviderSeed {
                dev: &mut *self.0,
                id,
            })?;
        }
        Ok(())
    }
}

/// 一家。
struct ProviderSeed<'a> {
    dev: &'a mut ModelsDev,
    id: String,
}

impl<'de> de::DeserializeSeed<'de> for ProviderSeed<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for ProviderSeed<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any value")
    }

    skip_non_objects!();

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let n = self.dev.push_provider(self.id);
        let mut api = None;
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "api" => api = map.next_value::<Value>()?.as_str().map(str::to_string),
                "models" => map.next_value_seed(ModelsSeed {
                    dev: &mut *self.dev,
                    provider: n,
                })?,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        let provider = &mut self.dev.providers[n as usize];
        let api = api.or_else(|| {
            OFFICIAL
                .iter()
                .find(|(id, _)| *id == provider.id)
                .map(|(_, url)| url.to_string())
        });
        provider.endpoint = api.as_deref().and_then(endpoint_key);
        Ok(())
    }
}

/// 一家名下的模型。
struct ModelsSeed<'a> {
    dev: &'a mut ModelsDev,
    provider: u32,
}

impl<'de> de::DeserializeSeed<'de> for ModelsSeed<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for ModelsSeed<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any value")
    }

    skip_non_objects!();

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(id) = map.next_key::<String>()? {
            if let Some(entry) = map.next_value::<RawModel>()?.0 {
                self.dev.push_model(self.provider, &id, &entry);
            }
        }
        Ok(())
    }
}

/// 一个模型条目里用得到的几个键（各自很小，读成 `Value`）；描述这类长字段读过去就丢。不是对象的条目是 `None`。
struct RawModel(Option<serde_json::Map<String, Value>>);

/// [`ModelsDev::push_model`] 要的键。
const MODEL_KEYS: [&str; 6] = [
    "name",
    "limit",
    "modalities",
    "reasoning_options",
    "cost",
    "canonical_model_id",
];

impl<'de> Deserialize<'de> for RawModel {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ModelVisitor).map(RawModel)
    }
}

struct ModelVisitor;

impl<'de> Visitor<'de> for ModelVisitor {
    type Value = Option<serde_json::Map<String, Value>>;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any value")
    }

    skip_non_objects!();

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut kept = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if MODEL_KEYS.contains(&key.as_str()) {
                kept.insert(key, map.next_value::<Value>()?);
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(Some(kept))
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
        "broken": "not an object",
        "odd": {"api": "https://odd.example/v1", "models": {"GLM-Odd": {"name": "Odd", "limit": {"context": 1000}},
            "bad": [1, 2], "skip": null}},
        "twins": {"models": {"Foo": {"limit": {"context": 1}}, "foo": {"limit": {"context": 2}},
            "x/foo": {"limit": {"context": 3}}, "FOO": {"limit": {"context": 4}},
            "dup": {"limit": {"context": 5}}, "dup": {"limit": {"context": 6}}},
            "api": "https://twins.example/v1"}
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
    fn ids_match_ignoring_case_and_prefixes() {
        let dev = dev();
        let odd = at(&[(Protocol::OpenaiChat, "https://odd.example/v1")]);
        for id in ["GLM-Odd", "glm-odd", "vendor/GLM-ODD:free", "glm-odd[1m]"] {
            let found = super::known(&odd, id, Some(&dev)).unwrap();
            assert_eq!(found.id, id);
            assert_eq!(found.context, Some(1000), "{id}");
        }
        // 不是对象的模型条目跳过，同一家别的照样能用。
        assert_eq!(super::known(&odd, "bad", Some(&dev)), None);
    }

    #[test]
    fn ties_go_to_the_entry_earlier_in_the_file() {
        let dev = dev();
        // `api` 写在 `models` 后面也认得出这家。
        let twins = at(&[(Protocol::OpenaiChat, "https://twins.example/v1")]);
        let context = |id: &str| super::known(&twins, id, Some(&dev)).and_then(|m| m.context);
        // 原样对得上的先用。
        assert_eq!(context("Foo"), Some(1));
        assert_eq!(context("foo"), Some(2));
        assert_eq!(context("FOO"), Some(4));
        // 只差大小写、规范化以后一样的，取文件里靠前的。
        assert_eq!(context("fOo"), Some(1));
        assert_eq!(context("y/foo"), Some(1));
        // 同一个 ID 写了两遍：取前面那条。
        assert_eq!(context("dup"), Some(5));
    }

    #[test]
    fn reading_the_file_is_the_same_as_parsing_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api.json");
        std::fs::write(&path, SAMPLE).unwrap();
        let read = ModelsDev::read(&path).unwrap();
        let relay = at(&[(Protocol::OpenaiChat, "https://relay.example/v1")]);
        let paas = at(&[(Protocol::OpenaiChat, "https://open.bigmodel.cn/api/paas/v4")]);
        for (provider, id) in [
            (&relay, "z-ai/glm-5.2"),
            (&paas, "glm-5.2"),
            (&relay, "deepseek-v4-pro"),
        ] {
            assert_eq!(
                super::known(provider, id, Some(&read)),
                super::known(provider, id, Some(&dev()))
            );
        }
        assert!(ModelsDev::read(&dir.path().join("missing.json")).is_err());
        assert!(ModelsDev::parse(b"[1, 2]").is_err());
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
