//! The module end to end, against servers of the tests' own: a command that speaks MCP
//! over its stdio (a few lines of sh), and an HTTP server that answers with JSON or a
//! stream and signs in with OAuth (`Fake`). The two at the end go out to real servers,
//! and are run by hand.

use super::http::http;
use super::*;
use crate::config::{Config, Dirs};
use crate::tool::{Call, Tool};
use async_trait::async_trait;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use base64::Engine;
use dirigent_protocol::McpState;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// A host's directories under `tmp`, with `servers` in its `agent.toml`.
fn dirs(tmp: &tempfile::TempDir, servers: impl IntoIterator<Item = (&'static str, Server)>) -> Dirs {
    let dirs = Dirs { config: tmp.path().join("config"), data: tmp.path().join("data") };
    let mcp = servers.into_iter().map(|(name, server)| (name.to_owned(), server)).collect();
    Config { mcp, ..Config::default() }.save(&dirs).unwrap();
    dirs
}

fn call() -> Call {
    Call { id: "call-1".into(), cancel: CancellationToken::new(), script: false }
}

fn names(servers: &Servers) -> Vec<String> {
    servers.tools().iter().map(|tool| tool.spec().name).collect()
}

fn tool(servers: &Servers, name: &str) -> Arc<dyn Tool> {
    servers.tools().into_iter().find(|tool| tool.spec().name == name).unwrap_or_else(|| panic!("no tool {name} among {:?}", names(servers)))
}

/// What a session was warned of, and the callback that collects it.
fn warnings() -> (Arc<Mutex<Vec<String>>>, impl Fn(String) + Send + Sync + 'static) {
    let warned = Arc::new(Mutex::new(vec![]));
    let collect = warned.clone();
    (warned, move |warning| collect.lock().unwrap().push(warning))
}

const SOON: Duration = Duration::from_secs(20);

/// An MCP server over stdio: it answers by the request's method, and says where it runs.
#[cfg(unix)]
pub(crate) const STDIO_SERVER: &str = r#"
echo "starting in $PWD" >&2
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"hang":true'*) continue ;;
    *'"method":"initialize"'*) result='{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"1"},"instructions":"Echo things."}' ;;
    *'"method":"tools/list"'*) result='{"tools":[{"name":"echo","description":"Say it back.\nTwice.","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}},{"name":"where-am-i","inputSchema":{}}]}' ;;
    *'"name":"where-am-i"'*) result="{\"content\":[{\"type\":\"text\",\"text\":\"$GREETING from $PWD, pid $$\"}]}" ;;
    *'"method":"tools/call"'*) result='{"content":[{"type":"text","text":"echo"}]}' ;;
    *) continue ;;
  esac
  printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$result"
done
"#;

#[cfg(unix)]
pub(crate) fn sh(script: &str) -> Server {
    Server { command: Some("sh".into()), args: vec!["-c".into(), script.into()], ..Server::default() }
}

#[cfg(unix)]
fn runs(pid: &str) -> bool {
    std::process::Command::new("sh").args(["-c", &format!("kill -0 {pid} 2>/dev/null")]).status().unwrap().success()
}

