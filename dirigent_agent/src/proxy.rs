//! The CLIProxyAPI client (github.com/router-for-me/CLIProxyAPI): the one way to a model.
//! The proxy holds the accounts; this asks it what models it serves (`Proxy::models`) and
//! streams a reply from one (`Proxy::stream`).
//!
//! Two wires, picked per model by who makes it (`Model::api`):
//!
//! - `anthropic`: Anthropic's Messages API (`/v1/messages`), for Claude.
//! - `codex`: OpenAI's Responses API as Codex speaks it (`/backend-api/codex/responses`),
//!   which the proxy translates for everything else.
//!
//! Both take the same `Request` and give the same `Chunk`s and `Reply`: nothing outside
//! this module knows which one a model is on, except that a reply's thinking only replays
//! on the wire that made it (`Message::Assistant::api`).
//!
//! The proxy is no pipe. For a Claude subscription (CLIProxyAPI 8.0.63, as seen from
//! outside, on either wire) the model gets Claude Code's system prompt and ours as part of
//! the conversation, the tools under names of the proxy's making (`bash` is
//! `mcp__..._bash` to the model, and `bash` again in its reply), and a note with today's
//! date before the first message. A prompt does well not to lean on a tool's exact name.

mod anthropic;
mod catalog;
mod codex;
mod failure;
#[cfg(test)]
mod live;
mod pricing;
mod sse;

pub use failure::Error;

use crate::config::{Config, Dirs};
use crate::tool::ToolSpec;
use dirigent_protocol::transcript::{Api, Assistant, Block, Stop, Usage, text_of};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// A model the proxy serves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Model {
    /// What requests name it by, and threads remember it as: `claude-opus-5-5`.
    pub id: String,
    /// For people: `Claude Opus 5.5`.
    pub name: String,
    /// Who makes it, as the proxy says: `anthropic`, `openai`, ...
    pub owner: String,
    pub api: Api,
    /// Tokens.
    pub context_window: u64,
    /// The most tokens a reply can have.
    pub max_output: u64,
    /// The reasoning efforts it takes, lowest first (`low`, ..., `max`); empty: it doesn't reason.
    pub efforts: Vec<String>,
    /// It takes images.
    pub images: bool,
    pub price: Price,
}

/// A model's list price, in dollars per million tokens. All zero: not known.
#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    /// What a request with a long prompt costs instead, for a model priced by context size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long: Option<Tier>,
}

/// The rates of a request whose prompt (`Usage::prompt`) is longer than `above` tokens.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Tier {
    pub above: u64,
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

/// A message of a request: the conversation as every wire can say it.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    User(Vec<Part>),
    /// A reply as the transcript has it: text, thinking and tool calls. A wire sends the
    /// thinking back only if it is its own (`api`) and signed.
    Assistant {
        content: Vec<Block>,
        api: Api,
    },
    ToolResult {
        call_id: String,
        content: Vec<Part>,
        is_error: bool,
    },
}

/// A piece of a user message or tool result.
#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Text(String),
    /// `data` is base64.
    Image {
        mime: String,
        data: String,
    },
}

/// One request for a reply.
#[derive(Debug, Clone)]
pub struct Request<'a> {
    pub model: &'a Model,
    pub system: &'a str,
    pub messages: &'a [Message],
    /// The tools declared to the model, all of them: which those are is the session's to say
    /// (`Session::declared`).
    pub tools: &'a [ToolSpec],
    /// One of `Model::efforts`; `None`: the model's default.
    pub effort: Option<&'a str>,
    /// The most tokens to reply with; `None`: `Model::max_output`.
    pub max_output: Option<u64>,
    /// Names the conversation to the provider's prompt cache: the thread's id. Empty: it has
    /// no name.
    pub cache_key: &'a str,
}

/// A piece of a reply as it streams. `index` is the block's place in the reply's content
/// (as it streams: a block that came to nothing is not in the `Reply`, and those after it
/// move up).
#[derive(Debug, Clone, PartialEq)]
pub enum Chunk {
    /// The provider began to answer.
    Start,
    Text {
        index: usize,
        delta: String,
    },
    Thinking {
        index: usize,
        delta: String,
    },
    /// A tool call, whole: `input` is as in the `Reply`.
    ToolCall {
        index: usize,
        id: String,
        name: String,
        input: Value,
    },
}

