//! 界面语言。文案都在 `crates/app/locales/app.yml`，代码里用 `t!("键")` 取，不写死。
//!
//! 用的是 rust-i18n。gpui-component 也用它，「当前语言」是进程级的一份，所以这里设一次，
//! 组件库自带的文案（输入框的右键菜单等）跟着一起换。语言代号照它的：`en`、`zh-CN`。
//! 键不存在时 `t!` 原样返回键名，界面上会直接看到 `agents.xxx`；下面的测试在源码里找所有
//! `t!("…")`，保证每个键在每种语言里都有。

use gpui_kit::*;

/// 支持的语言，按这个顺序匹配系统语言。
pub const LOCALES: &[&str] = &["zh-CN", "en"];

/// 系统语言都不支持时用英文。
const FALLBACK: &str = "en";

/// 启动时定语言：`CONFDECK_LANG` 环境变量 > 系统的首选语言 > 英文。
/// 环境变量给开发和 `drive` 脚本用，不改系统设置也能看另一种语言的界面。
/// 以后设置页存了偏好，插在环境变量和系统语言之间。
pub fn init() {
    let locale = std::env::var("CONFDECK_LANG")
        .ok()
        .and_then(|lang| supported(&lang))
        .or_else(|| sys_locale::get_locales().find_map(|lang| supported(&lang)))
        .unwrap_or(FALLBACK);
    rust_i18n::set_locale(locale);
}

/// 切换语言：所有窗口重画，托盘菜单的文字换掉。不认识的代号不动。
/// 现在只有 `drive` 脚本的 `lang` 命令调它，设置页做好后由设置页调。
#[cfg_attr(not(debug_assertions), allow(dead_code))]
pub fn set(locale: &str, cx: &mut App) {
    let Some(locale) = supported(locale) else {
        return;
    };
    if &*rust_i18n::locale() == locale {
        return;
    }
    rust_i18n::set_locale(locale);
    crate::tray::relabel(cx);
    cx.refresh_windows();
}

/// 把语言代号对到支持的语言，只看语言部分：`zh-Hans-CN`、`zh_CN`、`zh` 都是 `zh-CN`，`en-US` 是 `en`。
fn supported(tag: &str) -> Option<&'static str> {
    let lang = tag.split(['-', '_']).next()?.trim().to_ascii_lowercase();
    LOCALES
        .iter()
        .copied()
        .find(|locale| locale.split('-').next() == Some(lang.as_str()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::Path;

    use super::{LOCALES, supported};

    #[test]
    fn maps_system_tags_to_supported_locales() {
        assert_eq!(supported("zh-Hans-CN"), Some("zh-CN"));
        assert_eq!(supported("zh-Hant-TW"), Some("zh-CN"));
        assert_eq!(supported("zh_CN"), Some("zh-CN"));
        assert_eq!(supported("zh"), Some("zh-CN"));
        assert_eq!(supported("en-US"), Some("en"));
        assert_eq!(supported("EN"), Some("en"));
        assert_eq!(supported("ja-JP"), None);
        assert_eq!(supported(""), None);
    }

    /// 每种语言的键集合一样：少翻了会退回英文，界面上看不出来，所以在这里查。
    #[test]
    fn every_locale_has_the_same_keys() {
        let backend = crate::_rust_i18n_backend();
        let keys = |locale: &str| -> BTreeSet<String> {
            backend
                .messages_for_locale(locale)
                .unwrap_or_default()
                .into_iter()
                .map(|(key, _)| key.into_owned())
                .collect()
        };
        let reference = keys(LOCALES[0]);
        assert!(!reference.is_empty(), "{} has no messages", LOCALES[0]);
        for locale in &LOCALES[1..] {
            let other = keys(locale);
            let missing: Vec<_> = reference.difference(&other).collect();
            let extra: Vec<_> = other.difference(&reference).collect();
            assert!(
                missing.is_empty() && extra.is_empty(),
                "{locale} is missing {missing:?} and has extra {extra:?} compared with {}",
                LOCALES[0]
            );
        }
    }

    /// 源码里 `t!("…")` 用到的每个键在每种语言里都有。`t!` 找不到键时原样返回键名，不报错。
    /// 键用变量传的（标签栏）不在这里查，各自的模块里有测试。
    #[test]
    fn every_key_used_in_source_exists() {
        let backend = crate::_rust_i18n_backend();
        let mut keys = BTreeSet::new();
        collect_keys(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut keys,
        );
        assert!(keys.len() > 10, "found too few keys: {keys:?}");
        let mut missing = Vec::new();
        for key in &keys {
            for locale in LOCALES {
                if backend.translate(locale, key).is_none() {
                    missing.push(format!("{locale}: {key}"));
                }
            }
        }
        assert!(missing.is_empty(), "untranslated keys: {missing:#?}");
    }

    fn collect_keys(dir: &Path, keys: &mut BTreeSet<String>) {
        for entry in std::fs::read_dir(dir).expect("src directory is readable") {
            let path = entry.expect("directory entry").path();
            if path.is_dir() {
                collect_keys(&path, keys);
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("source file is readable");
            // 注释里的 `t!("键")` 是举例，不算。rustfmt 会把长的 `t!(` 拆成几行，键在下一行。
            let code: String = text
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            let mut rest = code.as_str();
            while let Some(start) = rest.find("t!(") {
                // `format!(` 的尾巴也是 `t!(`，只认前面不是标识符、不在字符串里的。
                let is_macro = !rest[..start]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '"');
                rest = rest[start + 3..].trim_start();
                let Some(quoted) = rest.strip_prefix('"') else {
                    continue;
                };
                let Some(end) = quoted.find('"') else { break };
                if is_macro {
                    keys.insert(quoted[..end].to_string());
                }
                rest = &quoted[end..];
            }
        }
    }
}
