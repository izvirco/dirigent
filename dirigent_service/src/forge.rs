//! This data dir's forge account: one per data dir, so a desktop and the service it embeds
//! share it (in-process), and a remote `dirigent_service` has its own. It's what `auth =
//! "forge"` hosts connect with (`hosts.rs`: a JWT for the host's url), what a public
//! listener takes forge tokens of (`auth.rs`: the same account only) and what the desktop's
//! leaderboard uploads as.
//!
//! `<data dir>/forge.json` (owner-only) holds the sign-in: the forge and app it was made
//! with, who (`sub`, username, picture) and the refresh token. It's written at sign-in and
//! only read after that: the token stays good until Dirigent is revoked on the forge's
//! account page or it goes 90 days unused. A JWT is the file's token POSTed to the forge,
//! cached per resource until a minute before it expires (ten minutes); the desktop, its
//! service and a command line refreshing at once is harmless. A refresh the forge refuses
//! (`invalid_grant`: revoked, or 90 days unused) ends the sign-in: the token is dropped, the
//! account kept (`Status::Ended`), until someone signs in again.
//!
//! Signing in: `BrowserLogin` (the desktop: the code flow with PKCE, answered on a loopback
//! port) or `login_device` (`dirigent_service login`, `dirigent login`: a code to enter in
//! any browser). Signing out forgets the token here; the forge's account page revokes it.

use anyhow::{Context, Result, bail};
use dirigent_auth::{CALLBACK_PATH, Device, Forge, Tokens};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// The forge and Dirigent's app on it (`DIRIGENT_FORGE_URL`, `DIRIGENT_FORGE_CLIENT_ID`).
pub fn forge() -> &'static Forge {
    static FORGE: LazyLock<Forge> = LazyLock::new(Forge::from_env);
    &FORGE
}

/// `forge.json`: where the sign-in is kept.
pub fn path() -> PathBuf {
    crate::data_dir().join("forge.json")
}

/// `forge.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct File {
    /// Where it signed in: a sign-in with another forge or app (a test's) counts as none.
    forge: String,
    client_id: String,
    sub: String,
    username: String,
    #[serde(default)]
    picture: String,
    /// The sign-in's refresh token; `None` once the forge refused it.
    #[serde(default)]
    refresh_token: Option<String>,
}

/// Who is signed in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    /// The forge's user id: who it is, across renames.
    pub sub: String,
    pub username: String,
    /// The forge profile picture's url.
    pub picture: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Status {
    #[default]
    SignedOut,
    SignedIn(Account),
    /// The forge refused the sign-in (`ENDED`).
    Ended(Account),
}

/// What an ended sign-in says, wherever it's shown.
pub const ENDED: &str = "Dirigent was revoked on the forge, or the sign-in went 90 days unused: sign in again";

impl Status {
    /// The account that can mint tokens now.
    pub fn signed_in(&self) -> Option<&Account> {
        match self {
            Status::SignedIn(a) => Some(a),
            _ => None,
        }
    }
}

/// Why there's no token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenError {
    NotSignedIn,
    Ended,
    /// The forge couldn't be asked (or said something unexpected): try again later.
    Forge(String),
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenError::NotSignedIn => f.write_str("not signed in to the forge (Settings › Forge providers, or the `login` command)"),
            TokenError::Ended => f.write_str(ENDED),
            TokenError::Forge(e) => write!(f, "forge: {e}"),
        }
    }
}

impl std::error::Error for TokenError {}

impl File {
    fn read(path: &Path, forge: &Forge) -> Option<File> {
        let text = std::fs::read_to_string(path).ok()?;
        // Read every few seconds by the desktop: a broken file is said once, not each time.
        static WARNED: std::sync::Once = std::sync::Once::new();
        let file: File = serde_json::from_str(&text)
            .inspect_err(|e| WARNED.call_once(|| warn!("{}: {e} (signed out until fixed)", path.display())))
            .ok()?;
        (file.forge == forge.url && file.client_id == forge.client_id).then_some(file)
    }

    /// The sign-in that can refresh now and its refresh token, or why there's none.
    fn signed_in(path: &Path, forge: &Forge) -> Result<(File, String), TokenError> {
        let file = File::read(path, forge).ok_or(TokenError::NotSignedIn)?;
        let refresh_token = file.refresh_token.clone().ok_or(TokenError::Ended)?;
        Ok((file, refresh_token))
    }

    fn write(&self, path: &Path) -> Result<()> {
        crate::hosts::write_private(path, &serde_json::to_string_pretty(self)?).with_context(|| format!("write {}", path.display()))
    }

    fn account(&self) -> Account {
        Account { sub: self.sub.clone(), username: self.username.clone(), picture: self.picture.clone() }
    }

