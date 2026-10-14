use std::borrow::Cow;

use gpui_kit::assets::{Assets as ComponentIcons, icon_assets};
use gpui_kit::*;

#[derive(rust_embed::RustEmbed)]
#[folder = "assets"]
#[include = "icons/**/*.svg"]
#[include = "icons/**/*.png"]
struct Own;

// 界面上用到的 Lucide 图标（`ui::icon` 的名字，`chevron-down` 写成 `ChevronDown`）。新用一个图标要加在这里，
// 不然画出来是空的；debug 构建会打出缺了哪个。
icon_assets!(
    Lucide,
    [
        Activity,
        ArrowLeft,
        ArrowUpRight,
        Check,
        ChevronDown,
        ChevronLeft,
        ChevronRight,
        ChevronUp,
        CircleAlert,
        CircleCheck,
        Copy,
        Ellipsis,
        ExternalLink,
        Eye,
        EyeOff,
        Info,
        KeyRound,
        Plus,
        RotateCcw,
        RotateCw,
        Search,
        Settings,
        SquareArrowOutUpRight,
        StickyNote,
        Terminal,
        X,
    ]
);

/// 先找 confdeck 自己的图标，再找上面挑的 Lucide 图标，最后是组件库自己用的那些（gpui-kit 的 `Assets`，约 100 个）。
/// 不用带全部 1830 个 Lucide 图标的 `AllAssets`，二进制小 1 MB。
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(file) = Own::get(path) {
            return Ok(Some(file.data));
        }
        if let Some(data) = Lucide.load(path)? {
            return Ok(Some(data));
        }
        let found = ComponentIcons.load(path);
        #[cfg(debug_assertions)]
        if !matches!(found, Ok(Some(_))) {
            eprintln!("asset missing: {path}");
        }
        found
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut names: Vec<SharedString> = Own::iter()
            .filter(|name| name.starts_with(path))
            .map(|name| SharedString::from(name.to_string()))
            .collect();
        names.extend(Lucide.list(path)?);
        names.extend(ComponentIcons.list(path)?);
        Ok(names)
    }
}
