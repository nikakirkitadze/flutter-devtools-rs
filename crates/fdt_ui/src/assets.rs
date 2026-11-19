//! Asset source: our icons first, then the component defaults.

use std::borrow::Cow;

use gpui_kit::SharedString;
use gpui_kit::assets::{Assets, icon_assets};

use gpui_kit::AssetSource;

icon_assets!(
    FdtIcons,
    [
        ChartGantt,
        Gauge,
        MemoryStick,
        ArrowDownUp,
        ScrollText,
        ListTree,
        Workflow,
        Upload,
        Settings,
        ChevronLeft,
        ChevronRight,
        Search,
        Keyboard,
        Info
    ]
);

/// Serves the icons fdt uses, then falls back to the component icon bundle.
#[derive(Clone, Copy, Debug, Default)]
pub struct FdtAssets;

impl AssetSource for FdtAssets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = FdtIcons.load(path)? {
            return Ok(Some(bytes));
        }
        // The component bundle errors on a missing path, so check that it has
        // the path first. Any error after that is real: log it and pass it on.
        if !Assets.list(path)?.iter().any(|name| name.as_ref() == path) {
            return Ok(None);
        }
        Assets.load(path).inspect_err(|err| {
            log::warn!("loading component asset {path:?} failed: {err:#}");
        })
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        let mut all = FdtIcons.list(path)?;
        for name in Assets.list(path)? {
            if !all.contains(&name) {
                all.push(name);
            }
        }
        Ok(all)
    }
}

/// Icons by what they mean in fdt.
pub mod icons {
    use gpui_kit::assets::IconName;

    pub const TIMELINE: IconName = IconName::ChartGantt;
    pub const PERFORMANCE: IconName = IconName::Gauge;
    pub const MEMORY: IconName = IconName::MemoryStick;
    pub const NETWORK: IconName = IconName::ArrowDownUp;
    pub const LOGS: IconName = IconName::ScrollText;
    pub const WIDGETS: IconName = IconName::ListTree;
    pub const STATE: IconName = IconName::Workflow;
    pub const EXPORT: IconName = IconName::Upload;
    pub const SETTINGS: IconName = IconName::Settings;
    pub const BACK: IconName = IconName::ChevronLeft;
    pub const FORWARD: IconName = IconName::ChevronRight;
    pub const SEARCH: IconName = IconName::Search;
    pub const KEYBOARD: IconName = IconName::Keyboard;
    pub const INFO: IconName = IconName::Info;
    pub const WARNING: IconName = IconName::TriangleAlert;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_our_icons_and_component_defaults() {
        assert!(matches!(FdtAssets.load("icons/gauge.svg"), Ok(Some(_))));
        assert!(matches!(
            FdtAssets.load("icons/arrow-down.svg"),
            Ok(Some(_))
        ));
        assert!(matches!(FdtAssets.load("icons/nope.svg"), Ok(None)));
    }
}