    fn status(&self) -> Status {
        match self.refresh_token {
            Some(_) => Status::SignedIn(self.account()),
            None => Status::Ended(self.account()),
        }
    }
}

/// The account as `forge.json` has it now.
pub fn status() -> Status {
    File::read(&path(), forge()).map_or(Status::SignedOut, |f| f.status())
}

/// A JWT for `resource` (a host's url, the leaderboard api's), from the cache or a refresh
/// (blocking: call from a thread that may wait on the network).
pub fn token(resource: &str) -> Result<String, TokenError> {
    static MINTER: Minter = Minter { cache: Mutex::new(BTreeMap::new()) };
    MINTER.token(&path(), forge(), resource, now(), |rt| forge().refresh(rt, resource))
}

/// Forget the sign-in here; `None` if there was none.
pub fn logout() -> Result<Option<Account>> {
    let path = path();
    let was = File::read(&path, forge()).map(|f| f.account());
    match std::fs::remove_file(&path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e).with_context(|| format!("remove {}", path.display())),
        _ => Ok(was),
    }
}

/// The JWT cache. Its lock is only held to look up and to store, not across a refresh: two
/// refreshes of one resource at once just both ask the forge.
#[derive(Default)]
struct Minter {
    /// Resource → (whose `sub`, the JWT, until when, unix seconds).
    cache: Mutex<BTreeMap<String, (String, String, i64)>>,
}

/// A cached JWT is used until this long before it expires.
const MARGIN_SECS: i64 = 60;

impl Minter {
    fn token(
        &self,
        path: &Path,
        forge: &Forge,
        resource: &str,
        now: i64,
        refresh: impl FnOnce(&str) -> Result<Tokens, dirigent_auth::Error>,
    ) -> Result<String, TokenError> {
        let (file, refresh_token) = File::signed_in(path, forge)?;
        if let Some((sub, jwt, until)) = self.cache.lock().unwrap().get(resource)
            && *sub == file.sub
            && now < *until
        {
            return Ok(jwt.clone());
        }
        match refresh(&refresh_token) {
            Ok(tokens) => {
                let until = now + tokens.expires_in as i64 - MARGIN_SECS;
                self.cache.lock().unwrap().insert(resource.to_owned(), (file.sub, tokens.access_token.clone(), until));
                Ok(tokens.access_token)
            }
            Err(e) if e.is("invalid_grant") => {
                // Only if the file still holds the refused token: a `login` meanwhile stays.
                let (mut current, now_held) = File::signed_in(path, forge)?;
                if now_held != refresh_token {
                    return Err(TokenError::Forge("the sign-in changed meanwhile: try again".into()));
                }
                warn!(user = %current.username, "forge sign-in ended: revoked, or 90 days unused");
                current.refresh_token = None;
                let _ = current.write(path);
                Err(TokenError::Ended)
            }
            Err(e) => Err(TokenError::Forge(e.to_string())),
        }
    }
}

/// What a sign-in ends in: who it is (userinfo), kept with its refresh token.
fn finish(tokens: Tokens) -> Result<Account> {
    let forge = forge();
    let refresh_token = tokens.refresh_token.context("the forge gave no refresh token: is Dirigent registered as a public app?")?;
    let me = forge.userinfo(&tokens.access_token)?;
    let file = File {
        forge: forge.url.clone(),
        client_id: forge.client_id.clone(),
        sub: me.sub,
        username: me.preferred_username,
        picture: me.picture,
        refresh_token: Some(refresh_token),
    };
    file.write(&path())?;
    info!(user = %file.username, "signed in to the forge");
    Ok(file.account())
}

/// Sign in with a device code (blocking): `show` tells the person where to enter which
/// code, then this polls until they answered.
pub fn login_device(show: impl FnOnce(&Device)) -> Result<Account> {
    let forge = forge();
    let device = forge.device()?;
    show(&device);
    let deadline = Instant::now() + Duration::from_secs(device.expires_in);
    let mut interval = device.interval.max(1);
    loop {
        std::thread::sleep(Duration::from_secs(interval));
        match forge.poll(&device.device_code) {
            Ok(tokens) => return finish(tokens),
            Err(e) if e.is("authorization_pending") => {}
            // RFC 8628 §3.5.
            Err(e) if e.is("slow_down") => interval += 5,
            Err(e) if e.is("access_denied") => bail!("the sign-in was turned down"),
            Err(e) if e.is("expired_token") => bail!("the code expired: start again"),
            // A blip on the way to the forge: keep polling (RFC 8628 §3.5), until the code expires.
            Err(dirigent_auth::Error::Http(e)) => warn!("forge poll: {e:#}"),
            Err(e) => return Err(e.into()),
        }
        if Instant::now() > deadline {
            bail!("the code expired: start again");
        }
    }
}

