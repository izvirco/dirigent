//! A server's entry in `agent.toml`, what makes one usable, and the names its tools get.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;
use url::Url;

/// How long a request to a server may take unless its entry says (`timeout`).
const TIMEOUT: Duration = Duration::from_secs(60);
/// The longest name a model's API takes for a tool.
const MAX_TOOL_NAME: usize = 64;

/// An MCP server's entry in `agent.toml` (`[mcp.<name>]`): the fields of the `mcpServers`
/// shape other clients use, so an entry copied from a server's docs fits. It is a command
/// to run and talk to over its stdio, or the address of a streamable HTTP server.
///
/// ```toml
/// [mcp.files]
/// command = "npx"
/// args = ["-y", "@modelcontextprotocol/server-filesystem", "~"]
///
/// [mcp.docs]
/// url = "https://example.com/mcp"
/// headers = { Authorization = "Bearer ${DOCS_TOKEN}" }
/// exposure = "direct"
/// ```
///
/// The values of `env` and `headers`, and `oauth.clientSecret`, may name environment
/// variables (`${NAME}`) or be a command's output (`!gh auth token`): `resolve`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Server {
    /// The command: one program, not a shell's line. `~/` is the home directory, here and
    /// in `args` and `cwd`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Added to the environment the command gets (the host's).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Where the command runs, from the thread's directory: that itself unless said.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Sent with every request to `url`. With an `Authorization` among them the server
    /// isn't signed in to with OAuth.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuth>,
    /// `stdio`, `http` or `streamable-http`, as other clients' entries say it: `command`
    /// and `url` already do.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// `false` keeps the entry without connecting to it.
    #[serde(skip_serializing_if = "is_true")]
    pub enabled: bool,
    /// What the server offers, in a sentence: what `describeNamespace` says of it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// How long a request may take, in seconds (60 unless said). A call's progress
    /// notifications start it anew.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    #[serde(skip_serializing_if = "is_default")]
    pub exposure: Exposure,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            command: None,
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
            url: None,
            headers: BTreeMap::new(),
            oauth: None,
            kind: None,
            enabled: true,
            description: None,
            timeout: None,
            exposure: Exposure::default(),
        }
    }
}

fn is_true(value: &bool) -> bool {
    *value
}

fn is_default(exposure: &Exposure) -> bool {
    *exposure == Exposure::default()
}

/// How a server's tools reach the model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Exposure {
    /// Only codemode scripts call them (`ToolSpec::deferred`): they cost the model's
    /// context nothing. In a thread with codemode off they are declared, as `Direct`.
    #[default]
    Codemode,
    /// Declared to the model, as the built-in tools are.
    Direct,
}

/// How this app is a client to an HTTP server's authorization server, where the defaults
/// don't do: it registers itself as "Dirigent" and listens for the browser on a free port.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct OAuth {
    /// A client registered by hand, for a server that doesn't register clients itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// The port the browser comes back to (`http://127.0.0.1:<port>/callback`), for a
    /// client registered with one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_port: Option<u16>,
    /// The redirect URI registered for `clientId`, when it is another: http, on
    /// `localhost`, `127.0.0.1` or `[::1]`. Without a port it gets `callbackPort` or a free one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_url: Option<String>,
    /// The scopes to ask for, separated by spaces; the ones the server names unless said.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// The name to register under, for a server that only registers clients it knows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    /// The authorization server's metadata document, for a server that names a wrong
    /// authorization server or none. Trusted as written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_server_metadata_url: Option<String>,
}

/// How a usable entry's server is reached.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Reach {
    /// The command to run.
    Stdio(String),
    Http(Url),
}

impl Server {
    /// How the server is reached, or why the entry can't be used, said for the user to
    /// fix it. `name` is the entry's.
    pub(crate) fn reach(&self, name: &str) -> Result<Reach, String> {
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return Err(format!("invalid server name \"{name}\" (use letters, digits, \"_\" and \"-\")"));
        }
        let invalid = |why: &str| format!("server \"{name}\": {why}");
        if self.timeout == Some(0) {
            return Err(invalid("timeout must be a positive number of seconds"));
        }
        let kind = self.kind.as_deref();
        if kind == Some("sse") {
            return Err(invalid("the legacy SSE transport is not supported; use the server's streamable HTTP URL"));
        }
        if let Some(url) = &self.url
            && matches!(kind, None | Some("http" | "streamable-http"))
        {
            let url = Url::parse(url).ok().filter(|u| matches!(u.scheme(), "http" | "https"));
            let url = url.ok_or_else(|| invalid("url must be an http or https URL"))?;
            if let Some(oauth) = &self.oauth {
                oauth.check().map_err(invalid)?;
            }
            return Ok(Reach::Http(url));
        }
        match &self.command {
            Some(command) if !command.trim().is_empty() && matches!(kind, None | Some("stdio")) => Ok(Reach::Stdio(command.clone())),
            _ => Err(format!("server \"{name}\" needs either \"command\" (stdio) or \"url\" (streamable HTTP)")),
        }
    }

    /// It signs in with OAuth: an HTTP server whose entry has no `Authorization` header.
    pub(crate) fn uses_oauth(&self) -> bool {
        self.url.is_some() && !self.headers.keys().any(|h| h.eq_ignore_ascii_case("authorization"))
    }

    pub(crate) fn timeout(&self) -> Duration {
        self.timeout.map_or(TIMEOUT, Duration::from_secs)
    }
}

