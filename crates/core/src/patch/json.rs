//! JSON 补丁（Claude Code 的 `settings.json`、Pi 的 `models.json` 等）。
//!
//! `serde_json` 开了 `preserve_order`，键保持文件里的顺序。但 `Map::remove` 在
//! `preserve_order` 下等于 `swap_remove`，会把最后一个键挪到被删键的位置，所以这里
//! 删除一律用 `shift_remove`，替换一律原位改值，新增的键追加到所在对象末尾。
//!
//! 允许 JSON5 的文件（Pi、OpenCode）交给 [`super::jsonc`] 改写，注释和排版原样留下，只动变了的值。
//! 改写不了（见那边的说明）或者改完重新解析和目标对不上时，退回下面的重新序列化。
//!
//! 重新序列化不保留原始排版：缩进沿用文件原有的（空格或 Tab），换行符和末尾换行
//! 照原样，但 `é` 这类转义会变成原字符、数字可能换写法，注释和尾逗号会丢。
//! 所以承诺的是：键、值、顺序不变；第一次写入可能规范化写法，之后的写入字节稳定。

use std::collections::HashSet;
use std::path::Path;

use serde::Serialize;
use serde_json::{Map, Value};

use super::{KeyPath, LivePatch, PatchError, decode_utf8, jsonc};

/// 按谓词清空的作用域：父对象里所有命中谓词的键（关键字段）。
#[derive(Clone)]
pub struct ClearScope {
    pub parent: KeyPath,
    pub is_floor: fn(&str) -> bool,
}

#[derive(Clone, Default)]
pub struct JsonPatch {
    /// 文件允许 JSON5 写法（注释、尾逗号、不加引号的键）。写回时保留注释和排版。
    pub json5: bool,
    /// 先清空的关键字段。目标值里也有的键不删，留给 `set` 原位改值。
    pub clear: Vec<ClearScope>,
    /// 按路径点名删掉的键。`set` 里有同一路径时跳过。
    pub remove: Vec<KeyPath>,
    /// 值还是这个才删：confdeck 上次写的、之后没被改过的键。`set` 里有同一路径时跳过。
    pub remove_if: Vec<(KeyPath, Value)>,
    /// 目标值：已存在就原位改值，不存在就追加到父对象末尾（父对象缺失时一并创建）。
    pub set: Vec<(KeyPath, Value)>,
    /// 这些对象被上面几步删空了，就连对象本身一起删掉（比如切回默认后不留一个空的 `env`）。
    /// 原来就是空的不动。
    pub drop_if_emptied: Vec<KeyPath>,
}

impl JsonPatch {
    pub fn apply_to(&self, path: &Path, doc: &mut Value) -> Result<(), PatchError> {
        let targets: HashSet<&KeyPath> = self.set.iter().map(|(key_path, _)| key_path).collect();
        let had_content: Vec<&KeyPath> = self
            .drop_if_emptied
            .iter()
            .filter(|key_path| {
                value_at(doc, key_path)
                    .and_then(Value::as_object)
                    .is_some_and(|map| !map.is_empty())
            })
            .collect();

        for scope in &self.clear {
            let Some(map) = object_at_mut(path, doc, &scope.parent.0)? else {
                continue;
            };
            let doomed: Vec<String> = map
                .keys()
                .filter(|key| (scope.is_floor)(key) && !targets.contains(&scope.parent.child(key)))
                .cloned()
                .collect();
            for key in doomed {
                map.shift_remove(&key);
            }
        }

        for key_path in &self.remove {
            if targets.contains(key_path) {
                continue;
            }
            let (parent, key) = split(key_path);
            if let Some(map) = object_at_mut(path, doc, parent)? {
                map.shift_remove(key);
            }
        }

        for (key_path, value) in &self.remove_if {
            if targets.contains(key_path) || value_at(doc, key_path) != Some(value) {
                continue;
            }
            let (parent, key) = split(key_path);
            if let Some(map) = object_at_mut(path, doc, parent)? {
                map.shift_remove(key);
            }
        }

        for (key_path, value) in &self.set {
            let (parent, key) = split(key_path);
            let map = ensure_object_mut(path, doc, parent)?;
            match map.get_mut(key) {
                Some(slot) => *slot = value.clone(),
                None => {
                    map.insert(key.clone(), value.clone());
                }
            }
        }

        for key_path in had_content {
            let emptied = value_at(doc, key_path)
                .and_then(Value::as_object)
                .is_some_and(Map::is_empty);
            if emptied {
                let (parent, key) = split(key_path);
                if let Some(map) = object_at_mut(path, doc, parent)? {
                    map.shift_remove(key);
                }
            }
        }

        Ok(())
    }
}

