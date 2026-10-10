//! 保留注释和排版的 JSONC / JSON5 改写：只动变了的值，其余字节原样留下。
//!
//! 移植自 ccs-lite（BlueOcean223/cc-switch）的 `jsonc_document.rs`。解析用 `json-five` 的 round-trip 语法树：
//! 每个节点带着它前后的空白和注释，原样输出就是原文。改写时拿改前、改后两个值逐层比：
//! - 对象：删掉的键连同它的逗号一起删，它前后的注释留给相邻的键；两边都有的键往下比；
//!   新增的键追加到末尾，缩进照兄弟键。
//! - 数组：按位置往下比，多的从末尾删，少的追加到末尾。
//! - 其余的值整个换掉。
//!
//! 新写的值按文件的缩进单位和换行符排版。往单行的对象（数组）里追加多行的值时，先把它展开成一项一行，
//! 否则会挤成 `{"a": 1, "b": {` 这样。
//!
//! 做不到时返回 `None`，由调用方退回整个重新序列化：json-five 解析不了 json5 认的写法、
//! 同一个键出现两次又要改它。

use std::collections::HashSet;

use json_five::rt::parser::{
    ArrayValueContext, JSONArrayContext, JSONArrayValue, JSONKeyValuePair, JSONObjectContext,
    JSONText, JSONValue, KeyValuePairContext,
};
use json_five::tokenize::{TokType, tokenize_rt_str};
use serde::Serialize;
use serde_json::{Map, Value};

/// 把 `source`（解析出来是 `before`）改成 `after`。`unit` 是一级缩进。
pub(super) fn rewrite(source: &str, before: &Value, after: &Value, unit: &[u8]) -> Option<String> {
    if before == after {
        return Some(source.to_string());
    }
    let mut text = parse(source)?;
    let layout = Layout {
        unit: std::str::from_utf8(unit).ok()?.to_string(),
        newline: if source.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        },
    };
    merge(&mut text.value, before, after, "", &layout)?;
    Some(text.to_string())
}

struct Layout {
    unit: String,
    newline: &'static str,
}

fn parse(source: &str) -> Option<JSONText> {
    let mut tokens = tokenize_rt_str(source).ok()?;
    // json-five 0.3.1 的两处边界错误（ccs-lite 发现的）：块注释吃掉了结尾的 `/`，但结束位置还停在它上面；
    // 行注释在文件末尾或 U+2028、U+2029 处结束时，结束位置可能落在最后一个字符的 UTF-8 字节中间。
    // 只修这两种，其余位置不动。
    for (start, kind, end) in &mut tokens.tok_spans {
        if *kind == TokType::BlockComment
            && source.as_bytes().get(*end) == Some(&b'/')
            && source
                .get(*start..*end)
                .is_some_and(|text| text.ends_with('*'))
        {
            *end += 1;
        }
        if *kind == TokType::LineComment {
            while *end < source.len() && !source.is_char_boundary(*end) {
                *end += 1;
            }
        }
    }
    json_five::rt::parser::from_tokens(&tokens).ok()
}

/// 语法树里的键保留着原文的转义，交给 json5 解码（双引号、单引号、不加引号三种写法都有）。
fn key_name(key: &JSONValue) -> Option<String> {
    let object: Map<String, Value> = json5::from_str(&format!("{{{key}:null}}")).ok()?;
    object.into_iter().next().map(|(key, _)| key)
}

