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

use crate::tool::ToolSpec;
use dirigent_protocol::transcript::{Api, Assistant, Block};
use serde_json::Value;
use std::time::Duration;

/// A model the proxy serves.
#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Price {
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
    /// The tools declared to the model.
    pub tools: &'a [ToolSpec],
    /// One of `Model::efforts`; `None`: the model's default.
    pub effort: Option<&'a str>,
    /// The most tokens to reply with; `None`: `Model::max_output`.
    pub max_output: Option<u64>,
    /// Names the conversation to the provider's prompt cache: the thread's id.
    pub cache_key: &'a str,
}

/// A piece of a reply as it streams. `index` is the block's place in the reply's content.
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