/// How a request went. `message` is what came of it, also when it failed or was cancelled
/// midway: the content up to there, with `Stop::Error` or `Stop::Aborted`. (`effort` and
/// `duration_ms` are the caller's to fill in.)
///
/// Tool calls in the content are whole: one whose input was still streaming is left out. A
/// call's `input` is the JSON object the model gave. What it gave that is no JSON object
/// (`call_input`) is there as that text, a JSON string: such a call is not to be run but
/// answered with an error, so the model makes it again. A reply that ran into the output
/// limit (`Stop::MaxTokens`) may have calls too: those made before, and the one it cut off.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub message: Assistant,
    /// Why it failed, for the caller to act on; the words are in `message.error`.
    pub failure: Option<Failure>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Failure {
    /// Worth another try: the network, an overloaded or rate-limited provider. After
    /// `retry_after`, if the provider said when.
    Transient {
        retry_after: Option<Duration>,
    },
    /// The conversation is longer than the model's context window.
    Overflow,
    /// The proxy is not set up, or turned the key down.
    Unauthorized,
    Other,
}

/// One completion without tools (`Proxy::complete`): a title, a summary.
#[derive(Debug, Clone, PartialEq)]
pub struct Completion {
    pub text: String,
    pub message: Assistant,
}

/// The client: one for the host, shared by its sessions (a clone is the same client). It
/// keeps the connections to the proxy and what the proxy said of its models.
#[derive(Clone)]
pub struct Proxy {
    inner: Arc<Inner>,
}

struct Inner {
    dirs: Dirs,
    http: reqwest::Client,
    catalog: catalog::Cache,
    /// How long the proxy may send nothing: `STALL`, or less for a test.
    stall: Duration,
}

/// Where the proxy is and the key it takes.
struct Connection {
    /// Without a trailing slash.
    url: String,
    key: String,
}

/// A reply has stalled when nothing came for this long: a provider that is only slow still
/// sends pings, or the thinking it does. Also how long the proxy has to begin its answer.
const STALL: Duration = Duration::from_secs(300);

impl Proxy {
    /// The client of the proxy `agent.toml` (in `dirs.config`) names. The file is read for
    /// every request, so a changed address or key applies to the next one.
    pub fn new(dirs: Dirs) -> Self {
        Self::with_stall(dirs, STALL)
    }

    /// `new`, with how long the proxy may send nothing.
    fn with_stall(dirs: Dirs, stall: Duration) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(format!("dirigent/{}", dirigent_protocol::release::VERSION))
            .connect_timeout(Duration::from_secs(15))
            .build()
            .expect("the HTTP client is built from constants");
        Self { inner: Arc::new(Inner { dirs, http, catalog: catalog::Cache::default(), stall }) }
    }

    /// Stream one reply: `on` gets it piece by piece, the `Reply` has the whole. Never fails:
    /// what went wrong is in the `Reply` (the content up to there, `Stop::Error`,
    /// `message.error`, `failure`), and trying again is the caller's to decide. `cancel` ends
    /// it with what came so far (`Stop::Aborted`).
    pub async fn stream(&self, request: &Request<'_>, on: &mut (dyn FnMut(Chunk) + Send), cancel: &CancellationToken) -> Reply {
        let (path, headers, body, mut wire): (_, _, _, Box<dyn Wire>) = match request.model.api {
            Api::Anthropic => {
                (anthropic::PATH, anthropic::headers(request), anthropic::body(request), Box::new(anthropic::Stream::default()))
            }
            Api::Codex => (codex::PATH, codex::headers(request), codex::body(request), Box::new(codex::Stream::default())),
        };
        let outcome = tokio::select! {
            biased;
            () = cancel.cancelled() => None,
            outcome = self.run(path, headers, body, wire.as_mut(), on) => Some(outcome),
        };
        reply(request.model, wire.take(), outcome)
    }

    /// One completion without tools: a title, a compaction's summary. An error's cause is an
    /// `Error`, which says whether another try is worth it.
    pub async fn complete(
        &self,
        model: &Model,
        system: &str,
        text: &str,
        effort: Option<&str>,
        cancel: &CancellationToken,
    ) -> anyhow::Result<Completion> {
        let messages = [Message::User(vec![Part::Text(text.to_owned())])];
        let request = Request { model, system, messages: &messages, tools: &[], effort, max_output: None, cache_key: "" };
        let Reply { message, failure } = self.stream(&request, &mut |_| {}, cancel).await;
        match message.stop {
            Stop::Aborted => anyhow::bail!("cancelled"),
            Stop::Error => {
                let message = message.error.unwrap_or_else(|| "The request failed.".into());
                Err(Error { failure: failure.unwrap_or(Failure::Other), message }.into())
            }
            Stop::End | Stop::ToolUse | Stop::MaxTokens => Ok(Completion { text: text_of(&message.content), message }),
        }
    }

    /// Send the request and feed what streams back to `wire`, until the reply is complete.
    async fn run(
        &self,
        path: &str,
        headers: Vec<(&'static str, String)>,
        body: Value,
        wire: &mut dyn Wire,
        on: &mut (dyn FnMut(Chunk) + Send),
    ) -> Result<(), Error> {
        let connection = self.connection()?;
        let mut request = self.inner.http.post(format!("{}{path}", connection.url)).bearer_auth(&connection.key);
        request = request.header("content-type", "application/json").header("accept", "text/event-stream");
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let stall = self.inner.stall;
        let stalled = || Error::transient(format!("The proxy sent nothing for {} minutes.", stall.as_secs() / 60));
        // The wait for the stream ends like one within it: a proxy that takes the request and
        // then says nothing, or an error whose body never ends, would hold the turn for good.
        let mut response = match tokio::time::timeout(stall, open(request.body(body.to_string()), &connection.url)).await {
            Ok(response) => response?,
            Err(_) => return Err(stalled()),
        };
        on(Chunk::Start);
        let mut events = sse::Decoder::default();
        loop {
            let bytes = match tokio::time::timeout(stall, response.chunk()).await {
                Err(_) => return Err(stalled()),
                Ok(Err(e)) => return Err(Error::transient(format!("The connection to the proxy broke: {}", failure::cause(&e)))),
                Ok(Ok(None)) => break,
                Ok(Ok(Some(bytes))) => bytes,
            };
            for event in events.push(&bytes) {
                if wire.event(event, on)? {
                    return Ok(());
                }
            }
        }
        if let Some(event) = events.finish()
            && wire.event(event, on)?
        {
            return Ok(());
        }
        Err(Error::transient("The connection to the proxy closed before the reply was complete."))
    }

    /// The proxy as `agent.toml` names it now.
    fn connection(&self) -> Result<Connection, Error> {
        let proxy = Config::load(&self.inner.dirs).map_err(|e| Error::other(format!("{e:#}")))?.proxy;
        let (url, key) = (address(&proxy.url), proxy.key.trim());
        if url.is_empty() || key.is_empty() {
            return Err(Error::not_set_up());
        }
        Ok(Connection { url, key: key.to_owned() })
    }

    /// GET `path` at the proxy: its JSON.
    async fn get(&self, connection: &Connection, path: &str) -> Result<Value, Error> {
        let request =
            self.inner.http.get(format!("{}{path}", connection.url)).bearer_auth(&connection.key).header("accept", "application/json");
        let response = request.timeout(Duration::from_secs(30)).send().await.map_err(|e| Error::unreachable(&connection.url, &e))?;
        let bytes = success(response).await?.bytes().await.map_err(|e| Error::unreachable(&connection.url, &e))?;
        serde_json::from_slice(&bytes).map_err(|_| Error::not_a_proxy())
    }
}

