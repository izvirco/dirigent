//! Signing in to an MCP server (`ClientMessage::McpLogin`): the server's OAuth
//! (`dirigent_agent::mcp::sign_in`) as a login's steps to the connection that asked, with
//! its answer (`LoginAnswer`) and its cancel (`LoginCancel`, or the connection going).
//!
//! The rest of Settings › MCP servers is `dirigent_agent::mcp`'s own: the service only
//! hands the requests over.

use crate::Service;
use crate::server::Conn;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use dirigent_agent::mcp::SignIn;
use dirigent_protocol::{LoginStep, ServerMessage};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::info;

/// The one question a sign-in asks (`SignIn::redirect_url`).
const PASTE: u32 = 1;

/// The sign-ins in progress, by the connection and the request that started them.
#[derive(Default)]
pub struct Logins {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    running: HashMap<(u64, u64), Running>,
    /// Cancelled before they started: requests are handled side by side, so a `LoginCancel`
    /// sent right after its `McpLogin` may get here first.
    cancelled: HashSet<(u64, u64)>,
}

struct Running {
    cancel: CancellationToken,
    /// Where the pasted address goes, while the sign-in asks for one.
    paste: Option<oneshot::Sender<String>>,
}

impl Logins {
    /// The user's answer to a question of login `login` on connection `conn`.
    pub fn answer(&self, conn: u64, login: u64, prompt: u32, value: String) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let running = inner.running.get_mut(&(conn, login)).ok_or_else(|| anyhow!("no such login"))?;
        if let Some(paste) = running.paste.take_if(|_| prompt == PASTE) {
            let _ = paste.send(value);
        }
        Ok(())
    }

    pub fn cancel(&self, conn: u64, login: u64) {
        let mut inner = self.inner.lock().unwrap();
        match inner.running.get(&(conn, login)) {
            Some(running) => running.cancel.cancel(),
            None => drop(inner.cancelled.insert((conn, login))),
        }
    }

    /// A connection is gone: nobody is left to finish the sign-ins it started (and a
    /// browser sign-in's callback listener would hold its port).
    pub fn disconnected(&self, conn: u64) {
        let mut inner = self.inner.lock().unwrap();
        inner.cancelled.retain(|(c, _)| *c != conn);
        for running in inner.running.iter().filter(|((c, _), _)| *c == conn).map(|(_, running)| running) {
            info!("login cancelled: its client left");
            running.cancel.cancel();
        }
    }
}

/// A sign-in as its connection sees it: the login's steps.
struct Steps<'a> {
    logins: &'a Logins,
    key: (u64, u64),
    out: mpsc::UnboundedSender<ServerMessage>,
}

impl Steps<'_> {
    fn step(&self, step: LoginStep) {
        let _ = self.out.send(ServerMessage::LoginStep { login: self.key.1, step });
    }
}

#[async_trait]
impl SignIn for Steps<'_> {
    fn open(&self, url: &str) {
        self.step(LoginStep::AuthUrl { url: url.to_owned() });
    }

    fn progress(&self, message: &str) {
        self.step(LoginStep::Progress { message: message.to_owned() });
    }

    async fn redirect_url(&self) -> Option<String> {
        let (paste, pasted) = oneshot::channel();
        self.logins.inner.lock().unwrap().running.get_mut(&self.key)?.paste = Some(paste);
        self.step(LoginStep::Prompt {
            n: PASTE,
            message: "If the browser can't reach this host, paste the address it ended up on:".into(),
            placeholder: Some("http://127.0.0.1:…/callback?code=…".into()),
        });
        // Dropped once the browser got here: the question needs no answer any more.
        struct Closed<'a>(&'a Steps<'a>);
        impl Drop for Closed<'_> {
            fn drop(&mut self) {
                self.0.step(LoginStep::PromptClosed { n: PASTE });
            }
        }
        let _closed = Closed(self);
        pasted.await.ok()
    }
}

impl Service {
    /// Run login `id` of connection `conn` to its end, its steps going to that connection;
    /// the reply ends it. Logged here, not as a failed request: a cancel is no failure of
    /// the service.
    pub(crate) async fn mcp_login(&self, conn: &Conn, id: u64, name: &str) -> ServerMessage {
        let (key, cancel) = ((conn.id, id), CancellationToken::new());
        {
            let mut inner = self.logins.inner.lock().unwrap();
            if inner.cancelled.remove(&key) {
                return ServerMessage::Error { reply_to: Some(id), message: "Sign-in cancelled".into() };
            }
            inner.running.insert(key, Running { cancel: cancel.clone(), paste: None });
        }
        info!(server = %name, "MCP sign-in started");
        // Straight into the connection, in order: the steps all come before the reply.
        let steps = Steps { logins: &self.logins, key, out: conn.out.clone() };
        let result = dirigent_agent::mcp::sign_in(&self.agent.dirs, name, &steps, &cancel).await;
        self.logins.inner.lock().unwrap().running.remove(&key);
        match result {
            Ok(()) => {
                info!(server = %name, "signed in to MCP server");
                ServerMessage::Ok { reply_to: id }
            }
            Err(e) => {
                info!(server = %name, "MCP sign-in ended: {e:#}");
                ServerMessage::Error { reply_to: Some(id), message: format!("{e:#}") }
            }
        }
    }
}