fn merge(
    node: &mut JSONValue,
    before: &Value,
    after: &Value,
    indent: &str,
    layout: &Layout,
) -> Option<()> {
    if before == after {
        return Some(());
    }
    match (node, before, after) {
        (
            JSONValue::JSONObject {
                key_value_pairs: pairs,
                context,
            },
            Value::Object(before),
            Value::Object(after),
        ) => {
            let (_, child) = object_layout(pairs, context, indent, layout);
            let mut seen = HashSet::new();
            let mut index = 0;
            while index < pairs.len() {
                let key = key_name(&pairs[index].key)?;
                let Some(wanted) = after.get(&key) else {
                    remove_pair(pairs, context, index);
                    continue;
                };
                if !seen.insert(key.clone()) {
                    // 同一个键又出现一次：值没变就留着，变了不知道改哪一个。
                    if before.get(&key) == Some(wanted) {
                        index += 1;
                        continue;
                    }
                    return None;
                }
                let current = before.get(&key).unwrap_or(&Value::Null);
                merge(&mut pairs[index].value, current, wanted, &child, layout)?;
                index += 1;
            }
            for (key, wanted) in after {
                if !seen.contains(key) {
                    append_pair(pairs, context, key, wanted, indent, layout)?;
                }
            }
            Some(())
        }
        (JSONValue::JSONArray { values, context }, Value::Array(before), Value::Array(after)) => {
            let (_, child) = array_layout(values, context, indent, layout);
            let common = values.len().min(before.len()).min(after.len());
            for index in 0..common {
                merge(
                    &mut values[index].value,
                    &before[index],
                    &after[index],
                    &child,
                    layout,
                )?;
            }
            while values.len() > after.len() {
                remove_value(values, context, values.len() - 1);
            }
            while values.len() < after.len() {
                let next = &after[values.len()];
                append_value(values, context, next, indent, layout)?;
            }
            Some(())
        }
        (node, _, after) => {
            *node = render(after, indent, layout)?;
            Some(())
        }
    }
}

/// 按文件的缩进单位排版一个新值；第二行起每行前面加上 `indent`。
fn render(value: &Value, indent: &str, layout: &Layout) -> Option<JSONValue> {
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(layout.unit.as_bytes());
    value
        .serialize(&mut serde_json::Serializer::with_formatter(
            &mut out, formatter,
        ))
        .ok()?;
    let pretty = String::from_utf8(out).ok()?;
    // JSON 字符串里的换行都是转义过的，按行拆不会拆进字符串里。
    let mut lines = pretty.split('\n');
    let mut text = lines.next().unwrap_or_default().to_string();
    for line in lines {
        text.push_str(layout.newline);
        text.push_str(indent);
        text.push_str(line);
    }
    Some(parse(&text)?.value)
}

/// 纯空白（没有注释）。
fn blank(whitespace: &str) -> bool {
    whitespace.chars().all(char::is_whitespace)
}

/// 空白里最后一个换行之后的缩进。
fn trailing_indent(whitespace: &str) -> Option<String> {
    let (_, tail) = whitespace.rsplit_once('\n')?;
    Some(
        tail.chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect(),
    )
}

/// （是不是多行，子项的缩进）。子项缩进取第一个子项所在行的缩进，取不到就是外层缩进加一级。
fn object_layout(
    pairs: &[JSONKeyValuePair],
    context: &Option<JSONObjectContext>,
    indent: &str,
    layout: &Layout,
) -> (bool, String) {
    let mut multiline = false;
    let mut child = None;
    if let Some(context) = context
        && context.wsc.0.contains('\n')
    {
        multiline = true;
        // 空的时候 `{` 后面的空白是括号前面的，缩进是外层的。
        if !pairs.is_empty() {
            child = trailing_indent(&context.wsc.0);
        }
    }
    for (index, pair) in pairs.iter().enumerate() {
        if let Some(context) = &pair.context {
            let (key_colon, colon_value, after_value, after_comma) = &context.wsc;
            multiline |= key_colon.contains('\n')
                || colon_value.contains('\n')
                || after_value.contains('\n')
                || after_comma.as_ref().is_some_and(|ws| ws.contains('\n'));
            // 最后一项逗号后面的空白在括号前面，缩进是外层的。
            if child.is_none() && index + 1 < pairs.len() {
                child = after_comma.as_deref().and_then(trailing_indent);
            }
        }
    }
    let child = child
        .filter(|child| !child.is_empty())
        .unwrap_or_else(|| format!("{indent}{}", layout.unit));
    (multiline, child)
}

