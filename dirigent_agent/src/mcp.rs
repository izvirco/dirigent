//! MCP servers: the client that connects to them (a command's stdio, or streamable HTTP
//! with OAuth), their tools as `Tool`s of a session, and what Settings › MCP servers does
//! to the entries in `agent.toml`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// An MCP server's entry in `agent.toml` (`[mcp.<name>]`): the fields of the `mcpServers`
/// shape other clients use, so an entry copied from a server's docs fits.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Server {
    /// A command to run and talk to over its stdio, ...
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// ... or the address of a streamable HTTP server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
}