/// How long a browser sign-in waits for the browser.
const BROWSER_FOR: Duration = Duration::from_secs(600);

/// A browser sign-in (the code flow with PKCE): `start` gives the forge's sign-in page to
/// open, which sends the browser back to a loopback port with a code; `finish` waits for it
/// (blocking).
pub struct BrowserLogin {
    listener: TcpListener,
    redirect: String,
    state: String,
    verifier: String,
}

impl BrowserLogin {
    /// Listen for the browser's return; the url to open in it.
    pub fn start() -> Result<(Self, String)> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).context("listen for the browser")?;
        let redirect = format!("http://127.0.0.1:{}{CALLBACK_PATH}", listener.local_addr()?.port());
        let state = dirigent_auth::random(16);
        let (verifier, challenge) = dirigent_auth::pkce();
        let url = forge().authorize_url(&redirect, &state, &challenge);
        Ok((Self { listener, redirect, state, verifier }, url))
    }

    /// Wait for the browser and sign in with what it brings. `None` if `cancel` was set or
    /// nobody came back in time.
    pub fn finish(self, cancel: &AtomicBool) -> Result<Option<Account>> {
        let Some((query, mut stream)) = wait_for_browser(&self.listener, &self.state, cancel)? else { return Ok(None) };
        let signed_in = match (query.get("code"), query.get("error")) {
            (Some(code), _) => forge().exchange_code(code, &self.verifier, &self.redirect).map_err(anyhow::Error::from).and_then(finish),
            (None, Some(e)) if e == "access_denied" => Err(anyhow::anyhow!("the sign-in was turned down")),
            (None, error) => Err(anyhow::anyhow!("the forge said {}", error.map_or("nothing", String::as_str))),
        };
        let page = match &signed_in {
            Ok(a) => format!("Signed in to the forge as {}. Back to Dirigent!", html(&a.username)),
            Err(e) => format!("Couldn't sign in: {}", html(&format!("{e:#}"))),
        };
        let _ = respond(&mut stream, "200 OK", &page);
        signed_in.map(Some)
    }
}

/// Serve the browser's return on `listener` until it brings our `state`: its query and the
/// connection to answer on. `None` if cancelled or given up.
fn wait_for_browser(listener: &TcpListener, state: &str, cancel: &AtomicBool) -> Result<Option<(HashMap<String, String>, TcpStream)>> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + BROWSER_FOR;
    while Instant::now() < deadline && !cancel.load(Ordering::Relaxed) {
        let mut stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        // Anything else that finds the port (a favicon fetch) gets a 404.
        let query = read_query(&mut stream).unwrap_or_default();
        if query.get("state").map(String::as_str) != Some(state) {
            let _ = respond(&mut stream, "404 Not Found", "Not found");
            continue;
        }
        return Ok(Some((query, stream)));
    }
    Ok(None)
}

/// The query of an HTTP request's target (`GET /?a=b HTTP/1.1`), decoded.
fn read_query(stream: &mut TcpStream) -> Result<HashMap<String, String>> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut buf = vec![0; 8192];
    let mut len = 0;
    while len < buf.len() && !buf[..len].windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut buf[len..])?;
        if n == 0 {
            break;
        }
        len += n;
    }
    let head = String::from_utf8_lossy(&buf[..len]);
    let target = head.lines().next().and_then(|l| l.split(' ').nth(1)).unwrap_or("");
    Ok(parse_query(target))
}

fn parse_query(target: &str) -> HashMap<String, String> {
    let query = target.split_once('?').map_or("", |(_, q)| q);
    form_urlencoded::parse(query.as_bytes()).into_owned().collect()
}

