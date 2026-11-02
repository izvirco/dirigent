//! The changelog. The repo's `CHANGELOG.md` is compiled in: a `## <version>` section per
//! release (`## <version> (draft)` until it's out), newest first, and nothing else (it's
//! shown as is). It opens by itself on the first start after an update (`after_update`), and
//! from the version beside the sidebar's title; either way the whole history, to scroll down
//! through (`Modal::Changelog`, `Dirigent::open_changelog`).

use crate::app::Dirigent;
use crate::cache::Cache;
use crate::markdown;
use crate::modal;
use crate::select::{Doc, Keys};
use crate::theme::u;
use gpui::{AnyElement, Context, div, prelude::*};
use semver::Version;
use tracing::warn;

const CHANGELOG: &str = include_str!("../../CHANGELOG.md");
/// `kv`: the newest version this data dir has run.
const LAST_VERSION_KEY: &str = "last_version";
const WIDTH: f32 = 620.;

/// At startup: whether this build is newer than the last one this data dir ran (`updated`).
pub fn after_update(cache: &Cache) -> bool {
    updated(cache, &Version::parse(env!("CARGO_PKG_VERSION")).expect("the crate version is semver"))
}

/// Whether `current` is newer than the version kept in `cache`; it's kept from now on, unless
/// a newer one ran here already. A data dir from before the version was kept counts as
/// updated; a new one (nothing kept in `kv` at all) doesn't.
fn updated(cache: &Cache, current: &Version) -> bool {
    let last = cache.get(LAST_VERSION_KEY).and_then(|v| Version::parse(&v).ok());
    if last.is_some_and(|l| l >= *current) {
        return false;
    }
    let used = !cache.kv_empty();
    if let Err(e) = cache.set(LAST_VERSION_KEY, &current.to_string()) {
        warn!("keep the version: {e:#}");
    }
    used
}

/// The modal: the changelog, scrolling once it's taller than the window. Its blocks select
/// and copy like the transcript's (`Doc::Changelog`; `Dirigent::selected_text` renders it
/// again to read them).
pub fn render(this: &Dirigent, cx: &mut Context<Dirigent>) -> AnyElement {
    let keys = &mut Keys::new(&this.selection, Doc::Changelog, 0);
    let column = div().px_8().py_6().child(markdown::render(CHANGELOG, &this.fonts.mono, keys, None, &[]));
    let body = div().id("changelog").min_h_0().overflow_y_scroll().child(column);
    modal::frame(div().w(u(WIDTH)).max_w_full().max_h_full().child(body), cx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_after_an_update() {
        let v = |s: &str| Version::parse(s).unwrap();
        let kept = |cache: &Cache| cache.get(LAST_VERSION_KEY);

        let cache = Cache::in_memory().unwrap();
        cache.set(LAST_VERSION_KEY, "0.1.0").unwrap();
        assert!(updated(&cache, &v("0.3.0")));
        assert_eq!(kept(&cache).as_deref(), Some("0.3.0"));
        // Once only.
        assert!(!updated(&cache, &v("0.3.0")));
        // An older build after a newer one: not an update, and the newer one stays kept.
        assert!(!updated(&cache, &v("0.2.0")));
        assert_eq!(kept(&cache).as_deref(), Some("0.3.0"));

        // A data dir from before the version was kept.
        let cache = Cache::in_memory().unwrap();
        cache.set("pane", "inbox").unwrap();
        assert!(updated(&cache, &v("0.1.0")));

        // A new data dir: not an update, but the version is kept.
        let cache = Cache::in_memory().unwrap();
        assert!(!updated(&cache, &v("0.3.0")));
        assert_eq!(kept(&cache).as_deref(), Some("0.3.0"));
    }
}
