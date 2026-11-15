//! A session's MCP servers: the enabled entries of `agent.toml`, each connected in the
//! background, and their tools as the session's `Tool`s, named `mcp__<server>__<tool>`.
//! A connection that drops (a command that exits, a session the server forgets) is made
//! again by the next call. Servers can be added while the session runs: computer use's.

use super::client::{Client, Error, ToolInfo, Transport};
use super::config::{self, Exposure, Reach, Server, expand_home, hex, namespace, tool_name};
use super::http::Http;
use super::oauth::Auth;
use super::stdio::Stdio;
use crate::codemode::Namespace;
use crate::config::{Config, Dirs};
use crate::tool::{Call, Content, Tool, ToolOutput, ToolSpec};
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

/// The waits between attempts to connect to an HTTP server that is busy or unreachable.
const CONNECT_RETRIES: [Duration; 2] = [Duration::from_millis(250), Duration::from_secs(1)];
/// The most text of a result the model gets: of more, the start and the end.
const MAX_TEXT_BYTES: usize = 20 * 1024;
/// What a script's call of an MCP tool resolves to (`ToolSpec::returns`): the result as
/// the server sent it.
const RESULT_TYPE: &str = "{ content: { type: \"text\" | \"image\" | \"audio\" | \"resource_link\" | \"resource\"; text?: string; data?: string; mimeType?: string; [key: string]: unknown }[]; structuredContent?: unknown; isError?: boolean }";

/// What whoever adds a server while the session runs hears of its connection: computer
/// use, which records what its screen's driver does.
#[async_trait]
pub trait Watch: Send + Sync {
    /// A connection to the server was made (the first, or a new one after it dropped):
    /// `client` takes requests of the watcher's own, before any tool call does.
    async fn connected(&self, _client: &Client) {}

    /// A call of the server's tool `tool` is about to go out, and waits for this.
    /// `client` is the connection it goes out on.
    async fn call_starts(&self, _client: &Client, _tool: &str, _input: &Value, _call: &Call) {}

    /// The call is over: `result` is the tool's (its `CallToolResult`), `None` if it
    /// failed, was cancelled, or was let go of by a run that stopped (which drops a call
    /// that takes too long to end). Every `call_starts` that returned is followed by one.
    fn call_ended(&self, _tool: &str, _result: Option<&Value>) {}
}

/// A watched call under way: `Watch::call_ended` is told when it is over, however it ends.
struct Watched<'a> {
    watch: Option<&'a dyn Watch>,
    tool: &'a str,
}

impl Watched<'_> {
    fn ended(mut self, result: Option<&Value>) {
        if let Some(watch) = self.watch.take() {
            watch.call_ended(self.tool, result);
        }
    }
}

impl Drop for Watched<'_> {
    /// The call was dropped before it had an answer.
    fn drop(&mut self) {
        if let Some(watch) = self.watch.take() {
            watch.call_ended(self.tool, None);
        }
    }
}

/// A server being added: forgotten again unless `Servers::add` gets to its end, so one
/// whose caller is dropped half-way (a stopped run) doesn't keep its name taken.
struct Adding<'a> {
    servers: &'a Mutex<BTreeMap<String, Entry>>,
    name: Option<&'a str>,
}

impl Drop for Adding<'_> {
    fn drop(&mut self) {
        if let Some(entry) = self.name.and_then(|name| self.servers.lock().unwrap().remove(name)) {
            entry.connection.shutdown.cancel();
        }
    }
}

/// What whoever adds a server while the session runs makes of it.
#[derive(Default, Clone)]
pub struct Added {
    /// The server's tools that exist for the session; `None`: all it offers.
    pub only: Option<Vec<String>>,
    /// How to use its tools, in place of what the server says (`Namespace::instructions`).
    pub instructions: Option<String>,
    pub watch: Option<Arc<dyn Watch>>,
    /// What the server's command started is not the session's to end (the apps on the
    /// user's desktop): when the connection goes, the command alone is ended, not its
    /// process group.
    pub leave_started: bool,
}

/// A session's servers (a clone is the same servers).
#[derive(Clone)]
pub struct Servers {
    inner: Arc<Inner>,
}

