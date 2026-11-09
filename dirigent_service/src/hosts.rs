//! Remote hosts live in `<config dir>/hosts.toml`, edited by hand or from the
//! desktop's Settings › Hosts. The file is the truth: the desktop rewrites it and then
//! `Dirigent::sync_hosts` diffs it against the running connections. `local` is implicit and
//! never in the file. The service reads the same file on its machine for `ListHosts`: the
//! hosts its agents can reach (the `dirigent` tool), so it lives here, in the lib both share.
//!
//! ```toml
//! [hosts]
//! hetzner = "ws://hetzner.tail1234.ts.net:7777/ws"          # tailscale: no auth
//! box = "ssh://seb@box.example.com"                          # over ssh (`tunnel.rs`)
//! prod = { url = "wss://dirigent.example.com/ws", key = "dk_…" }  # public: an API key
//! ```
//!
//! A host is a string (its address) or a table (`url` plus how to authenticate: `key`, an
//! API key from the remote's `dirigent_service key add`). The file holds keys, so it is
//! written owner-only on unix.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// The implicit host: the service embedded in this desktop.
pub const LOCAL: &str = "local";

const HEADER: &str = "# Dirigent remote hosts. `local` is implicit.\n\
# name = \"host[:port]\", a full \"ws://host:port/ws\" / \"wss://…\" url, or\n\
# \"ssh://[user@]host[:ssh port][/service port]\" (an ssh_config alias works; service port\n\
# default 7777). With an API key (a public service): name = { url = \"wss://…/ws\", key = \"dk_…\" }\n\n";

/// A host as `hosts.toml` has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    /// Normalized (`normalize`): `ws://`, `wss://` or `ssh://`.
    pub url: String,
    /// An API key of the remote's public listener, sent as `Authorization: Bearer`.
    pub key: Option<String>,
}

impl Host {
    pub fn new(url: String) -> Self {
        Self { url, key: None }
    }

    /// Where to connect and the bearer to send: an `ssh://` host through its tunnel
    /// (started on demand, `tunnel::open`), the others as they are.
    pub async fn endpoint(&self) -> Result<Endpoint> {
        let url = if self.url.starts_with("ssh://") { crate::tunnel::open(&self.url).await? } else { self.url.clone() };
        Ok(Endpoint { url, bearer: self.key.clone() })
    }
}

/// A host's resolved address: a `ws(s)://` url and the bearer token to connect with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub url: String,
    pub bearer: Option<String>,
}

pub fn path() -> PathBuf {
    crate::config_dir().join("hosts.toml")
}

/// Name → host, normalized. A missing file is an empty list; a broken one is an error so
/// the UI can say so instead of silently dropping hosts.
pub fn load() -> Result<BTreeMap<String, Host>> {
    let path = path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    parse(&text).with_context(|| format!("parse {}", path.display()))
}

fn parse(text: &str) -> Result<BTreeMap<String, Host>> {
    // Its message and line only: toml's own report quotes the line, which may hold a key.
    let mut file: toml::Table = toml::from_str(text).map_err(|e| {
        let line = e.span().map(|s| text[..s.start].matches('\n').count() + 1);
        anyhow::anyhow!("{}{}", e.message().replace('\n', ": "), line.map(|l| format!(" (line {l})")).unwrap_or_default())
    })?;
    let hosts = match file.remove("hosts") {
        None => return Ok(BTreeMap::new()),
        Some(toml::Value::Table(t)) => t,
        Some(_) => bail!("`hosts` must be a table"),
    };
    let mut out = BTreeMap::new();
    for (name, value) in hosts {
        if name == LOCAL {
            continue;
        }
        let host = match value {
            toml::Value::String(url) => Host::new(normalize(&url)?),
            toml::Value::Table(mut t) => {
                let mut string = |field: &str| match t.remove(field) {
                    None => Ok(None),
                    Some(toml::Value::String(s)) => Ok(Some(s)),
                    Some(_) => bail!("{name}.{field} must be a string"),
                };
                let url = string("url")?.with_context(|| format!("{name} has no url"))?;
                Host { url: normalize(&url)?, key: string("key")?.filter(|k| !k.is_empty()) }
            }
            _ => bail!("{name} must be an address or a table"),
        };
        out.insert(name, host);
    }
    Ok(out)
}

pub fn save(hosts: &BTreeMap<String, Host>) -> Result<()> {
    let path = path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_private(&path, &format!("{HEADER}{}", render(hosts))).with_context(|| format!("write {}", path.display()))
}

