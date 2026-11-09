//! Checking a JWT access token the forge issued for a resource (RFC 9068): RS256 only,
//! `typ: at+jwt`, signed by a key of the forge's JWKS (`Keys`, cached, fetched again for an
//! unknown `kid` at most once a minute), and its claims as `Expect` says. The forge's rule
//! for resource servers: check `client_id` as well as `iss`, `aud`, `exp` and `sub`, since
//! any app a user let in can ask for a token naming any resource.

use base64::Engine;
use base64::prelude::BASE64_URL_SAFE_NO_PAD;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Clock skew a token's `exp` is given.
const LEEWAY_SECS: i64 = 30;
/// An unknown `kid` refetches the JWKS, but not more often than this: a stream of made-up
/// kids mustn't become a stream of requests to the forge.
const REFETCH_EVERY: Duration = Duration::from_secs(60);
/// A JWKS fetch that failed is tried again after this; meanwhile tokens with keys we don't
/// have are `Keys` (503 for the api), not `UnknownKey`.
const RETRY_FAILED: Duration = Duration::from_secs(10);

/// What a token must say.
#[derive(Debug, Clone, Copy)]
pub struct Expect<'a> {
    /// The forge's url (`iss`).
    pub issuer: &'a str,
    /// This server, exactly as clients name it (`aud`).
    pub audience: &'a str,
    /// Dirigent's app (`client_id`).
    pub client_id: &'a str,
    /// The one account it must be (`sub`); any if `None`.
    pub subject: Option<&'a str>,
}

/// A checked token's claims.
#[derive(Debug, Clone, Deserialize)]
pub struct Claims {
    pub iss: String,
    /// The forge's user id: who it is, across renames.
    pub sub: String,
    aud: Audience,
    pub client_id: String,
    pub exp: i64,
    #[serde(default)]
    pub preferred_username: String,
}

/// RFC 7519 lets `aud` be one string or several.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

impl Audience {
    fn has(&self, aud: &str) -> bool {
        match self {
            Audience::One(a) => a == aud,
            Audience::Many(all) => all.iter().any(|a| a == aud),
        }
    }
}

/// Why a token was refused. The messages are for whoever sent it: they say what to fix and
/// nothing about the server's account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejected {
    Malformed,
    Algorithm,
    Type,
    UnknownKey,
    /// The JWKS couldn't be fetched.
    Keys(String),
    Signature,
    Issuer,
    Expired,
    Audience {
        expected: String,
    },
    Client,
    /// A valid token, of a forge account other than the one this server takes.
    Subject,
}

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Rejected::Malformed => f.write_str("malformed forge token"),
            Rejected::Algorithm => f.write_str("forge token not signed with RS256"),
            Rejected::Type => f.write_str("not a forge access token (typ at+jwt)"),
            Rejected::UnknownKey => f.write_str("forge token signed by an unknown key"),
            Rejected::Keys(why) => write!(f, "can't check forge tokens now: no forge keys ({why})"),
            Rejected::Signature => f.write_str("forge token signature is invalid"),
            Rejected::Issuer => f.write_str("forge token from another forge"),
            Rejected::Expired => f.write_str("forge token expired"),
            Rejected::Audience { expected } => write!(f, "forge token is for another server; this one is {expected}"),
            Rejected::Client => f.write_str("forge token was issued to another app than Dirigent"),
            Rejected::Subject => f.write_str("forge token is another forge account's than the one this server is signed in with"),
        }
    }
}

impl std::error::Error for Rejected {}

/// The forge's signing keys, by `kid`: RSA modulus and exponent.
pub struct Keys {
    fetch: Box<dyn Fn() -> anyhow::Result<String> + Send + Sync>,
    state: Mutex<KeyState>,
}

/// An RSA public key: modulus and exponent, big-endian.
type RsaKey = (Vec<u8>, Vec<u8>);

#[derive(Default)]
struct KeyState {
    keys: HashMap<String, RsaKey>,
    fetched: Option<Instant>,
    /// The last fetch failed: when, and why.
    failed: Option<(Instant, String)>,
}