impl OAuth {
    fn check(&self) -> Result<(), &'static str> {
        if self.callback_port == Some(0) {
            return Err("oauth.callbackPort must be a port number");
        }
        if let Some(callback) = &self.callback_url {
            let fits = |u: &Url| u.scheme() == "http" && is_loopback(u) && u.query().is_none() && u.fragment().is_none();
            let url = Url::parse(callback).ok().filter(fits);
            let url = url.ok_or("oauth.callbackUrl must be an http URI on localhost, 127.0.0.1, or [::1] without query or fragment")?;
            if let (Some(written), Some(port)) = (url.port(), self.callback_port)
                && written != port
            {
                return Err("oauth.callbackUrl and oauth.callbackPort name different ports");
            }
        }
        if self.client_name.as_ref().is_some_and(|name| name.trim().is_empty()) {
            return Err("oauth.clientName must not be empty");
        }
        if let Some(metadata) = &self.auth_server_metadata_url
            && !Url::parse(metadata).is_ok_and(|u| is_secure(&u))
        {
            return Err("oauth.authServerMetadataUrl must be an https URL, or http on localhost, 127.0.0.1, or [::1]");
        }
        Ok(())
    }
}

pub(crate) fn is_loopback(url: &Url) -> bool {
    matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
}

/// Credentials may go there: https, or this machine.
pub(crate) fn is_secure(url: &Url) -> bool {
    url.scheme() == "https" || (url.scheme() == "http" && is_loopback(url))
}

/// Every entry of `servers` with how it is reached, or why a session skips it: an entry
/// that can't be used, or one whose name differs from an earlier one's only in `-` and `_`
/// (their tools would share their names).
pub(crate) fn checked(servers: &BTreeMap<String, Server>) -> Vec<(&str, &Server, Result<Reach, String>)> {
    let mut taken: HashMap<String, &str> = HashMap::new();
    let mut checked = vec![];
    for (name, server) in servers {
        let reach = server.reach(name).and_then(|reach| match taken.get(&namespace(name)) {
            Some(other) => Err(format!("server \"{name}\" conflicts with \"{other}\"")),
            None => {
                taken.insert(namespace(name), name);
                Ok(reach)
            }
        });
        checked.push((name.as_str(), server, reach));
    }
    checked
}

/// The group a server's tools are in (`ToolSpec::namespace`): `mcp__<server>`, with `_`
/// for `-` as in the tools' names.
pub fn namespace(server: &str) -> String {
    format!("mcp__{}", server.replace('-', "_"))
}

/// What the model and scripts call `server`'s `tool` by: `mcp__<server>__<tool>`, with `_`
/// for everything but letters, digits and `_` (a script calls it by that name). One too
/// long for a model's API, or `taken` by another tool (`a-b` and `a_b` come to the same),
/// ends in a hash of what it stands for instead.
pub(crate) fn tool_name(server: &str, tool: &str, taken: impl Fn(&str) -> bool) -> String {
    let plain = format!("mcp__{server}__{tool}");
    let name: String = plain.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect();
    if name.len() <= MAX_TOOL_NAME && !taken(&name) {
        return name;
    }
    let hash = hex(&Sha256::digest(format!("{server}\0{tool}")));
    format!("{}_{}", &name[..name.len().min(MAX_TOOL_NAME - 9)], &hash[..8])
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A value of an entry as it is used. One that starts with `!` is a shell command, and
/// its output the value (`!gh auth token`). In any other, `${NAME}` and `$NAME` are
/// environment variables, and `$$` is a `$`. `what` names the value where it can't be had.
pub(crate) async fn resolve(value: &str, what: &str) -> Result<String, String> {
    if let Some(command) = value.strip_prefix('!') {
        return output_of(command).await.ok_or_else(|| format!("{what}: the command `{command}` gave no value"));
    }
    let (mut out, mut missing, mut rest) = (String::new(), vec![], value);
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        rest = &rest[at + 1..];
        if rest.starts_with(['$', '!']) {
            out.push_str(&rest[..1]);
            rest = &rest[1..];
            continue;
        }
        let braced =
            rest.strip_prefix('{').and_then(|r| r.split_once('}')).filter(|(name, _)| !name.is_empty() && name_len(name) == name.len());
        let (name, after) = match braced {
            Some(braced) => braced,
            None if name_len(rest) > 0 => rest.split_at(name_len(rest)),
            None => {
                out.push('$');
                continue;
            }
        };
        match std::env::var(name).ok().filter(|v| !v.is_empty()) {
            Some(value) => out.push_str(&value),
            None => missing.push(name),
        }
        rest = after;
    }
    out.push_str(rest);
    match missing.as_slice() {
        [] => Ok(out),
        [name] => Err(format!("{what}: the environment variable {name} is not set")),
        names => Err(format!("{what}: the environment variables {} are not set", names.join(", "))),
    }
}