/// The proxy's address from what was typed for it: `127.0.0.1:8317` will do, and so will
/// the address of one of its APIs (`.../v1`), which is what other clients ask for.
fn address(typed: &str) -> String {
    let url = typed.trim().trim_end_matches('/');
    let url = ["/v1", "/backend-api"].iter().find_map(|api| url.strip_suffix(api)).unwrap_or(url);
    if url.is_empty() || url.contains("://") { url.to_owned() } else { format!("http://{url}") }
}

/// Send `request` to the proxy at `url`: its answer once that is a stream to read, or why
/// it is none. Nothing here has a time limit of its own.
async fn open(request: reqwest::RequestBuilder, url: &str) -> Result<reqwest::Response, Error> {
    let response = success(request.send().await.map_err(|e| Error::unreachable(url, &e))?).await?;
    // Something else than a stream is nothing a wire can read: a JSON error sent as a
    // success, or a page of whatever else is at that address.
    let kind = response.headers().get("content-type").and_then(|kind| kind.to_str().ok()).unwrap_or("text/event-stream");
    if kind.starts_with("text/event-stream") {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    Err(serde_json::from_str::<Value>(&body).map_or_else(|_| Error::not_a_proxy(), |json| Error::in_stream(&json)))
}

/// `response` when it is a success, else why it is not.
async fn success(response: reqwest::Response) -> Result<reqwest::Response, Error> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let retry_after = failure::retry_after(response.headers());
    let body = response.text().await.unwrap_or_default();
    Err(Error::http(status.as_u16(), retry_after, &body))
}

/// A wire's reading of a streamed reply: its events as chunks and the reply they add up to.
trait Wire: Send {
    /// Take the stream's next event. `true`: the reply is complete.
    fn event(&mut self, event: sse::Event, on: &mut (dyn FnMut(Chunk) + Send)) -> Result<bool, Error>;

    /// The reply as far as the events got.
    fn take(&mut self) -> Partial;
}

