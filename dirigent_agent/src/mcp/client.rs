use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// The MCP version asked for, and the ones a server may answer with (one that doesn't
/// know ours says its latest).
pub(crate) const PROTOCOL_VERSION: &str = "2025-11-25";
const SUPPORTED_VERSIONS: [&str; 4] = [PROTOCOL_VERSION, "2025-06-18", "2025-03-26", "2024-11-05"];
const MAX_PAGES: usize = 1000;
pub(crate) const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    Rpc {
        code: i64,
        message: String,
    },
    Timeout(Duration),
    Aborted,
    Closed,
    Network(String),
    Http {
        status: u16,
        message: String,
    },
    AuthRequired,
    /// An HTTP server forgot the session (it restarted): the request didn't run.
    SessionExpired,
    Other(String),
}

impl Error {
    pub(crate) fn is_transient(&self) -> bool {
        match self {
            Self::Network(_) => true,
            Self::Http { status, .. } => matches!(status, 408 | 429) || (*status >= 500 && *status != 501),
            _ => false,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rpc { message, .. } => f.write_str(message),
            Self::Timeout(limit) => write!(f, "No answer from the server within {} s", limit.as_secs_f32()),
            Self::Aborted => f.write_str("The call was cancelled"),
            Self::Closed => f.write_str("The connection to the server closed"),
            Self::AuthRequired => f.write_str("The server wants a sign-in"),
            Self::SessionExpired => f.write_str("The server forgot the session"),
            Self::Network(message) | Self::Http { message, .. } | Self::Other(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {}

#[async_trait]
pub(crate) trait Transport: Send + Sync + 'static {
    async fn send(&self, message: &Value) -> Result<(), Error>;

    fn protocol_version(&self, _version: &str) {}

    fn last_words(&self) -> Option<String> {
        None
    }

    async fn close(&self);
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolInfo {
    pub name: String,
    pub description: Option<String>,
    pub input: Value,
}

impl ToolInfo {
    fn parse(item: &Value) -> Option<Self> {
        let name = item.get("name")?.as_str()?.to_owned();
        let mut input = item.get("inputSchema").filter(|schema| schema.is_object())?.clone();
        // A model's API wants an object's schema with its properties, which servers leave out.
        let schema = input.as_object_mut()?;
        schema.entry("type").or_insert(json!("object"));
        schema.entry("properties").or_insert(json!({}));
        let text = |at: &str| item.pointer(at).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
        let description = text("/description").or(text("/title")).or(text("/annotations/title")).map(str::to_owned);
        Some(Self { name, description, input })
    }
}

struct Pending {
    answer: oneshot::Sender<Result<Value, Error>>,
    progress: Arc<Notify>,
}

struct Told {
    has_tools: bool,
    instructions: Option<String>,
}

/// A connection to an MCP server. Requests may be made side by side.
pub struct Client {
    transport: Box<dyn Transport>,
    pending: Mutex<HashMap<u64, Pending>>,
    next_id: AtomicU64,
    closed: CancellationToken,
    timeout: Duration,
    root: Value,
    told: OnceLock<Told>,
    on_tools_changed: Box<dyn Fn() + Send + Sync>,
}

impl Client {
    pub(crate) fn new(
        transport: Box<dyn Transport>,
        mut incoming: mpsc::UnboundedReceiver<Value>,
        timeout: Duration,
        root: &Path,
        on_tools_changed: impl Fn() + Send + Sync + 'static,
    ) -> Arc<Self> {
        let name = root.file_name().map(|n| n.to_string_lossy().into_owned());
        let uri = url::Url::from_file_path(root).map(String::from).unwrap_or_else(|()| format!("file://{}", root.display()));
        let client = Arc::new(Self {
            transport,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            closed: CancellationToken::new(),
            timeout,
            root: json!({"uri": uri, "name": name}),
            told: OnceLock::new(),
            on_tools_changed: Box::new(on_tools_changed),
        });
        let weak = Arc::downgrade(&client);
        tokio::spawn(async move {
            while let Some(message) = incoming.recv().await {
                let Some(client) = weak.upgrade() else { return };
                client.receive(message);
            }
            if let Some(client) = weak.upgrade() {
                client.mark_closed();
            }
        });
        client
    }

    pub(crate) async fn initialize(&self) -> Result<(), Error> {
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {"roots": {}},
            "clientInfo": {"name": "dirigent", "version": dirigent_protocol::release::VERSION},
        });
        let result = self.exchange("initialize", Some(params), self.timeout, &CancellationToken::new()).await?;
        let version = result.get("protocolVersion").and_then(Value::as_str).unwrap_or_default();
        if !SUPPORTED_VERSIONS.contains(&version) {
            return Err(Error::Other(format!("The server speaks MCP version \"{version}\", which this app doesn't")));
        }
        self.transport.protocol_version(version);
        self.notify("notifications/initialized", None).await?;
        let instructions = result.get("instructions").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
        let told = Told { has_tools: result.pointer("/capabilities/tools").is_some(), instructions: instructions.map(str::to_owned) };
        let _ = self.told.set(told);
        Ok(())
    }

    pub fn instructions(&self) -> Option<&str> {
        self.told.get()?.instructions.as_deref()
    }

    pub fn is_closed(&self) -> bool {
        self.closed.is_cancelled()
    }

    pub(crate) fn last_words(&self) -> Option<String> {
        self.transport.last_words()
    }

    pub async fn list_tools(&self) -> Result<Vec<ToolInfo>, Error> {
        if !self.told.get().is_some_and(|told| told.has_tools) {
            return Ok(vec![]);
        }
        let invalid = || Error::Other("The server's list of tools isn't one".into());
        let (mut tools, mut cursors, mut cursor) = (vec![], HashSet::new(), None::<String>);
        for _ in 0..MAX_PAGES {
            let params = cursor.as_ref().map(|cursor| json!({"cursor": cursor}));
            let page = self.exchange("tools/list", params, self.timeout, &CancellationToken::new()).await?;
            for item in page.get("tools").and_then(Value::as_array).ok_or_else(invalid)? {
                tools.push(ToolInfo::parse(item).ok_or_else(invalid)?);
            }
            // Some servers end the list with `null` or `""` for leaving the cursor out.
            cursor = page.get("nextCursor").and_then(Value::as_str).filter(|c| !c.is_empty()).map(str::to_owned);
            match &cursor {
                None => return Ok(tools),
                Some(cursor) if !cursors.insert(cursor.clone()) => break,
                Some(_) => {}
            }
        }
        Err(Error::Other("The server's list of tools doesn't end".into()))
    }

    /// Call the server's tool `name`: its `CallToolResult`, as the server sent it (an
    /// error the tool reports is one, with `isError`). `cancel` stops waiting and tells
    /// the server; so does `timeout` running out, which progress the server reports on
    /// the call starts anew.
    pub async fn call_tool(&self, name: &str, input: Value, timeout: Duration, cancel: &CancellationToken) -> Result<Value, Error> {
        let arguments = if input.is_object() { input } else { json!({}) };
        let mut result = self.exchange("tools/call", Some(json!({"name": name, "arguments": arguments})), timeout, cancel).await?;
        // `content` is the spec's must, which servers with only structured content leave out.
        match result.as_object_mut() {
            Some(fields) if fields.get("content").is_none_or(Value::is_array) => {
                fields.entry("content").or_insert(json!([]));
                Ok(result)
            }
            _ => Err(Error::Other("The server's answer isn't a tool's result".into())),
        }
    }

    pub async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value, Error> {
        self.exchange(method, params, timeout, cancel).await
    }

    async fn exchange(
        &self,
        method: &str,
        mut params: Option<Value>,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value, Error> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        if cancel.is_cancelled() {
            return Err(Error::Aborted);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if method == "tools/call"
            && let Some(Value::Object(params)) = &mut params
        {
            params.insert("_meta".into(), json!({"progressToken": id}));
        }
        let mut message = json!({"jsonrpc": "2.0", "id": id, "method": method});
        if let Some(params) = params {
            message["params"] = params;
        }
        let (answer, mut answered) = oneshot::channel();
        let progress = Arc::new(Notify::new());
        self.pending.lock().unwrap().insert(id, Pending { answer, progress: progress.clone() });
        let _forget = Forget(self, id);

        // Sent while waiting: an HTTP server may hold the request until it has the answer.
        let send = self.transport.send(&message);
        let limit = tokio::time::sleep(timeout);
        tokio::pin!(send, limit);
        let mut sent = false;
        let gave_up = loop {
            tokio::select! {
                result = &mut send, if !sent => {
                    result?;
                    sent = true;
                }
                answer = &mut answered => return answer.unwrap_or(Err(Error::Closed)),
                () = progress.notified() => limit.as_mut().reset(Instant::now() + timeout),
                () = self.closed.cancelled() => return Err(Error::Closed),
                () = cancel.cancelled() => break Error::Aborted,
                () = &mut limit => break Error::Timeout(timeout),
            }
        };
        // The server is told to stop, except in the handshake, which the spec keeps free of it.
        if method != "initialize" {
            let reason = if gave_up == Error::Aborted { "Aborted" } else { "Request timed out" };
            let cancelled = self.notify("notifications/cancelled", Some(json!({"requestId": id, "reason": reason})));
            let _ = tokio::time::timeout(Duration::from_secs(2), cancelled).await;
        }
        Err(gave_up)
    }

    /// Tell the server something that takes no answer. Sending it gets as long as a
    /// request does, and no longer: over HTTP a server may leave the POST unanswered.
    pub(crate) async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), Error> {
        let mut message = json!({"jsonrpc": "2.0", "method": method});
        if let Some(params) = params {
            message["params"] = params;
        }
        let sent = tokio::time::timeout(self.timeout, self.transport.send(&message)).await;
        sent.unwrap_or(Err(Error::Timeout(self.timeout)))
    }

    fn receive(self: &Arc<Self>, mut message: Value) {
        let id = message.get("id").filter(|id| !id.is_null()).cloned();
        let method = message.get("method").and_then(Value::as_str).map(str::to_owned);
        match (id, method.as_deref()) {
            (Some(id), None) => {
                let Some(pending) = id.as_u64().and_then(|id| self.pending.lock().unwrap().remove(&id)) else { return };
                let answer = match message.get("error") {
                    Some(error) => {
                        let text = error.get("message").and_then(Value::as_str).unwrap_or("The server reported an error");
                        Err(Error::Rpc { code: error.get("code").and_then(Value::as_i64).unwrap_or(0), message: text.to_owned() })
                    }
                    None => Ok(message.get_mut("result").map(Value::take).unwrap_or_default()),
                };
                let _ = pending.answer.send(answer);
            }
            (Some(id), Some(method)) => {
                let answer = match method {
                    "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
                    "roots/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"roots": [self.root]}}),
                    _ => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("Method not found: {method}")}}),
                };
                let client = self.clone();
                tokio::spawn(async move { client.transport.send(&answer).await });
            }
            (None, Some("notifications/tools/list_changed")) => (self.on_tools_changed)(),
            (None, Some("notifications/progress")) => {
                let token = message.pointer("/params/progressToken").and_then(Value::as_u64);
                if let Some(pending) = token.and_then(|id| self.pending.lock().unwrap().get(&id).map(|p| p.progress.clone())) {
                    pending.notify_one();
                }
            }
            _ => {}
        }
    }

    fn mark_closed(&self) {
        self.closed.cancel();
        for (_, pending) in self.pending.lock().unwrap().drain() {
            let _ = pending.answer.send(Err(Error::Closed));
        }
    }

    pub(crate) async fn close(&self) {
        self.mark_closed();
        self.transport.close().await;
    }
}

struct Forget<'a>(&'a Client, u64);

impl Drop for Forget<'_> {
    fn drop(&mut self) {
        self.0.pending.lock().unwrap().remove(&self.1);
    }
}