fn array_layout(
    values: &[JSONArrayValue],
    context: &Option<JSONArrayContext>,
    indent: &str,
    layout: &Layout,
) -> (bool, String) {
    let mut multiline = false;
    let mut child = None;
    if let Some(context) = context
        && context.wsc.0.contains('\n')
    {
        multiline = true;
        // 空的时候 `[` 后面的空白是括号前面的，缩进是外层的。
        if !values.is_empty() {
            child = trailing_indent(&context.wsc.0);
        }
    }
    for (index, value) in values.iter().enumerate() {
        if let Some(context) = &value.context {
            let (after_value, after_comma) = &context.wsc;
            multiline |= after_value.contains('\n')
                || after_comma.as_ref().is_some_and(|ws| ws.contains('\n'));
            if child.is_none() && index + 1 < values.len() {
                child = after_comma.as_deref().and_then(trailing_indent);
            }
        }
    }
    let child = child
        .filter(|child| !child.is_empty())
        .unwrap_or_else(|| format!("{indent}{}", layout.unit));
    (multiline, child)
}

/// 单行时新项和前一项之间的空白：照抄已有的逗号后的空白，没有就是一个空格。
fn inline_separator<'a>(mut after_commas: impl Iterator<Item = Option<&'a String>>) -> String {
    after_commas
        .find_map(|ws| ws.filter(|ws| blank(ws)).cloned())
        .unwrap_or_else(|| " ".to_string())
}

fn object_context(context: &mut Option<JSONObjectContext>) -> &mut JSONObjectContext {
    context.get_or_insert_with(|| JSONObjectContext {
        wsc: (String::new(),),
    })
}

fn pair_context(pair: &mut JSONKeyValuePair) -> &mut KeyValuePairContext {
    pair.context.get_or_insert_with(|| KeyValuePairContext {
        wsc: (String::new(), String::new(), String::new(), None),
    })
}

fn array_context(context: &mut Option<JSONArrayContext>) -> &mut JSONArrayContext {
    context.get_or_insert_with(|| JSONArrayContext {
        wsc: (String::new(),),
    })
}

fn value_context(value: &mut JSONArrayValue) -> &mut ArrayValueContext {
    value.context.get_or_insert_with(|| ArrayValueContext {
        wsc: (String::new(), None),
    })
}

/// 把单行的对象展开成一个键一行。只改纯空白的间隔，带注释的不动。
fn expand_object(
    pairs: &mut [JSONKeyValuePair],
    context: &mut Option<JSONObjectContext>,
    indent: &str,
    child: &str,
    layout: &Layout,
) {
    let next = format!("{}{child}", layout.newline);
    let close = format!("{}{indent}", layout.newline);
    let opening = &mut object_context(context).wsc.0;
    if blank(opening) {
        *opening = if pairs.is_empty() { &close } else { &next }.clone();
    }
    let last = pairs.len().saturating_sub(1);
    for (index, pair) in pairs.iter_mut().enumerate() {
        let (_, _, after_value, after_comma) = &mut pair_context(pair).wsc;
        let gap = if index == last { &close } else { &next };
        match after_comma {
            Some(after_comma) => {
                if blank(after_value) {
                    after_value.clear();
                }
                if blank(after_comma) {
                    *after_comma = gap.clone();
                }
            }
            // 没有逗号的只能是最后一项，它后面的空白就是 `}` 前面的。
            None if blank(after_value) => *after_value = gap.clone(),
            None => {}
        }
    }
}

