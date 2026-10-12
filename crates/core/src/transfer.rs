//! 导入导出供应商（decisions「功能范围」）：存成一个文件带到别的机器，或者从这样的文件加回来。
//! 不是同步（同步在阶段 6，Key 加密后上传），也不导入 ccs-lite 的数据（decisions「产品和工程」）。
//!
//! - **格式**就是 `providers.json` 的格式，覆盖也在里面；导出时可以不带 Key（分享给别人用）。
//! - **导入**（[`merge`]）：新 ID 加在列表最后；同 ID 的换成文件里的那份，文件里没 Key 就留着这边的 Key；
//!   一模一样的不算改动。写进 agent 的事在 [`crate::apply::import`]。

use std::path::Path;

use crate::provider::Provider;
use crate::store::{self, StoreError};

/// 导出文件的内容。`keys` 为假时去掉每家的 Key。
pub fn export(providers: &[Provider], keys: bool) -> Vec<u8> {
    if keys {
        return store::to_bytes(providers);
    }
    let stripped: Vec<Provider> = providers
        .iter()
        .cloned()
        .map(|mut provider| {
            provider.key.clear();
            provider
        })
        .collect();
    store::to_bytes(&stripped)
}

/// 读一个导出的文件：格式、版本、ID、地址都要对，错了报行列（`path` 只用在错误信息里）。地址整理成规范写法。
pub fn read(bytes: &[u8], path: &Path) -> Result<Vec<Provider>, StoreError> {
    let mut providers = store::parse(bytes, path)?;
    for provider in &mut providers {
        provider.normalize();
    }
    Ok(providers)
}

/// 导入会改什么（确认用）：每一项是供应商的名字。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub added: Vec<String>,
    pub replaced: Vec<String>,
    /// 和这边一模一样的，不动。
    pub unchanged: Vec<String>,
}

impl Plan {
    pub fn changes(&self) -> usize {
        self.added.len() + self.replaced.len()
    }
}

pub fn plan(current: &[Provider], incoming: &[Provider]) -> Plan {
    let mut plan = Plan::default();
    for provider in incoming {
        match current.iter().find(|old| old.id == provider.id) {
            None => plan.added.push(provider.name.clone()),
            Some(old) if *old == merged(old, provider) => {
                plan.unchanged.push(provider.name.clone())
            }
            Some(_) => plan.replaced.push(provider.name.clone()),
        }
    }
    plan
}

/// 导入以后的列表：原来的顺序不变，同 ID 的就地换掉，新的接在后面。
pub fn merge(current: &[Provider], incoming: &[Provider]) -> Vec<Provider> {
    let mut list = current.to_vec();
    for provider in incoming {
        match list.iter_mut().find(|old| old.id == provider.id) {
            Some(slot) => *slot = merged(slot, provider),
            None => list.push(provider.clone()),
        }
    }
    list
}

/// 同 ID 的一家换成文件里的；文件里没 Key（不带 Key 导出的）就留着这边的。
fn merged(old: &Provider, incoming: &Provider) -> Provider {
    let mut provider = incoming.clone();
    if provider.key.is_empty() {
        provider.key = old.key.clone();
    }
    provider
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::tests::relay;

    #[test]
    fn export_can_leave_the_keys_out() {
        let providers = vec![relay()];
        let with = export(&providers, true);
        let without = export(&providers, false);
        assert!(String::from_utf8_lossy(&with).contains("sk-relay"));
        assert!(!String::from_utf8_lossy(&without).contains("sk-relay"));
        let back = read(&without, Path::new("x.json")).unwrap();
        assert_eq!(back[0].key, "");
        assert_eq!(back[0].endpoints, relay().endpoints);
    }

    #[test]
    fn import_adds_replaces_and_keeps_keys_the_file_does_not_have() {
        let mut other = relay();
        other.id = "other".into();
        other.name = "Other".into();
        let current = vec![relay(), other.clone()];

        let mut renamed = relay();
        renamed.name = "Relay 2".into();
        renamed.key.clear();
        let mut new = relay();
        new.id = "new".into();
        new.name = "New".into();
        let incoming = vec![renamed, other, new];

        assert_eq!(
            plan(&current, &incoming),
            Plan {
                added: vec!["New".into()],
                replaced: vec!["Relay 2".into()],
                unchanged: vec!["Other".into()],
            }
        );
        let merged = merge(&current, &incoming);
        let ids: Vec<&str> = merged.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["relay", "other", "new"]);
        assert_eq!(merged[0].name, "Relay 2");
        assert_eq!(merged[0].key, "sk-relay", "the file had no key");
    }

    #[test]
    fn broken_files_are_refused() {
        assert!(matches!(
            read(b"{\"version\": 1, \"providers\": [", Path::new("x.json")),
            Err(StoreError::Parse { .. })
        ));
        assert!(matches!(
            read(b"{\"version\": 9, \"providers\": []}", Path::new("x.json")),
            Err(StoreError::Version { found: 9, .. })
        ));
    }
}
