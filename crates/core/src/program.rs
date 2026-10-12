//! 找 agent 的命令行程序（`codex`、`grok`）。

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

/// 先找 `PATH`，再找常见的安装位置：从 Finder 启动的应用拿到的 `PATH` 只有系统目录。
pub fn find(home: &Path, name: &str) -> Option<PathBuf> {
    search(
        home,
        env::var_os("PATH").as_deref(),
        &["/opt/homebrew/bin", "/usr/local/bin"],
        name,
    )
}

/// mise、asdf 的 shim 按当前目录选 Node 版本，在这里用的是全局的那个。
fn search(home: &Path, path: Option<&OsStr>, system: &[&str], name: &str) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = path.map(env::split_paths).into_iter().flatten().collect();
    dirs.extend(
        [
            ".local/bin",
            ".npm-global/bin",
            ".bun/bin",
            ".volta/bin",
            "Library/pnpm",
            ".local/share/mise/shims",
            ".asdf/shims",
            ".cargo/bin",
            // Grok Build 的安装脚本装在这里。
            ".grok/bin",
        ]
        .iter()
        .map(|dir| home.join(dir)),
    );
    dirs.extend(system.iter().map(PathBuf::from));
    // nvm 装的 Node 各版本，新的在前。
    if let Ok(versions) = fs::read_dir(home.join(".nvm/versions/node")) {
        let mut versions: Vec<PathBuf> = versions
            .filter_map(Result::ok)
            .map(|entry| entry.path().join("bin"))
            .collect();
        versions.sort();
        dirs.extend(versions.into_iter().rev());
    }
    dirs.into_iter()
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_looks_in_path_then_the_usual_places() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let on_path = dir.path().join("bin");
        assert_eq!(search(&home, None, &[], "codex"), None);

        let nvm = home.join(".nvm/versions/node/v22.1.0/bin");
        fs::create_dir_all(&nvm).unwrap();
        fs::write(nvm.join("codex"), "").unwrap();
        assert_eq!(search(&home, None, &[], "codex"), Some(nvm.join("codex")));

        fs::create_dir_all(home.join(".local/bin")).unwrap();
        fs::write(home.join(".local/bin/codex"), "").unwrap();
        assert_eq!(
            search(&home, None, &[], "codex"),
            Some(home.join(".local/bin/codex"))
        );

        fs::create_dir_all(&on_path).unwrap();
        fs::write(on_path.join("codex"), "").unwrap();
        let path = env::join_paths([&on_path]).unwrap();
        assert_eq!(
            search(&home, Some(&path), &[], "codex"),
            Some(on_path.join("codex"))
        );

        fs::create_dir_all(home.join(".grok/bin")).unwrap();
        fs::write(home.join(".grok/bin/grok"), "").unwrap();
        assert_eq!(
            search(&home, None, &[], "grok"),
            Some(home.join(".grok/bin/grok"))
        );
    }
}