struct Inner {
    dirs: Dirs,
    /// The thread's directory: where commands run.
    cwd: PathBuf,
    /// The servers of `agent.toml` whose tools are scripts' (`scripted`).
    scripted: Vec<(String, Option<String>)>,
    warn: Box<dyn Fn(String) + Send + Sync>,
    servers: Mutex<BTreeMap<String, Entry>>,
    closed: AtomicBool,
}

struct Entry {
    connection: Arc<Connection>,
    /// `true` once the attempt to connect is over, however it went.
    settled: watch::Receiver<bool>,
}

impl Servers {
    /// Start connecting to every enabled server of `agent.toml`, in the background (on
    /// the tokio runtime this is called in): none holds the session up. `cwd` is the
    /// thread's directory. `warn` gets a text for the user about each entry that can't be
    /// used and each server that didn't connect (a sign-in it needs included), once.
    pub fn start(dirs: &Dirs, cwd: &Path, warn: impl Fn(String) + Send + Sync + 'static) -> Self {
        let warn: Box<dyn Fn(String) + Send + Sync> = Box::new(warn);
        let configured = Config::load(dirs).map(|config| config.mcp).unwrap_or_else(|e| {
            warn(format!("No MCP servers: {e:#}"));
            BTreeMap::new()
        });
        let checked = config::checked(&configured);
        let description = |server: &Server| server.description.as_deref().map(str::trim).filter(|d| !d.is_empty()).map(str::to_owned);
        let scripted = checked
            .iter()
            .filter(|(_, server, reach)| reach.is_ok() && server.enabled && server.exposure == Exposure::Codemode)
            .map(|(name, server, _)| (namespace(name), description(server)))
            .collect();
        let inner =
            Inner { dirs: dirs.clone(), cwd: cwd.to_owned(), scripted, warn, servers: Mutex::default(), closed: AtomicBool::new(false) };
        let servers = Self { inner: Arc::new(inner) };
        for (name, server, reach) in checked {
            match reach {
                Ok(reach) if server.enabled => servers.connect(name, server.clone(), reach),
                Ok(_) => {}
                Err(why) => (servers.inner.warn)(format!("An MCP server is skipped: {why} (Settings › MCP servers)")),
            }
        }
        servers
    }

    /// Take a server in and start connecting to it.
    fn connect(&self, name: &str, server: Server, reach: Reach) {
        let connection = Connection::new(&self.inner.dirs, name, server, reach, &self.inner.cwd, Added::default());
        let settled = self.connect_in_background(connection.clone());
        self.inner.servers.lock().unwrap().insert(name.to_owned(), Entry { connection, settled });
    }

