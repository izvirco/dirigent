//! One format all the way: what the model said is an `Entry`, in memory, in sqlite and on
//! the wire. Images are never inline: an entry names them by blob id (`Block::Image`), and
//! the host keeps the bytes (`ClientMessage::GetBlob`).

use crate::Question;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// One entry of a thread's history. A thread is a tree: each entry is a child of the one
/// before it (`parent_id`), except where an `EditMessage` branched off an earlier entry.
/// The branch that counts, what the model sees and clients show, is the last entry's and
/// its ancestors (`branch`); older branches stay in the history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// Append order within the thread, from 0. Given by the host as it stores the entry.
    #[serde(default)]
    pub seq: u64,
    pub id: String,
    pub parent_id: Option<String>,
    /// When it was written, unix millis: for a reply or a tool's result, when that ended
    /// (its `duration_ms` earlier it began).
    pub at: i64,
    pub body: Body,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Body {
    /// What the user sent: a prompt, a steer or a follow-up. Text and images.
    User {
        content: Vec<Block>,
    },
    Assistant(Assistant),
    ToolResult(ToolResult),
    /// Something the model was told that nobody typed.
    Notice(Notice),
    /// The context was compacted here: `summary` stands for everything before the entry
    /// `first_kept`. `model` and `usage` are of the request that wrote the summary.
    Compaction {
        summary: String,
        first_kept: String,
        /// The context's size before, in tokens.
        tokens_before: u64,
        model: String,
        usage: Usage,
    },
    /// Compacting the context failed. Not part of the model's context.
    CompactionFailed {
        /// What to tell the user: that it failed, and why.
        error: String,
    },
    /// Values a codemode script kept with `store()`, by key; `null` forgets one. Not part of
    /// the model's context: the scripts on this branch `load()` them.
    Store {
        values: BTreeMap<String, Value>,
    },
    /// A kind a newer host writes.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
        /// The images an assistant's text shows (`crate::images`): link target, as written,
        /// to blob id.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        blobs: BTreeMap<String, String>,
    },
    /// The model's reasoning, or the summary of it the provider gives. `signature` is the
    /// provider's proof of it, opaque, which the same wire (`Assistant::api`) wants back
    /// with the history; `redacted` reasoning has no text, only that.
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        redacted: bool,
    },
    ToolCall {
        id: String,
        name: String,
        input: Value,
    },
    Image {
        /// `image/png`, ...
        mime: String,
        /// The id of its bytes in the host's blobs (their sha256, hex).
        blob: String,
    },
}

impl Block {
    pub fn text(text: impl Into<String>) -> Self {
        Block::Text { text: text.into(), blobs: BTreeMap::new() }
    }
}

/// The text of `blocks`, its text blocks joined by newlines.
pub fn text_of(blocks: &[Block]) -> String {
    let texts = blocks.iter().filter_map(|b| match b {
        Block::Text { text, .. } if !text.is_empty() => Some(text.as_str()),
        _ => None,
    });
    texts.collect::<Vec<_>>().join("\n")
}