fn expand_array(
    values: &mut [JSONArrayValue],
    context: &mut Option<JSONArrayContext>,
    indent: &str,
    child: &str,
    layout: &Layout,
) {
    let next = format!("{}{child}", layout.newline);
    let close = format!("{}{indent}", layout.newline);
    let opening = &mut array_context(context).wsc.0;
    if blank(opening) {
        *opening = if values.is_empty() { &close } else { &next }.clone();
    }
    let last = values.len().saturating_sub(1);
    for (index, value) in values.iter_mut().enumerate() {
        let (after_value, after_comma) = &mut value_context(value).wsc;
        let gap = if index == last { &close } else { &next };
        match after_comma {
            Some(after_comma) => {
                if blank(after_value) {
                    after_value.clear();
                }
                if blank(after_comma) {
                    *after_comma = gap.clone();
                }
            }
            None if blank(after_value) => *after_value = gap.clone(),
            None => {}
        }
    }
}

/// 删掉一项后，把它前后两段空白拼成一段。前一段是纯空白就只留后一段，后一段决定下一项或括号的位置；
/// 前一段有注释就留着，去掉它末尾的空白再接后一段。后一段不以换行开头时原样相接，免得行注释吞掉后面的内容。
fn join(left: &str, right: &str) -> String {
    if blank(left) {
        right.to_string()
    } else if right.starts_with('\n') || right.starts_with("\r\n") {
        format!("{}{right}", left.trim_end())
    } else {
        format!("{left}{right}")
    }
}

fn remove_pair(
    pairs: &mut Vec<JSONKeyValuePair>,
    context: &mut Option<JSONObjectContext>,
    index: usize,
) {
    let removed = pairs.remove(index);
    let Some(removed) = removed.context else {
        return;
    };
    let (_, _, after_value, after_comma) = removed.wsc;
    // 被删项后面那一段：有逗号是逗号后面的，没有是它和 `}` 之间的。项里面的注释跟着项一起删。
    let has_comma = after_comma.is_some();
    let right = after_comma.unwrap_or(after_value);

    if index == 0 {
        let opening = &mut object_context(context).wsc.0;
        let joined = join(opening, &right);
        *opening = if pairs.is_empty() && blank(&joined) {
            String::new()
        } else {
            joined
        };
        return;
    }
    let keeps_comma = index < pairs.len() || has_comma;
    let previous = pair_context(&mut pairs[index - 1]);
    let gap = previous.wsc.3.take().unwrap_or_default();
    if keeps_comma {
        previous.wsc.3 = Some(join(&gap, &right));
    } else {
        // 删的是没有尾逗号的最后一项：前一项的逗号也去掉。
        let before_comma = std::mem::take(&mut previous.wsc.2);
        previous.wsc.2 = join(&format!("{before_comma}{gap}"), &right);
    }
}

fn remove_value(
    values: &mut Vec<JSONArrayValue>,
    context: &mut Option<JSONArrayContext>,
    index: usize,
) {
    let removed = values.remove(index);
    let Some(removed) = removed.context else {
        return;
    };
    let (after_value, after_comma) = removed.wsc;
    let has_comma = after_comma.is_some();
    let right = after_comma.unwrap_or(after_value);

    if index == 0 {
        let opening = &mut array_context(context).wsc.0;
        let joined = join(opening, &right);
        *opening = if values.is_empty() && blank(&joined) {
            String::new()
        } else {
            joined
        };
        return;
    }
    let keeps_comma = index < values.len() || has_comma;
    let previous = value_context(&mut values[index - 1]);
    let gap = previous.wsc.1.take().unwrap_or_default();
    if keeps_comma {
        previous.wsc.1 = Some(join(&gap, &right));
    } else {
        let before_comma = std::mem::take(&mut previous.wsc.0);
        previous.wsc.0 = join(&format!("{before_comma}{gap}"), &right);
    }
}