    fn connect_in_background(&self, connection: Arc<Connection>) -> watch::Receiver<bool> {
        let (done, settled) = watch::channel(false);
        let inner = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            // Why it failed is said once: a later call that fails to connect says it to the model.
            if let Err(e) = connection.client().await
                && let Some(inner) =
                    inner.upgrade().filter(|_| !connection.warned.swap(true, Ordering::Relaxed) && !connection.shutdown.is_cancelled())
            {
                (inner.warn)(e.to_string());
            }
            let _ = done.send(true);
        });
        settled
    }

    /// The servers of `agent.toml` whose tools are scripts' alone in a thread with codemode
    /// (`Exposure::Codemode`), by their tools' group, each with what its entry says it
    /// offers: what such a thread's model is told there is. As configured, whatever became
    /// of the connections, so it says the same from the session's first request on.
    pub fn scripted(&self) -> &[(String, Option<String>)] {
        &self.inner.scripted
    }

    /// Before a request, or a script: wait, at most `limit`, for the servers that are
    /// still connecting. `direct_only`: only for those whose tools the model is always
    /// told of (`Exposure::Direct`), which is all a request of a thread with codemode
    /// needs; a script, and a thread without codemode, reach all. A server that waited for
    /// a sign-in connects once there is one.
    pub async fn ready(&self, direct_only: bool, limit: Duration) {
        let mut waiting = vec![];
        for entry in self.inner.servers.lock().unwrap().values_mut() {
            if entry.connection.signed_in_since() {
                entry.settled = self.connect_in_background(entry.connection.clone());
            }
            if !direct_only || entry.connection.server.exposure == Exposure::Direct {
                waiting.push(entry.settled.clone());
            }
        }
        let all = async {
            for mut settled in waiting {
                let _ = settled.wait_for(|settled| *settled).await;
            }
        };
        let _ = tokio::time::timeout(limit, all).await;
    }

    /// The tools of the servers connected so far, as the session's tools. Those of a
    /// server whose connection dropped stay: a call connects again.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let connections: Vec<_> = self.inner.servers.lock().unwrap().values().map(|entry| entry.connection.clone()).collect();
        let (mut taken, mut tools) = (HashSet::new(), Vec::<Arc<dyn Tool>>::new());
        for connection in connections {
            let server = &connection.name;
            let offered = connection.tools();
            // Names that come to the same one all end in their hash: which of them would
            // keep the plain name doesn't hang on the order the server lists them in.
            let plain: Vec<String> = offered.iter().map(|tool| tool_name(server, &tool.name, |_| false)).collect();
            for tool in offered {
                let name =
                    tool_name(server, &tool.name, |name| taken.contains(name) || plain.iter().filter(|plain| *plain == name).count() > 1);
                if !taken.insert(name.clone()) {
                    continue;
                }
                let spec = ToolSpec {
                    description: tool.description.unwrap_or_else(|| format!("MCP tool {} from server {server}", tool.name)),
                    namespace: Some(namespace(server)),
                    returns: Some(RESULT_TYPE.to_owned()),
                    deferred: connection.server.exposure == Exposure::Codemode,
                    ..ToolSpec::new(&name, "", tool.input)
                };
                tools.push(Arc::new(McpTool { spec, tool: tool.name, connection: connection.clone() }));
            }
        }
        tools
    }

    /// What `describeNamespace` tells of a server: by its tools' group (`mcp__dev_tools`)
    /// or its own name (`dev-tools`).
    pub fn namespace(&self, name: &str) -> Option<Namespace> {
        let group = namespace(name.strip_prefix("mcp__").unwrap_or(name));
        let servers = self.inner.servers.lock().unwrap();
        let connection = servers.values().map(|entry| &entry.connection).find(|c| namespace(&c.name) == group)?;
        let description = connection.server.description.as_deref().map(str::trim).filter(|d| !d.is_empty());
        Some(Namespace { name: group, description: description.map(str::to_owned), instructions: connection.instructions() })
    }

    /// Add a server to the running session and connect to it: its tools are there when
    /// this returns. `Err` says why it isn't: the entry can't be used, the name is
    /// another server's, it didn't connect.
    pub async fn add(&self, name: &str, server: Server, with: Added) -> Result<(), String> {
        let reach = server.reach(name)?;
        let connection = {
            let mut servers = self.inner.servers.lock().unwrap();
            // Looked at with the servers held, as `close` takes them: none slips in behind it.
            if self.inner.closed.load(Ordering::Relaxed) {
                return Err("The session's MCP servers are closed".into());
            }
            if let Some(other) = servers.keys().find(|other| namespace(other) == namespace(name)) {
                return Err(format!("There is an MCP server named \"{other}\" already"));
            }
            let connection = Connection::new(&self.inner.dirs, name, server, reach, &self.inner.cwd, with);
            // Settled from the start: this waits for it itself.
            let entry = Entry { connection: connection.clone(), settled: watch::channel(true).1 };
            servers.insert(name.to_owned(), entry);
            connection
        };
        let mut adding = Adding { servers: &self.inner.servers, name: Some(name) };
        let connected = connection.client().await;
        adding.name = None;
        if let Err(e) = connected {
            self.remove(name).await;
            return Err(e.to_string());
        }
        Ok(())
    }

    /// Disconnect from server `name` and forget it: its tools are gone. `false`: there is none.
    pub async fn remove(&self, name: &str) -> bool {
        let entry = self.inner.servers.lock().unwrap().remove(name);
        match entry {
            Some(entry) => {
                entry.connection.close().await;
                true
            }
            None => false,
        }
    }

    /// The session ends: every connection closes, every command exits.
    pub async fn close(&self) {
        self.inner.closed.store(true, Ordering::Relaxed);
        let entries = std::mem::take(&mut *self.inner.servers.lock().unwrap());
        let closing: Vec<_> = entries.into_values().map(|entry| tokio::spawn(async move { entry.connection.close().await })).collect();
        for closed in closing {
            let _ = closed.await;
        }
    }
}

