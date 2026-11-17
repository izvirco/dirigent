use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    /// What the model calls it by: `bash`, `mcp__<server>__<tool>`.
    pub name: String,
    pub description: String,
    /// JSON Schema of its input, an object.
    pub input: Value,
    /// Its line in the system prompt's list of tools; `None`: not listed there.
    pub snippet: Option<String>,
    /// Rules for the system prompt, said once whichever tools bring them.
    pub guidelines: Vec<String>,
    /// The group codemode lists it under and `describeNamespace` answers for: an MCP
    /// server's `mcp__<server>`.
    pub namespace: Option<String>,
    /// The TypeScript type a script's call resolves to, when that is a value of its own
    /// (`ToolOutput::value`) and not the text.
    pub returns: Option<String>,
    /// Its scripts call it for a thread that has codemode, which is then not told of it (an
    /// MCP server's tools, unless the server is set to `direct`): `Session::declared`.
    pub deferred: bool,
}

impl ToolSpec {
    pub fn new(name: &str, description: &str, input: Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input,
            snippet: None,
            guidelines: vec![],
            namespace: None,
            returns: None,
            deferred: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Content {
    Text(String),
    /// An image as the model is to see it: `mime` is `image/png`, ..., `data` the file's bytes.
    Image {
        mime: String,
        data: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ToolOutput {
    pub content: Vec<Content>,
    pub is_error: bool,
    /// What a codemode script's call resolves to in place of the text (`ToolSpec::returns`).
    pub value: Option<Value>,
}

impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        Self { content: vec![Content::Text(text.into())], ..Self::default() }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self { is_error: true, ..Self::text(text) }
    }

    /// Its text, the text pieces joined by newlines.
    pub fn as_text(&self) -> String {
        let texts = self.content.iter().filter_map(|c| match c {
            Content::Text(t) => Some(t.as_str()),
            Content::Image { .. } => None,
        });
        texts.collect::<Vec<_>>().join("\n")
    }
}

#[derive(Debug, Clone)]
pub struct Call {
    /// The model's id for the call (`Block::ToolCall::id`); a script's calls share their
    /// script's.
    pub id: String,
    /// Cancelled when the user stops the run: the tool ends what it started and returns.
    pub cancel: CancellationToken,
    /// A codemode script calls, not the model: what comes back is the script's to cut down,
    /// so the tool need not shorten it for a context window.
    pub script: bool,
}

/// A tool of one session: made for it, with what it works on (the thread's directory, its
/// jobs, a server's connection) inside.
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;

    /// Run it with `input`, which is the model's: of the schema's shape only if the model
    /// kept to it. An `Err` is a failed call like `ToolOutput::error`, with its message for
    /// the model to read.
    async fn call(&self, input: Value, call: &Call) -> anyhow::Result<ToolOutput>;
}