/// Whether the process `pid` is gone, soon: one that was ended is there until whoever it
/// was left to has seen it end.
#[cfg(unix)]
async fn gone(pid: &str) -> bool {
    for _ in 0..100 {
        if !runs(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// `STDIO_SERVER` as a command that starts another (as `npx` does its server, and a
/// screen's driver an app), whose pid it says for a greeting.
#[cfg(unix)]
fn starts_another() -> Server {
    sh(&format!("sleep 300 >/dev/null 2>&1 & GREETING=$!\n{STDIO_SERVER}"))
}

#[cfg(unix)]
#[tokio::test]
async fn a_session_has_the_tools_of_its_commands() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("sub")).unwrap();
    let files = Server {
        env: [("GREETING".to_owned(), "hello".to_owned())].into(),
        cwd: Some("sub".into()),
        description: Some(" Files. ".into()),
        ..sh(STDIO_SERVER)
    };
    let entries = [
        ("files", files),
        ("direct", Server { exposure: Exposure::Direct, ..sh(STDIO_SERVER) }),
        ("off", Server { enabled: false, ..sh("exit 1") }),
        ("broken", sh("echo 'no license' >&2; exit 3")),
        ("missing", Server { command: Some("no-such-mcp-server".into()), ..Server::default() }),
        ("neither", Server::default()),
    ];
    let dirs = dirs(&tmp, entries);
    let (warned, warn) = warnings();
    let servers = Servers::start(&dirs, tmp.path(), warn);
    assert!(servers.tools().is_empty(), "nothing waits for the servers");
    servers.ready(false, SOON).await;

    // Named for the model; what isn't exposed directly is its scripts' alone, if it has codemode.
    assert_eq!(names(&servers), ["mcp__direct__echo", "mcp__direct__where_am_i", "mcp__files__echo", "mcp__files__where_am_i"]);
    let spec = tool(&servers, "mcp__files__echo").spec();
    assert_eq!((spec.description.as_str(), spec.namespace.as_deref(), spec.deferred), ("Say it back.\nTwice.", Some("mcp__files"), true));
    assert_eq!(spec.input, json!({"type": "object", "properties": {"text": {"type": "string"}}}));
    let spec = tool(&servers, "mcp__direct__where_am_i").spec();
    assert_eq!((spec.description.as_str(), spec.deferred), ("MCP tool where-am-i from server direct", false));
    assert_eq!(spec.input, json!({"type": "object", "properties": {}}));
    let files = servers.namespace("files").unwrap();
    assert_eq!(
        (files.name.as_str(), files.description.as_deref(), files.instructions.as_deref()),
        ("mcp__files", Some("Files."), Some("Echo things."))
    );
    assert_eq!(servers.namespace("mcp__files"), Some(files));
    assert!(servers.namespace("off").is_none());
    // What a thread with codemode is told there is: the usable servers that are its scripts'.
    let scripted: Vec<&str> = servers.scripted().iter().map(|(name, _)| name.as_str()).collect();
    assert!(scripted.contains(&"mcp__files") && scripted.contains(&"mcp__broken"), "{scripted:?}");
    assert!(!scripted.iter().any(|name| ["mcp__direct", "mcp__off", "mcp__neither"].contains(name)), "{scripted:?}");

    // What didn't connect is said once, with what the command last said.
    let mut warned = warned.lock().unwrap().clone();
    warned.sort();
    assert_eq!(
        warned[0],
        "An MCP server is skipped: server \"neither\" needs either \"command\" (stdio) or \"url\" (streamable HTTP) (Settings › MCP servers)"
    );
    assert_eq!(warned[1], "MCP server \"broken\" failed to connect: The connection to the server closed\nno license");
    assert!(
        warned[2].starts_with("MCP server \"missing\" failed to connect: Couldn't run `no-such-mcp-server`: ")
            && warned[2].ends_with("on the host's PATH?)"),
        "{}",
        warned[2]
    );
    assert_eq!(warned.len(), 3);

    // The command runs where the entry says, from the thread's directory, with its environment.
    let said = tool(&servers, "mcp__files__where_am_i").call(json!({}), &call()).await.unwrap().as_text();
    let (said, pid) = said.split_once(", pid ").unwrap();
    assert_eq!(said, format!("hello from {}", tmp.path().join("sub").canonicalize().unwrap().display()));
    let other = tool(&servers, "mcp__direct__where_am_i");
    let other_pid = other.call(json!({}), &call()).await.unwrap().as_text().rsplit(' ').next().unwrap().to_owned();
    assert!(runs(pid) && runs(&other_pid));

    // A command that went away is run again by the next call.
    std::process::Command::new("kill").arg(&other_pid).status().unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let again = other.call(json!({}), &call()).await.unwrap().as_text();
    assert!(!again.ends_with(&format!(" {other_pid}")), "{again}");

    // The commands go with the session, and a tool still held says so.
    servers.close().await;
    assert!(!runs(pid));
    assert_eq!(other.call(json!({}), &call()).await.unwrap_err().to_string(), "MCP server \"direct\" is shut down");
    assert!(servers.tools().is_empty());
}

/// What a watcher saw, in order.
#[cfg(unix)]
#[derive(Default)]
struct Seen(Mutex<Vec<String>>);

#[cfg(unix)]
#[async_trait]
impl Watch for Seen {
    async fn connected(&self, client: &Client) {
        // A request of the watcher's own, on the session's connection.
        let own = client.call_tool("echo", json!({}), SOON, &CancellationToken::new()).await.unwrap();
        self.0.lock().unwrap().push(format!("connected, own call: {}", own["content"][0]["text"]));
    }

    async fn call_starts(&self, _client: &Client, tool: &str, input: &Value, _call: &Call) {
        self.0.lock().unwrap().push(format!("{tool} starts with {input}"));
    }

    fn call_ended(&self, tool: &str, result: Option<&Value>) {
        self.0.lock().unwrap().push(format!("{tool} ended: {}", result.is_some()));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_server_is_added_to_a_running_session_and_watched() {
    let tmp = tempfile::tempdir().unwrap();
    let dirs = dirs(&tmp, [("mine", sh(STDIO_SERVER))]);
    let servers = Servers::start(&dirs, tmp.path(), |warning| panic!("{warning}"));
    let seen = Arc::new(Seen::default());
    let only = Some(vec!["where-am-i".into()]);
    let with = Added { only, instructions: Some("The guide.".into()), watch: Some(seen.clone()), ..Added::default() };
    servers.add("screen", Server { description: Some("A screen.".into()), ..sh(STDIO_SERVER) }, with).await.unwrap();

    // There when `add` returns: the tools it was let, the guide in the place of the server's own.
    let mine: Vec<String> = names(&servers).into_iter().filter(|name| name.starts_with("mcp__screen__")).collect();
    assert_eq!(mine, ["mcp__screen__where_am_i"]);
    assert_eq!(servers.namespace("mcp__screen").unwrap().instructions.as_deref(), Some("The guide."));
    let where_am_i = tool(&servers, "mcp__screen__where_am_i");
    let pid = where_am_i.call(json!({"a": 1}), &call()).await.unwrap().as_text().rsplit(' ').next().unwrap().to_owned();
    assert_eq!(*seen.0.lock().unwrap(), ["connected, own call: \"echo\"", "where-am-i starts with {\"a\":1}", "where-am-i ended: true"]);
    // A call that is let go of before it has an answer (a stopped run drops it) has ended too.
    assert!(tokio::time::timeout(Duration::from_millis(200), where_am_i.call(json!({"hang": true}), &call())).await.is_err());
    assert_eq!(seen.0.lock().unwrap()[3..], ["where-am-i starts with {\"hang\":true}", "where-am-i ended: false"]);

    // A name that is taken, a command that doesn't speak: said, and nothing is left of them.
    let taken = servers.add("mine", sh(STDIO_SERVER), Added::default()).await.unwrap_err();
    assert_eq!(taken, "There is an MCP server named \"mine\" already");
    let broken = servers.add("other", sh("echo 'cannot open display' >&2"), Added::default()).await.unwrap_err();
    assert_eq!(broken, "MCP server \"other\" failed to connect: The connection to the server closed\ncannot open display");
    assert!(servers.namespace("other").is_none());
    // So with one whose adding is let go of half-way: its name is free again.
    assert!(tokio::time::timeout(Duration::from_millis(200), servers.add("other", sh("sleep 5"), Added::default())).await.is_err());
    servers.add("other", sh(STDIO_SERVER), Added::default()).await.unwrap();

    assert!(servers.remove("screen").await && !servers.remove("screen").await);
    assert!(!runs(&pid) && !names(&servers).iter().any(|name| name.starts_with("mcp__screen__")));
    servers.close().await;
}

/// What a command started goes with its connection, unless it is to stay: what a screen's
/// driver launched on the user's desktop.
#[cfg(unix)]
#[tokio::test]
async fn what_a_command_started_goes_with_it_unless_it_is_to_stay() {
    let tmp = tempfile::tempdir().unwrap();
    let servers = Servers::start(&dirs(&tmp, []), tmp.path(), |warning| panic!("{warning}"));
    servers.add("goes", starts_another(), Added::default()).await.unwrap();
    servers.add("stays", starts_another(), Added { leave_started: true, ..Added::default() }).await.unwrap();
    let started = async |server: &str| {
        let said = tool(&servers, &format!("mcp__{server}__where_am_i")).call(json!({}), &call()).await.unwrap().as_text();
        said.split(' ').next().unwrap().to_owned()
    };
    let (goes, stays) = (started("goes").await, started("stays").await);
    assert!(runs(&goes) && runs(&stays));
    servers.close().await;
    assert!(gone(&goes).await && runs(&stays));
    std::process::Command::new("kill").arg(&stays).status().unwrap();
}

/// A close that is let go of half-way (whoever closed was dropped: a stopped run's tool
/// call) ends what the command started all the same.
#[cfg(unix)]
#[tokio::test]
async fn a_close_let_go_of_half_way_ends_the_commands_group() {
    use client::Transport;
    // A command that exits neither when its stdin closes nor when told to: closing it takes a while.
    let script = r#"sleep 300 >/dev/null 2>&1 & echo "{\"started\": $!}"; trap '' TERM; while :; do sleep 1; done"#;
    let tmp = tempfile::tempdir().unwrap();
    let (command, mut said) = stdio::Stdio::spawn("sh", &["-c".into(), script.into()], &Default::default(), tmp.path(), true).unwrap();
    let started = said.recv().await.unwrap()["started"].to_string();
    assert!(runs(&started));
    assert!(tokio::time::timeout(Duration::from_millis(100), command.close()).await.is_err());
    assert!(gone(&started).await);
}

/// An HTTP MCP server, with an authorization server at the same address if it wants a
/// sign-in (`auth`).
#[derive(Default)]
struct Fake {
    base: String,
    auth: bool,
    /// The methods of the requests it took, in order.
    seen: Mutex<Vec<String>>,
    /// The session it knows: another after it "restarted".
    session: Mutex<String>,
    /// Answer tool calls with a stream of events, and say on it that the tools changed.
    stream: AtomicBool,
    changed: AtomicBool,
    /// The access token and the refresh token it takes, and how many it gave out.
    access: Mutex<String>,
    refresh: Mutex<String>,
    issued: AtomicUsize,
    registered: AtomicUsize,
    /// The answer a stream ended without: what is asked for again gets it.
    later: Mutex<Option<Value>>,
    /// What the client answered when asked for its roots.
    roots: Mutex<Value>,
    /// The sign-in under way: what its token request must match.
    code_challenge: Mutex<String>,
    redirect: Mutex<String>,
    /// The authorization server it names, if not itself.
    issuer: Mutex<Option<String>>,
    /// It takes notifications and never answers them.
    mute: AtomicBool,
}

impl Fake {
    async fn serve(auth: bool) -> Arc<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let fake = Arc::new(Self { base, auth, session: Mutex::new("s1".into()), ..Self::default() });
        let app = Router::new()
            .route("/mcp", post(mcp).get(resumed).delete(given_back))
            .route("/.well-known/oauth-protected-resource/mcp", get(resource_metadata))
            .route("/.well-known/oauth-authorization-server", get(auth_server_metadata))
            .route("/register", post(register))
            .route("/token", post(token))
            .with_state(fake.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        fake
    }

    fn server(&self) -> Server {
        Server { url: Some(format!("{}/mcp", self.base)), headers: [("X-Key".to_owned(), "k".to_owned())].into(), ..Server::default() }
    }

    fn saw(&self, method: &str) -> bool {
        self.seen.lock().unwrap().iter().any(|seen| seen == method)
    }

    /// The authorization server's sign-in page, as the user who says yes: where it sends
    /// the browser back to.
    fn authorize(&self, url: &str) -> String {
        let url = url::Url::parse(url).unwrap();
        let query: HashMap<String, String> = url.query_pairs().into_owned().collect();
        assert_eq!(url.path(), "/authorize");
        assert_eq!(
            (query["response_type"].as_str(), query["code_challenge_method"].as_str(), query["client_id"].as_str()),
            ("code", "S256", "client-1")
        );
        assert_eq!((query["scope"].as_str(), query["resource"].as_str()), ("read", format!("{}/mcp", self.base).as_str()));
        *self.code_challenge.lock().unwrap() = query["code_challenge"].clone();
        *self.redirect.lock().unwrap() = query["redirect_uri"].clone();
        format!("{}?code=code-1&state={}", query["redirect_uri"], query["state"])
    }

    fn tokens(&self) -> Response {
        let n = self.issued.fetch_add(1, Ordering::Relaxed) + 1;
        *self.access.lock().unwrap() = format!("access-{n}");
        *self.refresh.lock().unwrap() = format!("refresh-{n}");
        // The first one has lapsed as it comes.
        let expires_in = if n == 1 { 0 } else { 3600 };
        Json(json!({"access_token": format!("access-{n}"), "token_type": "Bearer", "expires_in": expires_in, "refresh_token": format!("refresh-{n}")})).into_response()
    }
}

async fn mcp(State(fake): State<Arc<Fake>>, headers: HeaderMap, body: String) -> Response {
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok()).unwrap_or_default().to_owned();
    if fake.auth && header("authorization") != format!("Bearer {}", fake.access.lock().unwrap()) {
        let challenge = format!("Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource/mcp\"", fake.base);
        return (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, challenge)]).into_response();
    }
    let message: Value = serde_json::from_str(&body).unwrap();
    let (id, method) = (message["id"].clone(), message["method"].as_str().unwrap_or_default().to_owned());
    fake.seen.lock().unwrap().push(method.clone());
    let session = fake.session.lock().unwrap().clone();
    if method == "initialize" {
        let result = json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {"listChanged": true}}, "serverInfo": {"name": "fake", "version": "1"}});
        return ([("mcp-session-id", session)], Json(json!({"jsonrpc": "2.0", "id": id, "result": result}))).into_response();
    }
    if header("mcp-session-id") != session {
        return StatusCode::NOT_FOUND.into_response();
    }
    if header("mcp-protocol-version") != "2025-06-18" || header("x-key") != "k" {
        return (StatusCode::BAD_REQUEST, "a header is missing").into_response();
    }
    if id.is_null() || method.is_empty() {
        if fake.mute.load(Ordering::Relaxed) {
            std::future::pending::<()>().await;
        }
        // A notification, or the answer to a request of this server's own.
        *fake.roots.lock().unwrap() = message["result"]["roots"].clone();
        return StatusCode::ACCEPTED.into_response();
    }
    let tool = |name: &str| json!({"name": name, "inputSchema": {"type": "object"}});
    let result = match (method.as_str(), message["params"]["name"].as_str().unwrap_or_default()) {
        // In two pages.
        ("tools/list", _) if message["params"]["cursor"].is_null() => json!({"tools": [tool("echo")], "nextCursor": "more"}),
        ("tools/list", _) if fake.changed.load(Ordering::Relaxed) => {
            json!({"tools": [tool("slow"), tool("fail"), tool("new")], "nextCursor": ""})
        }
        ("tools/list", _) => json!({"tools": [tool("slow"), tool("fail")]}),
        ("tools/call", "slow") => {
            tokio::time::sleep(Duration::from_secs(60)).await;
            json!({"content": []})
        }
        ("tools/call", "fail") => json!({"content": [{"type": "text", "text": "no such thing"}], "isError": true}),
        ("tools/call", _) => json!({"content": [{"type": "text", "text": message["params"]["arguments"]["text"]}]}),
        _ => return Json(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "Method not found"}})).into_response(),
    };
    let answer = json!({"jsonrpc": "2.0", "id": id, "result": result});
    if method == "tools/call" && fake.stream.load(Ordering::Relaxed) {
        // As a stream: word that the tools changed, a request of the server's own, and
        // then it ends, without the answer.
        fake.changed.store(true, Ordering::Relaxed);
        *fake.later.lock().unwrap() = Some(answer);
        let changed = json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"});
        let asks = json!({"jsonrpc": "2.0", "id": "srv-1", "method": "roots/list"});
        let events = format!(": on its way\r\n\r\nevent: message\r\ndata: {changed}\r\n\r\ndata: {asks}\n\nretry: 10\nid: e1\n\n");
        return ([(header::CONTENT_TYPE, "text/event-stream")], events).into_response();
    }
    Json(answer).into_response()
}

