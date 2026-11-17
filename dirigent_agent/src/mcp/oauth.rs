use super::client::{Error, PROTOCOL_VERSION};
use super::config::{self, OAuth, hex, is_secure, namespace};
use super::http::{ERROR_BODY_BYTES, http, network};
use crate::config::{Dirs, write_private};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use url::Url;

const REQUEST_LIMIT: Duration = Duration::from_secs(15);
const REFRESH_AHEAD: u64 = 30;
const BROWSER_LIMIT: Duration = Duration::from_secs(300);
const CLIENT_NAME: &str = "Dirigent";

#[async_trait]
pub trait SignIn: Send + Sync {
    /// The user is to sign in at `url`, in a browser.
    fn open(&self, url: &str);

    fn progress(&self, message: &str);

    /// Ask for the address the browser ended up on (`http://127.0.0.1:…/callback?code=…`),
    /// for when it can't reach this host: asked right after `open`, and dropped once the
    /// browser did get here (the question needs no answer any more). `None`, or nothing
    /// pasted: the user gave the sign-in up.
    async fn redirect_url(&self) -> Option<String>;
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Login {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client: Option<Registered>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_server: Option<AuthServer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Registered {
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub redirect_uris: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_endpoint_auth_method: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Tokens {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct AuthServer {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_types_supported: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_challenge_methods_supported: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_endpoint_auth_methods_supported: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_response_iss_parameter_supported: Option<bool>,
}

pub(crate) struct Store {
    path: PathBuf,
}

static FILE: Mutex<()> = Mutex::new(());

impl Store {
    pub(crate) fn new(dirs: &Dirs) -> Self {
        Self { path: dirs.data.join("mcp-auth.json") }
    }

    pub(crate) fn key(name: &str, url: &Url) -> String {
        format!("{}|{url}", namespace(name))
    }

    fn read(&self) -> BTreeMap<String, Login> {
        let Ok(text) = std::fs::read_to_string(&self.path) else { return BTreeMap::new() };
        serde_json::from_str(&text).unwrap_or_else(|e| {
            tracing::warn!("{}: {e}: its sign-ins are lost", self.path.display());
            BTreeMap::new()
        })
    }

    pub(crate) fn load(&self, key: &str) -> Login {
        self.read().remove(key).unwrap_or_default()
    }

    pub(crate) fn update(&self, key: &str, change: impl FnOnce(&mut Login)) -> Result<()> {
        let _one_at_a_time = FILE.lock().unwrap();
        let mut all = self.read();
        let mut login = all.remove(key).unwrap_or_default();
        change(&mut login);
        if login != Login::default() {
            all.insert(key.to_owned(), login);
        }
        let save = || {
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let partial = self.path.with_extension("json.part");
            write_private(&partial, &serde_json::to_string_pretty(&all)?)?;
            std::fs::rename(&partial, &self.path)
        };
        save().with_context(|| format!("write {}", self.path.display()))
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Challenge {
    resource_metadata: Option<Url>,
    scope: Option<String>,
    error: Option<String>,
}

impl Challenge {
    fn parse(header: Option<&str>) -> Self {
        let Some(header) = header else { return Self::default() };
        let scheme = header.split_whitespace().next().unwrap_or_default();
        if !scheme.eq_ignore_ascii_case("bearer") && !scheme.eq_ignore_ascii_case("dpop") {
            return Self::default();
        }
        // `name=value` or `name="value"`, after the scheme or a comma.
        let field = |name: &str| {
            let (lower, needle) = (header.to_ascii_lowercase(), format!("{name}="));
            let starts = lower.match_indices(&needle).map(|(at, _)| at);
            let at = starts.into_iter().find(|at| *at > 0 && matches!(lower.as_bytes()[*at - 1], b',' | b' ' | b'\t'))?;
            let rest = &header[at + needle.len()..];
            let value = match rest.strip_prefix('"') {
                Some(quoted) => quoted.split('"').next(),
                None => rest.split([',', ' ', '\t']).next(),
            };
            value.filter(|v| !v.is_empty()).map(str::to_owned)
        };
        Self {
            resource_metadata: field("resource_metadata").and_then(|url| Url::parse(&url).ok()),
            scope: field("scope"),
            error: field("error"),
        }
    }
}

pub(crate) struct Auth {
    store: Store,
    key: String,
    name: String,
    url: Url,
    settings: OAuth,
    challenge: Mutex<Challenge>,
}

/// One refresh at a time of a sign-in (by its key in the store): servers rotate refresh
/// tokens, and two refreshes with the same one lose the sign-in.
static REFRESH: Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>> = Mutex::new(BTreeMap::new());

impl Auth {
    pub(crate) fn new(dirs: &Dirs, name: &str, url: &Url, settings: Option<&OAuth>) -> Self {
        let store = Store::new(dirs);
        Self {
            store,
            key: Store::key(name, url),
            name: name.to_owned(),
            url: url.clone(),
            settings: settings.cloned().unwrap_or_default(),
            challenge: Mutex::default(),
        }
    }

    pub(crate) fn stored_token(&self) -> Option<String> {
        self.store.load(&self.key).tokens.map(|tokens| tokens.access_token)
    }

    pub(crate) fn sign_out(&self) -> Result<bool> {
        let had = self.store.load(&self.key) != Login::default();
        self.store.update(&self.key, |login| *login = Login::default())?;
        Ok(had)
    }

    pub(crate) async fn token(self: &Arc<Self>) -> Option<String> {
        let tokens = self.store.load(&self.key).tokens?;
        let lapses = tokens.expires_at.is_some_and(|at| at <= now() + REFRESH_AHEAD);
        if !lapses || tokens.refresh_token.is_none() {
            return Some(tokens.access_token);
        }
        let _ = self.refresh(Some(&tokens.access_token)).await;
        self.stored_token()
    }

    pub(crate) async fn unauthorized(self: &Arc<Self>, www_authenticate: Option<&str>, sent: Option<&str>) -> Result<(), Error> {
        let challenge = Challenge::parse(www_authenticate);
        let more_scope = challenge.error.as_deref() == Some("insufficient_scope");
        *self.challenge.lock().unwrap() = challenge;
        // A refresh keeps the scope: more of it is a new sign-in.
        if more_scope {
            return Err(Error::AuthRequired);
        }
        self.refresh(sent).await
    }

    async fn refresh(self: &Arc<Self>, stale: Option<&str>) -> Result<(), Error> {
        let (this, stale) = (self.clone(), stale.map(str::to_owned));
        let refreshing = tokio::spawn(async move { this.refresh_now(stale.as_deref()).await });
        refreshing.await.unwrap_or(Err(Error::Closed))
    }

    async fn refresh_now(&self, stale: Option<&str>) -> Result<(), Error> {
        let refresh = REFRESH.lock().unwrap().entry(self.key.clone()).or_default().clone();
        let _one_at_a_time = refresh.lock().await;
        let login = self.store.load(&self.key);
        let Some(tokens) = &login.tokens else { return Err(Error::AuthRequired) };
        if Some(tokens.access_token.as_str()) != stale {
            return Ok(());
        }
        let (Some(refresh_token), Some(client)) = (&tokens.refresh_token, self.client(&login).await?) else {
            return Err(Error::AuthRequired);
        };
        let failed = |e: Error| Error::Other(format!("Refreshing the sign-in failed: {e}"));
        let (server, resource) = match &login.auth_server {
            Some(server) => (server.clone(), login.resource.clone()),
            None => {
                let found = self.discover().await.map_err(failed)?;
                (found.server, found.resource)
            }
        };
        let grant = [("grant_type", "refresh_token"), ("refresh_token", refresh_token.as_str())];
        let fresh = match token_request(&server, &client, resource.as_deref(), &grant).await {
            Ok(fresh) => fresh,
            Err(Refusal::Said { code, .. }) if matches!(code.as_str(), "invalid_grant" | "invalid_client" | "unauthorized_client") => {
                let gone = |login: &mut Login| {
                    login.tokens = None;
                    if code != "invalid_grant" {
                        *login = Login::default();
                    }
                };
                return if self.settle(tokens, gone)? { Err(Error::AuthRequired) } else { Ok(()) };
            }
            Err(e) => return Err(failed(Error::Other(e.to_string()))),
        };
        // A refresh that says no more of them keeps the refresh token and the scope.
        let fresh = Tokens {
            refresh_token: fresh.refresh_token.or(tokens.refresh_token.clone()),
            scope: fresh.scope.or(tokens.scope.clone()),
            ..fresh
        };
        self.settle(tokens, |login| login.tokens = Some(fresh)).map(drop)
    }

    // MUST KILL: rename: `settle` → `store_if_still` returning a named outcome instead of a bare bool (false: the user signed in or out meanwhile)
    fn settle(&self, refreshed: &Tokens, change: impl FnOnce(&mut Login)) -> Result<bool, Error> {
        let mut still = false;
        let stored = self.store.update(&self.key, |login| {
            still = login.tokens.as_ref() == Some(refreshed);
            if still {
                change(login);
            }
        });
        stored.map(|()| still).map_err(|e| Error::Other(format!("{e:#}")))
    }

    async fn client(&self, login: &Login) -> Result<Option<Registered>, Error> {
        let Some(id) = &self.settings.client_id else { return Ok(login.client.clone()) };
        let client_secret = match &self.settings.client_secret {
            Some(secret) => {
                Some(config::resolve(secret, &format!("MCP server \"{}\": oauth.clientSecret", self.name)).await.map_err(Error::Other)?)
            }
            None => None,
        };
        Ok(Some(Registered { client_id: id.clone(), client_secret, ..Registered::default() }))
    }

    async fn discover(&self) -> Result<Found, Error> {
        let hint = self.challenge.lock().unwrap().resource_metadata.clone();
        discover(&self.url, hint.as_ref(), self.settings.auth_server_metadata_url.as_deref()).await
    }

    pub(crate) async fn sign_in(&self, ui: &dyn SignIn) -> Result<()> {
        ui.progress("Asking the server how to sign in…");
        let found = self.discover().await?;
        let login = self.store.load(&self.key);
        let registered = login.client.as_ref().and_then(|client| client.redirect_uris.first());
        let callback = Callback::listen(&self.settings, registered.map(String::as_str)).await?;
        let client = match self.client(&login).await? {
            // One registered for where this sign-in listens. Any other can't be
            // redirected here: it goes, and the tokens that were its own.
            Some(client) if self.settings.client_id.is_some() || client.redirect_uris.contains(&callback.redirect) => client,
            _ => {
                let name = self.settings.client_name.as_deref().unwrap_or(CLIENT_NAME);
                let client = register(&found.server, &callback.redirect, name).await?;
                self.store.update(&self.key, |login| *login = Login { client: Some(client.clone()), ..Login::default() })?;
                client
            }
        };

        let challenge = self.challenge.lock().unwrap().clone();
        let granted = login.tokens.as_ref().and_then(|tokens| tokens.scope.as_deref());
        let more = challenge.error.as_deref() == Some("insufficient_scope") && challenge.scope.is_some();
        let scope = scopes([self.settings.scope.as_deref(), granted.filter(|_| more), challenge.scope.as_deref()]).or(found.scopes.clone());

        let verifier = URL_SAFE_NO_PAD.encode(random::<32>());
        let state = hex(&random::<32>());
        let url = authorization_url(&found, &client.client_id, &callback.redirect, &state, &verifier, scope.as_deref())?;
        ui.open(url.as_str());
        let answer = tokio::select! {
            answer = callback.wait(&state) => answer?,
            pasted = ui.redirect_url() => match pasted.filter(|pasted| !pasted.trim().is_empty()) {
                Some(pasted) => Answer::pasted(&pasted, &callback.redirect, &state)?,
                None => bail!("Sign-in cancelled"),
            },
            () = tokio::time::sleep(BROWSER_LIMIT) => bail!("Nobody signed in within {} minutes", BROWSER_LIMIT.as_secs() / 60),
        };
        // A code is only taken to the server it is from (RFC 9207).
        if (answer.iss.is_some() || found.server.authorization_response_iss_parameter_supported == Some(true))
            && answer.iss.as_deref() != Some(&found.server.issuer)
        {
            bail!("The sign-in came back from {}, not from {}", answer.iss.as_deref().unwrap_or("nobody it names"), found.server.issuer);
        }

        let grant = [
            ("grant_type", "authorization_code"),
            ("code", answer.code.as_str()),
            ("code_verifier", verifier.as_str()),
            ("redirect_uri", callback.redirect.as_str()),
        ];
        let tokens = match token_request(&found.server, &client, found.resource.as_deref(), &grant).await {
            Ok(tokens) => tokens,
            Err(e) => {
                if matches!(&e, Refusal::Said { code, .. } if code == "invalid_client" || code == "unauthorized_client") {
                    self.store.update(&self.key, |login| *login = Login::default())?;
                }
                bail!("{e}");
            }
        };
        // An answer that names no scope granted what was asked for.
        let tokens = Tokens { scope: tokens.scope.or(scope), ..tokens };
        self.store.update(&self.key, |login| {
            login.tokens = Some(tokens);
            login.auth_server = Some(found.server.clone());
            login.resource = found.resource.clone();
        })?;
        *self.challenge.lock().unwrap() = Challenge::default();
        Ok(())
    }
}

fn now() -> u64 {
    std::time::UNIX_EPOCH.elapsed().map_or(0, |since| since.as_secs())
}

fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).expect("the system's randomness");
    bytes
}

fn scopes<const N: usize>(lists: [Option<&str>; N]) -> Option<String> {
    let mut all: Vec<&str> = vec![];
    for scope in lists.into_iter().flatten().flat_map(str::split_whitespace) {
        if !all.contains(&scope) {
            all.push(scope);
        }
    }
    (!all.is_empty()).then(|| all.join(" "))
}

struct Found {
    server: AuthServer,
    resource: Option<String>,
    scopes: Option<String>,
}

enum Miss {
    Status(u16),
    Network(Error),
    Invalid,
}

impl Miss {
    fn elsewhere(&self) -> bool {
        matches!(self, Self::Status(status) if (400..500).contains(status) || *status == 502)
    }
}

async fn metadata(url: &Url) -> Result<Value, Miss> {
    let request = http().get(url.clone()).header("Accept", "application/json").header("MCP-Protocol-Version", PROTOCOL_VERSION);
    let response = request.timeout(REQUEST_LIMIT).send().await.map_err(|e| Miss::Network(network(e)))?;
    if !response.status().is_success() {
        return Err(Miss::Status(response.status().as_u16()));
    }
    let body = response.bytes().await.map_err(|e| Miss::Network(network(e)))?;
    serde_json::from_slice(&body).map_err(|_| Miss::Invalid)
}

fn well_known(base: &Url, kind: &str) -> Url {
    let path = base.path().strip_suffix('/').unwrap_or(base.path());
    base.join(&format!("/.well-known/{kind}{path}")).expect("a path")
}

/// A server without metadata is its own authorization server, at the endpoints MCP once fixed.
async fn discover(url: &Url, hint: Option<&Url>, metadata_url: Option<&str>) -> Result<Found, Error> {
    let places = match hint {
        Some(hint) => vec![hint.clone()],
        None if url.path() == "/" => vec![well_known(url, "oauth-protected-resource")],
        None => vec![well_known(url, "oauth-protected-resource"), url.join("/.well-known/oauth-protected-resource").expect("a path")],
    };
    let mut resource = None;
    for place in places {
        match metadata(&place).await {
            Ok(found) => resource = Some(found),
            Err(Miss::Network(e)) => return Err(e),
            Err(miss) if miss.elsewhere() => continue,
            Err(_) => {}
        }
        break;
    }
    let strings = |field: &str| strings(resource.as_ref(), field);
    let scopes = Some(strings("scopes_supported").join(" ")).filter(|scopes| !scopes.is_empty());
    let resource_id = match resource.as_ref().and_then(|r| r.get("resource")?.as_str()) {
        Some(id) => Some(resource_for(url, id)?),
        None => None,
    };

    let load = |miss: Miss, at: &Url| match miss {
        Miss::Network(e) => e,
        Miss::Status(status) => Error::Other(format!("The authorization server's metadata at {at} is not to be had (HTTP {status})")),
        Miss::Invalid => Error::Other(format!("The authorization server's metadata at {at} is not JSON")),
    };
    if let Some(written) = metadata_url {
        let at = Url::parse(written)
            .ok()
            .filter(is_secure)
            .ok_or_else(|| Error::Other(format!("oauth.authServerMetadataUrl ({written}) must be an https URL")))?;
        let server = AuthServer::parse(metadata(&at).await.map_err(|miss| load(miss, &at))?)?;
        return Ok(Found { server, resource: resource_id, scopes });
    }
    let issuer = strings("authorization_servers").first().map(|s| (*s).to_owned()).unwrap_or_else(|| url.join("/").expect("a path").into());
    let base = Url::parse(&issuer).ok().filter(is_http);
    let base =
        base.ok_or_else(|| Error::Other(format!("The server names an authorization server that is no http(s) address: {issuer}")))?;
    let mut places = vec![well_known(&base, "oauth-authorization-server"), well_known(&base, "openid-configuration")];
    if base.path() != "/" {
        // OpenID Connect's other place, after the issuer's path. Set, not joined: a path
        // that starts with `//` would be read as a host, another one or none at all.
        let mut after = base.clone();
        after.set_path(&format!("{}/.well-known/openid-configuration", base.path().trim_end_matches('/')));
        after.set_query(None);
        after.set_fragment(None);
        places.push(after);
    }
    for place in places {
        let found = match metadata(&place).await {
            Ok(found) => found,
            Err(miss) if miss.elsewhere() => continue,
            Err(miss) => return Err(load(miss, &place)),
        };
        let server = AuthServer::parse(found)?;
        if server.issuer.trim_end_matches('/') != issuer.trim_end_matches('/') {
            return Err(Error::Other(format!("The authorization server at {issuer} says it is {}", server.issuer)));
        }
        return Ok(Found { server, resource: resource_id, scopes });
    }
    let at = |path: &str| base.join(path).expect("a path").to_string();
    let server = AuthServer {
        issuer,
        authorization_endpoint: at("/authorize"),
        token_endpoint: at("/token"),
        registration_endpoint: Some(at("/register")),
        ..AuthServer::default()
    };
    Ok(Found { server, resource: resource_id, scopes })
}

fn strings<'a>(document: Option<&'a Value>, field: &str) -> Vec<&'a str> {
    let list = document.and_then(|document| document.get(field)?.as_array());
    list.into_iter().flatten().filter_map(Value::as_str).collect()
}

fn resource_for(server: &Url, named: &str) -> Result<String, Error> {
    let within = |named: &Url| {
        let dir = |url: &Url| format!("{}/", url.path().trim_end_matches('/'));
        named.origin() == server.origin() && dir(server).starts_with(&dir(named))
    };
    match Url::parse(named) {
        Ok(url) if within(&url) => Ok(named.to_owned()),
        _ => Err(Error::Other(format!("The server says it is {named}, which {server} isn't part of"))),
    }
}

impl AuthServer {
    fn parse(document: Value) -> Result<Self, Error> {
        let unusable = |why: String| Error::Other(format!("The authorization server's metadata is not usable: {why}"));
        let server: Self = serde_json::from_value(document).map_err(|e| unusable(e.to_string()))?;
        for address in [&server.issuer, &server.authorization_endpoint, &server.token_endpoint] {
            if !Url::parse(address).is_ok_and(|url| is_http(&url)) {
                return Err(unusable(format!("{address} is no http(s) address")));
            }
        }
        Ok(Self { registration_endpoint: server.registration_endpoint.filter(|at| !at.is_empty()), ..server })
    }
}

fn is_http(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
}

fn start_of(body: &str) -> &str {
    let body = body.trim();
    &body[..body.floor_char_boundary(ERROR_BODY_BYTES)]
}

async fn register(server: &AuthServer, redirect: &str, name: &str) -> Result<Registered> {
    let Some(endpoint) = &server.registration_endpoint else {
        bail!(
            "The authorization server doesn't register clients itself: register one with it by hand and name it in the entry (oauth.clientId)"
        )
    };
    let endpoint = Url::parse(endpoint).ok().filter(is_http);
    let endpoint = endpoint.context("The authorization server registers clients at what is no http(s) address")?;
    let client = json!({
        "client_name": name,
        "redirect_uris": [redirect],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
        // OpenID Connect servers take a client for a web app unless told, and refuse
        // those a redirect to this machine.
        "application_type": "native",
    });
    let request = http().post(endpoint).header("Accept", "application/json").header("Content-Type", "application/json");
    let response = request.body(client.to_string()).timeout(REQUEST_LIMIT).send().await.map_err(network)?;
    let status = response.status();
    let body = response.text().await.map_err(network)?;
    if !status.is_success() {
        // Some servers only register the clients they know, by name.
        let hint = if status.as_u16() == 403 && name == CLIENT_NAME {
            "The server may only let clients it knows sign in: give the entry a client name it accepts (oauth.clientName), such as Claude Code. "
        } else {
            ""
        };
        bail!("{hint}The authorization server refused to register the app (HTTP {status}): {}", start_of(&body));
    }
    let registered: Registered =
        serde_json::from_str(&body).map_err(|e| anyhow!("The authorization server's registration is not usable: {e}"))?;
    let redirect_uris = if registered.redirect_uris.is_empty() { vec![redirect.to_owned()] } else { registered.redirect_uris };
    Ok(Registered { client_secret: registered.client_secret.filter(|secret| !secret.is_empty()), redirect_uris, ..registered })
}

fn authorization_url(found: &Found, client_id: &str, redirect: &str, state: &str, verifier: &str, scope: Option<&str>) -> Result<Url> {
    let server = &found.server;
    if server.response_types_supported.as_ref().is_some_and(|types| !types.iter().any(|t| t == "code")) {
        bail!("The authorization server doesn't do the code flow, the one this app signs in with");
    }
    if server.code_challenge_methods_supported.as_ref().is_some_and(|methods| !methods.iter().any(|m| m == "S256")) {
        bail!("The authorization server doesn't do PKCE with S256, which this app signs in with");
    }
    let mut url = Url::parse(&server.authorization_endpoint)
        .ok()
        .filter(is_http)
        .context("The authorization server's sign-in page is no http(s) address")?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier));
    let mut query = url.query_pairs_mut();
    query.append_pair("response_type", "code").append_pair("client_id", client_id);
    query.append_pair("code_challenge", &challenge).append_pair("code_challenge_method", "S256");
    query.append_pair("redirect_uri", redirect).append_pair("state", state);
    if let Some(scope) = scope {
        query.append_pair("scope", scope);
        // A refresh token is only given with the user's consent, which is then asked for.
        if scope.split_whitespace().any(|s| s == "offline_access") {
            query.append_pair("prompt", "consent");
        }
    }
    if let Some(resource) = &found.resource {
        query.append_pair("resource", resource);
    }
    drop(query);
    Ok(url)
}