/// A server's tool, as the session's.
struct McpTool {
    spec: ToolSpec,
    /// The server's name for it.
    tool: String,
    connection: Arc<Connection>,
}

#[async_trait]
impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    async fn call(&self, input: Value, call: &Call) -> anyhow::Result<ToolOutput> {
        let result = self.connection.call(&self.tool, input, call).await?;
        Ok(output(&self.connection.name, &self.tool, result, call.script))
    }
}

/// How a server's connection is.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) enum Status {
    #[default]
    Connecting,
    Connected,
    /// It wants a sign-in.
    NeedsAuth,
    /// Why it didn't connect: the error, and a command's last stderr lines.
    Failed(String),
    Closed,
}

/// One server and the connection to it, made when first asked for and again when it dropped.
pub(crate) struct Connection {
    pub name: String,
    pub server: Server,
    reach: Reach,
    cwd: PathBuf,
    /// Its sign-in, if it is one that signs in with OAuth.
    pub auth: Option<Arc<Auth>>,
    with: Added,
    live: Mutex<Live>,
    /// Held while connecting: one attempt at a time.
    opening: tokio::sync::Mutex<()>,
    /// Cancelled by `close`, which ends an attempt under way.
    shutdown: CancellationToken,
    /// The user was told that it didn't connect.
    warned: AtomicBool,
}

#[derive(Default)]
struct Live {
    client: Option<Arc<Client>>,
    tools: Vec<ToolInfo>,
    instructions: Option<String>,
    status: Status,
    /// The access token that was stored when the server wanted a sign-in: another one
    /// since means the user signed in.
    token_then: Option<String>,
}

impl Connection {
    pub(crate) fn new(dirs: &Dirs, name: &str, server: Server, reach: Reach, cwd: &Path, with: Added) -> Arc<Self> {
        let auth = match &reach {
            Reach::Http(url) if server.uses_oauth() => Some(Arc::new(Auth::new(dirs, name, url, server.oauth.as_ref()))),
            _ => None,
        };
        let (live, opening, warned) = Default::default();
        Arc::new(Self {
            name: name.to_owned(),
            server,
            reach,
            cwd: cwd.to_owned(),
            auth,
            with,
            live,
            opening,
            shutdown: CancellationToken::new(),
            warned,
        })
    }

    pub(crate) fn status(&self) -> Status {
        self.live.lock().unwrap().status.clone()
    }

    /// The server's tools as last listed: those the session is to have.
    pub(crate) fn tools(&self) -> Vec<ToolInfo> {
        let tools = self.live.lock().unwrap().tools.clone();
        match &self.with.only {
            Some(only) => tools.into_iter().filter(|tool| only.contains(&tool.name)).collect(),
            None => tools,
        }
    }

    fn instructions(&self) -> Option<String> {
        self.with.instructions.clone().or_else(|| self.live.lock().unwrap().instructions.clone())
    }

    /// A sign-in was stored since the server wanted one.
    fn signed_in_since(&self) -> bool {
        let token_then = match &*self.live.lock().unwrap() {
            Live { status: Status::NeedsAuth, token_then, .. } => token_then.clone(),
            _ => return false,
        };
        self.auth.as_ref().is_some_and(|auth| auth.stored_token() != token_then)
    }

    fn current(&self) -> Option<Arc<Client>> {
        self.live.lock().unwrap().client.clone().filter(|client| !client.is_closed())
    }