impl LivePatch for JsonPatch {
    fn apply(&self, path: &Path, pre: Option<&[u8]>) -> Result<Vec<u8>, PatchError> {
        rewrite(path, pre, self.json5, |doc| self.apply_to(path, doc))
    }
}

/// 解析写前内容，交给 `edit` 改，再写回。允许 JSON5 的文件保留注释和排版，见模块说明。
pub fn rewrite(
    path: &Path,
    pre: Option<&[u8]>,
    json5: bool,
    edit: impl FnOnce(&mut Value) -> Result<(), PatchError>,
) -> Result<Vec<u8>, PatchError> {
    let (mut doc, style) = parse(path, pre, json5)?;
    let before = doc.clone();
    edit(&mut doc)?;

    // parse 已经确认过是 UTF-8。空文件没有可保留的东西。
    let source = pre
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .map(|text| text.strip_prefix('\u{feff}').unwrap_or(text))
        .filter(|text| !text.trim().is_empty());
    if let Some(source) = source.filter(|_| json5) {
        let edited = jsonc::rewrite(source, &before, &doc, &style.indent)
            .filter(|out| parse_text(path, out, true).is_ok_and(|reparsed| reparsed == doc));
        if let Some(out) = edited {
            let bom = if style.bom { "\u{feff}" } else { "" };
            return Ok(format!("{bom}{out}").into_bytes());
        }
    }
    Ok(serialize(&doc, &style))
}

/// 从 `old` 改成 `new` 的补丁，规则见 [`super::delta`]。两个文档的顶层都是对象。
pub fn delta(old: &Value, new: &Value, json5: bool) -> JsonPatch {
    let mut patch = JsonPatch {
        json5,
        ..JsonPatch::default()
    };
    diff(&KeyPath::root(), old, new, &mut patch);
    patch
}

fn diff(at: &KeyPath, old: &Value, new: &Value, patch: &mut JsonPatch) {
    match (old, new) {
        (Value::Object(old), Value::Object(new)) => {
            for key in old.keys().filter(|key| !new.contains_key(*key)) {
                patch.remove.push(at.child(key));
            }
            for (key, value) in new {
                match old.get(key) {
                    Some(before) => diff(&at.child(key), before, value, patch),
                    None => added(&at.child(key), value, patch),
                }
            }
        }
        _ if old == new => {}
        _ => patch.set.push((at.clone(), new.clone())),
    }
}

fn added(at: &KeyPath, value: &Value, patch: &mut JsonPatch) {
    match value {
        Value::Object(map) if !map.is_empty() => {
            for (key, child) in map {
                added(&at.child(key), child, patch);
            }
        }
        _ => patch.set.push((at.clone(), value.clone())),
    }
}

/// 文档里 `path` 处的值。
pub fn value_at<'a>(doc: &'a Value, path: &KeyPath) -> Option<&'a Value> {
    path.0
        .iter()
        .try_fold(doc, |current, segment| current.get(segment))
}

/// 文件原有的排版习惯，重新序列化时沿用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonStyle {
    indent: Vec<u8>,
    trailing_newline: bool,
    crlf: bool,
    bom: bool,
}

impl Default for JsonStyle {
    fn default() -> Self {
        Self {
            indent: b"  ".to_vec(),
            trailing_newline: true,
            crlf: false,
            bom: false,
        }
    }
}

