use std::borrow::Cow;

use gpui_kit::*;

#[derive(rust_embed::RustEmbed)]
#[folder = "assets"]
#[include = "icons/**/*.svg"]
#[include = "icons/**/*.png"]
struct Own;

/// 先找 confdeck 自己的图标，找不到再交给 gpui-kit 自带的 Lucide 图标库。
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(file) = Own::get(path) {
            return Ok(Some(file.data));
        }
        gpui_kit::assets::AllAssets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut names: Vec<SharedString> = Own::iter()
            .filter(|name| name.starts_with(path))
            .map(|name| SharedString::from(name.to_string()))
            .collect();
        names.extend(gpui_kit::assets::AllAssets.list(path)?);
        Ok(names)
    }
}
