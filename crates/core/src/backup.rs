//! 备份：`~/.confdeck/backups/<owner>/<文件名>/<时间>_<原因>.<扩展名>`。
//!
//! `owner` 是 agent 的 id（`claude-code`、`codex`…），confdeck 自己的文件是 `confdeck`。
//! 文件名可读，不开 confdeck 也能直接拷回去。什么时候备份由调用方决定（见 [`crate::engine`]），
//! 这里只管存、列、删：
//!
//! - 和最新一份内容完全相同时不重复存。
//! - 「原件」（confdeck 第一次写这个文件之前的内容）一直保留；其余只留最近 [`KEEP`] 份。
//! - 认不出的文件（`.DS_Store`、用户自己放进来的）不列出，也不删。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{NaiveDateTime, Timelike as _};

use crate::data::DataDir;
use crate::fsx::{self, Mode};

/// 原件以外，每个文件保留的份数。
pub const KEEP: usize = 10;

const TIME_FORMAT: &str = "%Y-%m-%d_%H-%M-%S";
/// `TIME_FORMAT` 输出的长度。
const TIME_LEN: usize = 19;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// confdeck 第一次写这个文件之前的内容。
    Original,
    /// 文件被 confdeck 以外的程序或人改过。
    External,
    /// 「原始文件」页保存之前。
    BeforeEdit,
    /// 从备份恢复之前。
    BeforeRestore,
    /// confdeck 自己的 `providers.json` 保存之前。
    BeforeSave,
}

impl Reason {
    const ALL: [Reason; 5] = [
        Self::Original,
        Self::External,
        Self::BeforeEdit,
        Self::BeforeRestore,
        Self::BeforeSave,
    ];

    /// 写进文件名的词。界面上按语言显示，不直接显示它。
    pub fn slug(self) -> &'static str {
        match self {
            Self::Original => "original",
            Self::External => "external",
            Self::BeforeEdit => "before-edit",
            Self::BeforeRestore => "before-restore",
            Self::BeforeSave => "before-save",
        }
    }
}

/// 一组备份：哪个 owner 的哪个文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shelf {
    pub owner: String,
    pub file: String,
}

impl Shelf {
    pub fn new(owner: &str, file: &str) -> Self {
        Self {
            owner: owner.to_string(),
            file: file.to_string(),
        }
    }

    pub fn dir(&self, data: &DataDir) -> PathBuf {
        data.backups().join(&self.owner).join(&self.file)
    }