    /// The connection, made now if there is none. The error says why there is none, for
    /// the user or the model to read: its status has the same.
    pub(crate) async fn client(self: &Arc<Self>) -> Result<Arc<Client>, Error> {
        let shut_down = || Error::Other(format!("MCP server \"{}\" is shut down", self.name));
        if self.shutdown.is_cancelled() {
            return Err(shut_down());
        }
        if let Some(client) = self.current() {
            return Ok(client);
        }
        let _opening = self.opening.lock().await;
        if let Some(client) = self.current() {
            return Ok(client);
        }
        let mut retries = CONNECT_RETRIES.iter().filter(|_| matches!(self.reach, Reach::Http(_)));
        let (error, said) = loop {
            let failed = match self.connect().await {
                Ok(client) => return Ok(client),
                Err(failed) => failed,
            };
            match retries.next().filter(|_| failed.0.is_transient()) {
                Some(wait) => tokio::select! {
                    () = tokio::time::sleep(*wait) => {}
                    () = self.shutdown.cancelled() => break failed,
                },
                None => break failed,
            }
        };
        let mut live = self.live.lock().unwrap();
        if self.shutdown.is_cancelled() {
            live.status = Status::Closed;
            return Err(shut_down());
        }
        if error == Error::AuthRequired {
            drop(live);
            return Err(self.needs_sign_in());
        }
        let why = said.map_or(error.to_string(), |said| format!("{error}\n{said}"));
        live.status = Status::Failed(why.clone());
        Err(Error::Other(format!("MCP server \"{}\" failed to connect: {why}", self.name)))
    }

    /// The server wants a sign-in: said as the error, and remembered. The tools it had
    /// stay, and say the same when called.
    fn needs_sign_in(&self) -> Error {
        let token_then = self.auth.as_ref().and_then(|auth| auth.stored_token());
        let mut live = self.live.lock().unwrap();
        (live.client, live.status, live.token_then) = (None, Status::NeedsAuth, token_then);
        Error::Other(format!("MCP server \"{}\" needs a sign-in: Settings › MCP servers has it.", self.name))
    }

    /// One attempt to connect: the transport, the handshake, the server's tools. What
    /// went wrong comes with what the server last said (a command's stderr).
    async fn connect(self: &Arc<Self>) -> Result<Arc<Client>, (Error, Option<String>)> {
        self.live.lock().unwrap().status = Status::Connecting;
        let (transport, incoming) = self.transport().await.map_err(|e| (e, None))?;
        let this = Arc::downgrade(self);
        let tools_changed = move || {
            if let Some(this) = this.upgrade() {
                tokio::spawn(async move { this.refresh_tools().await });
            }
        };
        let client = Client::new(transport, incoming, self.server.timeout(), &self.cwd, tools_changed);
        let setup = async {
            client.initialize().await?;
            let tools = client.list_tools().await?;
            if let Some(watch) = &self.with.watch {
                watch.connected(&client).await;
            }
            Ok(tools)
        };
        let tools = tokio::select! {
            tools = setup => tools,
            () = self.shutdown.cancelled() => Err(Error::Closed),
        };
        match tools {
            Ok(tools) => {
                let instructions = client.instructions().map(str::to_owned);
                *self.live.lock().unwrap() =
                    Live { client: Some(client.clone()), tools, instructions, status: Status::Connected, token_then: None };
                Ok(client)
            }
            Err(e) => {
                client.close().await;
                Err((e, client.last_words()))
            }
        }
    }

    async fn transport(&self) -> Result<(Box<dyn Transport>, mpsc::UnboundedReceiver<Value>), Error> {
        // The entry's values as they are used: `${NAME}` is the environment's.
        let resolved = async |values: &BTreeMap<String, String>, what: &str| {
            let mut resolved = BTreeMap::new();
            for (name, value) in values {
                resolved.insert(name.clone(), config::resolve(value, &format!("{what} \"{name}\"")).await.map_err(Error::Other)?);
            }
            Ok::<_, Error>(resolved)
        };
        match &self.reach {
            Reach::Http(url) => {
                let (http, incoming) = Http::new(url.clone(), &resolved(&self.server.headers, "header").await?, self.auth.clone())?;
                Ok((Box::new(http), incoming))
            }
            Reach::Stdio(command) => {
                let args: Vec<String> = self.server.args.iter().map(|arg| expand_home(arg)).collect();
                let cwd = self.server.cwd.as_ref().map_or(self.cwd.clone(), |cwd| self.cwd.join(expand_home(cwd)));
                let env = resolved(&self.server.env, "env").await?;
                let (stdio, incoming) = Stdio::spawn(&expand_home(command), &args, &env, &cwd, !self.with.leave_started)?;
                Ok((Box::new(stdio), incoming))
            }
        }
    }

