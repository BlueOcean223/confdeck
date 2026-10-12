//! Codex 的模型目录文件 `~/.codex/confdeck-models.json`，`config.toml` 的 `model_catalog_json` 指向它。
//! 写了它，Codex 的 `/model` 列出这家的模型，按目录里的窗口压缩；不写的话第三方模型按兜底数据算 272K 窗口。
//!
//! 目录里每个模型都要带系统提示词，Codex 原样拿它换掉自带的。提示词从用户装的 Codex 取（用户 10-10 定的）：
//! 跑 `codex debug models --bundled`，克隆一个普通 Responses 的内置模型（不是 code mode、不走 Responses Lite 的，
//! 0.153.4 上是 gpt-5.5），把提示词里的「based on GPT-5」换成这家的模型名。工具设置用 Codex 对不认识的模型的那一套：
//! 不声明 freeform `apply_patch`（第三方网关不一定支持这种工具），模型经 `exec_command` 调 `apply_patch` 照样能改文件。
//! 找不到 `codex`、跑不成、挑不出模板时不写目录，和以前一样，原因（[`Skipped`]）交给界面说一声。
//! Codex 升级以后由 [`crate::apply::refresh_codex_catalog`] 重写。核实过程见 notes/codex-catalog-prompt.md。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

use serde_json::{Map, Value as Json, json};

use super::AGENT;
use crate::engine::LiveFile;
use crate::fsx::Mode;
use crate::patch::Format;
use crate::provider::{Modality, Model};

/// `model_catalog_json` 写的值。相对路径按 `CODEX_HOME` 算（0.153.4 实测），同步到别的机器也对。
pub const FILE_NAME: &str = "confdeck-models.json";

/// `codex debug models --bundled` 最多等多久（平时约 50 毫秒）。
const TIMEOUT: Duration = Duration::from_secs(10);

pub fn path(home: &Path) -> PathBuf {
    home.join(".codex").join(FILE_NAME)
}

/// 里面没有 Key。
pub fn live_file(home: &Path) -> LiveFile {
    LiveFile {
        owner: AGENT.id(),
        path: path(home),
        mode: Mode::Keep,
        format: Format::Json,
    }
}

/// 没写成目录的原因。界面要说一声：`/model` 列的还是 GPT 模型，这家的模型按 272K 窗口算。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Skipped {
    #[error("没找到 codex")]
    NotFound,
    /// 超时、退出码不是 0、输出不是 JSON。太旧的 Codex 没有这条命令。
    #[error("codex debug models --bundled 没跑成")]
    Failed,
    /// Codex 改了内置模型的格式，或者不再有普通 Responses 的模型。
    #[error("Codex 内置的模型里挑不出能照着写的")]
    NoTemplate,
}

/// 生成这家供应商的目录文件内容。
pub fn generate(home: &Path, models: &[&Model]) -> Result<Vec<u8>, Skipped> {
    let codex = locate(home).ok_or(Skipped::NotFound)?;
    let bundled = bundled(&codex).ok_or(Skipped::Failed)?;
    build(&bundled, models).ok_or(Skipped::NoTemplate)
}

/// 用 `codex debug models --bundled` 的输出生成目录文件内容。挑不出模板时为 `None`。
pub fn build(bundled: &Json, models: &[&Model]) -> Option<Vec<u8>> {
    let template = template(bundled)?;
    let entries: Vec<Json> = models
        .iter()
        .enumerate()
        .map(|(index, model)| Json::Object(entry(template, model, index)))
        .collect();
    let mut bytes = serde_json::to_vec_pretty(&json!({ "models": entries })).ok()?;
    bytes.push(b'\n');
    Some(bytes)
}

/// 优先级最高的普通 Responses 模型：有提示词，不是 code mode（`tool_mode`），不走 Responses Lite。
/// 后两种是 OpenAI 自己的后端才有的，提示词也是照着那套工具写的。
fn template(bundled: &Json) -> Option<&Map<String, Json>> {
    bundled
        .get("models")?
        .as_array()?
        .iter()
        .filter_map(Json::as_object)
        .filter(|model| {
            model
                .get("base_instructions")
                .and_then(Json::as_str)
                .is_some_and(|text| !text.trim().is_empty())
                && model.get("tool_mode").is_none_or(Json::is_null)
                && !model
                    .get("use_responses_lite")
                    .and_then(Json::as_bool)
                    .unwrap_or(false)
        })
        .min_by_key(|model| {
            model
                .get("priority")
                .and_then(Json::as_i64)
                .unwrap_or(i64::MAX)
        })
}