enum Refusal {
    Said { code: String, description: String },
    Other(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Said { code, description } if code == description => write!(f, "The authorization server said {code}"),
            Self::Said { code, description } => write!(f, "The authorization server said {code}: {description}"),
            Self::Other(why) => f.write_str(why),
        }
    }
}

async fn token_request(
    server: &AuthServer,
    client: &Registered,
    resource: Option<&str>,
    grant: &[(&str, &str)],
) -> Result<Tokens, Refusal> {
    let endpoint = Url::parse(&server.token_endpoint).ok().filter(is_secure);
    let endpoint = endpoint
        .ok_or_else(|| Refusal::Other(format!("The token endpoint {} is not https: nothing is sent there", server.token_endpoint)))?;
    let offered = server.token_endpoint_auth_methods_supported.as_deref().unwrap_or_default();
    let takes = |method: &str| offered.is_empty() || offered.iter().any(|m| m == method);
    let registered = client
        .token_endpoint_auth_method
        .as_deref()
        .filter(|m| matches!(*m, "client_secret_basic" | "client_secret_post" | "none") && takes(m));
    let method = registered.unwrap_or(match &client.client_secret {
        Some(_) if takes("client_secret_basic") => "client_secret_basic",
        Some(_) if takes("client_secret_post") || !takes("none") => "client_secret_post",
        _ => "none",
    });
    let request = {
        let mut form = url::form_urlencoded::Serializer::new(String::new());
        form.extend_pairs(grant);
        if let Some(resource) = resource {
            form.append_pair("resource", resource);
        }
        let mut request =
            http().post(endpoint).header("Accept", "application/json").header("Content-Type", "application/x-www-form-urlencoded");
        match (&client.client_secret, method) {
            (Some(secret), "client_secret_basic") => request = request.basic_auth(&client.client_id, Some(secret)),
            (secret, _) => {
                form.append_pair("client_id", &client.client_id);
                if let Some(secret) = secret.as_ref().filter(|_| method == "client_secret_post") {
                    form.append_pair("client_secret", secret);
                }
            }
        }
        request.body(form.finish())
    };
    let other = |e: reqwest::Error| Refusal::Other(network(e).to_string());
    let response = request.timeout(REQUEST_LIMIT).send().await.map_err(other)?;
    let status = response.status();
    let text = response.text().await.map_err(other)?;
    let answer: Value = serde_json::from_str(&text).unwrap_or_default();
    // An error may come with any status: the body says first.
    if let Some(code) = answer.get("error").and_then(Value::as_str) {
        let description = answer.get("error_description").and_then(Value::as_str).unwrap_or(code);
        return Err(Refusal::Said { code: code.to_owned(), description: description.to_owned() });
    }
    let text_of = |field: &str| answer.get(field).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
    let Some(access_token) = text_of("access_token").filter(|_| status.is_success()) else {
        let said = if status.is_success() { String::new() } else { format!(": {}", start_of(&text)) };
        return Err(Refusal::Other(format!("The authorization server gave no token (HTTP {status}){said}")));
    };
    // Seconds, as a number or, from some, as text.
    let expires_in = answer.get("expires_in").and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok()));
    Ok(Tokens {
        access_token,
        refresh_token: text_of("refresh_token"),
        scope: text_of("scope"),
        expires_at: expires_in.map(|seconds| now() + seconds.max(0.0) as u64),
    })
}

