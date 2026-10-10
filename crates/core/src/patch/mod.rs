//! 按键补丁：以现有文件为底，只改补丁点名的键，其余键、值和顺序不变。
//!
//! 每种格式各一个补丁类型，都实现 [`LivePatch`]：输入写前的字节（文件不存在为 `None`），
//! 输出写后的字节。解析不了就报错，绝不退回空文档：从空文档开始写，等于清空了用户的配置。
//!
//! 移植自 ccs-lite（BlueOcean223/cc-switch）的 `live/patch`。

pub mod json;
mod jsonc;
pub mod toml;

use std::fmt;
use std::path::{Path, PathBuf};

/// 键在文档里的位置：从根开始的键序列。键里可能有点号（TOML 的 `model."grok-4.5"`），
/// 所以不用点分字符串。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeyPath(pub Vec<String>);

impl KeyPath {
    pub fn new(segments: &[&str]) -> Self {
        Self(segments.iter().map(|s| (*s).to_string()).collect())
    }

    pub fn root() -> Self {
        Self(Vec::new())
    }

    pub fn child(&self, key: &str) -> Self {
        let mut segments = self.0.clone();
        segments.push(key.to_string());
        Self(segments)
    }

    /// 拆成（父路径，最后一个键）。根路径没有最后一个键。
    fn split_last(&self) -> Option<(&[String], &String)> {
        self.0.split_last().map(|(last, parent)| (parent, last))
    }
}

impl fmt::Display for KeyPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return f.write_str("<root>");
        }
        f.write_str(&self.0.join("."))
    }
}

/// 在内存里算出一个文件的新内容，不写盘。
pub trait LivePatch {
    fn apply(&self, path: &Path, pre: Option<&[u8]>) -> Result<Vec<u8>, PatchError>;
}

/// 配置文件的格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Json,
    /// 允许注释、尾逗号（JSON5 / JSONC）。写回时注释和排版原样保留，只改动变了的值。
    Json5,
    Toml,
}

/// 从 `old` 改成 `new` 的补丁：`old` 有、`new` 没有的键删掉，`new` 里新增的键和值变了的键写上，
/// 两边一样的键不碰，文件里这些键的当前值保持原样。两边都是对象（表）的键逐层往下比，
/// 其余的值（含数组）整个比、整个写；新增的对象（TOML 里是标准表，内联表整个写）逐个写里面的键，
/// 文件里同一处已有的其他键不动。
///
/// 用来把供应商的改动写进在用它的 agent：`old`、`new` 是改前、改后的供应商各自写到空文件上的结果，
/// 见 [`crate::apply::save_provider`]。
pub fn delta(
    format: Format,
    path: &Path,
    old: &[u8],
    new: &[u8],
) -> Result<Box<dyn LivePatch>, PatchError> {
    match format {
        Format::Json | Format::Json5 => {
            let json5 = format == Format::Json5;
            let (old, _) = json::parse(path, Some(old), json5)?;
            let (new, _) = json::parse(path, Some(new), json5)?;
            Ok(Box::new(json::delta(&old, &new, json5)))
        }
        Format::Toml => {
            let old = toml::parse(path, Some(old))?;
            let new = toml::parse(path, Some(new))?;
            Ok(Box::new(toml::Delta::between(&old, &new)))
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PatchError {
    /// 文件解析不了。行号、列号从 1 开始。
    #[error("无法解析 {path}（第 {line} 行第 {column} 列）：{message}")]
    Parse {
        path: PathBuf,
        line: usize,
        column: usize,
        message: String,
    },
    /// 能解析，但要改的位置不是预期的形状，比如 `env` 是字符串而不是对象。
    #[error("{path} 里的 {key_path} 不是{expected}，为避免覆盖你的配置，没有写入")]
    Shape {
        path: PathBuf,
        key_path: KeyPath,
        expected: &'static str,
    },
}

/// 字节偏移 → (行, 列)，都从 1 开始。
pub(crate) fn line_column(text: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(text.len());
    let before = &text[..offset];
    let line = before.matches('\n').count() + 1;
    let column = before.rfind('\n').map_or(before.chars().count(), |nl| {
        before[nl + 1..].chars().count()
    }) + 1;
    (line, column)
}

pub(crate) fn decode_utf8<'a>(path: &Path, bytes: &'a [u8]) -> Result<&'a str, PatchError> {
    std::str::from_utf8(bytes).map_err(|err| {
        // valid_up_to 之前的部分一定是合法 UTF-8。
        let valid = std::str::from_utf8(&bytes[..err.valid_up_to()]).unwrap_or_default();
        let (line, column) = line_column(valid, err.valid_up_to());
        PatchError::Parse {
            path: path.to_path_buf(),
            line,
            column,
            message: "不是 UTF-8 文本".to_string(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_column_counts_from_one() {
        let text = "ab\ncde\nf";
        assert_eq!(line_column(text, 0), (1, 1));
        assert_eq!(line_column(text, 4), (2, 2));
        assert_eq!(line_column(text, text.len()), (3, 2));
    }

    #[test]
    fn non_utf8_is_a_parse_error() {
        let err = decode_utf8(Path::new("x.json"), b"ok\n\xff").expect_err("must refuse");
        assert!(
            matches!(
                err,
                PatchError::Parse {
                    line: 2,
                    column: 1,
                    ..
                }
            ),
            "{err:?}"
        );
    }
}