    /// The server said its tools changed: list them again.
    async fn refresh_tools(&self) {
        let Some(client) = self.current() else { return };
        match client.list_tools().await {
            Ok(tools) => {
                let mut live = self.live.lock().unwrap();
                if live.client.as_ref().is_some_and(|current| Arc::ptr_eq(current, &client)) {
                    live.tools = tools;
                }
            }
            Err(e) => tracing::warn!("mcp: {}: its changed tools couldn't be listed: {e}", self.name),
        }
    }

    /// Call the server's tool `tool`: its `CallToolResult`.
    async fn call(self: &Arc<Self>, tool: &str, input: Value, call: &Call) -> Result<Value, Error> {
        let mut again = false;
        loop {
            let client = self.client().await?;
            let watch = self.with.watch.as_deref();
            if let Some(watch) = watch {
                watch.call_starts(&client, tool, &input, call).await;
            }
            let watched = Watched { watch, tool };
            let result = client.call_tool(tool, input.clone(), self.server.timeout(), &call.cancel).await;
            watched.ended(result.as_ref().ok());
            match result {
                // The server restarted and doesn't know the session: the call didn't run,
                // and goes out again on a new one. The old connection is let go, not
                // closed: other calls on it find out the same way.
                Err(Error::SessionExpired) if !again => {
                    self.live.lock().unwrap().client.take_if(|current| Arc::ptr_eq(current, &client));
                    again = true;
                }
                // The sign-in ran out and can't be refreshed.
                Err(Error::AuthRequired) => {
                    client.close().await;
                    return Err(self.needs_sign_in());
                }
                // The command went away under the call: with what it last said.
                Err(Error::Closed) => {
                    let said = client.last_words().map(|said| format!("\n{said}")).unwrap_or_default();
                    return Err(Error::Other(format!("{}{said}", Error::Closed)));
                }
                result => return result,
            }
        }
    }

    /// Connect anew, with the sign-in as it is now.
    pub(crate) async fn reconnect(self: &Arc<Self>) -> Result<(), Error> {
        let client = self.live.lock().unwrap().client.take();
        if let Some(client) = client {
            client.close().await;
        }
        self.client().await.map(drop)
    }

    /// Disconnect for good: a command exits, an attempt to connect ends.
    pub(crate) async fn close(&self) {
        self.shutdown.cancel();
        // An attempt under way sees the shutdown and ends first.
        let _opening = self.opening.lock().await;
        let client = {
            let mut live = self.live.lock().unwrap();
            live.status = Status::Closed;
            live.client.take()
        };
        if let Some(client) = client {
            client.close().await;
        }
    }
}

/// A tool's result for the model (its text and images, an error it reports) and for a
/// script, which gets the result as the server sent it. The model's text is cut to
/// `MAX_TEXT_BYTES`; a script (`script`) cuts down what it gets itself.
fn output(server: &str, tool: &str, mut result: Value, script: bool) -> ToolOutput {
    let is_error = result.get("isError").and_then(Value::as_bool).unwrap_or(false);
    let blocks = result.get("content").and_then(Value::as_array);
    let mut content: Vec<Content> = blocks.into_iter().flatten().map(content_of).collect();
    // Servers should say structured content as text too, and don't always.
    if let Some(structured) = result.get("structuredContent").filter(|_| content.is_empty()) {
        content.push(Content::Text(serde_json::to_string_pretty(structured).unwrap_or_default()));
    }
    if is_error && !content.iter().any(|c| matches!(c, Content::Text(text) if !text.is_empty())) {
        content.push(Content::Text(format!("MCP tool {server}/{tool} returned an error")));
    }
    if !script {
        content = limited(content);
    }
    if let Some(fields) = result.as_object_mut() {
        fields.remove("_meta");
    }
    ToolOutput { content, is_error, value: Some(result) }
}