/// 解析写前内容。文件不存在或只有空白时视为空对象（这是唯一允许的「空底」）；
/// 顶层不是对象时报错，不覆盖。
pub fn parse(
    path: &Path,
    pre: Option<&[u8]>,
    json5: bool,
) -> Result<(Value, JsonStyle), PatchError> {
    let Some(bytes) = pre else {
        return Ok((Value::Object(Map::new()), JsonStyle::default()));
    };
    let text = decode_utf8(path, bytes)?;
    let (bom, text) = match text.strip_prefix('\u{feff}') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    if text.trim().is_empty() {
        return Ok((
            Value::Object(Map::new()),
            JsonStyle {
                bom,
                ..JsonStyle::default()
            },
        ));
    }

    let doc = parse_text(path, text, json5)?;
    if !doc.is_object() {
        return Err(PatchError::Shape {
            path: path.to_path_buf(),
            key_path: KeyPath::root(),
            expected: "对象",
        });
    }

    let style = JsonStyle {
        indent: detect_indent(text).unwrap_or_else(|| b"  ".to_vec()),
        trailing_newline: text.ends_with('\n'),
        crlf: text.contains("\r\n"),
        bom,
    };
    Ok((doc, style))
}

/// 先按标准 JSON 解析；允许 JSON5 时失败了再按 JSON5 解析，报错也报 JSON5 的位置。
fn parse_text(path: &Path, text: &str, json5: bool) -> Result<Value, PatchError> {
    let strict = match serde_json::from_str(text) {
        Ok(doc) => return Ok(doc),
        Err(err) => err,
    };
    if !json5 {
        return Err(PatchError::Parse {
            path: path.to_path_buf(),
            line: strict.line(),
            column: strict.column(),
            message: strict.to_string(),
        });
    }
    json5::from_str(text).map_err(|err| {
        // json5 的行列从 0 开始。
        let (line, column) = err.position().map_or((0, 0), |at| (at.line, at.column));
        PatchError::Parse {
            path: path.to_path_buf(),
            line: line + 1,
            column: column + 1,
            message: err.to_string(),
        }
    })
}

pub fn serialize(doc: &Value, style: &JsonStyle) -> Vec<u8> {
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(&style.indent);
    let mut serializer = serde_json::Serializer::with_formatter(&mut out, formatter);
    doc.serialize(&mut serializer)
        .expect("a JSON value always serializes into memory");
    if style.trailing_newline {
        out.push(b'\n');
    }
    if style.crlf {
        let mut converted = Vec::with_capacity(out.len() + out.len() / 16);
        for byte in out {
            if byte == b'\n' {
                converted.push(b'\r');
            }
            converted.push(byte);
        }
        out = converted;
    }
    if style.bom {
        let mut with_bom = "\u{feff}".as_bytes().to_vec();
        with_bom.extend_from_slice(&out);
        out = with_bom;
    }
    out
}

/// 第一行缩进过的行，它的前导空白就是一级缩进（serde 的美化输出和各 agent 自己
/// 写的文件都是逐级缩进）。空格和 Tab 混用时不认，退回两个空格。
fn detect_indent(text: &str) -> Option<Vec<u8>> {
    text.lines().skip(1).find_map(|line| {
        let content = line.trim_start();
        if content.is_empty() {
            return None;
        }
        let leading = &line[..line.len() - content.len()];
        let uniform = leading.bytes().all(|b| b == b' ') || leading.bytes().all(|b| b == b'\t');
        (!leading.is_empty() && uniform).then(|| leading.as_bytes().to_vec())
    })
}

fn split(key_path: &KeyPath) -> (&[String], &String) {
    key_path
        .split_last()
        .expect("patch paths must name a key, not the document root")
}

fn shape_error(path: &Path, segments: &[String]) -> PatchError {
    PatchError::Shape {
        path: path.to_path_buf(),
        key_path: KeyPath(segments.to_vec()),
        expected: "对象",
    }
}

/// 沿路径找到对象；中途缺失返回 `None`，中途不是对象报错。
fn object_at_mut<'a>(
    path: &Path,
    doc: &'a mut Value,
    segments: &[String],
) -> Result<Option<&'a mut Map<String, Value>>, PatchError> {
    let mut current = doc;
    for (depth, segment) in segments.iter().enumerate() {
        let map = current
            .as_object_mut()
            .ok_or_else(|| shape_error(path, &segments[..depth]))?;
        match map.get_mut(segment) {
            Some(next) => current = next,
            None => return Ok(None),
        }
    }
    current
        .as_object_mut()
        .map(Some)
        .ok_or_else(|| shape_error(path, segments))
}

