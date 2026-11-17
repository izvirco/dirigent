use super::config::{self, Reach, Server, namespace};
use super::oauth::{Auth, SignIn, Store};
use super::servers::{Added, Connection, Status};
use crate::config::{Config, Dirs};
use anyhow::{Result, anyhow, bail};
use dirigent_protocol::{McpServer, McpServerStatus, McpState, McpTool};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const CHECK_LIMIT: Duration = Duration::from_secs(45);

/// Every server in `agent.toml`, by name, with why a session skips it if it does, and the
/// file's path. Connects to none.
pub fn list(dirs: &Dirs) -> Result<(Vec<McpServer>, String)> {
    let servers = Config::load(dirs)?.mcp;
    let store = Store::new(dirs);
    let listed = config::checked(&servers).into_iter().map(|(name, server, reach)| {
        let signed_in = matches!(&reach, Ok(Reach::Http(url)) if store.load(&Store::key(name, url)).tokens.is_some());
        McpServer { name: name.to_owned(), config: serde_json::to_value(server).unwrap_or_default(), error: reach.err(), signed_in }
    });
    Ok((listed.collect(), dirs.config_file().to_string_lossy().into_owned()))
}

/// Add server `name` with `config` (an entry as `list` gives them), or replace its entry.
/// Refused, saying why, if a session couldn't use it. A sign-in is for an address: one
/// stored for the address the entry had goes.
pub fn save(dirs: &Dirs, name: &str, config: Value) -> Result<()> {
    let mut server: Server = serde_json::from_value(config).map_err(|e| anyhow!("server \"{name}\": {e}"))?;
    server.oauth = server.oauth.filter(|oauth| *oauth != Default::default());
    let reach = server.reach(name).map_err(|why| anyhow!(why))?;
    let mut replaced = None;
    Config::update(dirs, |config| {
        let close = config.mcp.keys().find(|other| namespace(other) == namespace(name));
        if let Some(other) = close.filter(|_| !config.mcp.contains_key(name)) {
            bail!("\"{name}\" is too close to \"{other}\": pick a name that differs in more than - and _");
        }
        replaced = config.mcp.insert(name.to_owned(), server);
        Ok(())
    })?;
    match replaced.map(|replaced| replaced.reach(name)) {
        Some(Ok(was)) if was != reach => forget(dirs, name, &was),
        _ => Ok(()),
    }
}

/// Remove server `name`, and its sign-in with it.
pub fn remove(dirs: &Dirs, name: &str) -> Result<()> {
    let mut removed = None;
    Config::update(dirs, |config| {
        removed = config.mcp.remove(name);
        Ok(())
    })?;
    let Some(server) = removed else { bail!("No MCP server named \"{name}\"") };
    server.reach(name).map_or(Ok(()), |was| forget(dirs, name, &was))
}

fn forget(dirs: &Dirs, name: &str, was: &Reach) -> Result<()> {
    match was {
        Reach::Http(url) => Auth::new(dirs, name, url, None).sign_out().map(drop),
        Reach::Stdio(_) => Ok(()),
    }
}

fn connection(dirs: &Dirs, name: &str) -> Result<Arc<Connection>> {
    let servers = Config::load(dirs)?.mcp;
    let (_, server, reach) = config::checked(&servers)
        .into_iter()
        .find(|(other, ..)| *other == name)
        .ok_or_else(|| anyhow!("No MCP server named \"{name}\""))?;
    let home = std::env::home_dir().unwrap_or_else(|| dirs.data.clone());
    Ok(Connection::new(dirs, name, server.clone(), reach.map_err(|why| anyhow!(why))?, &home, Added::default()))
}

/// Connect to server `name`, say how that went and what tools it offers, and disconnect.
pub async fn check(dirs: &Dirs, name: &str) -> Result<McpServerStatus> {
    let connection = connection(dirs, name)?;
    let connected = tokio::time::timeout(CHECK_LIMIT, connection.client()).await;
    let failed = |error: String| McpServerStatus { state: McpState::Failed, tools: vec![], error: Some(error) };
    let status = match (connected, connection.status()) {
        (Err(_), _) => failed(format!("No answer within {} s", CHECK_LIMIT.as_secs())),
        (Ok(Ok(_)), _) => {
            let first_line = |text: String| text.lines().next().map(|line| line.trim().to_owned()).filter(|line| !line.is_empty());
            let tools =
                connection.tools().into_iter().map(|tool| McpTool { name: tool.name, description: tool.description.and_then(first_line) });
            McpServerStatus { state: McpState::Connected, tools: tools.collect(), error: None }
        }
        (Ok(Err(_)), Status::NeedsAuth) => McpServerStatus { state: McpState::NeedsAuth, tools: vec![], error: None },
        (Ok(Err(_)), Status::Failed(why)) => failed(why),
        (Ok(Err(e)), _) => failed(e.to_string()),
    };
    connection.close().await;
    Ok(status)
}

/// Sign in to server `name`, which the user does in a browser: `ui` shows them where, and
/// `cancel` gives it up. Done when the server takes a connection with the new sign-in.
pub async fn sign_in(dirs: &Dirs, name: &str, ui: &dyn SignIn, cancel: &CancellationToken) -> Result<()> {
    let connection = connection(dirs, name)?;
    let Some(auth) = &connection.auth else {
        bail!("{name} doesn't sign in with OAuth: only an HTTP server without an Authorization header does")
    };
    let signing_in = async {
        if let Err(e) = connection.client().await {
            match connection.status() {
                Status::NeedsAuth => {}
                Status::Failed(why) => bail!(why),
                _ => bail!(e),
            }
            auth.sign_in(ui).await?;
            ui.progress("Connecting…");
            connection.reconnect().await.map_err(|e| anyhow!("Signed in, but {e}"))?;
        }
        Ok(())
    };
    let done = tokio::select! {
        done = signing_in => done,
        () = cancel.cancelled() => Err(anyhow!("Sign-in cancelled")),
    };
    connection.close().await;
    done
}

pub fn sign_out(dirs: &Dirs, name: &str) -> Result<()> {
    match &connection(dirs, name)?.auth {
        Some(auth) => auth.sign_out().map(drop),
        None => bail!("{name} doesn't sign in"),
    }
}