impl Keys {
    /// The JWKS at `url`, fetched when first needed (blocking).
    pub fn new(url: String) -> Self {
        Self::with_fetch(move || {
            let agent = ureq::Agent::new_with_config(ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(10))).build());
            Ok(agent.get(&url).call().map_err(|e| anyhow::anyhow!("GET {url}: {e}"))?.body_mut().read_to_string()?)
        })
    }

    /// Keys from `fetch`, which answers with the JWKS's JSON (tests stub it).
    pub fn with_fetch(fetch: impl Fn() -> anyhow::Result<String> + Send + Sync + 'static) -> Self {
        Self { fetch: Box::new(fetch), state: Mutex::default() }
    }

    /// `token` checked against `expect` at unix time `now` (seconds): its claims, or why
    /// not. May fetch the JWKS (blocking): the first time, and for a `kid` it doesn't know.
    pub fn verify(&self, token: &str, expect: &Expect, now: i64) -> Result<Claims, Rejected> {
        #[derive(Deserialize)]
        struct Header {
            alg: String,
            #[serde(default)]
            typ: String,
            #[serde(default)]
            kid: String,
        }
        let mut parts = token.split('.');
        let (Some(header), Some(payload), Some(signature), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
            return Err(Rejected::Malformed);
        };
        let header: Header = decode_json(header)?;
        // Never "none", never HS256 with the public key as the secret: one algorithm.
        if header.alg != "RS256" {
            return Err(Rejected::Algorithm);
        }
        let typ = header.typ.to_ascii_lowercase();
        if typ != "at+jwt" && typ != "application/at+jwt" {
            return Err(Rejected::Type);
        }
        let (n, e) = self.key(&header.kid)?;
        let signature = BASE64_URL_SAFE_NO_PAD.decode(signature).map_err(|_| Rejected::Malformed)?;
        let signed = &token[..header_len(token)];
        ring::signature::RsaPublicKeyComponents { n: &n, e: &e }
            .verify(&ring::signature::RSA_PKCS1_2048_8192_SHA256, signed.as_bytes(), &signature)
            .map_err(|_| Rejected::Signature)?;
        let claims: Claims = decode_json(payload)?;
        if claims.iss.trim_end_matches('/') != expect.issuer.trim_end_matches('/') {
            return Err(Rejected::Issuer);
        }
        if claims.exp + LEEWAY_SECS < now {
            return Err(Rejected::Expired);
        }
        if !claims.aud.has(expect.audience) {
            return Err(Rejected::Audience { expected: expect.audience.to_owned() });
        }
        if claims.client_id != expect.client_id {
            return Err(Rejected::Client);
        }
        if expect.subject.is_some_and(|sub| sub != claims.sub) {
            return Err(Rejected::Subject);
        }
        Ok(claims)
    }

    /// The key `kid` names, fetching the JWKS if it's new to us (rate-limited). A fetch that
    /// failed is `Keys` (the forge's trouble, not the token's) until it may be tried again.
    fn key(&self, kid: &str) -> Result<RsaKey, Rejected> {
        let mut state = self.state.lock().unwrap();
        if let Some(key) = state.keys.get(kid) {
            return Ok(key.clone());
        }
        if let Some((at, why)) = &state.failed
            && at.elapsed() < RETRY_FAILED
        {
            return Err(Rejected::Keys(why.clone()));
        }
        if state.fetched.is_some_and(|at| at.elapsed() < REFETCH_EVERY) {
            return Err(Rejected::UnknownKey);
        }
        // Held while fetching: one fetch at a time, the others then find its keys.
        let fetched = (self.fetch)().and_then(|jwks| parse_jwks(&jwks));
        match fetched {
            Ok(keys) => {
                state.keys = keys;
                state.fetched = Some(Instant::now());
                state.failed = None;
                state.keys.get(kid).cloned().ok_or(Rejected::UnknownKey)
            }
            Err(e) => {
                let why = format!("{e:#}");
                state.failed = Some((Instant::now(), why.clone()));
                Err(Rejected::Keys(why))
            }
        }
    }
}

