//! Exposes embedded application assets to GPUI.

use std::{
    borrow::Cow,
    collections::HashMap,
    sync::{LazyLock, RwLock},
};

use gpui::{AssetSource, Result, SharedString};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "asset"]
#[include = "icon/*.svg"]
pub(crate) struct Assets;

static GENERATED_SVGS: LazyLock<RwLock<HashMap<String, Vec<u8>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

pub(crate) fn insert_generated_svg(path: String, svg: String) {
    GENERATED_SVGS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(path, svg.into_bytes());
}

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(svg) = GENERATED_SVGS
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(path)
            .cloned()
        {
            return Ok(Some(Cow::Owned(svg)));
        }
        Ok(Self::get(path).map(|asset| asset.data))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(Self::iter()
            .filter(|asset_path| asset_path.starts_with(path))
            .map(SharedString::from)
            .collect())
    }
}
