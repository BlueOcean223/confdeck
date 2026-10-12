//! 行级差异（备份页的「差异」）：一份备份和现在的文件差在哪。Myers 算法，只在行上比，
//! 结果按「上下文几行」压缩，中间没变的一长段折成「⋯ N 行」。

/// 差异里的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Same(String),
    Added(String),
    Removed(String),
    /// 折起来的没变的行数。
    Skipped(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edit {
    Keep,
    Insert,
    Delete,
}

/// 从 `old` 到 `new` 的差异，变了的地方前后各留 `context` 行。两边一样时只有一个 `Skipped`（或者空）。
pub fn rows(old: &str, new: &str, context: usize) -> Vec<Row> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let edits = edits(&a, &b);

    let mut full = Vec::with_capacity(edits.len());
    let (mut i, mut j) = (0, 0);
    for edit in edits {
        match edit {
            Edit::Keep => {
                full.push(Row::Same(a[i].to_string()));
                i += 1;
                j += 1;
            }
            Edit::Delete => {
                full.push(Row::Removed(a[i].to_string()));
                i += 1;
            }
            Edit::Insert => {
                full.push(Row::Added(b[j].to_string()));
                j += 1;
            }
        }
    }
    compact(full, context)
}

/// 没变的一段超过 `2 * context` 行（开头、结尾超过 `context` 行）时折起来。
fn compact(full: Vec<Row>, context: usize) -> Vec<Row> {
    let mut out = Vec::new();
    let mut run: Vec<Row> = Vec::new();
    let flush = |run: &mut Vec<Row>, out: &mut Vec<Row>, at_end: bool| {
        let at_start = out.is_empty();
        let keep_front = if at_start { 0 } else { context };
        let keep_back = if at_end { 0 } else { context };
        if run.len() > keep_front + keep_back {
            let skipped = run.len() - keep_front - keep_back;
            let tail = run.split_off(run.len() - keep_back);
            run.truncate(keep_front);
            out.append(run);
            out.push(Row::Skipped(skipped));
            out.extend(tail);
        } else {
            out.append(run);
        }
    };
    for row in full {
        if matches!(row, Row::Same(_)) {
            run.push(row);
        } else {
            flush(&mut run, &mut out, false);
            out.push(row);
        }
    }
    flush(&mut run, &mut out, true);
    out
}

/// 太大的差异不算了（内存按 d² 长），整个当成删掉重写。
const MAX_D: usize = 3000;

/// Myers：最短编辑脚本。
fn edits(a: &[&str], b: &[&str]) -> Vec<Edit> {
    let (n, m) = (a.len() as i64, b.len() as i64);
    let max = (n + m) as usize;
    if max == 0 {
        return Vec::new();
    }
    let offset = max as i64 + 1;
    let width = 2 * max + 3;
    let mut v = vec![0i64; width];
    let mut trace: Vec<Vec<i64>> = Vec::new();
    let mut found = None;
    'outer: for d in 0..=max.min(MAX_D) {
        trace.push(v.clone());
        let d = d as i64;
        let mut k = -d;
        while k <= d {
            let idx = (k + offset) as usize;
            let mut x = if k == -d || (k != d && v[idx - 1] < v[idx + 1]) {
                v[idx + 1]
            } else {
                v[idx - 1] + 1
            };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[idx] = x;
            if x >= n && y >= m {
                found = Some(d);
                break 'outer;
            }
            k += 2;
        }
    }
    let Some(d_final) = found else {
        let mut all = vec![Edit::Delete; a.len()];
        all.extend(std::iter::repeat_n(Edit::Insert, b.len()));
        return all;
    };

    // 从终点沿着每一步的 v 倒推。
    let mut edits = Vec::new();
    let (mut x, mut y) = (n, m);
    for d in (0..=d_final).rev() {
        let v = &trace[d as usize];
        let k = x - y;
        let idx = (k + offset) as usize;
        let prev_k = if k == -d || (k != d && v[idx - 1] < v[idx + 1]) {
            k + 1
        } else {
            k - 1
        };
        let prev_x = v[(prev_k + offset) as usize];
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            edits.push(Edit::Keep);
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            if x == prev_x {
                edits.push(Edit::Insert);
            } else {
                edits.push(Edit::Delete);
            }
        }
        x = prev_x;
        y = prev_y;
    }
    edits.reverse();
    edits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(rows: &[Row]) -> String {
        rows.iter()
            .map(|row| match row {
                Row::Same(line) => format!(" {line}"),
                Row::Added(line) => format!("+{line}"),
                Row::Removed(line) => format!("-{line}"),
                Row::Skipped(n) => format!("~{n}"),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn same_text_is_one_skipped_block() {
        assert_eq!(rows("a\nb\nc\n", "a\nb\nc\n", 3), vec![Row::Skipped(3)]);
        assert!(rows("", "", 3).is_empty());
    }

    #[test]
    fn replace_insert_and_delete() {
        assert_eq!(text(&rows("a\nb\nc", "a\nx\nc", 3)), " a\n-b\n+x\n c");
        assert_eq!(text(&rows("a\nc", "a\nb\nc", 3)), " a\n+b\n c");
        assert_eq!(text(&rows("a\nb\nc", "a\nc", 3)), " a\n-b\n c");
        assert_eq!(text(&rows("", "a\nb", 3)), "+a\n+b");
        assert_eq!(text(&rows("a\nb", "", 3)), "-a\n-b");
    }

    #[test]
    fn long_unchanged_runs_fold_with_context() {
        let old: Vec<String> = (1..=20).map(|n| n.to_string()).collect();
        let mut new = old.clone();
        new[9] = "ten".into();
        let rows = rows(&old.join("\n"), &new.join("\n"), 2);
        assert_eq!(text(&rows), "~7\n 8\n 9\n-10\n+ten\n 11\n 12\n~8");
    }

    #[test]
    fn crlf_lines_compare_like_lf() {
        assert_eq!(rows("a\r\nb\r\n", "a\nb\n", 1), vec![Row::Skipped(2)]);
    }

    #[test]
    fn edit_script_replays_to_the_new_text() {
        let old = "one\ntwo\nthree\nfour\nfive\nsix";
        let new = "zero\none\nthree\n3.5\nfour\nsix\nseven";
        let replayed: Vec<String> = rows(old, new, 100)
            .into_iter()
            .filter_map(|row| match row {
                Row::Same(line) | Row::Added(line) => Some(line),
                Row::Removed(_) | Row::Skipped(_) => None,
            })
            .collect();
        assert_eq!(replayed.join("\n"), new);
    }
}