/// The stream taken up again after event `e1`: the answer it ended without. The server
/// has no stream of its own.
async fn resumed(State(fake): State<Arc<Fake>>, headers: HeaderMap) -> Response {
    let after = headers.get("last-event-id").and_then(|id| id.to_str().ok());
    match fake.later.lock().unwrap().take().filter(|_| after == Some("e1")) {
        Some(answer) => ([(header::CONTENT_TYPE, "text/event-stream")], format!("data: {answer}\n\n")).into_response(),
        None => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

async fn given_back(State(fake): State<Arc<Fake>>) {
    fake.seen.lock().unwrap().push("DELETE".into());
}

async fn resource_metadata(State(fake): State<Arc<Fake>>) -> Json<Value> {
    let issuer = fake.issuer.lock().unwrap().clone().unwrap_or(fake.base.clone());
    Json(json!({"resource": format!("{}/mcp", fake.base), "authorization_servers": [issuer], "scopes_supported": ["read"]}))
}

async fn auth_server_metadata(State(fake): State<Arc<Fake>>) -> Json<Value> {
    let at = |path: &str| format!("{}{path}", fake.base);
    Json(json!({
        "issuer": fake.base,
        "authorization_endpoint": at("/authorize"),
        "token_endpoint": at("/token"),
        "registration_endpoint": at("/register"),
        "response_types_supported": ["code"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
    }))
}

async fn register(State(fake): State<Arc<Fake>>, Json(client): Json<Value>) -> Response {
    if client["client_name"] != "Dirigent" || client["token_endpoint_auth_method"] != "none" {
        return (StatusCode::FORBIDDEN, "not a client this knows").into_response();
    }
    fake.registered.fetch_add(1, Ordering::Relaxed);
    (StatusCode::CREATED, Json(json!({"client_id": "client-1", "client_secret": null, "redirect_uris": client["redirect_uris"]})))
        .into_response()
}

async fn token(State(fake): State<Arc<Fake>>, Form(form): Form<HashMap<String, String>>) -> Response {
    let field = |name: &str| form.get(name).cloned().unwrap_or_default();
    let refused = |error: &str| (StatusCode::BAD_REQUEST, Json(json!({"error": error}))).into_response();
    if field("client_id") != "client-1" || field("resource") != format!("{}/mcp", fake.base) {
        return refused("invalid_request");
    }
    match field("grant_type").as_str() {
        "authorization_code" => {
            let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(field("code_verifier")));
            let ours = field("code") == "code-1"
                && challenge == *fake.code_challenge.lock().unwrap()
                && field("redirect_uri") == *fake.redirect.lock().unwrap();
            if ours { fake.tokens() } else { refused("invalid_grant") }
        }
        "refresh_token" if !field("refresh_token").is_empty() && field("refresh_token") == *fake.refresh.lock().unwrap() => fake.tokens(),
        _ => refused("invalid_grant"),
    }
}

#[tokio::test]
async fn an_http_server_answers_with_json_or_a_stream() {
    let fake = Fake::serve(false).await;
    let tmp = tempfile::tempdir().unwrap();
    let dirs = dirs(&tmp, [("web", Server { exposure: Exposure::Direct, timeout: Some(1), ..fake.server() })]);
    let servers = Servers::start(&dirs, tmp.path(), |warning| panic!("{warning}"));
    servers.ready(true, SOON).await;

    // Both pages of its tools; its session and the protocol's version go with every request.
    assert_eq!(names(&servers), ["mcp__web__echo", "mcp__web__slow", "mcp__web__fail"]);
    let echo = tool(&servers, "mcp__web__echo");
    assert_eq!(echo.call(json!({"text": "hi"}), &call()).await.unwrap().as_text(), "hi");
    // An error the tool reports is its result: the model's to read, a script's to look at.
    let failed = tool(&servers, "mcp__web__fail").call(json!({}), &call()).await.unwrap();
    assert_eq!((failed.is_error, failed.as_text().as_str(), &failed.value.unwrap()["isError"]), (true, "no such thing", &json!(true)));

    // As a stream that ends before the answer, which is asked for again. On it, word
    // that the tools changed (they are listed again) and a request for the thread's directory.
    fake.stream.store(true, Ordering::Relaxed);
    assert_eq!(echo.call(json!({"text": "streamed"}), &call()).await.unwrap().as_text(), "streamed");
    let root = json!([{"uri": url::Url::from_file_path(tmp.path()).unwrap().as_str(), "name": tmp.path().file_name().unwrap().to_str()}]);
    let heard = async {
        while !names(&servers).contains(&"mcp__web__new".to_owned()) || *fake.roots.lock().unwrap() != root {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(SOON, heard).await.unwrap();

    // A call that takes longer than the entry lets it, and one the user stops: the server is told.
    let slow = tool(&servers, "mcp__web__slow");
    assert_eq!(slow.call(json!({}), &call()).await.unwrap_err().to_string(), "No answer from the server within 1 s");
    assert!(fake.saw("notifications/cancelled"));
    let stopped = call();
    let stop = stopped.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        stop.cancel();
    });
    assert_eq!(slow.call(json!({}), &stopped).await.unwrap_err().to_string(), "The call was cancelled");

    // The server restarted and knows the session no more: the call goes out on a new one.
    *fake.session.lock().unwrap() = "s2".into();
    assert_eq!(echo.call(json!({"text": "again"}), &call()).await.unwrap().as_text(), "again");
    assert_eq!(fake.seen.lock().unwrap().iter().filter(|method| *method == "initialize").count(), 2);

    servers.close().await;
    assert!(fake.saw("DELETE"));
}

/// A server that takes `notifications/initialized` and never answers it doesn't stay
/// connecting (which every request of a thread would wait for): it fails like a request.
#[tokio::test]
async fn a_notification_gets_as_long_as_a_request() {
    let fake = Fake::serve(false).await;
    fake.mute.store(true, Ordering::Relaxed);
    let tmp = tempfile::tempdir().unwrap();
    let dirs = dirs(&tmp, [("web", Server { timeout: Some(1), ..fake.server() })]);
    let status = check(&dirs, "web").await.unwrap();
    assert_eq!((status.state, status.error.as_deref()), (McpState::Failed, Some("No answer from the server within 1 s")));
}

/// The user at a sign-in, and their browser.
enum Browser {
    /// It reaches this host: the authorization server sends it back to the loopback port.
    Reaches(Arc<Fake>),
    /// It doesn't (the host is remote): the user pastes the address it ended up on.
    Pastes(Arc<Fake>, Mutex<Option<String>>),
    /// The user never signs in.
    Leaves,
}

#[async_trait]
impl SignIn for Browser {
    fn open(&self, url: &str) {
        match self {
            Self::Reaches(fake) => {
                let back = fake.authorize(url);
                tokio::spawn(async move { http().get(back).send().await.unwrap() });
            }
            Self::Pastes(fake, pasted) => *pasted.lock().unwrap() = Some(fake.authorize(url)),
            Self::Leaves => {}
        }
    }

    fn progress(&self, _message: &str) {}

    async fn redirect_url(&self) -> Option<String> {
        match self {
            Self::Pastes(_, pasted) => pasted.lock().unwrap().clone(),
            _ => std::future::pending().await,
        }
    }
}

#[tokio::test]
async fn an_http_server_is_signed_in_to_with_oauth() {
    let fake = Fake::serve(true).await;
    let tmp = tempfile::tempdir().unwrap();
    let dirs = dirs(&tmp, [("docs", fake.server())]);
    let state = async || check(&dirs, "docs").await.unwrap().state;
    let signed_in = || list(&dirs).unwrap().0[0].signed_in;
    let never = CancellationToken::new();

    // Nobody has signed in: a check says so, and a session, once.
    assert_eq!((state().await, signed_in()), (McpState::NeedsAuth, false));
    let (warned, warn) = warnings();
    let servers = Servers::start(&dirs, tmp.path(), warn);
    servers.ready(false, SOON).await;
    assert_eq!(*warned.lock().unwrap(), ["MCP server \"docs\" needs a sign-in: Settings › MCP servers has it."]);
    assert!(servers.tools().is_empty());

    // A sign-in the user walks away from is given up.
    let cancel = CancellationToken::new();
    let (gave_up, _) = tokio::join!(sign_in(&dirs, "docs", &Browser::Leaves, &cancel), async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        cancel.cancel();
    });
    assert_eq!(gave_up.unwrap_err().to_string(), "Sign-in cancelled");

    // The user signs in, in a browser that comes back to this host. The token it got has
    // lapsed already: connecting with it refreshes it first.
    sign_in(&dirs, "docs", &Browser::Reaches(fake.clone()), &never).await.unwrap();
    assert!(signed_in());
    assert_eq!((fake.registered.load(Ordering::Relaxed), fake.issued.load(Ordering::Relaxed)), (1, 2));
    let stored = std::fs::read_to_string(dirs.data.join("mcp-auth.json")).unwrap();
    assert!(
        stored.contains(&format!("\"mcp__docs|{}/mcp\"", fake.base)) && stored.contains("access-2") && !stored.contains("access-1"),
        "{stored}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(dirs.data.join("mcp-auth.json")).unwrap().permissions().mode() & 0o777, 0o600);
    }

    // The session that waited for it has the server's tools by its next request.
    servers.ready(false, SOON).await;
    assert_eq!(names(&servers), ["mcp__docs__echo", "mcp__docs__slow", "mcp__docs__fail"]);
    let echo = tool(&servers, "mcp__docs__echo");
    assert_eq!(echo.call(json!({"text": "hi"}), &call()).await.unwrap().as_text(), "hi");

    // The server takes the token no longer: it is refreshed, and the call goes through.
    fake.access.lock().unwrap().clear();
    assert_eq!(echo.call(json!({"text": "refreshed"}), &call()).await.unwrap().as_text(), "refreshed");
    assert_eq!(fake.issued.load(Ordering::Relaxed), 3);

    // Nor the refresh token: only the user can help, and the call says so.
    fake.access.lock().unwrap().clear();
    fake.refresh.lock().unwrap().clear();
    let said = echo.call(json!({"text": "x"}), &call()).await.unwrap_err().to_string();
    assert_eq!(said, "MCP server \"docs\" needs a sign-in: Settings › MCP servers has it.");
    assert_eq!((state().await, signed_in()), (McpState::NeedsAuth, false));

    // This time the browser can't reach the host, and the user pastes where it ended up.
    // The app is the client it registered as before.
    sign_in(&dirs, "docs", &Browser::Pastes(fake.clone(), Mutex::default()), &never).await.unwrap();
    assert_eq!((fake.registered.load(Ordering::Relaxed), fake.issued.load(Ordering::Relaxed)), (1, 4));
    let status = check(&dirs, "docs").await.unwrap();
    assert_eq!((status.state, status.tools.len(), status.error), (McpState::Connected, 3, None));
    assert_eq!(echo.call(json!({"text": "back"}), &call()).await.unwrap().as_text(), "back");

    sign_out(&dirs, "docs").unwrap();
    assert_eq!((state().await, signed_in()), (McpState::NeedsAuth, false));
    assert_eq!(std::fs::read_to_string(dirs.data.join("mcp-auth.json")).unwrap(), "{}");
    servers.close().await;
}

/// The authorization server a server names is its word: one that is no http(s) address
/// is said, not gone to.
#[tokio::test]
async fn an_authorization_server_is_an_http_address_or_none() {
    let fake = Fake::serve(true).await;
    let tmp = tempfile::tempdir().unwrap();
    let dirs = dirs(&tmp, [("docs", fake.server())]);
    for named in ["mailto:someone@example.com", "urn:ietf:params:oauth"] {
        *fake.issuer.lock().unwrap() = Some(named.into());
        let said = sign_in(&dirs, "docs", &Browser::Leaves, &CancellationToken::new()).await.unwrap_err().to_string();
        assert_eq!(said, format!("The server names an authorization server that is no http(s) address: {named}"));
    }
    // Nor is a path that would read as a host (`//[x`: as none at all) taken for one: the
    // metadata is looked for at the server that is named and, with none there, the
    // sign-in goes on at the endpoints MCP once fixed, where the app registers.
    *fake.issuer.lock().unwrap() = Some(format!("{}//[x", fake.base));
    let cancel = CancellationToken::new();
    let (gave_up, ()) = tokio::join!(sign_in(&dirs, "docs", &Browser::Leaves, &cancel), async {
        while fake.registered.load(Ordering::Relaxed) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        cancel.cancel();
    });
    assert_eq!(gave_up.unwrap_err().to_string(), "Sign-in cancelled");
}

#[tokio::test]
async fn settings_list_save_and_remove_the_files_servers() {
    let tmp = tempfile::tempdir().unwrap();
    let dirs = dirs(&tmp, [("a-b", Server { command: Some("srv".into()), ..Server::default() }), ("broken", Server::default())]);
    let listed = || list(&dirs).unwrap().0;

    let (servers, path) = list(&dirs).unwrap();
    assert_eq!(path, dirs.config_file().display().to_string());
    assert_eq!(
        servers.iter().map(|s| (s.name.as_str(), &s.config, s.error.as_deref())).collect::<Vec<_>>(),
        [
            ("a-b", &json!({"command": "srv"}), None),
            ("broken", &json!({}), Some("server \"broken\" needs either \"command\" (stdio) or \"url\" (streamable HTTP)")),
        ]
    );

    // An entry as the app's form sends it: saved as it is, or refused saying why.
    let docs = json!({"url": "https://x.dev/mcp", "headers": {"X-Key": "${KEY}"}, "oauth": {"clientName": "Claude Code"}, "enabled": false, "exposure": "direct", "timeout": 5});
    save(&dirs, "docs", docs.clone()).unwrap();
    assert_eq!(listed()[2].config, docs);
    save(&dirs, "docs", json!({"url": "https://x.dev/mcp", "oauth": {}})).unwrap();
    assert_eq!(listed()[2].config, json!({"url": "https://x.dev/mcp"}));
    let refused = |name: &str, config: Value| save(&dirs, name, config).unwrap_err().to_string();
    assert_eq!(refused("a_b", json!({"command": "srv"})), "\"a_b\" is too close to \"a-b\": pick a name that differs in more than - and _");
    assert_eq!(refused("docs", json!({"url": "x.dev"})), "server \"docs\": url must be an http or https URL");
    assert!(refused("docs", json!({"command": "srv", "exposure": "deferred"})).starts_with("server \"docs\": unknown variant `deferred`"));
    assert!(refused("docs", json!({"command": ["srv"]})).starts_with("server \"docs\": invalid type: sequence, expected a string"));
    save(&dirs, "a-b", json!({"command": "other"})).unwrap();
    assert_eq!(listed()[0].config, json!({"command": "other"}));

    let text = std::fs::read_to_string(dirs.config_file()).unwrap();
    assert!(text.contains("[mcp.docs]\nurl = \"https://x.dev/mcp\"\n"), "{text}");

    // A sign-in is for an address: it goes when the entry gets another, and with the entry.
    let sign_in = |address: &str| {
        let signed_in = |login: &mut oauth::Login| login.tokens = Some(oauth::Tokens { access_token: "t".into(), ..Default::default() });
        oauth::Store::new(&dirs).update(&oauth::Store::key("docs", &address.parse().unwrap()), signed_in).unwrap();
    };
    let stored = || std::fs::read_to_string(dirs.data.join("mcp-auth.json")).unwrap();
    sign_in("https://x.dev/mcp");
    save(&dirs, "docs", json!({"url": "https://x.dev/mcp", "description": "Docs"})).unwrap();
    assert!(listed()[2].signed_in);
    save(&dirs, "docs", json!({"url": "https://y.dev/mcp"})).unwrap();
    assert_eq!(stored(), "{}");
    sign_in("https://y.dev/mcp");
    remove(&dirs, "docs").unwrap();
    assert_eq!(stored(), "{}");
    assert_eq!(remove(&dirs, "docs").unwrap_err().to_string(), "No MCP server named \"docs\"");
    assert_eq!(listed().len(), 2);

    // What can't be had says why, in the server's place.
    assert_eq!(check(&dirs, "nope").await.unwrap_err().to_string(), "No MCP server named \"nope\"");
    assert!(check(&dirs, "broken").await.unwrap_err().to_string().contains("needs either"));
    assert_eq!(sign_out(&dirs, "a-b").unwrap_err().to_string(), "a-b doesn't sign in");
    let failed = check(&dirs, "a-b").await.unwrap();
    assert_eq!(failed.state, McpState::Failed);
    assert!(failed.error.unwrap().starts_with("Couldn't run `other`: "));
}

#[test]
fn pis_servers_and_sign_ins_are_imported() {
    let tmp = tempfile::tempdir().unwrap();
    let dirs = Dirs { config: tmp.path().join("config"), data: tmp.path().join("data") };
    let agent = tmp.path().join("agent");
    std::fs::create_dir(&agent).unwrap();
    let mcp = json!({
        "autoEnableCodemode": true,
        "mcpServers": {
            "files": {"type": "stdio", "command": "npx", "args": ["-y", "srv", "~"], "env": {"A": "${B}"}, "cwd": "/w", "enabled": false},
            "sentry": {"url": "https://mcp.sentry.dev/mcp", "exposure": "deferred", "toolExposure": {"find_*": "direct"}, "timeout": 2.5},
            "figma": {"url": "https://mcp.figma.com/mcp", "oauth": {"clientName": "Claude Code", "clientRegistration": "cimd"}, "exposure": "codemode-deferred"},
            "own": {"url": "https://own.dev/mcp", "oauth": {"clientId": "mine"}},
            "gh": {"url": "https://api.githubcopilot.com/mcp/", "auth": {"provider": "github-copilot"}},
            "old": {"type": "sse", "url": "https://old.dev/sse"},
            "sentry_": {"command": 5},
        },
    });
    std::fs::write(agent.join("mcp.json"), format!("\u{feff}{mcp}")).unwrap();
    let metadata = json!({"issuer": "https://mcp.sentry.dev", "authorization_endpoint": "https://mcp.sentry.dev/oauth/authorize", "token_endpoint": "https://mcp.sentry.dev/oauth/token", "response_types_supported": ["code"], "extra": 1});
    let auth = json!({
        "mcp__sentry|https://mcp.sentry.dev/mcp": {
            "serverUrl": "https://mcp.sentry.dev/mcp",
            "clientInformation": {"client_id": "c1", "redirect_uris": ["http://127.0.0.1:4000/callback"], "client_name": "pi"},
            "tokens": {"access_token": "a1", "token_type": "bearer", "expires_in": 3600, "refresh_token": "r1", "scope": "org:read"},
            "tokensExpireAt": 1_800_000_000_500u64,
            "codeVerifier": "v",
            "discovery": {"authorizationServerUrl": "https://mcp.sentry.dev", "authorizationServerMetadata": metadata, "resourceMetadata": {"resource": "https://mcp.sentry.dev/mcp"}},
        },
        // Signed in as pi's client id document: nothing registered.
        "mcp__figma|https://mcp.figma.com/mcp": {"serverUrl": "https://mcp.figma.com/mcp", "tokens": {"access_token": "a2", "token_type": "bearer"}},
        // Written by an older pi, by the address alone; the client is the entry's.
        "https://own.dev/mcp": {"serverUrl": "https://own.dev/mcp", "tokens": {"access_token": "a3", "token_type": "bearer"}},
    });
    std::fs::write(agent.join("mcp-auth.json"), auth.to_string()).unwrap();

    let imported = import_pi(&agent, &dirs, true);
    let entries: Vec<(&str, Value)> =
        imported.servers.iter().map(|(name, server)| (name.as_str(), serde_json::to_value(server).unwrap())).collect();
    assert_eq!(
        entries,
        [
            ("figma", json!({"url": "https://mcp.figma.com/mcp", "oauth": {"clientName": "Claude Code"}})),
            ("files", json!({"command": "npx", "args": ["-y", "srv", "~"], "env": {"A": "${B}"}, "cwd": "/w", "enabled": false})),
            ("own", json!({"url": "https://own.dev/mcp", "oauth": {"clientId": "mine"}})),
            ("sentry", json!({"url": "https://mcp.sentry.dev/mcp", "timeout": 3})),
        ]
    );
    assert_eq!(
        imported.notes,
        [
            "MCP server sentry: its tools all are reached the way the server is set to now (it had settings per tool)",
            "MCP server sentry: its tools are called from codemode scripts now (they were found with tool_search, which is gone)",
            "MCP server figma: a sign-in registers the app with the server now (it was told to sign in as pi's client id document)",
            "MCP server gh is left out: it sent the credentials of pi's provider \"github-copilot\", and there are no providers any more",
            "MCP server sentry_ is left out: expected a string",
            "MCP server old is left out: server \"old\": the legacy SSE transport is not supported; use the server's streamable HTTP URL",
            "MCP server figma: sign in again (Settings › MCP servers): pi was signed in under its own name there",
        ]
    );

    // The sign-ins that are the app's to use are ours now, as a sign-in of ours leaves them.
    let ours: Value = serde_json::from_str(&std::fs::read_to_string(dirs.data.join("mcp-auth.json")).unwrap()).unwrap();
    assert_eq!(
        ours.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["mcp__own|https://own.dev/mcp", "mcp__sentry|https://mcp.sentry.dev/mcp"]
    );
    let sentry = &ours["mcp__sentry|https://mcp.sentry.dev/mcp"];
    assert_eq!(sentry["client"], json!({"client_id": "c1", "redirect_uris": ["http://127.0.0.1:4000/callback"]}));
    assert_eq!(sentry["tokens"], json!({"access_token": "a1", "refresh_token": "r1", "scope": "org:read", "expires_at": 1_800_000_000}));
    assert_eq!(
        (sentry["auth_server"]["token_endpoint"].as_str(), sentry["resource"].as_str()),
        (Some("https://mcp.sentry.dev/oauth/token"), Some("https://mcp.sentry.dev/mcp"))
    );
    // Another install (a test instance that starts from this one's files) gets the
    // servers, and none of the sign-ins: they are this install's.
    let other = Dirs { config: tmp.path().join("other/config"), data: tmp.path().join("other/data") };
    let copied = import_pi(&agent, &other, false);
    assert_eq!(copied.servers, imported.servers);
    assert!(!other.data.exists());
    // Nothing there: nothing imported, nothing wrong.
    assert_eq!(import_pi(&tmp.path().join("none"), &dirs, true), Imported::default());
}

/// Cua Driver, the MCP server computer use runs: `cargo test -p dirigent_agent cua_driver -- --ignored`
/// (it is on PATH in the dev shell).
#[tokio::test]
#[ignore = "runs cua-driver"]
async fn cua_driver_lists_its_tools() {
    let tmp = tempfile::tempdir().unwrap();
    let dirs = dirs(&tmp, []);
    let servers = Servers::start(&dirs, tmp.path(), |warning| panic!("{warning}"));
    let env = [
        ("CUA_TELEMETRY_ENABLED", "0"),
        ("CUA_DRIVER_RS_UPDATE_CHECK", "0"),
        ("CUA_DRIVER_RS_HOME", &tmp.path().join("driver").display().to_string()),
    ];
    let driver = Server {
        command: Some("cua-driver".into()),
        args: vec!["mcp".into(), "--no-overlay".into()],
        env: env.map(|(name, value)| (name.to_owned(), value.to_owned())).into(),
        ..Server::default()
    };
    let with = Added { only: Some(vec!["get_desktop_state".into(), "click".into()]), ..Added::default() };
    servers.add("user_desktop", driver, with).await.unwrap();
    assert_eq!(names(&servers), ["mcp__user_desktop__click", "mcp__user_desktop__get_desktop_state"]);
    let guide = servers.namespace("mcp__user_desktop").unwrap().instructions.unwrap();
    println!("{} characters of instructions, starting: {}", guide.len(), guide.lines().next().unwrap());
    servers.close().await;
}

/// Public servers, over the network: `cargo test -p dirigent_agent public_http -- --ignored`.
/// DeepWiki's takes anyone; Sentry's wants a sign-in.
#[tokio::test]
#[ignore = "needs the network"]
async fn public_http_servers_answer() {
    let tmp = tempfile::tempdir().unwrap();
    let url = |url: &str| Server { url: Some(url.into()), ..Server::default() };
    let dirs = dirs(&tmp, [("deepwiki", url("https://mcp.deepwiki.com/mcp")), ("sentry", url("https://mcp.sentry.dev/mcp"))]);
    assert_eq!(check(&dirs, "sentry").await.unwrap().state, McpState::NeedsAuth);
    let (warned, warn) = warnings();
    let servers = Servers::start(&dirs, tmp.path(), warn);
    servers.ready(false, SOON).await;
    assert_eq!(*warned.lock().unwrap(), ["MCP server \"sentry\" needs a sign-in: Settings › MCP servers has it."]);
    println!("deepwiki: {:?}", names(&servers));
    let structure = tool(&servers, "mcp__deepwiki__read_wiki_structure")
        .call(json!({"repoName": "modelcontextprotocol/rust-sdk"}), &call())
        .await
        .unwrap();
    println!("{}", structure.as_text().lines().take(5).collect::<Vec<_>>().join("\n"));
    assert!(!structure.is_error && !structure.as_text().is_empty());
    servers.close().await;
}
