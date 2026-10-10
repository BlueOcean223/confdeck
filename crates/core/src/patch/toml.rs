//! TOML 补丁（Codex、Grok Build 的 `config.toml`），基于 `toml_edit`：注释、空行、
//! 键的写法和表的位置都原样保留，只改补丁点名的键。

use std::path::Path;

use toml_edit::{Decor, DocumentMut, Item, Table, TableLike, Value};

use super::{KeyPath, LivePatch, PatchError, decode_utf8, line_column};

/// 改一份已经解析好的 TOML 文档。实现了它的补丁自动实现 [`LivePatch`]：解析、改、再输出。
pub trait TomlDocPatch {
    fn apply_to(&self, path: &Path, doc: &mut DocumentMut) -> Result<(), PatchError>;
}

impl<T: TomlDocPatch> LivePatch for T {
    fn apply(&self, path: &Path, pre: Option<&[u8]>) -> Result<Vec<u8>, PatchError> {
        let mut doc = parse(path, pre)?;
        self.apply_to(path, &mut doc)?;
        Ok(doc.to_string().into_bytes())
    }
}

/// 解析写前内容；文件不存在时是空文档。
pub fn parse(path: &Path, pre: Option<&[u8]>) -> Result<DocumentMut, PatchError> {
    let Some(bytes) = pre else {
        return Ok(DocumentMut::new());
    };
    let text = decode_utf8(path, bytes)?;
    text.parse::<DocumentMut>().map_err(|err| {
        let (line, column) = line_column(text, err.span().map_or(0, |span| span.start));
        PatchError::Parse {
            path: path.to_path_buf(),
            line,
            column,
            message: err.message().to_string(),
        }
    })
}

/// 原位改值：已有就沿用原来的空白和行尾注释，没有就追加到表末尾。
pub fn put_value(table: &mut dyn TableLike, key: &str, value: &Value) {
    match table.get_mut(key) {
        Some(Item::Value(slot)) => {
            let decor = slot.decor().clone();
            *slot = value.clone();
            *slot.decor_mut() = decor;
        }
        Some(slot) => *slot = Item::Value(value.clone()),
        None => {
            table.insert(key, Item::Value(value.clone()));
        }
    }
}

/// 删掉 `parent` 表里的 `key`，保住它上方的注释。
///
/// toml_edit 把一行注释记在它下面那个键（或表头）的前缀里，直接删键会把注释一起删掉，
/// 文件开头的说明就常常挂在第一个键 `model` 上。这里把注释挪走，让它留在文件里原来的位置：
/// 删的是值，挪到同一张表里下一个值的前缀；后面没有值了，或删的是一整张表，挪到下一张表的
/// 表头前；再没有表就放到文件末尾。被删的表里面的注释随表一起删。
pub fn remove_key(doc: &mut DocumentMut, parent: &[&str], key: &str) -> Option<Item> {
    let parent_position = if parent.is_empty() {
        Some(0)
    } else {
        table_at(doc.as_table(), parent).and_then(Table::position)
    };

    let table = table_like_at_mut(doc.as_table_mut(), parent)?;
    let order: Vec<String> = table.iter().map(|(name, _)| name.to_string()).collect();
    let index = order.iter().position(|name| name == key)?;
    let key_comment = table.key(key).and_then(|k| comment_of(k.leaf_decor()));
    let removed = table.remove(key)?;
    let (comment, after) = match &removed {
        Item::Value(_) => (key_comment, parent_position),
        Item::Table(removed_table) => (comment_of(removed_table.decor()), removed_table.position()),
        _ => (None, None),
    };
    let Some(comment) = comment else {
        return Some(removed);
    };

    if matches!(removed, Item::Value(_)) {
        let next = order[index + 1..]
            .iter()
            .find(|name| matches!(table.get(name), Some(Item::Value(_))));
        if let Some(next) = next {
            let mut next_key = table.key_mut(next).expect("listed above");
            prepend(next_key.leaf_decor_mut(), &comment);
            return Some(removed);
        }
    }

    let next_table = after.and_then(|after| next_table_after(doc.as_table_mut(), after));
    match next_table {
        Some(table) => prepend(table.decor_mut(), &comment),
        None => {
            let trailing = doc.trailing().as_str().unwrap_or_default().to_string();
            doc.set_trailing(format!("{comment}{trailing}"));
        }
    }
    Some(removed)
}

/// 前缀里有注释时返回整个前缀（含注释前后的空行）。
fn comment_of(decor: &Decor) -> Option<String> {
    decor
        .prefix()
        .and_then(|prefix| prefix.as_str())
        .filter(|prefix| prefix.contains('#'))
        .map(str::to_string)
}

fn prepend(decor: &mut Decor, text: &str) {
    let old = decor
        .prefix()
        .and_then(|prefix| prefix.as_str())
        .unwrap_or_default()
        .to_string();
    decor.set_prefix(format!("{text}{old}"));
}

fn table_at<'a>(root: &'a Table, path: &[&str]) -> Option<&'a Table> {
    path.iter()
        .try_fold(root, |table, segment| table.get(segment)?.as_table())
}

fn table_like_at_mut<'a>(root: &'a mut Table, path: &[&str]) -> Option<&'a mut dyn TableLike> {
    let mut current: &mut dyn TableLike = root;
    for segment in path {
        current = current.get_mut(segment)?.as_table_like_mut()?;
    }
    Some(current)
}

