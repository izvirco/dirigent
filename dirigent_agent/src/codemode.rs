//! Codemode: the `codemode` tool runs a JavaScript script the model wrote, in a QuickJS
//! sandbox, which calls the session's other tools. Only what the script prints reaches the
//! model, so a script can run calls side by side and cut large results down first.
//!
//! The sandbox knows nothing of sessions: it reaches the tools through `ScriptHost`.

use crate::tool::{Call, ToolOutput, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::BTreeMap;

/// What a script's sandbox reaches of the session it runs in.
#[async_trait]
pub trait ScriptHost: Send + Sync {
    /// Every tool a script can call, the ones not declared to the model included
    /// (`ToolSpec::deferred`); not `codemode` itself.
    fn tools(&self) -> Vec<ToolSpec>;

    /// Call tool `name` for a script: `call` is the script's own (`Call::script`).
    async fn call(&self, name: &str, input: Value, call: &Call) -> ToolOutput;

    /// What `describeNamespace(name)` tells of a group of tools (`ToolSpec::namespace`).
    fn namespace(&self, name: &str) -> Option<Namespace>;

    /// The values earlier scripts kept with `store()`, as of this branch of the thread.
    fn stored(&self) -> BTreeMap<String, Value>;

    /// Keep what a script that succeeded stored: by key, `Value::Null` to forget one.
    fn store(&self, values: BTreeMap<String, Value>);
}

/// A group of tools: an MCP server.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Namespace {
    pub name: String,
    pub description: Option<String>,
    /// How to use its tools, as the server says (or dirigent, for a screen's driver).
    pub instructions: Option<String>,
}