fn append_pair(
    pairs: &mut Vec<JSONKeyValuePair>,
    context: &mut Option<JSONObjectContext>,
    key: &str,
    value: &Value,
    indent: &str,
    layout: &Layout,
) -> Option<()> {
    let (mut multiline, child) = object_layout(pairs, context, indent, layout);
    let value = render(value, &child, layout)?;
    if !multiline && value.to_string().contains('\n') {
        expand_object(pairs, context, indent, &child, layout);
        multiline = true;
    }
    let separator = if multiline {
        format!("{}{child}", layout.newline)
    } else {
        inline_separator(pairs.iter().map(|pair| {
            pair.context
                .as_ref()
                .and_then(|context| context.wsc.3.as_ref())
        }))
    };
    let mut pair = JSONKeyValuePair {
        key: render(&Value::String(key.to_string()), "", layout)?,
        value,
        context: Some(KeyValuePairContext {
            wsc: (String::new(), " ".to_string(), String::new(), None),
        }),
    };

    if let Some(previous) = pairs.last_mut() {
        // 前一项后面的空白（`}` 前面的那段）挪给新项，前一项改成「逗号 + 换行缩进」。
        let previous = pair_context(previous);
        let trailing_comma = previous.wsc.3.is_some();
        let closing = previous
            .wsc
            .3
            .take()
            .unwrap_or_else(|| std::mem::take(&mut previous.wsc.2));
        previous.wsc.3 = Some(separator);
        if trailing_comma {
            pair_context(&mut pair).wsc.3 = Some(closing);
        } else {
            pair_context(&mut pair).wsc.2 = closing;
        }
    } else {
        let opening = &mut object_context(context).wsc.0;
        let closing = std::mem::replace(opening, if multiline { separator } else { String::new() });
        pair_context(&mut pair).wsc.2 = closing;
    }
    pairs.push(pair);
    Some(())
}

