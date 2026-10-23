//! Remote hosts live in `~/.config/dirigent/hosts.toml`, edited by hand or from the
//! desktop's Hosts page. The file is the truth: the desktop rewrites it and then
//! `Dirigent::sync_hosts` diffs it against the running connections. `local` is implicit and
//! never in the file. The service reads the same file on its machine for `ListHosts`: the
//! hosts its agents can reach (the `dirigent` tool), so it lives here, in the lib both share.
//!
//! ```toml
//! [hosts]
//! hetzner = "ws://hetzner.tail1234.ts.net:7777/ws"
//! ```

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// The implicit host: the service embedded in this desktop.
pub const LOCAL: &str = "local";

const HEADER: &str = "# Dirigent remote hosts. `local` is implicit.\n# name = \"host[:port]\" or a full \"ws://host:port/ws\" url.\n\n";

#[derive(Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    hosts: BTreeMap<String, String>,
}

pub fn path() -> PathBuf {
    crate::config_dir().join("hosts.toml")
}

/// Name → url, normalized. A missing file is an empty list; a broken one is an error so
/// the UI can say so instead of silently dropping hosts.
pub fn load() -> Result<BTreeMap<String, String>> {
    let path = path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let file: File = toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    file.hosts.into_iter().filter(|(name, _)| name != LOCAL).map(|(name, url)| Ok((name, normalize(&url)?))).collect()
}

pub fn save(hosts: &BTreeMap<String, String>) -> Result<()> {
    let path = path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = toml::to_string(&File { hosts: hosts.clone() })?;
    std::fs::write(&path, format!("{HEADER}{body}")).with_context(|| format!("write {}", path.display()))
}

/// `host`, `host:port`, `ws://host:port`, `ws://host:port/ws` → `ws://host:port/ws`.
pub fn normalize(input: &str) -> Result<String> {
    let s = input.trim();
    if s.is_empty() {
        bail!("empty host address");
    }
    let (scheme, rest) = match s.split_once("://") {
        Some((scheme, rest)) if scheme == "ws" || scheme == "wss" => (scheme, rest),
        Some((scheme, _)) => bail!("unsupported scheme {scheme}://"),
        None => ("ws", s),
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/ws"),
    };
    if authority.is_empty() {
        bail!("missing host name");
    }
    // A port is a non-empty tail after the last colon, outside any IPv6 brackets.
    let has_port = authority.rsplit_once(':').is_some_and(|(_, p)| !p.is_empty() && !p.contains(']'));
    let authority = if has_port { authority.to_owned() } else { format!("{authority}:{}", crate::DEFAULT_PORT) };
    Ok(format!("{scheme}://{authority}{path}"))
}

#[cfg(test)]
mod tests {
    use super::normalize;

    #[test]
    fn normalizes_addresses() {
        assert_eq!(normalize("hetzner").unwrap(), "ws://hetzner:7777/ws");
        assert_eq!(normalize(" hetzner:8000 ").unwrap(), "ws://hetzner:8000/ws");
        assert_eq!(normalize("ws://hetzner:8000").unwrap(), "ws://hetzner:8000/ws");
        assert_eq!(normalize("wss://h.example/socket").unwrap(), "wss://h.example:7777/socket");
        assert_eq!(normalize("[fd7a::1]").unwrap(), "ws://[fd7a::1]:7777/ws");
        assert_eq!(normalize("[fd7a::1]:80").unwrap(), "ws://[fd7a::1]:80/ws");
        assert!(normalize("").is_err());
        assert!(normalize("http://x").is_err());
        assert!(normalize("ws:///ws").is_err());
    }
}