/// A reply as far as its stream got.
#[derive(Default)]
struct Partial {
    /// A tool call's input is null until all of it came (`call_input`).
    content: Vec<Block>,
    /// The tokens; what they cost is added at the end.
    usage: Usage,
    /// How many of `usage.cache_write` are kept for an hour, which costs more.
    cache_write_1h: u64,
    /// Why the model stopped, once the stream said so.
    stop: Option<Stop>,
}

/// The `Reply` of a stream that got as far as `partial` and ended as `outcome` says (none:
/// it was cancelled).
fn reply(model: &Model, partial: Partial, outcome: Option<Result<(), Error>>) -> Reply {
    let Partial { mut content, mut usage, cache_write_1h, stop } = partial;
    // What never got whole is no part of the reply: a call without its input, a block that
    // only began.
    content.retain(|block| match block {
        Block::ToolCall { input, .. } => !input.is_null(),
        Block::Text { text, .. } => !text.is_empty(),
        Block::Thinking { text, signature, .. } => !text.is_empty() || signature.is_some(),
        Block::Image { .. } => true,
    });
    let calls = content.iter().any(|block| matches!(block, Block::ToolCall { .. }));
    let outcome = outcome.map(|outcome| {
        outcome.and_then(|()| match stop {
            None => Err(Error::transient("The reply ended without saying why.")),
            // The limit stays the stop, with calls too: it says that the last may be cut off.
            Some(Stop::MaxTokens) => Ok(Stop::MaxTokens),
            // The calls say whether the run goes on, whatever the provider called the stop.
            Some(_) if calls => Ok(Stop::ToolUse),
            Some(_) => Ok(Stop::End),
        })
    });
    let (stop, error, failure) = match outcome {
        None => (Stop::Aborted, None, None),
        Some(Ok(stop)) => (stop, None, None),
        Some(Err(error)) => (Stop::Error, Some(error.message), Some(error.failure)),
    };
    usage.cost = pricing::cost(&model.price, &usage, cache_write_1h);
    let message = Assistant {
        content,
        model: model.id.clone(),
        provider: model.owner.clone(),
        api: model.api,
        effort: None,
        usage,
        stop,
        error,
        duration_ms: 0,
    };
    Reply { message, failure }
}

/// Said in place of an image to a model that takes none.
const NO_IMAGES: &str = "(image omitted: the model does not take images)";

/// The conversation's name for the provider, when it has one that fits a header.
fn session<'a>(request: &Request<'a>) -> Option<&'a str> {
    Some(request.cache_key).filter(|key| !key.is_empty() && key.chars().all(|c| c.is_ascii_graphic()))
}