/// A block of a result as the model can take it: text and images as they are, anything
/// else as a line saying what it is.
fn content_of(block: &Value) -> Content {
    let text = |at: &str| block.pointer(at).and_then(Value::as_str);
    let bytes = |at: &str| STANDARD.decode(text(at).unwrap_or_default().split_ascii_whitespace().collect::<String>());
    let image = |mime: &str, data: Result<Vec<u8>, _>| match data {
        Ok(data) => Content::Image { mime: mime.to_owned(), data },
        Err(_) => Content::Text(format!("[an image ({mime}) that isn't base64]")),
    };
    Content::Text(match text("/type").unwrap_or_default() {
        "text" => text("/text").unwrap_or_default().to_owned(),
        "image" => return image(text("/mimeType").unwrap_or("image/png"), bytes("/data")),
        "audio" => format!("[audio {} omitted]", text("/mimeType").unwrap_or_default()),
        "resource_link" => {
            let size = block.get("size").and_then(Value::as_u64).map(size);
            let details: Vec<String> = text("/mimeType").map(str::to_owned).into_iter().chain(size).collect();
            let details = if details.is_empty() { String::new() } else { format!(" ({})", details.join(", ")) };
            let description = text("/description").map(|d| format!(": {d}")).unwrap_or_default();
            format!(
                "[Resource {} \"{}\"{details}{description}]",
                text("/uri").unwrap_or_default(),
                text("/title").or(text("/name")).unwrap_or_default()
            )
        }
        "resource" => match (text("/resource/text"), text("/resource/mimeType")) {
            (Some(text), _) => text.to_owned(),
            (None, Some(mime)) if mime.starts_with("image/") => return image(mime, bytes("/resource/blob")),
            (None, mime) => {
                let (uri, blob) = (text("/resource/uri").unwrap_or_default(), bytes("/resource/blob").unwrap_or_default());
                let kind = mime.unwrap_or("unknown type").split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
                if kind.starts_with("text/") || kind == "application/json" || kind.ends_with("+json") || kind.ends_with("+xml") {
                    return Content::Text(String::from_utf8_lossy(&blob).into_owned());
                }
                // A file the model can pass on, named as the resource is.
                let extension = uri.rsplit('/').next().and_then(|file| file.rsplit_once('.')).map(|(_, extension)| extension);
                let extension =
                    extension.filter(|e| (1..=8).contains(&e.len()) && e.chars().all(|c| c.is_ascii_alphanumeric())).unwrap_or("bin");
                let kind = format!("{kind}, {}", size(blob.len() as u64));
                match save(&blob, extension) {
                    Ok(path) => format!("[Binary resource {uri} ({kind}) saved to {}]", path.display()),
                    Err(e) => format!("[Binary resource {uri} ({kind}) could not be saved: {e}]"),
                }
            }
        },
        other => format!("[unsupported MCP content {other}]"),
    })
}

fn size(bytes: u64) -> String {
    match bytes {
        ..1024 => format!("{bytes}B"),
        1024..1_048_576 => format!("{:.1}KB", bytes as f64 / 1024.),
        _ => format!("{:.1}MB", bytes as f64 / 1_048_576.),
    }
}

/// `content` with no more text than the model is to get of a result. More is one text,
/// its middle cut out (as Codex does it) and all of it in a file the model can read;
/// images stay.
fn limited(content: Vec<Content>) -> Vec<Content> {
    let texts: Vec<&str> = content.iter().filter_map(|c| if let Content::Text(text) = c { Some(text.as_str()) } else { None }).collect();
    let all = texts.join("\n");
    if all.len() <= MAX_TEXT_BYTES {
        return content;
    }
    let (mut head, mut tail) = (MAX_TEXT_BYTES / 2, all.len() - MAX_TEXT_BYTES / 2);
    while !all.is_char_boundary(head) {
        head -= 1;
    }
    while !all.is_char_boundary(tail) {
        tail += 1;
    }
    let where_it_is = match save(all.as_bytes(), "txt") {
        Ok(path) => format!("[Full output: {} (read it with offset/limit)]", path.display()),
        Err(e) => format!("[Could not save the full output: {e}]"),
    };
    let text = format!(
        "Warning: truncated output (original token count: {})\nTotal output lines: {}\n\n{}…{} chars truncated…{}\n\n{where_it_is}",
        all.len().div_ceil(4),
        all.lines().count(),
        &all[..head],
        all[head..tail].chars().count(),
        &all[tail..],
    );
    let images = content.into_iter().filter(|c| matches!(c, Content::Image { .. }));
    std::iter::once(Content::Text(text)).chain(images).collect()
}