fn entry(template: &Map<String, Json>, model: &Model, index: usize) -> Map<String, Json> {
    let mut entry = template.clone();
    let name = model.name.as_deref().unwrap_or(&model.id);
    let mut set = |key: &str, value: Json| {
        entry.insert(key.to_string(), value);
    };
    set("slug", json!(model.id));
    set("display_name", json!(name));
    set("description", json!(name));
    // 内置目录里藏起来的模板，用户写了的模型也要出现在选择器里。
    set("visibility", json!("list"));
    set("priority", json!(index));
    // 速度档、升级提示、Responses Lite 归 OpenAI 官方账号。
    set("additional_speed_tiers", json!([]));
    set("service_tiers", json!([]));
    set("availability_nux", Json::Null);
    set("upgrade", Json::Null);
    set("use_responses_lite", json!(false));
    // 工具和请求参数用 Codex 对不认识的模型的那一套（`model_info_from_slug`），第三方现在就是这么用的。
    set("apply_patch_tool_type", Json::Null);
    set("web_search_tool_type", json!("text"));
    set("supports_search_tool", json!(false));
    set("support_verbosity", json!(false));
    set("default_verbosity", Json::Null);
    set(
        "truncation_policy",
        json!({"mode": "bytes", "limit": 10000}),
    );
    set("default_reasoning_summary", json!("auto"));
    if let Some(context) = model.context {
        set("context_window", json!(context));
        set("max_context_window", json!(context));
    }
    let input: Vec<&str> = std::iter::once("text")
        .chain(model.input.contains(&Modality::Image).then_some("image"))
        .collect();
    set("input_modalities", json!(input));
    let (levels, default) = reasoning(template, &model.reasoning);
    set("supported_reasoning_levels", levels);
    set("default_reasoning_level", default);
    // 自动压缩阈值按窗口的 90% 算；默认服务档归官方账号。
    entry.remove("auto_compact_token_limit");
    entry.remove("default_service_tier");

    if let Some(Json::String(text)) = entry.get_mut("base_instructions") {
        *text = rename(text, name);
    }
    if let Some(Json::String(text)) = entry
        .get_mut("model_messages")
        .and_then(|messages| messages.get_mut("instructions_template"))
    {
        *text = rename(text, name);
    }
    entry
}

/// 推理档位：说明沿用模板里同名档位的。默认档：模板的默认在列表里就用它，否则 `high`，再否则最高的那档。
/// 不支持推理的模型两项都空，和 Codex 对不认识的模型一样。
fn reasoning(template: &Map<String, Json>, efforts: &[String]) -> (Json, Json) {
    let described = |effort: &str| {
        template
            .get("supported_reasoning_levels")
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
            .find(|level| level.get("effort").and_then(Json::as_str) == Some(effort))
            .and_then(|level| level.get("description"))
            .and_then(Json::as_str)
            .unwrap_or(effort)
            .to_string()
    };
    let levels: Vec<Json> = efforts
        .iter()
        .map(|effort| json!({"effort": effort, "description": described(effort)}))
        .collect();
    let template_default = template
        .get("default_reasoning_level")
        .and_then(Json::as_str);
    let default = template_default
        .filter(|level| efforts.iter().any(|effort| effort == level))
        .or_else(|| efforts.iter().map(String::as_str).find(|e| *e == "high"))
        .or_else(|| efforts.last().map(String::as_str));
    (Json::Array(levels), json!(default))
}

/// 提示词第一处「based on GPT-5」（版本号可以是 `5.5`、`6-astra`）换成「based on 模型名」。没有就原样返回。
fn rename(text: &str, name: &str) -> String {
    const MARK: &str = "based on GPT-";
    let Some(start) = text.find(MARK) else {
        return text.to_string();
    };
    let rest = &text[start + MARK.len()..];
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return text.to_string();
    }
    // 版本号后面接 `.5`、`-astra` 这样的段；句末的句号不算。
    let mut end = digits;
    loop {
        let tail = &rest[end..];
        let Some(separator) = tail.chars().next().filter(|c| matches!(c, '.' | '-')) else {
            break;
        };
        let segment = tail[1..]
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .count();
        if segment == 0 {
            break;
        }
        end += separator.len_utf8() + segment;
    }
    format!("{}based on {name}{}", &text[..start], &rest[end..])
}