fn html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn respond(stream: &mut TcpStream, status: &str, text: &str) -> std::io::Result<()> {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Dirigent</title>\
         <body style=\"font:16px system-ui;display:grid;place-items:center;height:90vh\"><p>{text}</p>"
    );
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn now() -> i64 {
    std::time::UNIX_EPOCH.elapsed().map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub const LOGIN_USAGE: &str = "login | logout | whoami";

/// `login` (a device code), `logout`, `whoami`: this data dir's forge account.
pub fn command(cmd: &str) -> Result<()> {
    let forge = forge();
    match cmd {
        "login" => {
            let account = login_device(|d| {
                eprintln!("To sign in to the forge, open {}\nand enter the code {}", d.verification_uri, d.user_code);
            })?;
            eprintln!("Signed in as {}.", account.username);
        }
        "logout" => match logout()? {
            Some(a) => eprintln!(
                "Signed out {} here. To end the sign-in on the forge too, revoke Dirigent at {}",
                a.username,
                forge.account_page()
            ),
            None => eprintln!("Not signed in."),
        },
        "whoami" => match status() {
            Status::SignedIn(a) => println!("{} ({})", a.username, a.sub),
            Status::Ended(a) => println!("{} ({}): {ENDED}", a.username, a.sub),
            Status::SignedOut => println!("Not signed in to {}.", forge.url),
        },
        _ => bail!("usage: {LOGIN_USAGE}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(access: &str) -> Tokens {
        Tokens { access_token: access.into(), refresh_token: None, expires_in: 600 }
    }

    fn signed_in(forge: &Forge, refresh: &str) -> File {
        File {
            forge: forge.url.clone(),
            client_id: forge.client_id.clone(),
            sub: "u1".into(),
            username: "seb".into(),
            picture: String::new(),
            refresh_token: Some(refresh.into()),
        }
    }

    fn never(_: &str) -> Result<Tokens, dirigent_auth::Error> {
        panic!("refreshed")
    }

    #[test]
    fn refreshes_cache_a_jwt_per_resource() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("forge.json");
        let forge = Forge::new("https://forge.example", "app");
        let m = Minter::default();
        assert_eq!(m.token(&path, &forge, "wss://a/ws", 0, never), Err(TokenError::NotSignedIn));

        signed_in(&forge, "r1").write(&path).unwrap();
        let jwt = m.token(&path, &forge, "wss://a/ws", 0, |rt| {
            assert_eq!(rt, "r1");
            Ok(tokens("jwt-a"))
        });
        assert_eq!(jwt.as_deref(), Ok("jwt-a"));
        // Cached until a minute before it expires; another resource gets its own.
        assert_eq!(m.token(&path, &forge, "wss://a/ws", 539, never).as_deref(), Ok("jwt-a"));
        assert_eq!(m.token(&path, &forge, "wss://b/ws", 1, |_| Ok(tokens("jwt-b"))).as_deref(), Ok("jwt-b"));
        assert_eq!(m.token(&path, &forge, "wss://a/ws", 540, |_| Ok(tokens("jwt-a2"))).as_deref(), Ok("jwt-a2"));
        // Unreachable: no token, still signed in. The file is never written after sign-in.
        let down = |_: &str| Err(dirigent_auth::Error::Http(anyhow::anyhow!("connection refused")));
        assert!(matches!(m.token(&path, &forge, "wss://c/ws", 541, down), Err(TokenError::Forge(_))));
        assert_eq!(File::read(&path, &forge), Some(signed_in(&forge, "r1")));
        // The file comes first, the cache second: signed out, its JWTs are out of reach.
        std::fs::remove_file(&path).unwrap();
        assert_eq!(m.token(&path, &forge, "wss://a/ws", 542, never), Err(TokenError::NotSignedIn));

        // Another account signed in meanwhile (`login` from the command line): not its JWTs.
        let mut other = signed_in(&forge, "o1");
        other.sub = "u2".into();
        other.write(&path).unwrap();
        assert_eq!(m.token(&path, &forge, "wss://a/ws", 543, |_| Ok(tokens("jwt-o"))).as_deref(), Ok("jwt-o"));

        // A sign-in made with another forge is none.
        assert_eq!(File::read(&path, &Forge::new("https://other.example", "app")), None);
    }

    #[test]
    fn a_refused_refresh_ends_the_sign_in() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("forge.json");
        let forge = Forge::new("https://forge.example", "app");
        let m = Minter::default();
        let refused = |_: &str| -> Result<Tokens, dirigent_auth::Error> {
            Err(dirigent_auth::Error::OAuth { error: "invalid_grant".into(), description: String::new() })
        };
        signed_in(&forge, "r1").write(&path).unwrap();
        // Refused while a `login` replaced the sign-in: the new one stays.
        let jwt = m.token(&path, &forge, "wss://a/ws", 0, |rt| {
            signed_in(&forge, "r2").write(&path).unwrap();
            refused(rt)
        });
        assert!(matches!(jwt, Err(TokenError::Forge(_))), "{jwt:?}");
        assert_eq!(File::read(&path, &forge), Some(signed_in(&forge, "r2")));

        assert_eq!(m.token(&path, &forge, "wss://a/ws", 1, refused), Err(TokenError::Ended));
        let file = File::read(&path, &forge).unwrap();
        assert_eq!(file.status(), Status::Ended(signed_in(&forge, "").account()));
        // And it stays that way, without asking the forge again.
        assert_eq!(m.token(&path, &forge, "wss://a/ws", 2, never), Err(TokenError::Ended));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn browser_return_query() {
        let q = parse_query("/forge/callback?state=abc&code=x%2Fy&error=access+denied");
        assert_eq!((q["state"].as_str(), q["code"].as_str(), q["error"].as_str()), ("abc", "x/y", "access denied"));
        assert!(parse_query("/favicon.ico").is_empty());
    }
}
