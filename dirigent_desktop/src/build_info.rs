//! Exposes build identity embedded by the desktop build script.

pub(crate) fn channel() -> &'static str {
    env!("DIRIGENT_UPDATE_CHANNEL")
}

pub(crate) fn version() -> &'static str {
    env!("DIRIGENT_RELEASE_VERSION")
}

pub(crate) fn target() -> &'static str {
    env!("DIRIGENT_UPDATE_TARGET")
}