/// 沿路径找到对象，缺失的层级追加到所在对象末尾。
fn ensure_object_mut<'a>(
    path: &Path,
    doc: &'a mut Value,
    segments: &[String],
) -> Result<&'a mut Map<String, Value>, PatchError> {
    let mut current = doc;
    for (depth, segment) in segments.iter().enumerate() {
        let map = current
            .as_object_mut()
            .ok_or_else(|| shape_error(path, &segments[..depth]))?;
        current = map
            .entry(segment.clone())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    current
        .as_object_mut()
        .ok_or_else(|| shape_error(path, segments))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn apply(patch: &JsonPatch, pre: &str) -> String {
        let out = patch
            .apply(Path::new("settings.json"), Some(pre.as_bytes()))
            .expect("apply");
        String::from_utf8(out).expect("utf8")
    }

    fn is_model(key: &str) -> bool {
        key == "model"
    }

    fn is_anthropic(key: &str) -> bool {
        key.starts_with("ANTHROPIC_")
    }

    fn floor_patch(env: &[(&str, Value)]) -> JsonPatch {
        JsonPatch {
            clear: vec![
                ClearScope {
                    parent: KeyPath::root(),
                    is_floor: is_model,
                },
                ClearScope {
                    parent: KeyPath::new(&["env"]),
                    is_floor: is_anthropic,
                },
            ],
            set: env
                .iter()
                .map(|(key, value)| (KeyPath::new(&["env", key]), value.clone()))
                .collect(),
            ..JsonPatch::default()
        }
    }

    const LIVE: &str = r#"{
  "env": {
    "ANTHROPIC_BASE_URL": "https://a.example",
    "CLAUDE_CODE_USE_POWERSHELL_TOOL": "1",
    "ANTHROPIC_AUTH_TOKEN": "sk-a",
    "ANTHROPIC_MODEL": "a-model"
  },
  "model": "a-model",
  "hooks": {
    "Stop": []
  }
}"#;

    #[test]
    fn floor_keys_are_replaced_in_place_and_the_rest_keeps_its_order() {
        let out = apply(
            &floor_patch(&[
                ("ANTHROPIC_AUTH_TOKEN", json!("sk-b")),
                ("ANTHROPIC_BASE_URL", json!("https://b.example")),
            ]),
            LIVE,
        );
        assert_eq!(
            out,
            r#"{
  "env": {
    "ANTHROPIC_BASE_URL": "https://b.example",
    "CLAUDE_CODE_USE_POWERSHELL_TOOL": "1",
    "ANTHROPIC_AUTH_TOKEN": "sk-b"
  },
  "hooks": {
    "Stop": []
  }
}"#
        );
    }

    #[test]
    fn removing_a_key_does_not_move_the_last_key_into_its_place() {
        let pre = "{\n  \"a\": 1,\n  \"model\": \"x\",\n  \"b\": 2,\n  \"c\": 3\n}";
        let out = apply(&floor_patch(&[]), pre);
        assert_eq!(out, "{\n  \"a\": 1,\n  \"b\": 2,\n  \"c\": 3\n}");
    }

    #[test]
    fn new_keys_go_to_the_end_of_their_parent_and_missing_parents_are_created() {
        let pre = "{\n  \"hooks\": {}\n}";
        let out = apply(&floor_patch(&[("ANTHROPIC_BASE_URL", json!("u"))]), pre);
        assert_eq!(
            out,
            "{\n  \"hooks\": {},\n  \"env\": {\n    \"ANTHROPIC_BASE_URL\": \"u\"\n  }\n}"
        );
    }

    #[test]
    fn remove_drops_named_paths_but_keeps_their_siblings() {
        let patch = JsonPatch {
            remove: vec![
                KeyPath::new(&["providers", "a"]),
                KeyPath::new(&["gone", "x"]),
            ],
            ..JsonPatch::default()
        };
        let out = apply(&patch, r#"{"providers": {"a": {}, "b": {}}}"#);
        let value: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value, json!({"providers": {"b": {}}}));
    }

    #[test]
    fn objects_emptied_by_the_patch_are_dropped_but_empty_ones_stay() {
        let patch = JsonPatch {
            drop_if_emptied: vec![KeyPath::new(&["env"])],
            ..floor_patch(&[])
        };
        let out = apply(&patch, r#"{"env": {"ANTHROPIC_MODEL": "x"}, "a": 1}"#);
        assert_eq!(
            serde_json::from_str::<Value>(&out).unwrap(),
            json!({"a": 1})
        );

        let out = apply(&patch, r#"{"env": {}, "a": 1}"#);
        assert_eq!(
            serde_json::from_str::<Value>(&out).unwrap(),
            json!({"env": {}, "a": 1})
        );

        let out = apply(&patch, r#"{"env": {"ANTHROPIC_MODEL": "x", "DEBUG": "1"}}"#);
        assert_eq!(
            serde_json::from_str::<Value>(&out).unwrap(),
            json!({"env": {"DEBUG": "1"}})
        );
    }

    #[test]
    fn second_write_is_byte_stable() {
        let patch = floor_patch(&[("ANTHROPIC_AUTH_TOKEN", json!("sk-b"))]);
        let once = apply(&patch, LIVE);
        assert_eq!(apply(&patch, &once), once);
    }

    #[test]
    fn keeps_indent_trailing_newline_line_endings_and_bom() {
        let pre =
            "{\r\n    \"a\": {\r\n        \"model\": 1\r\n    },\r\n    \"model\": \"x\"\r\n}\r\n";
        let out = apply(&floor_patch(&[]), pre);
        assert_eq!(
            out,
            "{\r\n    \"a\": {\r\n        \"model\": 1\r\n    }\r\n}\r\n"
        );

        let tabbed = "{\n\t\"a\": 1\n}\n";
        assert_eq!(apply(&JsonPatch::default(), tabbed), tabbed);

        let bom = "\u{feff}{\n  \"a\": 1\n}";
        assert_eq!(apply(&JsonPatch::default(), bom), bom);
    }

    #[test]
    fn absent_or_blank_files_start_from_an_empty_object() {
        let patch = floor_patch(&[("ANTHROPIC_BASE_URL", json!("u"))]);
        for pre in [None, Some(&b"  \n"[..])] {
            let out = patch.apply(Path::new("s.json"), pre).expect("apply");
            let value: Value = serde_json::from_slice(&out).unwrap();
            assert_eq!(value, json!({"env": {"ANTHROPIC_BASE_URL": "u"}}));
            assert!(out.ends_with(b"}\n"), "new files end with a newline");
        }
    }

    #[test]
    fn broken_files_are_refused_with_a_location() {
        let err = JsonPatch::default()
            .apply(Path::new("s.json"), Some(b"{\n  \"a\": 1,\n  oops\n}"))
            .expect_err("must refuse");
        assert!(matches!(err, PatchError::Parse { line: 3, .. }), "{err:?}");

        let err = JsonPatch::default()
            .apply(Path::new("s.json"), Some(b"[]"))
            .expect_err("must refuse");
        assert!(matches!(err, PatchError::Shape { .. }), "{err:?}");
    }

    #[test]
    fn a_non_object_parent_is_refused_instead_of_overwritten() {
        let patch = floor_patch(&[("ANTHROPIC_BASE_URL", json!("u"))]);
        let err = patch
            .apply(Path::new("s.json"), Some(br#"{"env": "oops"}"#))
            .expect_err("must refuse");
        match err {
            PatchError::Shape { key_path, .. } => assert_eq!(key_path, KeyPath::new(&["env"])),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn json5_files_are_read_only_when_allowed_and_keep_their_comments() {
        let pre = "{\n  // 本地模型\n  providers: {\n    ollama: { baseUrl: 'http://localhost:11434/v1', },\n  },\n}\n";
        let strict = JsonPatch::default().apply(Path::new("models.json"), Some(pre.as_bytes()));
        assert!(
            matches!(strict, Err(PatchError::Parse { line: 2, .. })),
            "{strict:?}"
        );

        let patch = JsonPatch {
            json5: true,
            set: vec![(
                KeyPath::new(&["providers", "x", "api"]),
                json!("anthropic-messages"),
            )],
            ..JsonPatch::default()
        };
        assert_eq!(
            apply(&patch, pre),
            "{\n  // 本地模型\n  providers: {\n    ollama: { baseUrl: 'http://localhost:11434/v1', },\n    \"x\": {\n      \"api\": \"anthropic-messages\"\n    },\n  },\n}\n"
        );
    }

    #[test]
    fn json5_files_fall_back_to_reserializing_when_comments_cannot_be_kept() {
        // 同一个键出现两次又要改它：不知道改哪一个，整个重新序列化，注释丢了但内容是对的。
        let patch = JsonPatch {
            json5: true,
            set: vec![(KeyPath::new(&["a"]), json!(3))],
            ..JsonPatch::default()
        };
        assert_eq!(
            apply(&patch, "{\n  // 说明\n  \"a\": 1,\n  \"a\": 2\n}\n"),
            "{\n  \"a\": 3\n}\n"
        );
        // BOM 照留。
        let out = apply(&patch, "\u{feff}{\n  // 说明\n  \"a\": 1\n}\n");
        assert_eq!(out, "\u{feff}{\n  // 说明\n  \"a\": 3\n}\n");
    }

    #[test]
    fn a_delta_touches_only_what_changed_between_two_renderings() {
        let old = json!({"env": {
            "ANTHROPIC_BASE_URL": "https://a.example",
            "ANTHROPIC_AUTH_TOKEN": "sk-a",
            "ANTHROPIC_MODEL": "m",
            "ANTHROPIC_CUSTOM_HEADERS": "x: 1"
        }});
        let new = json!({"env": {
            "ANTHROPIC_BASE_URL": "https://a.example",
            "ANTHROPIC_AUTH_TOKEN": "sk-b",
            "ANTHROPIC_MODEL": "m",
            "ANTHROPIC_SMALL_FAST_MODEL": "s"
        }, "headers": {"X-Team": "infra"}});
        let patch = delta(&old, &new, false);
        assert_eq!(
            patch.remove,
            [KeyPath::new(&["env", "ANTHROPIC_CUSTOM_HEADERS"])]
        );
        assert_eq!(
            patch.set,
            [
                (
                    KeyPath::new(&["env", "ANTHROPIC_AUTH_TOKEN"]),
                    json!("sk-b")
                ),
                (
                    KeyPath::new(&["env", "ANTHROPIC_SMALL_FAST_MODEL"]),
                    json!("s")
                ),
                (KeyPath::new(&["headers", "X-Team"]), json!("infra")),
            ]
        );

        // 文件里的其他键，和这家生成过、但这次没变的键（用户自己改过的模型名），都不动。
        let live = r#"{
  "model": "picked-in-claude",
  "env": {
    "ANTHROPIC_BASE_URL": "https://a.example",
    "ANTHROPIC_AUTH_TOKEN": "sk-a",
    "ANTHROPIC_MODEL": "edited-by-hand",
    "ANTHROPIC_CUSTOM_HEADERS": "x: 1"
  },
  "headers": {"Mine": "1"}
}"#;
        let out: Value = serde_json::from_str(&apply(&patch, live)).unwrap();
        assert_eq!(
            out,
            json!({
                "model": "picked-in-claude",
                "env": {
                    "ANTHROPIC_BASE_URL": "https://a.example",
                    "ANTHROPIC_AUTH_TOKEN": "sk-b",
                    "ANTHROPIC_MODEL": "edited-by-hand",
                    "ANTHROPIC_SMALL_FAST_MODEL": "s"
                },
                "headers": {"Mine": "1", "X-Team": "infra"}
            })
        );
    }

    #[test]
    fn a_delta_replaces_arrays_whole_and_drops_removed_objects_whole() {
        let old = json!({"p": {"models": [{"id": "a"}, {"id": "b"}], "headers": {"X": "1"}}});
        let new = json!({"p": {"models": [{"id": "a"}]}});
        let patch = delta(&old, &new, true);
        assert!(patch.json5);
        assert_eq!(patch.remove, [KeyPath::new(&["p", "headers"])]);
        assert_eq!(
            patch.set,
            [(KeyPath::new(&["p", "models"]), json!([{"id": "a"}]))]
        );
        assert!(delta(&old, &old, false).set.is_empty());
    }

    #[test]
    fn json5_errors_count_lines_from_one() {
        let patch = JsonPatch {
            json5: true,
            ..JsonPatch::default()
        };
        let err = patch
            .apply(Path::new("models.json"), Some(b"{\n  a: 1,\n  b: ]\n}"))
            .expect_err("must refuse");
        assert!(matches!(err, PatchError::Parse { line: 3, .. }), "{err:?}");
    }
}
