use crate::app::Dirigent;
use crate::cache::Cache;
use crate::markdown;
use crate::modal;
use crate::select::{Doc, Keys};
use crate::theme::u;
use gpui::{AnyElement, Context, div, prelude::*};
use semver::Version;
use tracing::warn;

const CHANGELOG: &str = include_str!("../../changelog.md");
const LAST_VERSION_KEY: &str = "last_version";
const WIDTH: f32 = 620.;

pub fn after_update(cache: &Cache) -> bool {
    updated(cache, &Version::parse(dirigent_protocol::release::VERSION).expect("the version is semver"))
}

// MUST KILL: rename: updated -> record_version, it keeps current as the last version while it checks
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

pub fn render(this: &Dirigent, cx: &mut Context<Dirigent>) -> AnyElement {
    let keys = &mut Keys::new(&this.selection, Doc::Changelog, 0);
    let column = div().px_8().py_6().child(markdown::render(CHANGELOG, &this.fonts.mono, keys, None, None, &[]));
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
        assert!(!updated(&cache, &v("0.3.0")));
        assert!(!updated(&cache, &v("0.2.0")));
        assert_eq!(kept(&cache).as_deref(), Some("0.3.0"));

        let cache = Cache::in_memory().unwrap();
        cache.set("pane", "inbox").unwrap();
        assert!(updated(&cache, &v("0.1.0")));

        let cache = Cache::in_memory().unwrap();
        assert!(!updated(&cache, &v("0.3.0")));
        assert_eq!(kept(&cache).as_deref(), Some("0.3.0"));
    }
}
