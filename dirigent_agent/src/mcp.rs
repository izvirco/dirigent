//! MCP servers: the client that connects to them (a command's stdio, or streamable HTTP
//! with OAuth), their tools as `Tool`s of a session, and what Settings › MCP servers does
//! to the entries in `agent.toml`.
//!
//! - `config`: a server's entry (`Server`), what makes one usable, the names its tools get.
//! - `client`: JSON-RPC 2.0 and MCP's lifecycle over a `Transport`, which is `stdio` (a
//!   command) or `http` (streamable HTTP).
//! - `oauth`: an HTTP server's sign-in and its tokens, in `<data dir>/mcp-auth.json`.
//! - `servers`: a session's servers (`Servers`): connected in the background, their tools
//!   as `Tool`s, servers added at run time (computer use, with `Watch`).
//! - `settings`: `list`, `save`, `remove`, `check`, `sign_in`, `sign_out`.
//! - `import`: pi's `mcp.json` and `mcp-auth.json` as ours.
//!
//! Hand-rolled on tokio, reqwest and serde_json, as pi's client was: a tool's result
//! reaches scripts as the JSON the server sent, a failed command's message has its last
//! stderr lines, and the sign-in talks through `SignIn`.

mod client;
mod config;
mod http;
mod import;
mod oauth;
mod servers;
mod settings;
mod stdio;
#[cfg(test)]
pub(crate) mod tests;

pub use client::{Client, Error, ToolInfo};
pub use config::{Exposure, OAuth, Server, namespace};
pub use import::{Imported, import_pi};
pub use oauth::SignIn;
pub use servers::{Added, Servers, Watch};
pub use settings::{check, list, remove, save, sign_in, sign_out};