/// Whether a bearer token has a JWT's form (`header.payload.signature`): how a server tells a
/// forge token from its own kinds before checking it.
pub fn looks_like_jwt(token: &str) -> bool {
    token.split('.').count() == 3
}

/// The JWS signing input's length: up to the second dot.
fn header_len(token: &str) -> usize {
    token.rfind('.').unwrap_or(0)
}

fn decode_json<T: serde::de::DeserializeOwned>(part: &str) -> Result<T, Rejected> {
    let bytes = BASE64_URL_SAFE_NO_PAD.decode(part).map_err(|_| Rejected::Malformed)?;
    serde_json::from_slice(&bytes).map_err(|_| Rejected::Malformed)
}

/// A JWKS's RSA signing keys by kid.
fn parse_jwks(text: &str) -> anyhow::Result<HashMap<String, RsaKey>> {
    #[derive(Deserialize)]
    struct Jwks {
        keys: Vec<Jwk>,
    }
    #[derive(Deserialize)]
    struct Jwk {
        kty: String,
        #[serde(default)]
        kid: String,
        #[serde(default)]
        n: String,
        #[serde(default)]
        e: String,
        #[serde(rename = "use", default)]
        usage: Option<String>,
    }
    let jwks: Jwks = serde_json::from_str(text)?;
    let b64 = |s: &str| BASE64_URL_SAFE_NO_PAD.decode(s);
    Ok(jwks
        .keys
        .into_iter()
        .filter(|k| k.kty == "RSA" && k.usage.as_deref().is_none_or(|u| u == "sig"))
        .filter_map(|k| Some((k.kid, (b64(&k.n).ok()?, b64(&k.e).ok()?))))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Signer, claims};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const NOW: i64 = 1_800_000_000;
    const EXPECT: Expect =
        Expect { issuer: "https://forge.example", audience: "wss://d.example/ws", client_id: "app", subject: Some("u1") };

    /// Keys whose fetch counts its calls and answers with `jwks()`.
    fn keys(jwks: impl Fn() -> String + Send + Sync + 'static) -> (Keys, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let keys = Keys::with_fetch(move || {
            counted.fetch_add(1, Ordering::Relaxed);
            Ok(jwks())
        });
        (keys, calls)
    }

    fn good() -> serde_json::Value {
        claims("https://forge.example", "u1", "wss://d.example/ws", "app", NOW + 600)
    }

    #[test]
    fn accepts_a_good_token_and_refuses_each_wrong_claim() {
        let signer = Signer::new();
        let jwks = signer.jwks();
        let (keys, calls) = keys(move || jwks.clone());
        let verify = |c: serde_json::Value| keys.verify(&signer.sign(&c), &EXPECT, NOW);
        let ok = verify(good()).unwrap();
        assert_eq!((ok.sub.as_str(), ok.preferred_username.as_str()), ("u1", "seb"));
        // Another subject is fine when none is asked for.
        let any = Expect { subject: None, ..EXPECT };
        let mut c = good();
        c["sub"] = "u2".into();
        assert_eq!(keys.verify(&signer.sign(&c), &any, NOW).unwrap().sub, "u2");

        let wrong = |field: &str, value: serde_json::Value| {
            let mut c = good();
            c[field] = value;
            verify(c).unwrap_err()
        };
        assert_eq!(wrong("iss", "https://other.example".into()), Rejected::Issuer);
        assert_eq!(wrong("aud", "wss://d.example:443/ws".into()), Rejected::Audience { expected: "wss://d.example/ws".into() });
        assert_eq!(wrong("client_id", "other-app".into()), Rejected::Client);
        assert_eq!(wrong("sub", "u2".into()), Rejected::Subject);
        assert_eq!(wrong("exp", (NOW - LEEWAY_SECS - 1).into()), Rejected::Expired);
        // Within the leeway.
        assert!(verify(claims("https://forge.example", "u1", "wss://d.example/ws", "app", NOW - 5)).is_ok());
        // An audience list naming us.
        assert!(
            verify({
                let mut c = good();
                c["aud"] = serde_json::json!(["x", "wss://d.example/ws"]);
                c
            })
            .is_ok()
        );
        // One JWKS fetch for all of that.
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn refuses_other_algorithms_types_and_tampering() {
        let signer = Signer::new();
        let jwks = signer.jwks();
        let (keys, _) = keys(move || jwks.clone());
        let token = signer.sign(&good());
        let with_header = |header: serde_json::Value| {
            let rest = token.split_once('.').unwrap().1;
            format!("{}.{rest}", BASE64_URL_SAFE_NO_PAD.encode(header.to_string()))
        };
        let kid = signer.kid();
        let v = |t: &str| keys.verify(t, &EXPECT, NOW).unwrap_err();
        assert_eq!(v(&with_header(serde_json::json!({ "alg": "none", "typ": "at+jwt", "kid": kid }))), Rejected::Algorithm);
        assert_eq!(v(&with_header(serde_json::json!({ "alg": "HS256", "typ": "at+jwt", "kid": kid }))), Rejected::Algorithm);
        // An id_token (typ JWT) isn't an access token.
        assert_eq!(v(&with_header(serde_json::json!({ "alg": "RS256", "typ": "JWT", "kid": kid }))), Rejected::Type);
        // Claims swapped under the signature.
        let mut forged = good();
        forged["sub"] = "u2".into();
        let parts: Vec<&str> = token.split('.').collect();
        let swapped = format!("{}.{}.{}", parts[0], BASE64_URL_SAFE_NO_PAD.encode(forged.to_string()), parts[2]);
        assert_eq!(v(&swapped), Rejected::Signature);
        for junk in ["", "a.b", "a.b.c.d", "eyJ.eyJ.x"] {
            assert_eq!(v(junk), Rejected::Malformed, "{junk}");
        }
    }

    #[test]
    fn unknown_kid_refetches_at_most_once_a_minute() {
        let signer = Signer::new();
        // The forge rotated its key after our first fetch: the JWKS has it only from the
        // second fetch on.
        let rotated = signer.jwks();
        let old = serde_json::json!({ "keys": [] }).to_string();
        let served = Arc::new(Mutex::new(old));
        let (keys, calls) = {
            let served = served.clone();
            keys(move || served.lock().unwrap().clone())
        };
        let token = signer.sign(&good());
        assert_eq!(keys.verify(&token, &EXPECT, NOW).unwrap_err(), Rejected::UnknownKey);
        *served.lock().unwrap() = rotated;
        // Within the minute: no second fetch.
        assert_eq!(keys.verify(&token, &EXPECT, NOW).unwrap_err(), Rejected::UnknownKey);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        keys.state.lock().unwrap().fetched = Some(Instant::now() - REFETCH_EVERY);
        assert!(keys.verify(&token, &EXPECT, NOW).is_ok());
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        // A forge that can't be reached says so.
        let down = Keys::with_fetch(|| anyhow::bail!("connection refused"));
        assert!(matches!(down.verify(&token, &EXPECT, NOW), Err(Rejected::Keys(_))));
    }

    #[test]
    fn a_failed_fetch_is_the_forges_trouble_until_retried() {
        let signer = Signer::new();
        let up = Arc::new(Mutex::new(false));
        let jwks = signer.jwks();
        let (keys, calls) = {
            let up = up.clone();
            keys(move || if *up.lock().unwrap() { jwks.clone() } else { "forge down".into() })
        };
        let token = signer.sign(&good());
        // Not `UnknownKey` (a 401: "your token is bad") while the forge can't be asked.
        assert!(matches!(keys.verify(&token, &EXPECT, NOW), Err(Rejected::Keys(_))));
        *up.lock().unwrap() = true;
        assert!(matches!(keys.verify(&token, &EXPECT, NOW), Err(Rejected::Keys(_))), "backs off");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        keys.state.lock().unwrap().failed.as_mut().unwrap().0 -= RETRY_FAILED;
        assert!(keys.verify(&token, &EXPECT, NOW).is_ok());
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }
}