/// The wire a model is reached over (`dirigent_agent`'s `proxy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Api {
    /// Anthropic's Messages: Claude.
    Anthropic,
    /// OpenAI's Responses as Codex speaks it: everything else.
    Codex,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Assistant {
    /// Thinking, text and tool calls, in the model's order.
    pub content: Vec<Block>,
    pub model: String,
    /// Who makes the model, as the proxy says: `anthropic`, `openai`. What usage is summed under.
    #[serde(default)]
    pub provider: String,
    pub api: Api,
    /// The reasoning effort asked for.
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub usage: Usage,
    pub stop: Stop,
    /// Why it failed (`Stop::Error`).
    #[serde(default)]
    pub error: Option<String>,
    /// From the request to the end of the reply.
    #[serde(default)]
    pub duration_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stop {
    /// The model was done: the reply ends the run.
    End,
    /// It called tools: the run goes on with their results.
    ToolUse,
    /// It ran into the output limit.
    MaxTokens,
    Error,
    /// The user stopped it.
    Aborted,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    /// Prompt tokens neither read from nor written to the cache.
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// Dollars at the model's list price (`dirigent_agent`'s `pricing`): what the tokens
    /// are worth, not what a subscription behind the proxy was billed.
    pub cost: Cost,
}

impl Usage {
    /// Every prompt token: fresh, read from cache and written to it.
    pub fn prompt(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

impl Cost {
    pub fn total(&self) -> f64 {
        self.input + self.output + self.cache_read + self.cache_write
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    /// The `Block::ToolCall::id` it answers.
    pub call_id: String,
    pub tool: String,
    /// Text and images.
    pub content: Vec<Block>,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default)]
    pub duration_ms: i64,
}

/// Something the model is told that nobody typed. `text` is what it reads; the rest is
/// for clients to show.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Notice {
    /// An `async_bash` job ended.
    Job {
        text: String,
        job_id: String,
        command: String,
        exit_code: i32,
        duration_ms: i64,
        /// The file its output went to.
        log: String,
    },
    /// A sub-agent's run ended: with a reply, waiting on the user (`asking`), or without
    /// either (`failed`: it was aborted, or broke).
    Child {
        text: String,
        thread_id: String,
        name: String,
        #[serde(default)]
        asking: bool,
        #[serde(default)]
        failed: bool,
    },
    /// The user's answers to an `ask_user` call: one list per question, or `None` when
    /// they dismissed it.
    Answer { text: String, ask_id: String, questions: Vec<Question>, answers: Option<Vec<Vec<String>>> },
    /// Something else its host tells it: that the thread works in another directory now.
    Info { text: String },
}

impl Notice {
    /// What the model reads.
    pub fn text(&self) -> &str {
        match self {
            Notice::Job { text, .. } | Notice::Child { text, .. } | Notice::Answer { text, .. } | Notice::Info { text } => text,
        }
    }
}

/// What happens in a thread between entries (`ServerMessage::Event`): nothing here is
/// stored, and a client that missed one loses nothing but the moment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A run began: the model works on the thread until `RunEnd`.
    RunStart,
    /// The run is over: nothing streams, runs or is queued any more.
    RunEnd,
    /// The model began a reply. Its content streams next, by block `index`; the `Entry`
    /// that follows has the whole of it.
    MessageStart,
    Text {
        index: usize,
        delta: String,
    },
    Thinking {
        index: usize,
        delta: String,
    },
    /// A tool call of the streaming reply, whole.
    ToolCall {
        index: usize,
        id: String,
        name: String,
        input: Value,
    },
    /// The tool call `id` runs.
    ToolStart {
        id: String,
    },
    ToolEnd {
        id: String,
    },
    /// The user's messages waiting for the model, by text: steers go in after its current
    /// tool calls, follow-ups once it would stop. Sent whenever either changes.
    Queue {
        steering: Vec<String>,
        follow_up: Vec<String>,
    },
    CompactionStart,
    CompactionEnd,
    /// A request failed in a way worth another try, which comes in `delay_ms`.
    Retry {
        attempt: u32,
        delay_ms: u64,
        error: String,
    },
    /// Something the user should know that is no part of the conversation.
    Warning {
        message: String,
    },
    /// The thread got the name generated from its first prompt (`ThreadSummary::name`).
    Titled,
    /// A step of the thread's computer use. `frame` is the id of the picture its screen
    /// shows now (0: none yet; `ClientMessage::GetScreen` has it), `action` what the agent
    /// just did there, in a few words, and `cursor` where it clicked, as fractions `[x, y]`
    /// of the picture; both absent when only the picture changed. `desktop`: it happened on
    /// the desktop of the host's user, not on a screen of the thread's own.
    Screen {
        frame: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        action: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor: Option<[f32; 2]>,
        #[serde(default)]
        desktop: bool,
    },
    /// An event a newer host sends.
    #[serde(other)]
    Unknown,
}