/// A tool call's id as both wires take it: a reply of one wire may be history on the other,
/// whose ids are of another make.
fn call_id(id: &str) -> String {
    id.chars().take(64).map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

/// A finished tool call's arguments, the text the model wrote, as the call's input: the JSON
/// object they are, mended if need be (`repair`). Anything else stays the text it is, as a
/// JSON string, which tells the caller not to run the call (`Reply`).
fn call_input(arguments: &str) -> Value {
    // A call without arguments streams none at all.
    if arguments.trim().is_empty() {
        return Value::Object(Default::default());
    }
    let object = |json: &str| serde_json::from_str::<Value>(json).ok().filter(Value::is_object);
    object(arguments).or_else(|| object(&repair(arguments))).unwrap_or_else(|| arguments.into())
}

/// `json` with the two things mended that models get wrong inside its strings (as pi-ai
/// did): a control character that is there raw (a line break in a command) is escaped, and
/// a backslash that begins no escape (a Windows path, a regex) is doubled.
fn repair(json: &str) -> String {
    let mut repaired = String::with_capacity(json.len());
    let mut in_string = false;
    let mut chars = json.char_indices();
    while let Some((at, c)) = chars.next() {
        match c {
            '"' => {
                in_string = !in_string;
                repaired.push(c);
            }
            '\\' if in_string => {
                // How much of what follows is the escape's: `uXXXX`, one of JSON's letters, or nothing.
                let escape = match &json.as_bytes()[at + 1..] {
                    [b'u', hex @ ..] if hex.len() >= 4 && hex[..4].iter().all(u8::is_ascii_hexdigit) => 5,
                    [b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't', ..] => 1,
                    _ => 0,
                };
                repaired.push_str(if escape == 0 { "\\\\" } else { &json[at..=at + escape] });
                if escape > 0 {
                    chars.nth(escape - 1);
                }
            }
            c if in_string && c < ' ' => repaired.push_str(&format!("\\u{:04x}", c as u32)),
            c => repaired.push(c),
        }
    }
    repaired
}

/// Unix millis.
fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub(super) fn model(id: &str, api: Api) -> Model {
        Model {
            id: id.into(),
            name: id.into(),
            owner: "test".into(),
            api,
            context_window: 1_000_000,
            max_output: 128_000,
            efforts: ["low", "medium", "high"].map(String::from).to_vec(),
            images: true,
            price: Price { input: 1.0, output: 5.0, cache_read: 0.1, cache_write: 1.25, long: None },
        }
    }

    /// Directories of a test's own.
    pub(super) fn dirs() -> (tempfile::TempDir, Dirs) {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs { config: tmp.path().join("config"), data: tmp.path().join("data") };
        (tmp, dirs)
    }

    /// Make the `agent.toml` in `dirs` name the proxy at `url`.
    pub(super) fn point_at(dirs: &Dirs, url: &str, key: &str) {
        Config::update(dirs, |config| {
            config.proxy = crate::config::Proxy { url: url.into(), key: key.into() };
            Ok(())
        })
        .unwrap();
    }

    /// A system prompt, a conversation and tools with a bit of everything a wire has to say:
    /// images, both wires' thinking, calls and their results, what goes unsaid.
    pub(super) fn request_parts() -> (&'static str, Vec<Message>, Vec<ToolSpec>) {
        let image = |data: &str| Part::Image { mime: "image/png".into(), data: data.into() };
        let text = |text: &str| Part::Text(text.into());
        let thinking = |text: &str, signature: Option<&str>, redacted| Block::Thinking {
            text: text.into(),
            signature: signature.map(str::to_owned),
            redacted,
        };
        let reasoning =
            r#"{"id":"rs_1","type":"reasoning","encrypted_content":"enc","summary":[{"type":"summary_text","text":"Thought."}]}"#;
        let messages = vec![
            Message::User(vec![text("Look at this."), image("aW1n")]),
            Message::Assistant {
                api: Api::Anthropic,
                content: vec![
                    thinking("Two calls.", Some("sig"), false),
                    thinking("", Some("opaque"), true),
                    thinking("Never signed.", None, false),
                    Block::text(" "),
                    Block::text("Running both."),
                    Block::ToolCall { id: "call_1".into(), name: "bash".into(), input: json!({"command": "ls"}) },
                    // One whose arguments were no JSON object (`call_input`).
                    Block::ToolCall { id: "call_2".into(), name: "bash".into(), input: json!("{\"command\": ") },
                ],
            },
            Message::ToolResult { call_id: "call_1".into(), content: vec![text("a"), text("b")], is_error: false },
            Message::ToolResult { call_id: "call_2".into(), content: vec![text("boom"), image("c2hvdA==")], is_error: true },
            Message::User(vec![text("And then?")]),
            Message::Assistant {
                api: Api::Codex,
                content: vec![
                    thinking("Thought.", Some(reasoning), false),
                    Block::text("Done."),
                    thinking("Cut short.", Some(reasoning), false),
                ],
            },
            Message::User(vec![text("Thanks.")]),
        ];
        let tools = vec![
            ToolSpec::new(
                "bash",
                "Run a command.",
                json!({"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}),
            ),
            ToolSpec::new("mcp__docs__find", "Find.", json!({"anyOf": [{"type": "object"}], "$defs": {"q": {"type": "string"}}})),
        ];
        ("Be terse.", messages, tools)
    }

    /// Feed a recorded stream to `wire`: what it made of it, the chunks it gave, and how the
    /// last event it took went.
    pub(super) fn replay(mut wire: impl Wire, stream: &str) -> (Partial, Vec<Chunk>, Result<bool, Error>) {
        let mut chunks = vec![];
        let mut decoder = sse::Decoder::default();
        let mut outcome = Ok(false);
        for event in decoder.push(stream.as_bytes()).into_iter().chain(decoder.finish()) {
            outcome = wire.event(event, &mut |chunk| chunks.push(chunk));
            if outcome != Ok(false) {
                break;
            }
        }
        (wire.take(), chunks, outcome)
    }

    /// What the stand-in proxy (`serve`) answers a request with.
    pub(super) struct Answer {
        status: u16,
        kind: &'static str,
        body: String,
        /// Keep the connection open after the body: a stream that stalls.
        stall: bool,
    }

    impl Answer {
        pub fn json(status: u16, body: &Value) -> Self {
            Self { status, kind: "application/json", body: body.to_string(), stall: false }
        }

        /// An event stream; the connection closes after it, or stays open when it is to `stall`.
        pub fn stream(body: &str, stall: bool) -> Self {
            Self { status: 200, kind: "text/event-stream", body: body.into(), stall }
        }
    }

    /// A stand-in for the proxy on a port of this machine: `answer` gets each request's path
    /// and body. Returns its address.
    pub(super) async fn serve(answer: impl Fn(&str, &str) -> Answer + Send + Sync + 'static) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let answer = Arc::new(answer);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let answer = answer.clone();
                tokio::spawn(async move {
                    // The request: its head, then as much body as the head says.
                    let mut request = vec![];
                    let mut buffer = [0u8; 16 * 1024];
                    let (head, length) = loop {
                        match socket.read(&mut buffer).await {
                            Ok(read) if read > 0 => request.extend_from_slice(&buffer[..read]),
                            _ => return,
                        }
                        if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&request[..end]).to_lowercase();
                            let length = head
                                .lines()
                                .find_map(|line| line.strip_prefix("content-length:"))
                                .map_or(0, |length| length.trim().parse::<usize>().unwrap());
                            break (end + 4, length);
                        }
                    };
                    while request.len() < head + length {
                        match socket.read(&mut buffer).await {
                            Ok(read) if read > 0 => request.extend_from_slice(&buffer[..read]),
                            _ => return,
                        }
                    }
                    let text = String::from_utf8_lossy(&request);
                    let path = text.split_whitespace().nth(1).unwrap_or_default();
                    let Answer { status, kind, body, stall } = answer(path, &text[head..]);
                    // No length: the body ends where the connection does, as a stream's.
                    let answer = format!("HTTP/1.1 {status} Whatever\r\ncontent-type: {kind}\r\nconnection: close\r\n\r\n{body}");
                    let _ = socket.write_all(answer.as_bytes()).await;
                    if stall {
                        tokio::time::sleep(Duration::from_secs(60)).await;
                    }
                });
            }
        });
        url
    }

    /// The transport, against a stand-in proxy that plays recorded streams.
    #[tokio::test]
    async fn a_reply_streams_over_http() {
        let whole = include_str!("proxy/fixtures/anthropic_tools.sse");
        // The stream up to the middle of its text.
        let half = &whole[..whole.find(" bash call").unwrap()];
        let half = format!("{}\n\n", &half[..half.rfind("\n\n").unwrap()]);
        let url = serve(move |path, body| match (path, body) {
            ("/v1/messages", body) if body.contains("deny") => Answer::json(401, &json!({"error": "Invalid API key"})),
            ("/v1/messages", body) if body.contains("busy") => {
                Answer::json(529, &json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}))
            }
            ("/v1/messages", body) if body.contains("page") => Answer { kind: "text/html", ..Answer::stream("<html>hello</html>", false) },
            ("/v1/messages", body) if body.contains("break") => Answer::stream(&half, false),
            ("/v1/messages", body) if body.contains("stall") => Answer::stream(&half, true),
            ("/v1/messages", _) => Answer::stream(whole, false),
            _ => Answer::json(404, &json!({"error": "not found"})),
        })
        .await;
        let (_tmp, dirs) = dirs();
        point_at(&dirs, &url, "key");
        let proxy = Proxy::new(dirs);
        let model = model("claude-haiku-5-5", Api::Anthropic);
        let ask = |system: &'static str| Request {
            model: &model,
            system,
            messages: &[],
            tools: &[],
            effort: None,
            max_output: None,
            cache_key: "",
        };
        let never = CancellationToken::new();

        let mut chunks = vec![];
        let reply = proxy.stream(&ask(""), &mut |chunk| chunks.push(chunk), &never).await;
        assert_eq!((reply.message.stop, reply.message.content.len(), reply.failure), (Stop::ToolUse, 4, None));
        assert_eq!(chunks[0], Chunk::Start);
        assert_eq!(chunks.iter().filter(|chunk| matches!(chunk, Chunk::ToolCall { .. })).count(), 2);
        // 2 tokens in, 280 out, 627 read from the cache and 25 written to it for an hour, at $1, $5,
        // $0.1 and twice $1 a million.
        let cost = reply.message.usage.cost;
        let close = |a: f64, b: f64| (a - b).abs() < 1e-12;
        assert!(
            close(cost.input, 2e-6) && close(cost.output, 1400e-6) && close(cost.cache_read, 62.7e-6) && close(cost.cache_write, 50e-6),
            "{cost:?}"
        );

        // The proxy says no, in the ways the caller tells apart.
        let reply = proxy.stream(&ask("deny"), &mut |_| {}, &never).await;
        assert_eq!((reply.message.stop, reply.failure), (Stop::Error, Some(Failure::Unauthorized)));
        assert!(reply.message.error.unwrap().starts_with("The proxy turned the request down: Invalid API key (HTTP 401)."));
        let reply = proxy.stream(&ask("busy"), &mut |_| {}, &never).await;
        assert_eq!(
            (reply.message.error.as_deref(), reply.failure),
            (Some("Overloaded (overloaded_error) (HTTP 529)"), Some(Failure::Transient { retry_after: None }))
        );
        let reply = proxy.stream(&ask("page"), &mut |_| {}, &never).await;
        assert_eq!(reply.failure, Some(Failure::Other));

        // The connection breaks midway: what came is kept, and another try is worth it.
        let reply = proxy.stream(&ask("break"), &mut |_| {}, &never).await;
        assert_eq!((reply.message.stop, reply.failure), (Stop::Error, Some(Failure::Transient { retry_after: None })));
        assert!(
            matches!(reply.message.content.as_slice(), [Block::Thinking { signature: Some(_), .. }, Block::Text { text, .. }] if text.ends_with("so the")),
            "{:?}",
            reply.message.content
        );
        assert_eq!((reply.message.usage.cache_read, reply.message.usage.output), (627, 2));

        // The user stops it once text streams: the same, as an abort.
        let stop = CancellationToken::new();
        let mut on = |chunk| {
            if matches!(chunk, Chunk::Text { .. }) {
                stop.cancel();
            }
        };
        let reply = proxy.stream(&ask("stall"), &mut on, &stop).await;
        assert_eq!((reply.message.stop, &reply.message.error, reply.failure), (Stop::Aborted, &None, None));
        assert!(matches!(reply.message.content.as_slice(), [Block::Thinking { .. }, Block::Text { text, .. }] if !text.is_empty()));
    }

    fn done(stop: Stop) -> Partial {
        Partial { stop: Some(stop), ..Partial::default() }
    }

    #[test]
    fn a_reply_is_what_got_whole() {
        let model = model("claude-haiku-5-5", Api::Anthropic);
        let call = |input: Value| Block::ToolCall { id: "c".into(), name: "bash".into(), input };
        let unsigned = Block::Thinking { text: String::new(), signature: None, redacted: false };
        let content = vec![unsigned, Block::text(""), Block::text("Hi."), call(json!({})), call(Value::Null)];
        let usage = Usage { input: 1_000_000, output: 1_000_000, cache_read: 1_000_000, cache_write: 1_000_000, ..Usage::default() };

        // Calls make it a tool use, whatever the provider called the stop; the costs are added.
        let reply =
            super::reply(&model, Partial { content: content.clone(), usage, cache_write_1h: 1_000_000, ..done(Stop::End) }, Some(Ok(())));
        assert_eq!(reply.message.content, [Block::text("Hi."), call(json!({}))]);
        assert_eq!((reply.message.stop, &reply.message.error, reply.failure), (Stop::ToolUse, &None, None));
        assert_eq!((reply.message.model.as_str(), reply.message.api), ("claude-haiku-5-5", Api::Anthropic));
        let cost = reply.message.usage.cost;
        assert_eq!((cost.input, cost.output, cost.cache_read, cost.cache_write), (1.0, 5.0, 0.1, 2.0));
        assert_eq!(super::reply(&model, done(Stop::ToolUse), Some(Ok(()))).message.stop, Stop::End);

        // Cancelled, failed, or never said why it stopped: what came stays.
        let reply = super::reply(&model, Partial { content: content.clone(), ..Partial::default() }, None);
        assert_eq!((reply.message.stop, reply.message.content.len(), reply.failure), (Stop::Aborted, 2, None));
        let reply = super::reply(&model, Partial { content, ..Partial::default() }, Some(Err(Error::transient("Overloaded"))));
        assert_eq!((reply.message.stop, reply.message.error.as_deref(), reply.message.content.len()), (Stop::Error, Some("Overloaded"), 2));
        assert_eq!(reply.failure, Some(Failure::Transient { retry_after: None }));
        assert!(matches!(super::reply(&model, Partial::default(), Some(Ok(()))).failure, Some(Failure::Transient { .. })));

        // A call whose arguments were no JSON object is a call still, for the caller to answer:
        // it doesn't fail the reply, and the output limit that may have cut it off stays the stop.
        let broken = |stop| Partial { content: vec![call(json!("{\"command\": \"ec"))], ..done(stop) };
        for (stop, becomes) in [(Stop::End, Stop::ToolUse), (Stop::MaxTokens, Stop::MaxTokens)] {
            let reply = super::reply(&model, broken(stop), Some(Ok(())));
            assert_eq!((reply.message.stop, reply.message.content.len(), reply.failure), (becomes, 1, None));
        }
    }

    #[test]
    fn a_calls_arguments_are_mended_or_kept_as_text() {
        assert_eq!(call_input(" "), json!({}));
        assert_eq!(call_input(r#"{"command": "ls", "n": 1}"#), json!({"command": "ls", "n": 1}));
        // A raw line break and tab in a string, and backslashes that begin no escape: before
        // a letter, as the last of `uXXXX` that is none, at the string's end. Escapes stay.
        let mended =
            call_input("{\"command\": \"echo a\n\techo b\", \"path\": \"C:\\Users\\x\\u12\", \"same\": \"a\\n\\\"b\\\" \\u00e9\\\\\"}");
        assert_eq!(mended, json!({"command": "echo a\n\techo b", "path": "C:\\Users\\x\\u12", "same": "a\n\"b\" é\\"}));
        // Outside of strings nothing is mended, and no object is no input: the text is kept.
        for text in ["{\"command\": \"ec", "{\"a\": 1,\u{1}}", "[1]", "null", "\"ls\""] {
            assert_eq!(call_input(text), Value::String(text.into()), "{text}");
        }
    }

    /// Nothing holds a turn for good: a proxy that goes quiet before its stream begins is
    /// given up on like one that does so within it, for another try.
    #[tokio::test]
    async fn a_proxy_that_never_answers_is_given_up_on() {
        // Takes the request and says nothing: a listener nobody accepts from does just that.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mute = format!("http://{}", listener.local_addr().unwrap());
        // An error whose body never ends.
        let endless = serve(|_, _| Answer { stall: true, ..Answer::json(500, &json!({"error": "Half an err"})) }).await;
        let (_tmp, dirs) = dirs();
        let proxy = Proxy::with_stall(dirs.clone(), Duration::from_millis(300));
        let model = model("claude-haiku-5-5", Api::Anthropic);
        let request = Request { model: &model, system: "", messages: &[], tools: &[], effort: None, max_output: None, cache_key: "" };
        for url in [mute, endless] {
            point_at(&dirs, &url, "key");
            let reply = proxy.stream(&request, &mut |_| {}, &CancellationToken::new()).await;
            assert_eq!((reply.message.stop, reply.failure), (Stop::Error, Some(Failure::Transient { retry_after: None })));
            assert!(reply.message.error.unwrap().starts_with("The proxy sent nothing for "));
        }
    }

    #[test]
    fn an_address_is_taken_as_typed() {
        for typed in ["http://127.0.0.1:8317", " 127.0.0.1:8317/ ", "http://127.0.0.1:8317/v1", "http://127.0.0.1:8317/backend-api/"] {
            assert_eq!(address(typed), "http://127.0.0.1:8317", "{typed}");
        }
        assert_eq!(address("https://proxy.example/cpa"), "https://proxy.example/cpa");
        assert_eq!(address(" "), "");
    }

    #[test]
    fn ids_fit_both_wires() {
        assert_eq!(call_id("toolu_01Sh-CNDp"), "toolu_01Sh-CNDp");
        assert_eq!(call_id("call_abc|fc_123:x"), "call_abc_fc_123_x");
        assert_eq!(call_id(&"x".repeat(80)).len(), 64);
    }

    /// Without a proxy in `agent.toml` nothing is asked of anyone: the reply says what to do.
    #[tokio::test]
    async fn without_a_proxy_the_reply_says_to_set_one_up() {
        let (_tmp, dirs) = dirs();
        let proxy = Proxy::new(dirs.clone());
        let model = model("claude-haiku-5-5", Api::Anthropic);
        let request = Request { model: &model, system: "", messages: &[], tools: &[], effort: None, max_output: None, cache_key: "" };
        // A session runs it on a task of its own: the request must be free to move between threads.
        fn sendable<T: Send>(future: T) -> T {
            future
        }
        let reply = sendable(proxy.stream(&request, &mut |_| {}, &CancellationToken::new())).await;
        assert_eq!((reply.message.stop, reply.failure), (Stop::Error, Some(Failure::Unauthorized)));
        assert_eq!(
            reply.message.error.as_deref(),
            Some("No model proxy is set up. Enter your CLIProxyAPI's address and key under Settings › Proxy.")
        );
        let error = proxy.models().await.unwrap_err();
        assert_eq!(error.downcast_ref::<Error>().map(|e| e.failure), Some(Failure::Unauthorized));

        // A proxy that isn't there is worth another try; a cancelled request is neither.
        point_at(&dirs, "http://127.0.0.1:9/", "key");
        let reply = proxy.stream(&request, &mut |_| {}, &CancellationToken::new()).await;
        assert_eq!((reply.message.stop, reply.failure), (Stop::Error, Some(Failure::Transient { retry_after: None })));
        assert!(
            reply.message.error.as_deref().unwrap().starts_with("Could not reach the proxy at http://127.0.0.1:9: "),
            "{:?}",
            reply.message.error
        );
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let reply = proxy.stream(&request, &mut |_| {}, &cancelled).await;
        assert_eq!((reply.message.stop, reply.message.error, reply.failure), (Stop::Aborted, None, None));
    }
}
