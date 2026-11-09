//! Dirigent's side of the forge (`https://forge.sebba.dev`, an OpenID provider): the
//! requests Dirigent's public app makes (`Forge`), and checking the JWTs the forge issues
//! for a resource (`jwt`).
//!
//! Dirigent is a public app (no client secret): a browser sign-in is the authorization code
//! flow with PKCE, its code sent to `http://127.0.0.1:<any port>/forge/callback`
//! (`CALLBACK_PATH`); a headless one the device flow. Either ends in a refresh token, good
//! until it's revoked or goes 90 days unused (the caller keeps it: `dirigent_service`
//! `forge.rs`), and each refresh names a `resource`: the access token is then a
//! ten-minute JWT for that server alone (`aud`), which the server checks with `jwt::Keys`.

pub mod jwt;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

use anyhow::{Context, anyhow};
use base64::Engine;
use base64::prelude::BASE64_URL_SAFE_NO_PAD;
use serde::Deserialize;
use std::time::Duration;

/// The forge, unless `DIRIGENT_FORGE_URL` says otherwise (tests: a local one).
pub const DEFAULT_URL: &str = "https://forge.sebba.dev";

/// Dirigent's public app on the forge, registered on Seb's account with the redirect URI
/// `http://127.0.0.1/forge/callback` (`DIRIGENT_FORGE_CLIENT_ID` overrides it). Desktops in
/// the field sign in as it: never change it.
pub const CLIENT_ID: &str = "01a1badc-4343-7193-b334-456b91f6ef5a";

/// The path of the loopback redirect URI the app is registered with; any port matches.
pub const CALLBACK_PATH: &str = "/forge/callback";

/// What Dirigent asks for: who you are (username, picture), nothing more.
const SCOPE: &str = "profile";

/// The forge and Dirigent's app on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forge {
    /// Its url, no trailing slash: the `iss` of its tokens.
    pub url: String,
    pub client_id: String,
}

/// What a token request ends in.
#[derive(Debug, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    /// A sign-in's refresh token (a refresh's answer repeats the one sent).
    pub refresh_token: Option<String>,
    /// Seconds the access token is good for.
    pub expires_in: u64,
}

/// A started device sign-in: show `user_code` and `verification_uri`, then `poll`.
#[derive(Debug, Deserialize)]
pub struct Device {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    /// Seconds to wait between polls.
    pub interval: u64,
    /// Seconds until `device_code` expires.
    pub expires_in: u64,
}

/// Who signed in, from the forge's userinfo.
#[derive(Debug, Clone, Deserialize)]
pub struct UserInfo {
    pub sub: String,
    pub preferred_username: String,
    #[serde(default)]
    pub picture: String,
}

/// A request that failed: the forge's refusal (RFC 6749 §5.2), or no answer.
#[derive(Debug)]
pub enum Error {
    /// `error` is the OAuth code (`invalid_grant`, `authorization_pending`, …).
    OAuth {
        error: String,
        description: String,
    },
    Http(anyhow::Error),
}