#[derive(Debug, PartialEq)]
struct Answer {
    code: String,
    iss: Option<String>,
}

impl Answer {
    fn parse(query: &HashMap<String, String>, state: &str) -> Result<Self, String> {
        if let Some(error) = query.get("error") {
            return Err(format!("The sign-in was turned down: {}", query.get("error_description").unwrap_or(error)));
        }
        if query.get("state").map(String::as_str) != Some(state) {
            return Err("That is another sign-in's address, not this one's".into());
        }
        let code = query.get("code").ok_or("The address has no code in it: the sign-in didn't get through")?;
        Ok(Self { code: code.clone(), iss: query.get("iss").cloned() })
    }

    fn pasted(pasted: &str, redirect: &str, state: &str) -> Result<Self> {
        let expected = Url::parse(redirect)?;
        match Url::parse(pasted.trim()) {
            Ok(url) if url.origin() == expected.origin() && url.path() == expected.path() => {
                Self::parse(&url.query_pairs().into_owned().collect(), state).map_err(|e| anyhow!(e))
            }
            _ => bail!("Paste the whole address the browser ended up on: it starts with {redirect}"),
        }
    }
}

struct Callback {
    listener: TcpListener,
    redirect: String,
}

impl Callback {
    async fn listen(settings: &OAuth, registered: Option<&str>) -> Result<Self> {
        let written = settings.callback_url.as_deref().unwrap_or("http://127.0.0.1/callback");
        let mut url = Url::parse(written).context("oauth.callbackUrl")?;
        // `localhost` is served on 127.0.0.1: browsers try that when ::1 refuses.
        let host = if url.host_str() == Some("[::1]") { IpAddr::V6(Ipv6Addr::LOCALHOST) } else { IpAddr::V4(Ipv4Addr::LOCALHOST) };
        let fixed = url.port().or(settings.callback_port);
        let wanted = fixed.or_else(|| Url::parse(registered?).ok()?.port());
        let listener = match TcpListener::bind((host, wanted.unwrap_or(0))).await {
            Err(_) if wanted.is_some() && fixed.is_none() => TcpListener::bind((host, 0)).await,
            listener => listener,
        };
        let listener = listener.with_context(|| format!("Couldn't listen for the browser on port {}", wanted.unwrap_or(0)))?;
        // An address written with its port is sent as written: servers compare the text.
        let redirect = match &settings.callback_url {
            Some(written) if url.port().is_some() => written.clone(),
            _ => {
                let _ = url.set_port(Some(listener.local_addr()?.port()));
                url.into()
            }
        };
        Ok(Self { listener, redirect })
    }