/// Keep `bytes` as a file for the model to read: in a directory of this process's own
/// under the system's temporary one, which only the user can look into.
fn save(bytes: &[u8], extension: &str) -> std::io::Result<PathBuf> {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    let mut random = [0; 8];
    getrandom::fill(&mut random).map_err(std::io::Error::other)?;
    let dir = DIR.get_or_init(|| std::env::temp_dir().join(format!("dirigent-mcp-{}", hex(&random[..4]))));
    let mut private = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut private, 0o700);
    private.recursive(true).create(dir)?;
    let path = dir.join(format!("{}.{extension}", hex(&random)));
    std::fs::write(&path, bytes)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text_of(output: &ToolOutput) -> String {
        output.as_text()
    }

    #[test]
    fn a_result_is_text_and_images_for_the_model_and_itself_for_a_script() {
        let png = STANDARD.encode(b"png");
        let result = json!({
            "content": [
                {"type": "text", "text": "hello"},
                {"type": "image", "mimeType": "image/png", "data": png},
                {"type": "audio", "mimeType": "audio/wav", "data": "AAAA"},
                {"type": "resource_link", "uri": "file:///a.txt", "name": "a", "mimeType": "text/plain", "size": 2048},
                {"type": "resource", "resource": {"uri": "file:///b.md", "text": "embedded"}},
                {"type": "resource", "resource": {"uri": "file:///c.json", "mimeType": "application/json", "blob": STANDARD.encode("{}")}},
                {"type": "hologram"},
            ],
            "structuredContent": {"n": 1},
            "_meta": {"x": 1},
        });
        let output = output("srv", "t", result, false);
        assert!(!output.is_error);
        assert_eq!(
            text_of(&output),
            "hello\n[audio audio/wav omitted]\n[Resource file:///a.txt \"a\" (text/plain, 2.0KB)]\nembedded\n{}\n[unsupported MCP content hologram]"
        );
        assert_eq!(output.content[1], Content::Image { mime: "image/png".into(), data: b"png".to_vec() });
        // A script gets what the server sent, without what is the protocol's own.
        let value = output.value.unwrap();
        assert_eq!((value["structuredContent"]["n"].as_i64(), value.get("_meta")), (Some(1), None));
        assert_eq!(value["content"].as_array().unwrap().len(), 7);
    }

    #[test]
    fn a_result_without_text_still_says_something() {
        let structured = output("srv", "t", json!({"content": [], "structuredContent": {"n": 1}}), false);
        assert_eq!(text_of(&structured), "{\n  \"n\": 1\n}");
        let failed = output("srv", "t", json!({"content": [], "isError": true}), false);
        assert_eq!((failed.is_error, text_of(&failed).as_str()), (true, "MCP tool srv/t returned an error"));
    }

    #[test]
    fn long_text_keeps_its_ends_for_the_model_and_all_of_it_for_a_script() {
        let long = format!("start\n{}é\nend", "ä".repeat(MAX_TEXT_BYTES));
        let result = json!({"content": [{"type": "text", "text": long}, {"type": "image", "mimeType": "image/png", "data": ""}]});
        let cut = output("srv", "t", result.clone(), false);
        let Content::Text(text) = &cut.content[0] else { panic!() };
        assert!(text.starts_with(&format!(
            "Warning: truncated output (original token count: {})\nTotal output lines: 3\n\nstart\nää",
            long.len().div_ceil(4)
        )));
        assert!(text.contains("ä…10246 chars truncated…ä") && text.contains("é\nend\n\n[Full output: "));
        assert!(matches!(cut.content[1], Content::Image { .. }) && cut.content.len() == 2);
        // The file has it all.
        let path = text.rsplit_once("[Full output: ").unwrap().1.split(' ').next().unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), long);
        std::fs::remove_file(path).unwrap();
        let whole = output("srv", "t", result, true);
        assert_eq!(whole.content[0], Content::Text(long));
    }
}