/// The branch of `entries` (a thread's, in `seq` order) that ends at the entry `leaf`: it
/// and its ancestors, oldest first; empty without such an entry. With the last entry as
/// `leaf`, the thread's current branch: all of it, unless an edit branched it.
pub fn branch<'a>(entries: &'a [Entry], leaf: &str) -> Vec<&'a Entry> {
    let Some(end) = entries.iter().rposition(|e| e.id == leaf) else { return vec![] };
    let entries = &entries[..=end];
    if entries.windows(2).all(|w| w[1].parent_id.as_deref() == Some(w[0].id.as_str())) {
        return entries.iter().collect();
    }
    let by_id: std::collections::HashMap<&str, &Entry> = entries.iter().map(|e| (e.id.as_str(), e)).collect();
    let mut branch = vec![&entries[end]];
    while let Some(parent) = branch[branch.len() - 1].parent_id.as_deref().and_then(|p| by_id.get(p))
        && branch.len() <= entries.len()
    {
        branch.push(parent);
    }
    branch.reverse();
    branch
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(id: &str, parent: Option<&str>, body: Body) -> Entry {
        Entry { seq: 0, id: id.into(), parent_id: parent.map(str::to_owned), at: 1, body }
    }

    #[test]
    fn bodies_keep_their_shape() {
        let user = Body::User { content: vec![Block::text("hi"), Block::Image { mime: "image/png".into(), blob: "ab".into() }] };
        assert_eq!(
            serde_json::to_value(&user).unwrap(),
            json!({"type":"user","content":[{"type":"text","text":"hi"},{"type":"image","mime":"image/png","blob":"ab"}]})
        );
        let assistant = Body::Assistant(Assistant {
            content: vec![
                Block::Thinking { text: "hm".into(), signature: Some("sig".into()), redacted: false },
                Block::ToolCall { id: "c".into(), name: "bash".into(), input: json!({"command":"ls"}) },
            ],
            model: "claude-opus-5-5".into(),
            provider: "anthropic".into(),
            api: Api::Anthropic,
            effort: Some("high".into()),
            usage: Usage { input: 1, output: 2, cache_read: 3, cache_write: 4, cost: Cost { input: 0.5, ..Cost::default() } },
            stop: Stop::ToolUse,
            error: None,
            duration_ms: 9,
        });
        assert_eq!(
            serde_json::to_value(&assistant).unwrap(),
            json!({"type":"assistant","model":"claude-opus-5-5","provider":"anthropic","api":"anthropic","effort":"high","stop":"tool_use","error":null,
                "duration_ms":9,
                "usage":{"input":1,"output":2,"cache_read":3,"cache_write":4,"cost":{"input":0.5,"output":0.0,"cache_read":0.0,"cache_write":0.0}},
                "content":[{"type":"thinking","text":"hm","signature":"sig"},{"type":"tool_call","id":"c","name":"bash","input":{"command":"ls"}}]})
        );
        let notice = Body::Notice(Notice::Info { text: "moved".into() });
        assert_eq!(serde_json::to_value(&notice).unwrap(), json!({"type":"notice","kind":"info","text":"moved"}));
        for body in [user, assistant, notice] {
            let e = entry("a", None, body);
            assert_eq!(serde_json::from_str::<Entry>(&serde_json::to_string(&e).unwrap()).unwrap(), e);
        }
        let newer = json!({"id":"a","parent_id":null,"at":1,"body":{"type":"hologram","x":1}});
        assert_eq!(serde_json::from_value::<Entry>(newer).unwrap().body, Body::Unknown);
        assert_eq!(serde_json::from_value::<Event>(json!({"type":"hologram"})).unwrap(), Event::Unknown);
    }

    #[test]
    fn branches_follow_parent_links() {
        let e = |id: &str, parent: Option<&str>| entry(id, parent, Body::User { content: vec![] });
        let entries = [e("a", None), e("b", Some("a")), e("c", Some("b")), e("d", Some("b")), e("e", Some("d"))];
        let ids = |b: Vec<&Entry>| b.iter().map(|e| e.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(branch(&entries, "e")), ["a", "b", "d", "e"]);
        assert_eq!(ids(branch(&entries, "c")), ["a", "b", "c"]);
        assert_eq!(ids(branch(&entries[..3], "c")), ["a", "b", "c"]);
        assert!(branch(&entries, "x").is_empty());
    }
}