fn append_value(
    values: &mut Vec<JSONArrayValue>,
    context: &mut Option<JSONArrayContext>,
    value: &Value,
    indent: &str,
    layout: &Layout,
) -> Option<()> {
    let (mut multiline, child) = array_layout(values, context, indent, layout);
    let value = render(value, &child, layout)?;
    if !multiline && value.to_string().contains('\n') {
        expand_array(values, context, indent, &child, layout);
        multiline = true;
    }
    let separator = if multiline {
        format!("{}{child}", layout.newline)
    } else {
        inline_separator(values.iter().map(|value| {
            value
                .context
                .as_ref()
                .and_then(|context| context.wsc.1.as_ref())
        }))
    };
    let mut item = JSONArrayValue {
        value,
        context: Some(ArrayValueContext {
            wsc: (String::new(), None),
        }),
    };

    if let Some(previous) = values.last_mut() {
        let previous = value_context(previous);
        let trailing_comma = previous.wsc.1.is_some();
        let closing = previous
            .wsc
            .1
            .take()
            .unwrap_or_else(|| std::mem::take(&mut previous.wsc.0));
        previous.wsc.1 = Some(separator);
        if trailing_comma {
            value_context(&mut item).wsc.1 = Some(closing);
        } else {
            value_context(&mut item).wsc.0 = closing;
        }
    } else {
        let opening = &mut array_context(context).wsc.0;
        let closing = std::mem::replace(opening, if multiline { separator } else { String::new() });
        value_context(&mut item).wsc.0 = closing;
    }
    values.push(item);
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 改写并确认结果解析出来就是目标值。
    fn edit_with(source: &str, after: &Value, unit: &[u8]) -> String {
        let before: Value = json5::from_str(source).unwrap();
        let out = rewrite(source, &before, after, unit).expect("rewrite");
        assert_eq!(json5::from_str::<Value>(&out).unwrap(), *after, "{out}");
        out
    }

    fn edit(source: &str, after: &Value) -> String {
        edit_with(source, after, b"  ")
    }

    #[test]
    fn comments_and_untouched_values_survive_with_both_line_endings() {
        let original = r#"/* 顶部 */{
	// 模型说明
	"model": "keep /* literal */ // text",
	"provider": { /* 供应商 */ "demo": {"options": {"apiKey": "old", "keep": 1,},},},
	"nested": [/* 数组 */ {"unchanged": true},],
}/* 尾部 */
"#;
        for source in [original.to_string(), original.replace('\n', "\r\n")] {
            let before: Value = json5::from_str(&source).unwrap();
            assert_eq!(rewrite(&source, &before, &before, b"\t").unwrap(), source);
            let mut after = before.clone();
            after["provider"]["demo"]["options"]["apiKey"] = json!("new");
            assert_eq!(
                edit_with(&source, &after, b"\t"),
                source.replace("old", "new")
            );
        }
    }

    #[test]
    fn block_comment_boundaries_do_not_swallow_other_fields() {
        for source in [
            "{/*中文*/\"keep\":1,// tail */\n\"edit\":0}/*end*/",
            "/**/{/*one*//*two*/\"keep\":1,\"edit\":0}/**/",
        ] {
            assert_eq!(
                edit(source, &json!({"keep": 1, "edit": 2})),
                source.replace(":0", ":2")
            );
        }
    }

    #[test]
    fn line_comments_ending_in_multibyte_characters_keep_their_bytes() {
        for suffix in [
            "// 中文",
            "// 😀",
            "/* 中文 */ // 尾",
            "// ab\u{2028}",
            "// 中文\u{2028}",
            "// 中文\u{2029}",
            "// 😀\u{2029}",
            "// 中文\r\n",
        ] {
            let source = format!(r#"{{"provider":{{"demo":{{"name":"old"}}}}}} {suffix}"#);
            let after = json!({"provider": {"demo": {"name": "new"}}});
            assert_eq!(edit(&source, &after), source.replace("old", "new"));
        }
        for separator in ['\u{2028}', '\u{2029}'] {
            for comment in ["ASCII", "中文", "😀"] {
                let source = format!("{{\"keep\":true,// {comment}{separator}\"edit\":0}}");
                assert_eq!(
                    edit(&source, &json!({"keep": true, "edit": 1})),
                    source.replace(":0", ":1")
                );
            }
        }
    }

    #[test]
    fn escaped_keys_are_matched_and_new_keys_are_escaped() {
        let source = r#"{"provider":{"quote\"key":1,'single\'key':2,unquoted:3},"keep":true}"#;
        let mut after: Value = json5::from_str(source).unwrap();
        after["provider"]["quote\"key"] = json!(4);
        after["provider"]
            .as_object_mut()
            .unwrap()
            .shift_remove("single'key");
        for key in ["new\"key", "back\\slash", "line\nkey", "中文键"] {
            after["provider"][key] = json!({"nested": true});
        }
        let out = edit(source, &after);
        assert!(out.contains(r#""provider""#));
        assert!(out.contains("unquoted:3"));
    }

    #[test]
    fn arrays_and_objects_grow_and_shrink() {
        for source in [
            "{\n  /* keep */ \"items\": [1,2,3,], \"remove\": {},\n}",
            "{\r\n\t/* keep */ \"items\": [1,2,3], \"remove\": {}\r\n}",
        ] {
            let mut current = source.to_string();
            for after in [
                json!({"items": [1, 4], "added": {"x": [1, 2]}}),
                json!({"items": []}),
                json!({}),
                json!({"after": 1}),
            ] {
                current = edit(&current, &after);
                assert!(current.contains("/* keep */"), "{current}");
            }
        }
    }

    #[test]
    fn appending_keeps_trailing_commas_and_the_files_indent() {
        assert_eq!(
            edit_with(
                "{\n\t/*keep*/ \"items\": [1,],\n}\n",
                &json!({"items": [1, 2], "new": {"a": 1}}),
                b"\t"
            ),
            "{\n\t/*keep*/ \"items\": [1,2,],\n\t\"new\": {\n\t\t\"a\": 1\n\t},\n}\n"
        );
    }

    #[test]
    fn removing_an_entry_leaves_no_blank_line() {
        let source = "{\n  \"a\": 1,\n  \"b\": 2,\n  \"c\": 3\n}\n";
        assert_eq!(
            edit(source, &json!({"a": 1, "c": 3})),
            "{\n  \"a\": 1,\n  \"c\": 3\n}\n"
        );
        assert_eq!(
            edit(source, &json!({"a": 1, "b": 2})),
            "{\n  \"a\": 1,\n  \"b\": 2\n}\n"
        );
        assert_eq!(
            edit(source, &json!({"b": 2, "c": 3})),
            "{\n  \"b\": 2,\n  \"c\": 3\n}\n"
        );
        assert_eq!(edit(source, &json!({})), "{}\n");
        // 前一项行尾的注释留着。
        assert_eq!(
            edit("{\n  \"a\": 1, // 说明\n  \"b\": 2\n}", &json!({"a": 1})),
            "{\n  \"a\": 1 // 说明\n}"
        );
        assert_eq!(
            edit("[\n  1,\n  2,\n  3,\n]", &json!([1, 3])),
            "[\n  1,\n  3,\n]"
        );
    }

    #[test]
    fn adding_then_removing_an_entry_gives_back_the_original() {
        let entry = json!({"npm": "@ai-sdk/openai-compatible", "models": {"m": {}}});
        for source in [
            "{\n  // 本地\n  \"provider\": {\n    \"ollama\": {},\n  },\n}\n",
            "{\n  \"provider\": {\n    \"ollama\": {}\n  }\n}",
            "{\n  \"provider\": {\n    // 只有注释\n  }\n}",
        ] {
            let before: Value = json5::from_str(source).unwrap();
            let mut after = before.clone();
            after["provider"]["confdeck-x"] = entry.clone();
            let added = edit(source, &after);
            assert_eq!(edit(&added, &before), source, "{added}");
        }
    }

    #[test]
    fn single_line_objects_are_expanded_for_multi_line_values() {
        assert_eq!(
            edit(
                r#"{"$schema": "https://opencode.ai/config.json"}"#,
                &json!({"$schema": "https://opencode.ai/config.json", "provider": {"x": {"a": 1}}})
            ),
            "{\n  \"$schema\": \"https://opencode.ai/config.json\",\n  \"provider\": {\n    \"x\": {\n      \"a\": 1\n    }\n  }\n}"
        );
        assert_eq!(
            edit("{}\n", &json!({"provider": {"x": 1}})),
            "{\n  \"provider\": {\n    \"x\": 1\n  }\n}\n"
        );
        // 单行的值保持单行。
        assert_eq!(
            edit(
                r#"{"a": 1, "b": [1]}"#,
                &json!({"a": 1, "b": [1, 2], "c": true})
            ),
            r#"{"a": 1, "b": [1, 2], "c": true}"#
        );
    }

    #[test]
    fn the_closing_bracket_indent_is_not_taken_for_the_items() {
        assert_eq!(
            edit(
                "{\n  \"p\": { \"a\": 1,\n  },\n}",
                &json!({"p": {"a": 1, "b": 2}})
            ),
            "{\n  \"p\": { \"a\": 1,\n    \"b\": 2,\n  },\n}"
        );
    }

    #[test]
    fn duplicate_keys_that_must_change_are_refused() {
        let source = r#"{"a": 1, "a": 2, "b": 0}"#;
        let before: Value = json5::from_str(source).unwrap();
        assert!(rewrite(source, &before, &json!({"a": 3, "b": 0}), b"  ").is_none());
        // 不动它的话照样能改别的键。
        assert_eq!(
            edit(source, &json!({"a": 2, "b": 1})),
            r#"{"a": 1, "a": 2, "b": 1}"#
        );
    }
}
