//! Releases and self-update: what `dirigent_api` serves at
//! `/api/v0/updates/{channel}/{platform}` and the desktop's `update.rs` checks every few
//! minutes. Everything here is a contract with every installed version: change it additively.

use serde::{Deserialize, Serialize};

/// A channel's current release for one platform. `dirigent_api publish` writes it to the
/// bucket's `channels/{channel}/{platform}.json` and the api serves it as is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Release {
    /// Semver, compared against the running build's `CARGO_PKG_VERSION`.
    pub version: String,
    /// The installer (Windows) to download. Its minisign signature is at `{url}.minisig`.
    pub url: String,
    #[serde(default)]
    pub notes: String,
}

/// The minisign public keys releases are signed with. The desktop installs nothing signed
/// by another key. Rotating: ship the new key next to the old one, sign with the new one
/// once every install has it, then drop the old one. The secret key is the release
/// workflow's `DIRIGENT_SIGNING_KEY`.
pub const KEYS: &[&str] = &["RWT82xTuCE1EejF1X7w7AlD8DJdb3VVsT33rDa94gewvnwteQIjTkzIQ"];

/// The signed (trusted) comment of a release's signature: it binds the signature to the
/// version and platform, so an older signed installer can't be passed off as a newer one.
pub fn trusted_comment(version: &str, platform: &str) -> String {
    format!("dirigent {version} {platform}")
}

/// This build's platform as releases name it: `windows-x86_64`, `linux-x86_64`, ...
pub fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// A channel or platform name: it becomes part of urls and bucket keys.
pub fn valid_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}