    /// 备份文件的扩展名沿用原文件的，编辑器能认出格式。
    fn extension(&self) -> Option<&str> {
        Path::new(&self.file)
            .extension()
            .and_then(|ext| ext.to_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backup {
    pub path: PathBuf,
    pub time: NaiveDateTime,
    pub reason: Reason,
    pub size: u64,
    /// 同一秒里有几份时的序号，从 1 开始。
    seq: u32,
}

/// 列出一个文件的备份，最新的在前。目录不存在时是空列表。
pub fn list(data: &DataDir, shelf: &Shelf) -> io::Result<Vec<Backup>> {
    let entries = match fs::read_dir(shelf.dir(data)) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let mut backups = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some((time, reason, seq)) = name.to_str().and_then(parse_name) else {
            continue;
        };
        let meta = entry.metadata()?;
        if !meta.is_file() {
            continue;
        }
        backups.push(Backup {
            path: entry.path(),
            time,
            reason,
            size: meta.len(),
            seq,
        });
    }
    backups.sort_by_key(|backup| std::cmp::Reverse((backup.time, backup.seq)));
    Ok(backups)
}

/// 存一份备份并清掉超出的旧备份。和最新一份内容相同时不存，返回 `None`。
pub fn take(
    data: &DataDir,
    shelf: &Shelf,
    bytes: &[u8],
    reason: Reason,
    now: NaiveDateTime,
) -> io::Result<Option<Backup>> {
    let existing = list(data, shelf)?;
    if let Some(latest) = existing.first()
        && fs::read(&latest.path)? == bytes
    {
        return Ok(None);
    }
    // 文件名只精确到秒。
    let now = now.with_nanosecond(0).unwrap_or(now);
    let dir = shelf.dir(data);
    let seq = existing
        .iter()
        .filter(|backup| backup.time == now)
        .map(|backup| backup.seq)
        .max()
        .map_or(1, |seq| seq + 1);
    let mut name = format!("{}_{}", now.format(TIME_FORMAT), reason.slug());
    if seq > 1 {
        name.push_str(&format!("-{seq}"));
    }
    if let Some(ext) = shelf.extension() {
        name.push('.');
        name.push_str(ext);
    }
    let path = dir.join(name);
    fsx::write_atomic(&path, bytes, Mode::Private)?;
    prune(data, shelf)?;
    Ok(Some(Backup {
        path,
        time: now,
        reason,
        size: bytes.len() as u64,
        seq,
    }))
}

/// 原件全留，其余只留最近 [`KEEP`] 份。
pub fn prune(data: &DataDir, shelf: &Shelf) -> io::Result<()> {
    let others = list(data, shelf)?
        .into_iter()
        .filter(|backup| backup.reason != Reason::Original);
    for backup in others.skip(KEEP) {
        fs::remove_file(&backup.path)?;
    }
    Ok(())
}

/// 「清理」：删掉原件以外的所有备份，返回释放的字节数。
pub fn clean(data: &DataDir, shelf: &Shelf) -> io::Result<u64> {
    let mut freed = 0;
    for backup in list(data, shelf)? {
        if backup.reason != Reason::Original {
            fs::remove_file(&backup.path)?;
            freed += backup.size;
        }
    }
    Ok(freed)
}

fn parse_time(text: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(text, TIME_FORMAT).ok()
}

/// `2026-10-10_15-00-02_external-2.toml` → (时间, 原因, 序号)。
fn parse_name(name: &str) -> Option<(NaiveDateTime, Reason, u32)> {
    let time = parse_time(name.get(..TIME_LEN)?)?;
    let rest = name.get(TIME_LEN..)?.strip_prefix('_')?;
    let stem = rest.split('.').next().unwrap_or(rest);
    Reason::ALL.into_iter().find_map(|reason| {
        let tail = stem.strip_prefix(reason.slug())?;
        let seq = match tail {
            "" => 1,
            _ => tail
                .strip_prefix('-')?
                .parse()
                .ok()
                .filter(|&seq| seq > 1)?,
        };
        Some((time, reason, seq))
    })
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;

    use super::*;

    fn at(hour: u32, minute: u32, second: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 10, 10)
            .unwrap()
            .and_hms_opt(hour, minute, second)
            .unwrap()
    }

    fn setup() -> (tempfile::TempDir, DataDir, Shelf) {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::new(dir.path().join(".confdeck"));
        (dir, data, Shelf::new("codex", "config.toml"))
    }

    fn names(data: &DataDir, shelf: &Shelf) -> Vec<String> {
        list(data, shelf)
            .unwrap()
            .iter()
            .map(|b| b.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn names_are_readable() {
        let (_dir, data, shelf) = setup();
        take(&data, &shelf, b"one", Reason::Original, at(9, 0, 0)).unwrap();
        take(&data, &shelf, b"two", Reason::External, at(15, 0, 2)).unwrap();
        assert_eq!(
            names(&data, &shelf),
            [
                "2026-10-10_15-00-02_external.toml",
                "2026-10-10_09-00-00_original.toml"
            ]
        );
        let dir = data.backups().join("codex").join("config.toml");
        assert_eq!(
            fs::read(dir.join("2026-10-10_09-00-00_original.toml")).unwrap(),
            b"one"
        );
    }

    #[test]
    fn identical_content_is_not_stored_twice() {
        let (_dir, data, shelf) = setup();
        assert!(
            take(&data, &shelf, b"same", Reason::External, at(9, 0, 0))
                .unwrap()
                .is_some()
        );
        assert!(
            take(&data, &shelf, b"same", Reason::External, at(9, 5, 0))
                .unwrap()
                .is_none()
        );
        assert_eq!(list(&data, &shelf).unwrap().len(), 1);
    }

    #[test]
    fn backups_in_the_same_second_get_a_sequence_number() {
        let (_dir, data, shelf) = setup();
        for bytes in [b"a", b"b", b"c"] {
            take(&data, &shelf, bytes, Reason::BeforeEdit, at(9, 0, 0)).unwrap();
        }
        assert_eq!(
            names(&data, &shelf),
            [
                "2026-10-10_09-00-00_before-edit-3.toml",
                "2026-10-10_09-00-00_before-edit-2.toml",
                "2026-10-10_09-00-00_before-edit.toml"
            ]
        );
    }

    #[test]
    fn prune_keeps_the_original_and_the_latest_ten() {
        let (_dir, data, shelf) = setup();
        for minute in 0..15 {
            let bytes = format!("v{minute}");
            let reason = if minute == 0 {
                Reason::Original
            } else {
                Reason::External
            };
            take(&data, &shelf, bytes.as_bytes(), reason, at(9, minute, 0)).unwrap();
        }
        let backups = list(&data, &shelf).unwrap();
        assert_eq!(backups.len(), KEEP + 1);
        assert_eq!(backups.last().unwrap().reason, Reason::Original);
        assert_eq!(fs::read(&backups[0].path).unwrap(), b"v14");
        assert_eq!(fs::read(&backups[KEEP - 1].path).unwrap(), b"v5");
    }

    #[test]
    fn clean_leaves_only_the_original_and_unknown_files() {
        let (_dir, data, shelf) = setup();
        take(&data, &shelf, b"one", Reason::Original, at(9, 0, 0)).unwrap();
        take(&data, &shelf, b"two", Reason::External, at(9, 1, 0)).unwrap();
        take(&data, &shelf, b"three", Reason::BeforeEdit, at(9, 2, 0)).unwrap();
        let stray = shelf.dir(&data).join("notes.txt");
        fs::write(&stray, "mine").unwrap();

        assert_eq!(clean(&data, &shelf).unwrap(), 8);
        assert_eq!(names(&data, &shelf), ["2026-10-10_09-00-00_original.toml"]);
        assert!(stray.exists());
    }

    #[test]
    fn parse_name_rejects_foreign_files() {
        assert!(parse_name("2026-10-10_09-00-00_external.json").is_some());
        assert!(parse_name("2026-10-10_09-00-00_external").is_some());
        for name in [
            ".DS_Store",
            "settings.json",
            "2026-10-10_09-00-00_unknown.json",
            "2026-10-10_09-00-00_external-1.json",
            "2026-13-10_09-00-00_external.json",
        ] {
            assert!(parse_name(name).is_none(), "{name}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn backups_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, data, shelf) = setup();
        let backup = take(&data, &shelf, b"sk-secret", Reason::External, at(9, 0, 0))
            .unwrap()
            .unwrap();
        let mode = fs::metadata(&backup.path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