/// How much of the start of `s` is a variable's name.
fn name_len(s: &str) -> usize {
    if !s.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
        return 0;
    }
    s.bytes().take_while(|b| b.is_ascii_alphanumeric() || *b == b'_').count()
}

/// What a shell command prints, trimmed; `None` if it fails, takes over 10 s or prints nothing.
async fn output_of(command: &str) -> Option<String> {
    let (shell, flag) = if cfg!(windows) { ("cmd", "/C") } else { ("sh", "-c") };
    let mut shell = tokio::process::Command::new(shell);
    shell.arg(flag).arg(command).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(10), shell.output()).await.ok()?.ok()?;
    let said = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (output.status.success() && !said.is_empty()).then_some(said)
}

/// `~` and `~/…` as the home directory, as a shell would have it.
pub(crate) fn expand_home(value: &str) -> String {
    let below = match value.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with(std::path::is_separator) => rest.trim_start_matches(std::path::is_separator),
        _ => return value.to_owned(),
    };
    match std::env::home_dir() {
        Some(home) if below.is_empty() => home.to_string_lossy().into_owned(),
        Some(home) => home.join(below).to_string_lossy().into_owned(),
        None => value.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn server(entry: serde_json::Value) -> Server {
        serde_json::from_value(entry).unwrap()
    }

    #[test]
    fn an_entry_reads_as_other_clients_write_it_and_says_only_what_is_set() {
        let entry = json!({"type": "http", "url": "https://x.dev/mcp", "headers": {"X-Key": "${KEY}"}, "oauth": {"clientName": "Claude Code", "callbackPort": 8765}, "timeout": 5, "someone": "else's"});
        let http = server(entry);
        assert_eq!((http.enabled, http.exposure, http.timeout()), (true, Exposure::Codemode, Duration::from_secs(5)));
        assert_eq!(http.oauth.as_ref().unwrap().client_name.as_deref(), Some("Claude Code"));
        let stdio = Server {
            command: Some("npx".into()),
            args: vec!["-y".into(), "srv".into()],
            env: [("A".to_owned(), "1".to_owned())].into(),
            enabled: false,
            exposure: Exposure::Direct,
            ..Server::default()
        };
        assert_eq!(
            serde_json::to_value(&stdio).unwrap(),
            json!({"command": "npx", "args": ["-y", "srv"], "env": {"A": "1"}, "enabled": false, "exposure": "direct"})
        );
        assert_eq!(
            serde_json::to_value(Server { kind: None, ..http.clone() }).unwrap()["oauth"],
            json!({"clientName": "Claude Code", "callbackPort": 8765})
        );

        // In the file: tables after values, whatever the fields' order.
        let config = crate::config::Config { mcp: [("docs".to_owned(), http), ("files".to_owned(), stdio)].into(), ..Default::default() };
        let text = toml::to_string(&config).unwrap();
        assert!(
            text.contains("[mcp.docs]\nurl = \"https://x.dev/mcp\"") && text.contains("[mcp.docs.oauth]\ncallbackPort = 8765"),
            "{text}"
        );
        assert_eq!(toml::from_str::<crate::config::Config>(&text).unwrap(), config);
        assert_eq!(toml::from_str::<Server>("command = \"srv\"").unwrap(), Server { command: Some("srv".into()), ..Server::default() });
    }

    #[test]
    fn an_entry_that_cant_be_used_says_what_to_fix() {
        let why = |name: &str, entry: serde_json::Value| server(entry).reach(name).unwrap_err();
        assert_eq!(why("my server", json!({"command": "srv"})), "invalid server name \"my server\" (use letters, digits, \"_\" and \"-\")");
        assert_eq!(why("a", json!({})), "server \"a\" needs either \"command\" (stdio) or \"url\" (streamable HTTP)");
        assert_eq!(
            why("a", json!({"type": "stdio", "url": "https://x.dev"})),
            "server \"a\" needs either \"command\" (stdio) or \"url\" (streamable HTTP)"
        );
        assert!(why("a", json!({"type": "sse", "url": "https://x.dev/sse"})).contains("legacy SSE transport is not supported"));
        assert_eq!(why("a", json!({"url": "ftp://x.dev"})), "server \"a\": url must be an http or https URL");
        assert_eq!(why("a", json!({"command": "srv", "timeout": 0})), "server \"a\": timeout must be a positive number of seconds");
        assert!(
            why("a", json!({"url": "https://x.dev", "oauth": {"callbackUrl": "https://x.dev/cb"}}))
                .contains("oauth.callbackUrl must be an http URI on localhost")
        );
        assert!(
            why("a", json!({"url": "https://x.dev", "oauth": {"callbackUrl": "http://localhost:1/cb", "callbackPort": 2}}))
                .contains("different ports")
        );
        assert!(
            why("a", json!({"url": "https://x.dev", "oauth": {"authServerMetadataUrl": "http://as.dev/meta"}}))
                .contains("must be an https URL")
        );
        assert_eq!(
            server(json!({"url": "http://localhost:1/mcp", "command": "ignored"})).reach("a-b_1"),
            Ok(Reach::Http(Url::parse("http://localhost:1/mcp").unwrap()))
        );
        // An exposure pi had and this hasn't is said by the file's reader.
        assert!(
            serde_json::from_value::<Server>(json!({"command": "srv", "exposure": "deferred"}))
                .unwrap_err()
                .to_string()
                .contains("expected `codemode` or `direct`")
        );

        // Names that differ in `-` and `_` only: the second is skipped, as is an entry that is no use.
        let servers = [("a-b", json!({"command": "x"})), ("a_b", json!({"command": "y"})), ("a.b", json!({"command": "z"}))]
            .map(|(name, entry)| (name.to_owned(), server(entry)));
        let servers = BTreeMap::from(servers);
        let checked: Vec<_> = checked(&servers).into_iter().map(|(name, _, reach)| (name, reach.err())).collect();
        assert_eq!(checked[0], ("a-b", None));
        assert!(checked[1].1.as_ref().unwrap().starts_with("invalid server name \"a.b\""));
        assert_eq!(checked[2], ("a_b", Some("server \"a_b\" conflicts with \"a-b\"".into())));
        assert!(
            server(json!({"url": "https://x.dev"})).uses_oauth()
                && !server(json!({"url": "https://x.dev", "headers": {"authorization": "Bearer x"}})).uses_oauth()
        );
    }

    #[test]
    fn tools_are_named_for_models_and_scripts() {
        assert_eq!(namespace("dev-tools"), "mcp__dev_tools");
        assert_eq!(tool_name("dev-tools", "get.item/v2", |_| false), "mcp__dev_tools__get_item_v2");
        // Taken by another tool, or too long for a model's API: a hash of what it is tells it apart.
        let taken = tool_name("a", "b-c", |name| name == "mcp__a__b_c");
        assert_eq!(taken, format!("mcp__a__b_c_{}", &hex(&Sha256::digest("a\0b-c"))[..8]));
        let long = tool_name("server", &"t".repeat(80), |_| false);
        assert_eq!((long.len(), &long[..55]), (64, &format!("mcp__server__{}", "t".repeat(42))[..]));
        assert_ne!(long, tool_name("server", &"t".repeat(81), |_| false));
    }

    #[tokio::test]
    async fn values_name_the_environment_or_a_command() {
        let path = std::env::var("PATH").unwrap();
        assert_eq!(resolve("Bearer ${PATH}:$PATH.", "x").await.unwrap(), format!("Bearer {path}:{path}."));
        assert_eq!(resolve("5$$ $! ${not a name} ${open $", "x").await.unwrap(), "5$ ! ${not a name} ${open $");
        assert_eq!(
            resolve("$MCP_TEST_UNSET_1", "header \"X\"").await.unwrap_err(),
            "header \"X\": the environment variable MCP_TEST_UNSET_1 is not set"
        );
        assert!(
            resolve("${MCP_TEST_UNSET_1}${MCP_TEST_UNSET_2}", "x")
                .await
                .unwrap_err()
                .ends_with("MCP_TEST_UNSET_1, MCP_TEST_UNSET_2 are not set")
        );
        #[cfg(unix)]
        {
            assert_eq!(resolve("!printf ' tok en \n'", "x").await.unwrap(), "tok en");
            assert_eq!(resolve("!exit 1", "env \"T\"").await.unwrap_err(), "env \"T\": the command `exit 1` gave no value");
        }
        let home = std::env::home_dir().unwrap();
        assert_eq!(
            (expand_home("~"), expand_home("~/x"), expand_home("~x/y")),
            (home.display().to_string(), home.join("x").display().to_string(), "~x/y".to_owned())
        );
    }
}
