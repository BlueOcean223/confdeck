//! 文件操作的几个基本件：读（不存在算 `None`）、跟着软链接找到真正的文件、原子写入、hash。

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest as _, Sha256};

/// 文件内容的 SHA-256（十六进制）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Digest(pub String);

impl Digest {
    pub fn of(bytes: &[u8]) -> Self {
        Self(
            Sha256::digest(bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        )
    }
}

/// 文件不存在时是 `None`，其余读取错误照报。
pub fn read_optional(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// 跟着软链接找到真正要写的文件。
///
/// 很多人把 `~/.claude/settings.json` 链到 dotfiles 仓库里。原子写入是「写临时文件再改名」，
/// 直接对链接改名会把链接换成普通文件，仓库里那份就不再更新了，所以要写到链接指向的文件。
/// 只解析最后一段；中间的目录是链接没关系，改名在同一个目录里进行。链接指向的文件不存在时
/// 返回那个路径（会新建它）。
pub fn resolve(path: &Path) -> io::Result<PathBuf> {
    let mut current = path.to_path_buf();
    for _ in 0..32 {
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let link = fs::read_link(&current)?;
                current = match current.parent() {
                    Some(parent) if link.is_relative() => parent.join(link),
                    _ => link,
                };
            }
            Ok(_) => return Ok(current),
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(current),
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::other(format!(
        "{} 的软链接层数太多",
        path.display()
    )))
}

/// 新文件的权限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 0600：文件里有 Key。已有文件也会改成 0600。
    Private,
    /// 已有文件沿用原来的权限，新文件按 umask。
    Keep,
}

/// 写好、还没替换目标的临时文件。`commit` 改名替换，没提交就丢弃时删掉临时文件。
#[derive(Debug)]
pub struct Staged {
    /// 提交后为 `None`：临时文件已经变成目标，不能再删。
    tmp: Option<PathBuf>,
    path: PathBuf,
}

impl Staged {
    pub fn commit(mut self) -> io::Result<()> {
        let tmp = self.tmp.take().expect("committed only once");
        if let Err(err) = fs::rename(&tmp, &self.path) {
            let _ = fs::remove_file(&tmp);
            return Err(err);
        }
        sync_dir(parent_dir(&self.path))
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if let Some(tmp) = &self.tmp {
            let _ = fs::remove_file(tmp);
        }
    }
}

/// 在目标旁边写一个临时文件并 fsync。目录不存在时一并创建。
pub fn stage(path: &Path, bytes: &[u8], mode: Mode) -> io::Result<Staged> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let dir = parent_dir(path);
    fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "路径没有文件名"))?;

    let (tmp, mut file) = loop {
        let mut tmp_name = name.to_os_string();
        tmp_name.push(format!(
            ".confdeck-tmp.{}.{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let tmp = dir.join(tmp_name);
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        if mode == Mode::Private {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&tmp) {
            Ok(file) => break (tmp, file),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    };
    let staged = Staged {
        tmp: Some(tmp.clone()),
        path: path.to_path_buf(),
    };

    #[cfg(unix)]
    if mode == Mode::Keep
        && let Ok(meta) = fs::metadata(path)
    {
        fs::set_permissions(&tmp, meta.permissions())?;
    }
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(staged)
}

/// 原子写入：临时文件 → fsync → 改名 → fsync 目录。
pub fn write_atomic(path: &Path, bytes: &[u8], mode: Mode) -> io::Result<()> {
    stage(path, bytes, mode)?.commit()
}

fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

/// 改名后 fsync 所在目录，断电后改名才算数。
fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    fs::File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn digest_is_sha256_hex() {
        assert_eq!(
            Digest::of(b"abc").0,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn missing_files_read_as_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_optional(&dir.path().join("x")).unwrap(), None);
    }

    #[test]
    fn atomic_write_creates_dirs_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("settings.json");
        write_atomic(&path, b"one", Mode::Keep).unwrap();
        write_atomic(&path, b"two", Mode::Keep).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
        assert_eq!(names(&dir.path().join("a")), ["settings.json"]);
    }

    #[test]
    fn a_dropped_stage_removes_its_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        drop(stage(&path, b"x", Mode::Keep).unwrap());
        assert!(names(dir.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn private_files_are_0600_and_kept_files_keep_their_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;

        let private = dir.path().join("private.json");
        fs::write(&private, "x").unwrap();
        fs::set_permissions(&private, fs::Permissions::from_mode(0o644)).unwrap();
        write_atomic(&private, b"y", Mode::Private).unwrap();
        assert_eq!(mode(&private), 0o600);

        let kept = dir.path().join("kept.json");
        fs::write(&kept, "x").unwrap();
        fs::set_permissions(&kept, fs::Permissions::from_mode(0o640)).unwrap();
        write_atomic(&kept, b"y", Mode::Keep).unwrap();
        assert_eq!(mode(&kept), 0o640);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_followed_so_the_link_survives() {
        let dir = tempfile::tempdir().unwrap();
        let dotfiles = dir.path().join("dotfiles");
        fs::create_dir(&dotfiles).unwrap();
        fs::write(dotfiles.join("settings.json"), "old").unwrap();
        let link = dir.path().join("settings.json");
        std::os::unix::fs::symlink("dotfiles/settings.json", &link).unwrap();

        let target = resolve(&link).unwrap();
        assert_eq!(target, dir.path().join("dotfiles/settings.json"));
        write_atomic(&target, b"new", Mode::Keep).unwrap();

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&link).unwrap(), b"new");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_loops_are_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::os::unix::fs::symlink(&b, &a).unwrap();
        std::os::unix::fs::symlink(&a, &b).unwrap();
        assert!(resolve(&a).is_err());
    }
}
