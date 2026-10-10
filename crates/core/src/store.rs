//! confdeck 自己的数据：`~/.confdeck/providers.json`。
//!
//! 整个文件由程序重写，格式固定（两格缩进、末尾换行）。里面有 Key，所以权限是 0600，
//! 写入先写临时文件再改名，写到一半断电也不会留下半个文件。读不了的文件报错，不覆盖。

use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::provider::{Provider, ProviderError};

pub const VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct ProvidersFile {
    version: u32,
    #[serde(default)]
    providers: Vec<Provider>,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("读写 {path} 失败：{source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("无法解析 {path}（第 {line} 行第 {column} 列）：{message}")]
    Parse {
        path: PathBuf,
        line: usize,
        column: usize,
        message: String,
    },
    #[error("{path} 是更新版本的 confdeck 写的（格式版本 {found}），请先升级")]
    Version { path: PathBuf, found: u32 },
    #[error("{path} 里的数据有误：{source}")]
    Invalid {
        path: PathBuf,
        source: ProviderError,
    },
}

/// 默认的数据文件位置：`~/.confdeck/providers.json`。
pub fn default_path() -> Option<PathBuf> {
    std::env::home_dir().map(|home| home.join(".confdeck").join("providers.json"))
}

/// 读供应商列表。文件不存在时是空列表。
pub fn load(path: &Path) -> Result<Vec<Provider>, StoreError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(StoreError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let file: ProvidersFile = serde_json::from_str(&text).map_err(|err| StoreError::Parse {
        path: path.to_path_buf(),
        line: err.line(),
        column: err.column(),
        message: err.to_string(),
    })?;
    if file.version > VERSION {
        return Err(StoreError::Version {
            path: path.to_path_buf(),
            found: file.version,
        });
    }
    validate(&file.providers).map_err(|source| StoreError::Invalid {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(file.providers)
}

/// 写供应商列表。先校验，校验不过不写。
pub fn save(path: &Path, providers: &[Provider]) -> Result<(), StoreError> {
    validate(providers).map_err(|source| StoreError::Invalid {
        path: path.to_path_buf(),
        source,
    })?;
    let file = ProvidersFile {
        version: VERSION,
        providers: providers.to_vec(),
    };
    let mut bytes = serde_json::to_vec_pretty(&file).expect("providers always serialize");
    bytes.push(b'\n');
    write_private(path, &bytes).map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn validate(providers: &[Provider]) -> Result<(), ProviderError> {
    let mut seen = HashSet::new();
    for provider in providers {
        provider.validate()?;
        if !seen.insert(provider.id.as_str()) {
            return Err(ProviderError::DuplicateId {
                id: provider.id.clone(),
            });
        }
    }
    Ok(())
}

/// 原子地写一个只有自己能读的文件：同目录下的临时文件 → fsync → 改名。
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "路径没有文件名"))?;
    let mut tmp_name = name.to_os_string();
    tmp_name.push(format!(".tmp-{}", std::process::id()));
    let tmp = dir.join(tmp_name);

    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::tests::relay;

    #[test]
    fn missing_file_is_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let providers = load(&dir.path().join("providers.json")).unwrap();
        assert!(providers.is_empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("providers.json");
        let mut second = relay();
        second.id = "second".into();
        let providers = vec![relay(), second];

        save(&path, &providers).unwrap();
        assert_eq!(load(&path).unwrap(), providers);

        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("{\n  \"version\": 1,\n  \"providers\": [\n"),
            "{text}"
        );
        assert!(text.ends_with("}\n"));
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_is_private_and_leaves_no_temp_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("providers.json");
        fs::write(&path, "{\"version\": 1}").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        save(&path, &[relay()]).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("providers.json")]);
    }

    #[test]
    fn broken_file_is_reported_and_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("providers.json");
        let broken = "{\n  \"version\": 1,\n  \"providers\": [\n    {\"id\": \n";
        fs::write(&path, broken).unwrap();

        let err = load(&path).expect_err("must refuse");
        assert!(matches!(err, StoreError::Parse { line: 5, .. }), "{err:?}");
        assert_eq!(fs::read_to_string(&path).unwrap(), broken);
    }

    #[test]
    fn newer_versions_and_duplicate_ids_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("providers.json");
        fs::write(&path, "{\"version\": 2, \"providers\": []}").unwrap();
        assert!(matches!(
            load(&path),
            Err(StoreError::Version { found: 2, .. })
        ));

        let err = save(&path, &[relay(), relay()]).expect_err("must refuse");
        assert!(
            matches!(
                err,
                StoreError::Invalid {
                    source: ProviderError::DuplicateId { .. },
                    ..
                }
            ),
            "{err:?}"
        );
        // 校验不过时不写。
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\"version\": 2, \"providers\": []}"
        );
    }
}