/// 找 `codex`。测试里一律找不到，除非测试自己指定（[`tests::use_codex`]）。
fn locate(home: &Path) -> Option<PathBuf> {
    #[cfg(test)]
    {
        let _ = home;
        tests::FAKE_CODEX.with(|fake| fake.borrow().clone())
    }
    #[cfg(not(test))]
    crate::program::find(home, "codex")
}

/// 跑 `codex debug models --bundled`。`CODEX_HOME` 指到一个临时目录：这条命令不读配置，
/// 但会在 `CODEX_HOME` 下建 `tmp/arg0`，不往用户的 `~/.codex` 里留东西。
fn bundled(codex: &Path) -> Option<Json> {
    static RUNS: AtomicUsize = AtomicUsize::new(0);
    let run_id = RUNS.fetch_add(1, Ordering::Relaxed);
    let scratch = env::temp_dir().join(format!("confdeck-codex-{}-{run_id}", std::process::id()));
    fs::create_dir_all(&scratch).ok()?;
    let output = run(Command::new(codex)
        .args(["debug", "models", "--bundled"])
        .env("CODEX_HOME", &scratch));
    let _ = fs::remove_dir_all(&scratch);
    serde_json::from_slice(&output?).ok()
}

/// 跑命令，取标准输出；超时或退出码不是 0 为 `None`。
fn run(command: &mut Command) -> Option<Vec<u8>> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // 输出有半兆，边跑边读，免得管道写满卡住。
    let mut stdout = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut stdout, &mut bytes).map(|_| bytes)
    });
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let bytes = reader.join().ok()?.ok()?;
    status.success().then_some(bytes)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::RefCell;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    thread_local! {
        pub(super) static FAKE_CODEX: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    }

    /// 这个测试线程里的 `codex` 换成一个脚本，输出 `bundled`。
    pub(crate) fn use_codex(dir: &Path, bundled: &Json) {
        let data = dir.join("bundled.json");
        fs::write(&data, serde_json::to_vec(bundled).unwrap()).unwrap();
        let script = dir.join("codex");
        fs::write(&script, format!("#!/bin/sh\ncat '{}'\n", data.display())).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        FAKE_CODEX.with(|fake| *fake.borrow_mut() = Some(script));
    }

    /// 这个测试线程里又找不到 `codex` 了。
    pub(crate) fn no_codex() {
        FAKE_CODEX.with(|fake| *fake.borrow_mut() = None);
    }

    /// 照 0.153.4 的内置目录缩出来的三个模型：code mode 的、普通的、更旧的。
    pub(crate) fn sample() -> Json {
        json!({"models": [
            {
                "slug": "gpt-6-astra", "display_name": "GPT-6-Astra", "priority": 1,
                "tool_mode": "code_mode_only", "use_responses_lite": true,
                "base_instructions": "You are Codex, an agent based on GPT-6. Code mode."
            },
            {
                "slug": "gpt-5.5", "display_name": "GPT-5.5", "description": "Frontier", "priority": 12,
                "tool_mode": null, "use_responses_lite": false, "visibility": "list",
                "shell_type": "unified_exec", "apply_patch_tool_type": "freeform",
                "web_search_tool_type": "text_and_image", "supports_search_tool": true,
                "support_verbosity": true, "default_verbosity": "low",
                "truncation_policy": {"mode": "tokens", "limit": 10000},
                "default_reasoning_summary": "none", "supported_in_api": true,
                "default_reasoning_level": "medium",
                "supported_reasoning_levels": [
                    {"effort": "low", "description": "Fast responses"},
                    {"effort": "medium", "description": "Balanced"},
                    {"effort": "high", "description": "Deeper"}
                ],
                "context_window": 272000, "max_context_window": 272000,
                "service_tiers": [{"id": "priority"}], "additional_speed_tiers": ["fast"],
                "default_service_tier": "priority", "experimental_supported_tools": [],
                "input_modalities": ["text", "image"],
                "base_instructions": "You are Codex, a coding agent based on GPT-5. Use `apply_patch`.",
                "model_messages": {"instructions_template": "You are Codex, a coding agent based on GPT-5. {{ personality }}"}
            },
            {
                "slug": "gpt-5.2", "priority": 29, "tool_mode": null,
                "base_instructions": "You are GPT-5.2 running in the Codex CLI."
            }
        ]})
    }

    fn model(id: &str) -> Model {
        Model {
            id: id.into(),
            ..Model::default()
        }
    }

    fn entries(bytes: &[u8]) -> Vec<Json> {
        let catalog: Json = serde_json::from_slice(bytes).unwrap();
        catalog["models"].as_array().unwrap().clone()
    }

    #[test]
    fn entries_clone_the_plain_responses_model() {
        let pro = Model {
            id: "deepseek-v4-pro".into(),
            name: Some("DeepSeek V4 Pro".into()),
            context: Some(1_048_576),
            input: vec![Modality::Text],
            reasoning: vec!["low".into(), "high".into(), "max".into()],
            ..Model::default()
        };
        let flash = model("deepseek-flash");
        let out = build(&sample(), &[&pro, &flash]).unwrap();
        let entries = entries(&out);
        let first = &entries[0];
        assert_eq!(first["slug"], "deepseek-v4-pro");
        assert_eq!(first["display_name"], "DeepSeek V4 Pro");
        assert_eq!(first["priority"], 0);
        assert_eq!(first["visibility"], "list");
        assert_eq!(
            first["base_instructions"],
            "You are Codex, a coding agent based on DeepSeek V4 Pro. Use `apply_patch`."
        );
        assert_eq!(
            first["model_messages"]["instructions_template"],
            "You are Codex, a coding agent based on DeepSeek V4 Pro. {{ personality }}"
        );
        assert_eq!(first["context_window"], 1_048_576);
        assert_eq!(first["max_context_window"], 1_048_576);
        assert_eq!(first["input_modalities"], json!(["text"]));
        assert_eq!(
            first["supported_reasoning_levels"],
            json!([
                {"effort": "low", "description": "Fast responses"},
                {"effort": "high", "description": "Deeper"},
                {"effort": "max", "description": "max"}
            ])
        );
        assert_eq!(first["default_reasoning_level"], "high");
        // 工具设置换成兜底那套，官方账号的字段清掉，别的照模板。
        assert_eq!(first["apply_patch_tool_type"], Json::Null);
        assert_eq!(first["web_search_tool_type"], "text");
        assert_eq!(first["support_verbosity"], false);
        assert_eq!(first["service_tiers"], json!([]));
        assert!(first.get("default_service_tier").is_none());
        assert_eq!(first["shell_type"], "unified_exec");
        assert_eq!(first["supported_in_api"], true);

        // 不知道窗口、不支持推理的照模板的窗口，推理档位两项都空。
        let second = &entries[1];
        assert_eq!(second["display_name"], "deepseek-flash");
        assert_eq!(second["context_window"], 272_000);
        assert_eq!(second["supported_reasoning_levels"], json!([]));
        assert_eq!(second["default_reasoning_level"], Json::Null);
        assert_eq!(out.last(), Some(&b'\n'));
    }

    #[test]
    fn no_template_means_no_catalog() {
        let only_code_mode = json!({"models": [sample()["models"][0].clone()]});
        assert_eq!(build(&only_code_mode, &[&model("m")]), None);
        assert_eq!(build(&json!({}), &[&model("m")]), None);
    }

    #[test]
    fn the_identity_line_names_the_model() {
        assert_eq!(
            rename("an agent based on GPT-5. Next", "GLM"),
            "an agent based on GLM. Next"
        );
        assert_eq!(
            rename("based on GPT-5.5-mini, and", "K3"),
            "based on K3, and"
        );
        assert_eq!(rename("based on GPT-6-astra.", "K3"), "based on K3.");
        assert_eq!(rename("based on GPT-x", "K3"), "based on GPT-x");
        assert_eq!(
            rename("You are GPT-5.2 running", "K3"),
            "You are GPT-5.2 running"
        );
    }

    #[test]
    fn a_script_stands_in_for_codex() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(generate(dir.path(), &[&model("m")]), Err(Skipped::NotFound));

        use_codex(dir.path(), &json!("not a catalog"));
        assert_eq!(
            generate(dir.path(), &[&model("m")]),
            Err(Skipped::NoTemplate)
        );
        fs::write(dir.path().join("bundled.json"), "Usage: codex debug ...").unwrap();
        assert_eq!(generate(dir.path(), &[&model("m")]), Err(Skipped::Failed));

        use_codex(dir.path(), &sample());
        let out = generate(dir.path(), &[&model("m")]).unwrap();
        assert_eq!(entries(&out)[0]["slug"], "m");
    }
}