/// 文件里排在 `after` 后面的第一张有表头的表（隐式表和点号键形成的表不输出表头，不算）。
fn next_table_after(root: &mut Table, after: usize) -> Option<&mut Table> {
    let position = header_positions(root)
        .into_iter()
        .filter(|&position| position > after)
        .min()?;
    table_with_position(root, position)
}

fn has_header(table: &Table) -> bool {
    !table.is_implicit() && !table.is_dotted()
}

fn child_tables(item: &Item) -> Vec<&Table> {
    match item {
        Item::Table(table) => vec![table],
        Item::ArrayOfTables(array) => array.iter().collect(),
        _ => Vec::new(),
    }
}

fn header_positions(table: &Table) -> Vec<usize> {
    let mut positions = Vec::new();
    for (_, item) in table.iter() {
        for child in child_tables(item) {
            if has_header(child) {
                positions.extend(child.position());
            }
            positions.extend(header_positions(child));
        }
    }
    positions
}

fn table_with_position(table: &mut Table, position: usize) -> Option<&mut Table> {
    for (_, item) in table.iter_mut() {
        let children: Vec<&mut Table> = match item {
            Item::Table(child) => vec![child],
            Item::ArrayOfTables(array) => array.iter_mut().collect(),
            _ => continue,
        };
        for child in children {
            if has_header(child) && child.position() == Some(position) {
                return Some(child);
            }
            if let Some(found) = table_with_position(child, position) {
                return Some(found);
            }
        }
    }
    None
}

/// 路径上的这一段应该是表，却不是。
pub fn shape_error(path: &Path, segments: &[String]) -> PatchError {
    PatchError::Shape {
        path: path.to_path_buf(),
        key_path: KeyPath(segments.to_vec()),
        expected: "表",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Noop;

    impl TomlDocPatch for Noop {
        fn apply_to(&self, _: &Path, _: &mut DocumentMut) -> Result<(), PatchError> {
            Ok(())
        }
    }

    #[test]
    fn broken_files_are_refused_with_the_line_number() {
        let err = Noop
            .apply(Path::new("c.toml"), Some(b"a = 1\nb = \n"))
            .expect_err("must refuse");
        assert!(matches!(err, PatchError::Parse { line: 2, .. }), "{err:?}");
    }

    #[test]
    fn missing_file_parses_as_an_empty_document() {
        let out = Noop.apply(Path::new("c.toml"), None).expect("empty doc");
        assert!(out.is_empty());
    }

    #[test]
    fn untouched_files_come_back_byte_for_byte() {
        let text = "# 我的设置\nmodel = \"x\"   # 行尾注释\n\n[tui]\nnotifications = true\n";
        let out = Noop
            .apply(Path::new("c.toml"), Some(text.as_bytes()))
            .expect("apply");
        assert_eq!(String::from_utf8(out).unwrap(), text);
    }

    fn removed(text: &str, parent: &[&str], keys: &[&str]) -> String {
        let mut doc: DocumentMut = text.parse().unwrap();
        for key in keys {
            assert!(
                remove_key(&mut doc, parent, key).is_some(),
                "{key} should exist"
            );
        }
        doc.to_string()
    }

    #[test]
    fn a_removed_keys_comment_moves_to_the_next_value() {
        let text = "# 文件说明\n\nmodel = \"x\"\nkeep = 1\n";
        assert_eq!(removed(text, &[], &["model"]), "# 文件说明\n\nkeep = 1\n");
    }

    #[test]
    fn a_comment_with_no_value_after_it_stays_above_the_next_table() {
        let text = "keep = 1\n# 推理强度\neffort = \"high\"\n\n[tui]\nnotifications = true\n";
        assert_eq!(
            removed(text, &[], &["effort"]),
            "keep = 1\n# 推理强度\n\n[tui]\nnotifications = true\n"
        );

        let text = "[agents]\n# 子代理\nmodel = \"x\"\n\n[tui]\na = 1\n";
        assert_eq!(
            removed(text, &["agents"], &["model"]),
            "[agents]\n# 子代理\n\n[tui]\na = 1\n"
        );
    }

    #[test]
    fn a_comment_in_the_last_table_goes_to_the_end_of_the_file() {
        let text = "[tui]\na = 1\n# 末尾\nb = 2\n";
        assert_eq!(removed(text, &["tui"], &["b"]), "[tui]\na = 1\n# 末尾\n");
    }

    #[test]
    fn a_removed_tables_header_comment_moves_to_the_next_table() {
        let text = "[a]\nx = 1\n# 下面是中转\n[p]\ny = 2\n\n[[skills.config]]\nname = \"s\"\n";
        assert_eq!(
            removed(text, &[], &["p"]),
            "[a]\nx = 1\n# 下面是中转\n\n[[skills.config]]\nname = \"s\"\n"
        );
    }

    #[test]
    fn the_next_table_is_found_by_file_position() {
        // 子表 [a.c] 写在父表 [a] 前面也是合法的 TOML。
        let text = "k = 1\n# 说明\nv = 2\n\n[a.c]\nx = 1\n\n[a]\ny = 2\n";
        assert_eq!(
            removed(text, &[], &["v"]),
            "k = 1\n# 说明\n\n[a.c]\nx = 1\n\n[a]\ny = 2\n"
        );
    }

    #[test]
    fn put_value_keeps_the_existing_decor() {
        let mut doc: DocumentMut = "model = \"x\"   # 行尾注释\n".parse().unwrap();
        put_value(doc.as_table_mut(), "model", &Value::from("y"));
        put_value(doc.as_table_mut(), "added", &Value::from(true));
        assert_eq!(
            doc.to_string(),
            "model = \"y\"   # 行尾注释\nadded = true\n"
        );
    }
}