/// The `[hosts]` table: a plain host as its url, one with a key as an inline table.
fn render(hosts: &BTreeMap<String, Host>) -> String {
    let string = |s: &str| toml::Value::String(s.to_owned()).to_string();
    let mut out = String::from("[hosts]\n");
    for (name, host) in hosts {
        let bare = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        let name = if bare { name.clone() } else { string(name) };
        let value = match &host.key {
            None => string(&host.url),
            Some(key) => format!("{{ url = {}, key = {} }}", string(&host.url), string(key)),
        };
        out.push_str(&format!("{name} = {value}\n"));
    }
    out
}

/// Write `text` to `path` readable by its owner only (unix; it holds keys).
fn write_private(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.mode(0o600);
        // `mode` only applies to a new file: tighten an existing one before writing to it.
        if path.exists() {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    options.open(path)?.write_all(text.as_bytes())
}

/// `host`, `host:port`, `ws://host:port`, `ws://host:port/ws` → `ws://host:port/ws`.
/// `wss://` is a TLS proxy in front of a public listener: no port means 443, as anywhere.
/// `ssh://…` as `tunnel::Ssh` reads it, written back canonically.
pub fn normalize(input: &str) -> Result<String> {
    let s = input.trim();
    if s.is_empty() {
        bail!("empty host address");
    }
    if s.starts_with("ssh://") {
        return Ok(crate::tunnel::Ssh::parse(s)?.to_string());
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
    let authority = if has_port || scheme == "wss" { authority.to_owned() } else { format!("{authority}:{}", crate::DEFAULT_PORT) };
    Ok(format!("{scheme}://{authority}{path}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_addresses() {
        assert_eq!(normalize("hetzner").unwrap(), "ws://hetzner:7777/ws");
        assert_eq!(normalize(" hetzner:8000 ").unwrap(), "ws://hetzner:8000/ws");
        assert_eq!(normalize("ws://hetzner:8000").unwrap(), "ws://hetzner:8000/ws");
        assert_eq!(normalize("wss://h.example").unwrap(), "wss://h.example/ws");
        assert_eq!(normalize("wss://h.example:8443/socket").unwrap(), "wss://h.example:8443/socket");
        assert_eq!(normalize("[fd7a::1]").unwrap(), "ws://[fd7a::1]:7777/ws");
        assert_eq!(normalize("[fd7a::1]:80").unwrap(), "ws://[fd7a::1]:80/ws");
        assert_eq!(normalize("ssh://seb@box:22/7777/").unwrap(), "ssh://seb@box:22");
        assert!(normalize("").is_err());
        assert!(normalize("http://x").is_err());
        assert!(normalize("ws:///ws").is_err());
    }

    #[test]
    fn parses_and_round_trips_both_forms() {
        let text = r#"
            [hosts]
            plain = "hetzner"
            box = "ssh://box/7778"
            prod = { url = "wss://d.example.com/ws", key = "dk_abc" }
            local = "ignored"
        "#;
        let hosts = parse(text).unwrap();
        let want = BTreeMap::from([
            ("plain".to_owned(), Host::new("ws://hetzner:7777/ws".into())),
            ("box".to_owned(), Host::new("ssh://box/7778".into())),
            ("prod".to_owned(), Host { url: "wss://d.example.com/ws".into(), key: Some("dk_abc".into()) }),
        ]);
        assert_eq!(hosts, want);
        let rendered = render(&hosts);
        assert!(rendered.contains("plain = \"ws://hetzner:7777/ws\"\n"), "{rendered}");
        assert!(rendered.contains("prod = { url = \"wss://d.example.com/ws\", key = \"dk_abc\" }\n"), "{rendered}");
        assert_eq!(parse(&format!("{HEADER}{rendered}")).unwrap(), want);
        // Names that aren't bare keys are quoted.
        let odd = BTreeMap::from([("my host".to_owned(), Host::new("ws://h:1/ws".into()))]);
        assert_eq!(parse(&render(&odd)).unwrap(), odd);
        assert!(parse("[hosts]\nx = { key = \"dk_\" }").is_err());
        assert!(parse("[hosts]\nx = 1").is_err());
        // A syntax error says where, without quoting the line (it may hold a key).
        let e = parse("[hosts]\nx = { url = \"a\", key = \"dk_secret\" \n").unwrap_err().to_string();
        assert!(e.contains("line 2") && !e.contains("dk_secret"), "{e}");
    }
}
