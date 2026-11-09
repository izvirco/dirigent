//! The public listener and who may use it.
//!
//! `<config dir>/service.toml` (optional; absent = no public listener) adds a listener for a
//! TLS reverse proxy (Caddy etc.) to forward to:
//!
//! ```toml
//! [public]
//! listen = "127.0.0.1:7778"
//! url = "wss://dirigent.example.com/ws"   # the address clients use, through the proxy
//! ```
//!
//! Auth is a property of the listener, not of the peer: the proxy connects from loopback
//! too. Every upgrade on the public listener carries `Authorization: Bearer <token>`
//! (`authorize`, before the upgrade); the loopback and tailscale listeners stay open.
//! A token is one of, told apart by its form:
//! - an API key (`dk_…`, `dirigent_service key add`), kept as its sha256 in `service.db`.
//!   A revoked key is refused from its next connect; live connections stay.
//! - a forge JWT (`header.payload.signature`), taken while this service is signed in to the
//!   forge (`forge.rs`) and only of that same account: the forge's signature (`Gate`'s
//!   JWKS), `iss` the forge, `aud` this service's `[public] url`, `client_id` Dirigent's app,
//!   `sub` the account this service is signed in with, not expired. Clients mint it for the
//!   host's url in their `hosts.toml`; both sides normalize it (`hosts::normalize`), so
//!   `[public] url` must be the address clients use, in any spelling of it. A token stays
//!   good for up to ten minutes after the account revokes Dirigent on the forge.

use crate::Service;
use crate::storage::Storage;
use anyhow::{Context, Result};
use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use dirigent_auth::jwt::{Expect, Keys};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

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
    /// The url clients reach it at, through the proxy: what forge tokens must be issued for
    /// (their audience). Without it, only API keys.
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

/// What the public listener checks forge tokens with.
pub struct Gate {
    /// `[public] url`, normalized: the audience forge tokens must name; or why there's none.
    audience: Result<String, String>,
    forge: dirigent_auth::Forge,
    /// The forge's signing keys, fetched when first needed.
    keys: Keys,
}

impl Gate {
    pub fn new(public: &Public) -> Self {
        let audience = match &public.url {
            None => Err("this service has no [public] url in service.toml".to_owned()),
            Some(url) => crate::hosts::normalize(url).map_err(|e| format!("this service's [public] url is invalid ({e:#})")),
        };
        let forge = crate::forge::forge().clone();
        let keys = Keys::new(forge.jwks_url());
        Self { audience, forge, keys }
    }

    /// The audience forge tokens must name, or why none are taken.
    pub fn audience(&self) -> Result<&str, &str> {
        self.audience.as_deref().map_err(String::as_str)
    }
}

/// Who a public connection authenticated as.
#[derive(Debug, PartialEq)]
pub enum Principal {
    /// An API key, by its name.
    Key(String),
    /// A forge token of the account this service is signed in with, by username.
    Forge(String),
}

impl std::fmt::Display for Principal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Principal::Key(name) => write!(f, "key {name}"),
            Principal::Forge(user) => write!(f, "forge account {user}"),
        }
    }
}

/// Whether a request on a listener may upgrade: anyone on an open one (no gate: `None`),
/// only a valid bearer on the public one. Off the async threads: checking a forge token
/// may fetch the forge's keys.
pub async fn admit(gate: Option<Arc<Gate>>, svc: Arc<Service>, headers: HeaderMap) -> Result<Option<Principal>, String> {
    let Some(gate) = gate else { return Ok(None) };
    let account = || crate::forge::status().signed_in().map(|a| a.sub.clone());
    tokio::task::spawn_blocking(move || authorize(&gate, &svc.storage, &headers, account, now()))
        .await
        .map_err(|e| format!("auth check failed: {e}"))?
        .map(Some)
}

/// The bearer token of a request on the public listener, checked: whose it is, or why not
/// (said to the client: what to fix, nothing about this service's account). `account` is
/// the forge account this service is signed in with, asked for only for a forge token.
fn authorize(
    gate: &Gate,
    storage: &Mutex<Storage>,
    headers: &HeaderMap,
    account: impl FnOnce() -> Option<String>,
    now: i64,
) -> Result<Principal, String> {
    let value = headers.get(AUTHORIZATION).ok_or("no bearer token")?.to_str().map_err(|_| "bad authorization header")?;
    let (scheme, token) = value.trim().split_once(' ').ok_or("bad authorization header")?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return Err("not a bearer token".into());
    }
    let token = token.trim();
    if token.starts_with(KEY_PREFIX) {
        let name = storage.lock().unwrap().key_name(&hash(token)).map_err(|_| "key lookup failed")?;
        return name.map(Principal::Key).ok_or_else(|| "unknown key".into());
    }
    if dirigent_auth::jwt::looks_like_jwt(token) {
        let audience = gate.audience.as_ref().map_err(|why| format!("this host takes API keys only: {why}"))?;
        let Some(sub) = account() else {
            return Err("this host isn't signed in to the forge, so it takes API keys only".into());
        };
        let expect = Expect { issuer: &gate.forge.url, audience, client_id: &gate.forge.client_id, subject: Some(&sub) };
        let claims = gate.keys.verify(token, &expect, now).map_err(|e| e.to_string())?;
        return Ok(Principal::Forge(claims.preferred_username));
    }
    Err("unknown token".into())
}

