//! The public listener and who may use it.
//!
//! `<config dir>/service.toml` (optional; absent = no public listener) adds a listener for a
//! TLS reverse proxy (Caddy etc.) to forward to:
//!
//! ```toml
//! [public]
//! listen = "127.0.0.1:7778"
//! url = "wss://dirigent.example.com/ws"   # where clients reach it, through the proxy
//! ```
//!
//! Auth is a property of the listener, not of the peer: the proxy connects from loopback
//! too. Every upgrade on the public listener carries `Authorization: Bearer <token>`
//! (`authorize`, before the upgrade); the loopback and tailscale listeners stay open.
//! A token is an API key (`dk_…`, `dirigent_service key add`), kept as its sha256 in
//! `service.db`. A revoked key is refused from its next connect; live connections stay.

use crate::storage::Storage;
use anyhow::{Context, Result};
use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use std::sync::Mutex;

/// What API keys start with: a bearer that does is looked up as one.
const KEY_PREFIX: &str = "dk_";

#[derive(Deserialize, Default)]
pub struct ServiceToml {
    pub public: Option<Public>,
}

#[derive(Deserialize)]
pub struct Public {
    /// Where the proxy forwards to. Any address: every connection must authenticate.
    pub listen: SocketAddr,
    /// The url clients reach it at, through the proxy (logged at start).
    pub url: Option<String>,
}

/// `<config dir>/service.toml`; a missing file is the default (no public listener).
pub fn load() -> Result<ServiceToml> {
    let path = crate::config_dir().join("service.toml");
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("parse {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ServiceToml::default()),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Who a public connection authenticated as.
#[derive(Debug, PartialEq)]
pub enum Principal {
    /// An API key, by its name.
    Key(String),
}

impl std::fmt::Display for Principal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Principal::Key(name) => write!(f, "key {name}"),
        }
    }
}

/// Whether a request on a listener may upgrade: anyone on an open one (`None`), only a
/// known bearer on the public one.
pub fn admit(public: bool, storage: &Mutex<Storage>, headers: &HeaderMap) -> Result<Option<Principal>, &'static str> {
    if public { authorize(storage, headers).map(Some) } else { Ok(None) }
}

/// The bearer token of a request on the public listener, checked: whose it is, or why not.
/// Each kind of token is told by its form; an API key by its prefix.
fn authorize(storage: &Mutex<Storage>, headers: &HeaderMap) -> Result<Principal, &'static str> {
    let value = headers.get(AUTHORIZATION).ok_or("no bearer token")?.to_str().map_err(|_| "bad authorization header")?;
    let (scheme, token) = value.trim().split_once(' ').ok_or("bad authorization header")?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return Err("not a bearer token");
    }
    let token = token.trim();
    if token.starts_with(KEY_PREFIX) {
        let name = storage.lock().unwrap().key_name(&hash(token)).map_err(|_| "key lookup failed")?;
        return name.map(Principal::Key).ok_or("unknown key");
    }
    Err("unknown token")
}

fn hash(key: &str) -> String {
    format!("{:x}", Sha256::digest(key.as_bytes()))
}

/// A new API key's text: `dk_` and 32 random bytes, base64url.
fn new_key() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("random: {e}"))?;
    Ok(format!("{KEY_PREFIX}{}", base64::Engine::encode(&base64::prelude::BASE64_URL_SAFE_NO_PAD, bytes)))
}

pub const KEY_USAGE: &str = "key add <name> | key list | key revoke <name>";

/// `key add <name>` (prints the key, once), `key list`, `key revoke <name>`: the public
/// listener's API keys, in this instance's `service.db`.
pub fn key_command(args: &[String]) -> Result<()> {
    let storage = Storage::open(&crate::data_dir().join("service.db"))?;
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args[..] {
        ["add", name] => {
            let key = new_key()?;
            storage.add_key(name, &hash(&key), crate::runtime::now())?;
            println!("{key}");
            eprintln!("Key {name:?} added. It is shown only now: put it in the client's hosts.toml.");
        }
        ["list"] => {
            for (name, at) in storage.keys()? {
                let at = chrono::DateTime::from_timestamp_millis(at).map(|t| t.format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default();
                println!("{name}\t{at}");
            }
        }
        ["revoke", name] => {
            anyhow::ensure!(storage.revoke_key(name)?, "no key named {name:?}");
            eprintln!("Key {name:?} revoked: new connections with it are refused; open ones stay until they close.");
        }
        _ => anyhow::bail!("usage: {KEY_USAGE}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(auth: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(a) = auth {
            h.insert(AUTHORIZATION, a.parse().unwrap());
        }
        h
    }

    #[test]
    fn keys_are_stored_hashed_and_looked_up() {
        let s = Storage::in_memory().unwrap();
        let key = new_key().unwrap();
        assert!(key.starts_with("dk_") && key.len() >= 3 + 43, "{key}");
        assert_ne!(key, new_key().unwrap());
        s.add_key("laptop", &hash(&key), 1).unwrap();
        assert!(s.add_key("laptop", &hash("dk_other"), 2).is_err());
        assert!(s.add_key(" ", &hash("dk_blank"), 2).is_err());
        assert_eq!(s.key_name(&hash(&key)).unwrap().as_deref(), Some("laptop"));
        assert_eq!(s.key_name(&key).unwrap(), None, "only the hash is a key");
        assert_eq!(s.keys().unwrap(), [("laptop".to_owned(), 1)]);
        assert!(s.revoke_key("laptop").unwrap());
        assert!(!s.revoke_key("laptop").unwrap());
        assert_eq!(s.key_name(&hash(&key)).unwrap(), None);
    }

    #[test]
    fn public_listener_wants_a_known_bearer_open_one_none() {
        let s = Mutex::new(Storage::in_memory().unwrap());
        let key = new_key().unwrap();
        s.lock().unwrap().add_key("ci", &hash(&key), 0).unwrap();
        let public = |auth: Option<&str>| admit(true, &s, &headers(auth));
        assert_eq!(public(Some(&format!("Bearer {key}"))), Ok(Some(Principal::Key("ci".into()))));
        assert_eq!(public(Some(&format!("bearer  {key} "))), Ok(Some(Principal::Key("ci".into()))));
        assert!(public(None).is_err());
        assert!(public(Some("Bearer dk_wrong")).is_err());
        assert!(public(Some(&format!("Basic {key}"))).is_err());
        assert!(public(Some("Bearer eyJhbGciOi.x.y")).is_err());
        s.lock().unwrap().revoke_key("ci").unwrap();
        assert!(public(Some(&format!("Bearer {key}"))).is_err());
        // Loopback and tailscale: no bearer needed.
        assert_eq!(admit(false, &s, &headers(None)), Ok(None));
    }
}
