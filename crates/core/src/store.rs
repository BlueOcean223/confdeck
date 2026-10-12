//! confdeck 自己的数据：`~/.confdeck/providers.json`。
//!
//! 整个文件由程序重写，格式固定（两格缩进、末尾换行）。里面有 Key，所以权限是 0600，
//! 写入先写临时文件再改名，写到一半断电也不会留下半个文件。读不了的文件报错，不覆盖。
//! 每次保存前把旧内容备份一份（提案 08），和上一份相同的不重复存。

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use chrono::Local;
use serde::{Deserialize, Serialize};

use crate::backup::{self, Reason, Shelf};
use crate::data::DataDir;
use crate::engine::{Engine, EngineError, LiveFile};
use crate::fsx::{self, Mode};
use crate::patch::Format;
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
    #[error(transparent)]
    Lock(EngineError),
}

pub fn shelf() -> Shelf {
    Shelf::new("confdeck", "providers.json")
}

/// `providers.json` 当成一个可恢复的文件（备份页「恢复」走 [`Engine::restore`]）。里面有 Key，0600。
pub fn live_file(data: &DataDir) -> LiveFile {
    LiveFile {
        owner: "confdeck",
        path: data.providers(),
        mode: Mode::Private,
        format: Format::Json,
    }
}

/// 读供应商列表。文件不存在时是空列表。
pub fn load(data: &DataDir) -> Result<Vec<Provider>, StoreError> {
    let path = data.providers();
    let Some(bytes) = fsx::read_optional(&path).map_err(|source| StoreError::Io {
        path: path.clone(),
        source,
    })?
    else {
        return Ok(Vec::new());
    };
    parse(&bytes, &path)
}

/// 按 `providers.json` 的格式解析（导入的文件也是这个格式）：版本比程序新、ID 重复、校验不过都报错。
/// `path` 只用在错误信息里。
pub fn parse(bytes: &[u8], path: &Path) -> Result<Vec<Provider>, StoreError> {
    let file: ProvidersFile = serde_json::from_slice(bytes).map_err(|err| StoreError::Parse {
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

/// `providers.json` 的内容：两格缩进，末尾换行。导出的文件也是它。
pub fn to_bytes(providers: &[Provider]) -> Vec<u8> {
    let file = ProvidersFile {
        version: VERSION,
        providers: providers.to_vec(),
    };
    let mut bytes = serde_json::to_vec_pretty(&file).expect("providers always serialize");
    bytes.push(b'\n');
    bytes
}

/// 写供应商列表。先校验，校验不过不写；内容有变化时先备份旧内容。
pub fn save(engine: &Engine, providers: &[Provider]) -> Result<(), StoreError> {
    let data = engine.data();
    let path = data.providers();
    validate(providers).map_err(|source| StoreError::Invalid {
        path: path.clone(),
        source,
    })?;
    let bytes = to_bytes(providers);

    let _lock = engine.lock().map_err(StoreError::Lock)?;
    let io_error = |path: PathBuf| move |source| StoreError::Io { path, source };
    let old = fsx::read_optional(&path).map_err(io_error(path.clone()))?;
    match old {
        Some(old) if old == bytes => return Ok(()),
        Some(old) => {
            let shelf = shelf();
            let now = Local::now().naive_local();
            backup::take(data, &shelf, &old, Reason::BeforeSave, now)
                .map_err(io_error(shelf.dir(data)))?;
        }
        None => {}
    }
    fsx::write_atomic(&path, &bytes, Mode::Private).map_err(io_error(path.clone()))
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

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::provider::tests::relay;

    fn setup() -> (tempfile::TempDir, Engine) {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::new(DataDir::new(dir.path().join(".confdeck")));
        (dir, engine)
    }

    #[test]
    fn missing_file_is_an_empty_list() {
        let (_dir, engine) = setup();
        assert!(load(engine.data()).unwrap().is_empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let (_dir, engine) = setup();
        let mut second = relay();
        second.id = "second".into();
        let providers = vec![relay(), second];

        save(&engine, &providers).unwrap();
        assert_eq!(load(engine.data()).unwrap(), providers);

        let text = fs::read_to_string(engine.data().providers()).unwrap();
        assert!(
            text.starts_with("{\n  \"version\": 1,\n  \"providers\": [\n"),
            "{text}"
        );
        assert!(text.ends_with("}\n"));
    }

    #[test]
    fn each_change_keeps_the_previous_version() {
        let (_dir, engine) = setup();
        let shelf = shelf();
        save(&engine, &[relay()]).unwrap();
        assert!(
            backup::list(engine.data(), &shelf).unwrap().is_empty(),
            "nothing to back up yet"
        );

        let first = fs::read(engine.data().providers()).unwrap();
        let mut renamed = relay();
        renamed.name = "Relay 2".into();
        save(&engine, &[renamed.clone()]).unwrap();
        // 内容没变的保存不写也不备份。
        save(&engine, &[renamed]).unwrap();

        let backups = backup::list(engine.data(), &shelf).unwrap();
        assert_eq!(backups.len(), 1);
        assert_eq!(backups[0].reason, Reason::BeforeSave);
        assert_eq!(fs::read(&backups[0].path).unwrap(), first);
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, engine) = setup();
        let path = engine.data().providers();
        fs::create_dir_all(engine.data().root()).unwrap();
        fs::write(&path, "{\"version\": 1}").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        save(&engine, &[relay()]).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn broken_file_is_reported_and_left_alone() {
        let (_dir, engine) = setup();
        let path = engine.data().providers();
        let broken = "{\n  \"version\": 1,\n  \"providers\": [\n    {\"id\": \n";
        fs::create_dir_all(engine.data().root()).unwrap();
        fs::write(&path, broken).unwrap();

        let err = load(engine.data()).expect_err("must refuse");
        assert!(matches!(err, StoreError::Parse { line: 5, .. }), "{err:?}");
        assert_eq!(fs::read_to_string(&path).unwrap(), broken);
    }

    #[test]
    fn newer_versions_and_duplicate_ids_are_refused() {
        let (_dir, engine) = setup();
        let path = engine.data().providers();
        fs::create_dir_all(engine.data().root()).unwrap();
        fs::write(&path, "{\"version\": 2, \"providers\": []}").unwrap();
        assert!(matches!(
            load(engine.data()),
            Err(StoreError::Version { found: 2, .. })
        ));

        let err = save(&engine, &[relay(), relay()]).expect_err("must refuse");
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