impl Error {
    pub fn is(&self, code: &str) -> bool {
        matches!(self, Error::OAuth { error, .. } if error == code)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::OAuth { error, description } if description.is_empty() => write!(f, "the forge said {error}"),
            Error::OAuth { description, .. } => write!(f, "the forge said: {description}"),
            Error::Http(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for Error {}

impl Forge {
    /// `DIRIGENT_FORGE_URL` / `DIRIGENT_FORGE_CLIENT_ID`, else the real forge and app.
    pub fn from_env() -> Self {
        let var = |name: &str| std::env::var(name).ok().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
        Self::new(
            &var("DIRIGENT_FORGE_URL").unwrap_or_else(|| DEFAULT_URL.into()),
            &var("DIRIGENT_FORGE_CLIENT_ID").unwrap_or_else(|| CLIENT_ID.into()),
        )
    }

    pub fn new(url: &str, client_id: &str) -> Self {
        Self { url: url.trim_end_matches('/').to_owned(), client_id: client_id.to_owned() }
    }

    pub fn jwks_url(&self) -> String {
        format!("{}/api/oauth/jwks", self.url)
    }

    /// Where the forge account page lets you revoke Dirigent: signing out for good.
    pub fn account_page(&self) -> String {
        format!("{}/settings", self.url)
    }

    /// The browser's sign-in page, answering at `redirect` with `state` (PKCE: `challenge`).
    pub fn authorize_url(&self, redirect: &str, state: &str, challenge: &str) -> String {
        let query = form_urlencoded::Serializer::new(String::new())
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", redirect)
            .append_pair("scope", SCOPE)
            .append_pair("state", state)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256")
            .finish();
        format!("{}/oauth/authorize?{query}", self.url)
    }

    /// A browser sign-in's code for its tokens (blocking).
    pub fn exchange_code(&self, code: &str, verifier: &str, redirect: &str) -> Result<Tokens, Error> {
        self.token(&[("grant_type", "authorization_code"), ("code", code), ("code_verifier", verifier), ("redirect_uri", redirect)])
    }

    /// An access token for a refresh token: a JWT for `resource` (blocking).
    pub fn refresh(&self, refresh_token: &str, resource: &str) -> Result<Tokens, Error> {
        self.token(&[("grant_type", "refresh_token"), ("refresh_token", refresh_token), ("resource", resource)])
    }

    /// Start a device sign-in (blocking).
    pub fn device(&self) -> Result<Device, Error> {
        let url = format!("{}/api/oauth/device", self.url);
        post(&url, &[("client_id", &self.client_id), ("scope", SCOPE)])
    }

    /// One poll of a device sign-in: `authorization_pending` until someone answers (blocking).
    pub fn poll(&self, device_code: &str) -> Result<Tokens, Error> {
        self.token(&[("grant_type", "urn:ietf:params:oauth:grant-type:device_code"), ("device_code", device_code)])
    }

    /// Who an (opaque) access token is for (blocking).
    pub fn userinfo(&self, access_token: &str) -> Result<UserInfo, Error> {
        let url = format!("{}/api/oauth/userinfo", self.url);
        let mut res = agent()
            .get(&url)
            .header("authorization", format!("Bearer {access_token}"))
            .call()
            .map_err(|e| Error::Http(anyhow!("GET {url}: {e}")))?;
        let body = res.body_mut().read_to_string().map_err(|e| Error::Http(e.into()))?;
        serde_json::from_str(&body).with_context(|| format!("GET {url}: {body}")).map_err(Error::Http)
    }

    fn token(&self, form: &[(&str, &str)]) -> Result<Tokens, Error> {
        let mut form = form.to_vec();
        form.push(("client_id", &self.client_id));
        post(&format!("{}/api/oauth/token", self.url), &form)
    }
}

/// POST a form; the answer as `T`, or the forge's OAuth error.
fn post<T: serde::de::DeserializeOwned>(url: &str, form: &[(&str, &str)]) -> Result<T, Error> {
    let http = |e: anyhow::Error| Error::Http(e.context(format!("POST {url}")));
    let mut res =
        agent().post(url).config().http_status_as_error(false).build().send_form(form.iter().copied()).map_err(|e| http(e.into()))?;
    let status = res.status();
    let body = res.body_mut().read_to_string().map_err(|e| http(e.into()))?;
    if !status.is_success() {
        #[derive(Deserialize)]
        struct Refusal {
            error: String,
            #[serde(default)]
            error_description: String,
        }
        return Err(match serde_json::from_str::<Refusal>(&body) {
            Ok(r) => Error::OAuth { error: r.error, description: r.error_description },
            Err(_) => http(anyhow!("{status} {}", body.chars().take(200).collect::<String>())),
        });
    }
    serde_json::from_str(&body).map_err(|e| http(anyhow!("{e}: {}", body.chars().take(200).collect::<String>())))
}

fn agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .user_agent(format!("Dirigent/{}", env!("CARGO_PKG_VERSION")))
            .timeout_global(Some(Duration::from_secs(20)))
            .build(),
    )
}

/// `n` random bytes, base64url: a PKCE verifier, a sign-in's state.
pub fn random(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes).expect("system randomness");
    BASE64_URL_SAFE_NO_PAD.encode(bytes)
}

/// A PKCE pair (RFC 7636, S256): the verifier to keep, the challenge to send.
pub fn pkce() -> (String, String) {
    let verifier = random(32);
    let challenge = challenge(&verifier);
    (verifier, challenge)
}

fn challenge(verifier: &str) -> String {
    BASE64_URL_SAFE_NO_PAD.encode(ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_the_verifiers_sha256() {
        let (verifier, challenge) = pkce();
        assert_eq!((verifier.len(), challenge.len()), (43, 43));
        assert_eq!(challenge, super::challenge(&verifier));
        // A known pair (`openssl dgst -sha256 -binary | base64url`).
        assert_eq!(super::challenge("dBjftJeZ4CVP-mJ92Y_UTeTd17ACIvzwPqOdnHFdT4M"), "42yQDYzNTx-4CZHCqLSnfKs8Q_KaySTuFfX-DdV_abU");
    }

    #[test]
    fn authorize_url_carries_the_request() {
        let forge = Forge::new("https://forge.example/", "app");
        let url = forge.authorize_url("http://127.0.0.1:5000/forge/callback", "st", "ch");
        assert_eq!(
            url,
            "https://forge.example/oauth/authorize?response_type=code&client_id=app&redirect_uri=http%3A%2F%2F127.0.0.1%3A5000%2Fforge%2Fcallback\
             &scope=profile&state=st&code_challenge=ch&code_challenge_method=S256"
        );
    }
}