fn hash(key: &str) -> String {
    format!("{:x}", Sha256::digest(key.as_bytes()))
}

fn now() -> i64 {
    std::time::UNIX_EPOCH.elapsed().map(|d| d.as_secs() as i64).unwrap_or(0)
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
    use dirigent_auth::testing::{Signer, claims};

    fn headers(auth: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(a) = auth {
            h.insert(AUTHORIZATION, a.parse().unwrap());
        }
        h
    }

    const FORGE: &str = "https://forge.example";
    const NOW: i64 = 1_800_000_000;

    /// A gate for `url`, with the test signer's keys.
    fn test_gate(url: Option<&str>, signer: &Signer) -> Gate {
        let public = Public { listen: "127.0.0.1:0".parse().unwrap(), url: url.map(String::from) };
        let jwks = signer.jwks();
        Gate {
            audience: Gate::new(&public).audience,
            forge: dirigent_auth::Forge::new(FORGE, "app"),
            keys: Keys::with_fetch(move || Ok(jwks.clone())),
        }
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
    fn public_listener_wants_a_known_key() {
        let s = Mutex::new(Storage::in_memory().unwrap());
        let key = new_key().unwrap();
        s.lock().unwrap().add_key("ci", &hash(&key), 0).unwrap();
        let gate = test_gate(Some("wss://d.example/ws"), &Signer::new());
        let public = |auth: Option<&str>| authorize(&gate, &s, &headers(auth), || None, NOW);
        assert_eq!(public(Some(&format!("Bearer {key}"))), Ok(Principal::Key("ci".into())));
        assert_eq!(public(Some(&format!("bearer  {key} "))), Ok(Principal::Key("ci".into())));
        assert!(public(None).is_err());
        assert!(public(Some("Bearer dk_wrong")).is_err());
        assert!(public(Some(&format!("Basic {key}"))).is_err());
        assert!(public(Some("Bearer nonsense")).is_err());
        s.lock().unwrap().revoke_key("ci").unwrap();
        assert!(public(Some(&format!("Bearer {key}"))).is_err());
    }

    #[test]
    fn forge_tokens_of_the_services_own_account_for_its_url() {
        let s = Mutex::new(Storage::in_memory().unwrap());
        let signer = Signer::new();
        // Written another way than clients normalize it: still the same audience.
        let gate = test_gate(Some("WSS://d.example:443/ws"), &signer);
        let jwt = |sub: &str, aud: &str| format!("Bearer {}", signer.sign(&claims(FORGE, sub, aud, "app", NOW + 600)));
        let check = |auth: &str, account: Option<&str>| authorize(&gate, &s, &headers(Some(auth)), || account.map(String::from), NOW);
        assert_eq!(check(&jwt("u1", "wss://d.example/ws"), Some("u1")), Ok(Principal::Forge("seb".into())));
        let refused = |auth: &str, account: Option<&str>| check(auth, account).unwrap_err();
        assert!(refused(&jwt("u2", "wss://d.example/ws"), Some("u1")).contains("another forge account"));
        let wrong_aud = refused(&jwt("u1", "wss://other.example/ws"), Some("u1"));
        assert!(wrong_aud.contains("wss://d.example/ws"), "{wrong_aud}");
        assert!(refused(&jwt("u1", "wss://d.example/ws"), None).contains("isn't signed in"));
        // The service's sub isn't said.
        assert!(!refused(&jwt("u2", "wss://d.example/ws"), Some("u1")).contains("u1"));
        // No public url: no audience to check, so no forge tokens.
        let no_url = test_gate(None, &signer);
        let e = authorize(&no_url, &s, &headers(Some(&jwt("u1", "wss://d.example/ws"))), || Some("u1".into()), NOW).unwrap_err();
        assert!(e.contains("[public] url"), "{e}");
    }
}