    async fn wait(&self, state: &str) -> Result<Answer> {
        let path = Url::parse(&self.redirect)?.path().to_owned();
        let (answers, mut came) = mpsc::channel::<Result<Answer, String>>(1);
        loop {
            let stream = tokio::select! {
                accepted = self.listener.accept() => accepted?.0,
                Some(answer) = came.recv() => return answer.map_err(|e| anyhow!(e)),
            };
            // Each on its own: a browser opens connections it then leaves unused.
            let (path, state, answers) = (path.clone(), state.to_owned(), answers.clone());
            tokio::spawn(async move {
                let mut stream = stream;
                let Some(url) = requested(&mut stream).await else { return };
                let query: HashMap<String, String> = url.query_pairs().into_owned().collect();
                if url.path() != path {
                    return respond(&mut stream, "404 Not Found", "Not found").await;
                }
                if query.get("state") != Some(&state) {
                    return respond(&mut stream, "400 Bad Request", "This sign-in isn't waiting any more: start it again from Dirigent.")
                        .await;
                }
                let answer = Answer::parse(&query, &state);
                let page = match &answer {
                    Ok(_) => "Back to Dirigent, which finishes the sign-in: this page can be closed.".to_owned(),
                    Err(e) => e.replace('&', "&amp;").replace('<', "&lt;"),
                };
                respond(&mut stream, "200 OK", &page).await;
                let _ = answers.send(answer).await;
            });
        }
    }
}

async fn requested(stream: &mut TcpStream) -> Option<Url> {
    let mut head = Vec::new();
    let read = async {
        let mut chunk = [0; 1024];
        while head.len() < 16 * 1024 && !head.windows(4).any(|w| w == b"\r\n\r\n") {
            match stream.read(&mut chunk).await {
                Ok(n @ 1..) => head.extend_from_slice(&chunk[..n]),
                _ => break,
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(5), read).await.ok()?;
    let head = String::from_utf8_lossy(&head);
    let target = head.lines().next()?.split(' ').nth(1)?;
    Url::parse("http://localhost").ok()?.join(target).ok()
}

async fn respond(stream: &mut TcpStream, status: &str, text: &str) {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Dirigent</title>\
         <body style=\"font:16px system-ui;display:grid;place-items:center;height:90vh\"><p>{text}</p>"
    );
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(format!("{head}{body}").as_bytes()).await;
    let _ = stream.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_challenge_is_read_as_servers_write_it() {
        let challenge = Challenge::parse(Some(
            r#"Bearer realm="x", error="insufficient_scope", scope="read write",resource_metadata=https://x.dev/.well-known/oauth-protected-resource/mcp"#,
        ));
        assert_eq!(challenge.error.as_deref(), Some("insufficient_scope"));
        assert_eq!(challenge.scope.as_deref(), Some("read write"));
        assert_eq!(challenge.resource_metadata.unwrap().path(), "/.well-known/oauth-protected-resource/mcp");
        assert_eq!(Challenge::parse(Some(r#"Basic realm="x", scope="read""#)), Challenge::default());
        assert_eq!(Challenge::parse(Some(r#"Bearer error_description="scope=admin""#)), Challenge::default());
    }

    #[test]
    fn a_pasted_address_must_be_this_sign_ins() {
        let redirect = "http://127.0.0.1:4000/callback";
        let pasted = |address: &str| Answer::pasted(address, redirect, "s1").map_err(|e| e.to_string());
        assert_eq!(
            pasted(" http://127.0.0.1:4000/callback?code=c&state=s1&iss=https%3A%2F%2Fas ").unwrap(),
            Answer { code: "c".into(), iss: Some("https://as".into()) }
        );
        assert!(pasted("http://127.0.0.1:4000/callback?code=c&state=other").unwrap_err().contains("another sign-in"));
        assert!(
            pasted("http://127.0.0.1:4001/callback?code=c&state=s1").unwrap_err().contains("starts with http://127.0.0.1:4000/callback")
        );
        assert!(pasted("c").unwrap_err().contains("whole address"));
        assert!(
            pasted("http://127.0.0.1:4000/callback?error=access_denied&error_description=No+thanks&state=s1")
                .unwrap_err()
                .contains("No thanks")
        );
    }

    #[test]
    fn a_resource_must_cover_the_server() {
        let server = Url::parse("https://x.dev/team/mcp").unwrap();
        assert_eq!(resource_for(&server, "https://x.dev/team").unwrap(), "https://x.dev/team");
        assert!(resource_for(&server, "https://x.dev/team/mcp").is_ok());
        assert!(resource_for(&server, "https://x.dev/tea").is_err());
        assert!(resource_for(&server, "https://y.dev/team").is_err());
    }

    #[tokio::test]
    async fn a_client_is_registered_at_an_http_address_or_not_at_all() {
        let server = AuthServer { registration_endpoint: Some("mailto:someone@example.com".into()), ..AuthServer::default() };
        let refused = register(&server, "http://127.0.0.1:4000/callback", CLIENT_NAME).await.unwrap_err();
        assert_eq!(refused.to_string(), "The authorization server registers clients at what is no http(s) address");
    }

    struct Endpoint {
        server: AuthServer,
        asked: Arc<tokio::sync::Notify>,
        answer: Arc<tokio::sync::Notify>,
    }

    async fn token_endpoint(status: u16, body: Value) -> Endpoint {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = AuthServer { token_endpoint: format!("http://{}/token", listener.local_addr().unwrap()), ..AuthServer::default() };
        let endpoint = Endpoint { server, asked: Arc::default(), answer: Arc::default() };
        let (asked, answer) = (endpoint.asked.clone(), endpoint.answer.clone());
        let answered = move || {
            let (asked, answer, body) = (asked.clone(), answer.clone(), body.clone());
            async move {
                asked.notify_one();
                answer.notified().await;
                (axum::http::StatusCode::from_u16(status).unwrap(), axum::Json(body))
            }
        };
        let app = axum::Router::new().route("/token", axum::routing::post(answered));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        endpoint
    }

    fn signed_in(dirs: &Dirs, name: &str, endpoint: &Endpoint) -> Arc<Auth> {
        let auth = Arc::new(Auth::new(dirs, name, &Url::parse("https://x.dev/mcp").unwrap(), None));
        let login = Login {
            client: Some(Registered { client_id: "c".into(), ..Registered::default() }),
            tokens: Some(Tokens { access_token: "a1".into(), refresh_token: Some("r1".into()), ..Tokens::default() }),
            auth_server: Some(endpoint.server.clone()),
            resource: None,
        };
        auth.store.update(&auth.key, |stored| *stored = login).unwrap();
        auth
    }

    fn dirs(tmp: &tempfile::TempDir) -> Dirs {
        Dirs { config: tmp.path().join("config"), data: tmp.path().join("data") }
    }

    #[tokio::test]
    async fn an_answer_without_a_token_is_quoted_only_if_it_is_an_error() {
        let client = Registered { client_id: "c".into(), ..Registered::default() };
        let said = async |status: u16, body: Value| {
            let endpoint = token_endpoint(status, body).await;
            endpoint.answer.notify_one();
            token_request(&endpoint.server, &client, None, &[("grant_type", "refresh_token")]).await.unwrap_err().to_string()
        };
        assert_eq!(said(200, json!({"data": {"access_token": "SECRET"}})).await, "The authorization server gave no token (HTTP 200 OK)");
        let long = said(502, json!({"reason": "é".repeat(ERROR_BODY_BYTES)})).await;
        assert!(long.starts_with("The authorization server gave no token (HTTP 502 Bad Gateway): {\"reason\":\"éé"), "{long}");
        assert!(long.ends_with('é') && long.len() < ERROR_BODY_BYTES + 100, "{long}");
    }

    #[tokio::test]
    async fn a_refresh_leaves_what_the_user_did_meanwhile() {
        let tmp = tempfile::tempdir().unwrap();
        let refused = token_endpoint(400, json!({"error": "invalid_grant"})).await;
        let auth = signed_in(&dirs(&tmp), "refused", &refused);
        let again = Tokens { access_token: "a2".into(), refresh_token: Some("r2".into()), ..Tokens::default() };
        let (refreshed, ()) = tokio::join!(auth.refresh(Some("a1")), async {
            refused.asked.notified().await;
            auth.store.update(&auth.key, |login| login.tokens = Some(again.clone())).unwrap();
            refused.answer.notify_one();
        });
        assert_eq!((refreshed, auth.store.load(&auth.key).tokens.as_ref()), (Ok(()), Some(&again)));

        let granted = token_endpoint(200, json!({"access_token": "a3", "refresh_token": "r3"})).await;
        let auth = signed_in(&dirs(&tmp), "granted", &granted);
        let (refreshed, ()) = tokio::join!(auth.refresh(Some("a1")), async {
            granted.asked.notified().await;
            auth.sign_out().unwrap();
            granted.answer.notify_one();
        });
        assert_eq!((refreshed, auth.store.load(&auth.key)), (Ok(()), Login::default()));
    }

    #[tokio::test]
    async fn a_refresh_waits_for_no_other_servers() {
        let tmp = tempfile::tempdir().unwrap();
        let (slow, quick) =
            (token_endpoint(200, json!({"access_token": "a2"})).await, token_endpoint(200, json!({"access_token": "a2"})).await);
        let (waiting, other) = (signed_in(&dirs(&tmp), "slow", &slow), signed_in(&dirs(&tmp), "quick", &quick));
        quick.answer.notify_one();
        let (first, ()) = tokio::join!(waiting.refresh(Some("a1")), async {
            slow.asked.notified().await;
            let second = tokio::time::timeout(Duration::from_secs(5), other.refresh(Some("a1"))).await;
            assert_eq!(second.expect("held up by another server's refresh"), Ok(()));
            slow.answer.notify_one();
        });
        assert_eq!((first, waiting.stored_token().as_deref(), other.stored_token().as_deref()), (Ok(()), Some("a2"), Some("a2")));
    }
}
