use super::client::{Error, MAX_MESSAGE_BYTES, Transport};
use super::oauth::Auth;
use async_trait::async_trait;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, WWW_AUTHENTICATE};
use reqwest::{Method, RequestBuilder, Response};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use url::Url;

pub(crate) const ERROR_BODY_BYTES: usize = 500;
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);
const MAX_RECONNECTS: u32 = 5;

pub(crate) fn http() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    let build = || {
        let agent = format!("dirigent/{}", dirigent_protocol::release::VERSION);
        reqwest::Client::builder().user_agent(agent).connect_timeout(Duration::from_secs(30)).build().expect("an HTTP client")
    };
    CLIENT.get_or_init(build).clone()
}

pub(crate) fn network(e: reqwest::Error) -> Error {
    let e = e.without_url();
    let mut reason = e.to_string();
    let mut source = std::error::Error::source(&e);
    while let Some(cause) = source {
        reason = format!("{reason}: {cause}");
        source = cause.source();
    }
    Error::Network(reason)
}

pub(crate) struct Http(Arc<Shared>);

struct Shared {
    url: Url,
    headers: HeaderMap,
    auth: Option<Arc<Auth>>,
    incoming: mpsc::UnboundedSender<Value>,
    session: Mutex<Option<String>>,
    version: Mutex<Option<String>>,
    last_sent_token: Mutex<Option<String>>,
    closed: CancellationToken,
}

#[derive(Default)]
struct Cursor {
    last_event_id: Option<String>,
    retry: Option<Duration>,
    received: bool,
}

impl Http {
    pub(crate) fn new(
        url: Url,
        headers: &BTreeMap<String, String>,
        auth: Option<Arc<Auth>>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), Error> {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            match (HeaderName::try_from(name), HeaderValue::try_from(value)) {
                (Ok(name), Ok(value)) => map.insert(name, value),
                _ => return Err(Error::Other(format!("\"{name}\" can't be sent as a header"))),
            };
        }
        let (messages, incoming) = mpsc::unbounded_channel();
        let (session, version, last_sent_token) = Default::default();
        let shared =
            Shared { url, headers: map, auth, incoming: messages, session, version, last_sent_token, closed: CancellationToken::new() };
        Ok((Self(Arc::new(shared)), incoming))
    }
}

#[async_trait]
impl Transport for Http {
    async fn send(&self, message: &Value) -> Result<(), Error> {
        let this = &self.0;
        let body = serde_json::to_vec(message).expect("a JSON value");
        let response = this.fetch(Method::POST, Some(body), "application/json, text/event-stream", None).await?;
        let response = this.checked(response).await?;
        let (Some(id), Some(method)) = (message.get("id"), message.get("method").and_then(Value::as_str)) else {
            // A notification, or an answer of ours: taken, nothing comes back. The
            // server's own stream may only be opened once the handshake is done.
            if message.get("method").and_then(Value::as_str) == Some("notifications/initialized") {
                tokio::spawn(this.clone().listen());
            }
            return Ok(());
        };
        let status = response.status().as_u16();
        if matches!(status, 202 | 204) {
            return Err(Error::Http { status, message: format!("The server took `{method}` without answering it") });
        }
        match content_type(&response).as_deref() {
            Some("application/json") => {
                let (body, cut) = read(response, MAX_MESSAGE_BYTES).await?;
                let parsed = if cut { None } else { serde_json::from_slice::<Value>(&body).ok() };
                let messages = match parsed.ok_or_else(|| Error::Other("The server's answer isn't JSON".into()))? {
                    Value::Array(messages) => messages,
                    message => vec![message],
                };
                for message in messages {
                    let _ = this.incoming.send(message);
                }
                Ok(())
            }
            Some("text/event-stream") => {
                tokio::spawn(this.clone().answer(response, id.clone()));
                Ok(())
            }
            other => Err(Error::Http {
                status,
                message: format!("The server answered with {}, which MCP doesn't", other.unwrap_or("no content type")),
            }),
        }
    }

    fn protocol_version(&self, version: &str) {
        *self.0.version.lock().unwrap() = Some(version.to_owned());
    }

    async fn close(&self) {
        let this = &self.0;
        if this.closed.is_cancelled() {
            return;
        }
        this.closed.cancel();
        if this.session.lock().unwrap().is_some() {
            let token = this.last_sent_token.lock().unwrap().clone();
            let _ = this.request(Method::DELETE, token.as_deref()).timeout(Duration::from_secs(1)).send().await;
        }
    }
}

impl Drop for Http {
    fn drop(&mut self) {
        self.0.closed.cancel();
    }
}

impl Shared {
    fn request(&self, method: Method, token: Option<&str>) -> RequestBuilder {
        let mut request = http().request(method, self.url.clone()).headers(self.headers.clone());
        if let Some(session) = &*self.session.lock().unwrap() {
            request = request.header("Mcp-Session-Id", session);
        }
        if let Some(version) = &*self.version.lock().unwrap() {
            request = request.header("MCP-Protocol-Version", version);
        }
        match token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    async fn fetch(&self, method: Method, body: Option<Vec<u8>>, accept: &str, last_event_id: Option<&str>) -> Result<Response, Error> {
        let mut again = false;
        loop {
            let token = match &self.auth {
                Some(auth) => auth.token().await,
                None => None,
            };
            self.last_sent_token.lock().unwrap().clone_from(&token);
            let mut request = self.request(method.clone(), token.as_deref()).header(ACCEPT, accept);
            if let Some(body) = &body {
                request = request.header(CONTENT_TYPE, "application/json").body(body.clone());
            }
            if let Some(id) = last_event_id {
                request = request.header("Last-Event-ID", id);
            }
            let response = tokio::select! {
                response = request.send() => response.map_err(network)?,
                () = self.closed.cancelled() => return Err(Error::Closed),
            };
            let challenge = response.headers().get(WWW_AUTHENTICATE).and_then(|v| v.to_str().ok());
            let refused = match response.status().as_u16() {
                401 => true,
                403 => challenge.is_some_and(|c| c.to_ascii_lowercase().replace('"', "").contains("error=insufficient_scope")),
                _ => false,
            };
            let Some(auth) = self.auth.as_ref().filter(|_| refused && !again) else { return Ok(response) };
            auth.unauthorized(challenge, token.as_deref()).await?;
            again = true;
        }
    }

    async fn checked(&self, response: Response) -> Result<Response, Error> {
        let status = response.status();
        if status.is_success() {
            if let Some(session) = response.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
                *self.session.lock().unwrap() = Some(session.to_owned());
            }
            return Ok(response);
        }
        let body =
            read(response, ERROR_BODY_BYTES).await.map(|(body, _)| String::from_utf8_lossy(&body).trim().to_owned()).unwrap_or_default();
        let said = if body.is_empty() { String::new() } else { format!(": {body}") };
        Err(match status.as_u16() {
            401 if self.auth.is_some() => Error::AuthRequired,
            401 => Error::Http { status: 401, message: format!("The server turned the request's credentials down (HTTP 401){said}") },
            404 if self.session.lock().unwrap().is_some() => Error::SessionExpired,
            status => Error::Http { status, message: format!("The server answered HTTP {status}{said}") },
        })
    }

    async fn open_stream(&self, last_event_id: Option<&str>) -> Result<Option<Response>, Error> {
        let response = self.fetch(Method::GET, None, "text/event-stream", last_event_id).await?;
        if response.status().as_u16() == 405 {
            return Ok(None);
        }
        let response = self.checked(response).await?;
        match content_type(&response).as_deref() {
            Some("text/event-stream") => Ok(Some(response)),
            _ => Err(Error::Http { status: response.status().as_u16(), message: "The server's stream isn't one of events".into() }),
        }
    }

    async fn consume(&self, mut response: Response, cursor: &mut Cursor, answers: Option<&Value>) -> Result<bool, Error> {
        let mut events = Events::default();
        let mut answered = false;
        loop {
            let chunk = tokio::select! {
                chunk = response.chunk() => chunk.map_err(network)?,
                () = self.closed.cancelled() => return Err(Error::Closed),
            };
            let messages = match &chunk {
                Some(bytes) => events.feed(bytes, cursor)?,
                None => events.end(cursor),
            };
            for message in messages.iter().filter_map(|data| serde_json::from_str::<Value>(data).ok()) {
                cursor.received = true;
                answered |= answers.is_some_and(|id| message.get("id") == Some(id) && message.get("method").is_none());
                let _ = self.incoming.send(message);
            }
            if chunk.is_none() {
                return Ok(answered);
            }
        }
    }

    /// A server may end it before the answer: if its events had ids, the rest is asked for again.
    async fn answer(self: Arc<Self>, first: Response, id: Value) {
        let mut cursor = Cursor::default();
        let mut stream = Ok(Some(first));
        let mut attempt = 0;
        let failure = loop {
            let ended = match stream {
                Ok(Some(response)) => self.consume(response, &mut cursor, Some(&id)).await,
                Ok(None) => Ok(false),
                Err(e) => Err(e),
            };
            if matches!(ended, Ok(true) | Err(Error::Closed)) || self.closed.is_cancelled() {
                return;
            }
            let resumes =
                cursor.last_event_id.is_some() && attempt < MAX_RECONNECTS && ended.as_ref().err().is_none_or(Error::is_transient);
            if !resumes {
                break ended.err();
            }
            if std::mem::take(&mut cursor.received) {
                attempt = 0;
            }
            if !self.pause(attempt, cursor.retry).await {
                return;
            }
            attempt += 1;
            stream = self.open_stream(cursor.last_event_id.as_deref()).await;
        };
        let reason = failure.map_or("it ended without one".to_owned(), |e| e.to_string());
        let message = format!("The server's answer didn't arrive: {reason}");
        let _ = self.incoming.send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32603, "message": message}}));
    }

    async fn listen(self: Arc<Self>) {
        let mut cursor = Cursor::default();
        let mut attempt = 0;
        loop {
            let opened = Instant::now();
            let ended = match self.open_stream(cursor.last_event_id.as_deref()).await {
                Ok(None) => return,
                Ok(Some(response)) => self.consume(response, &mut cursor, None).await.map(drop),
                Err(e) => Err(e),
            };
            match ended {
                Ok(()) if cursor.received || opened.elapsed() > MAX_RECONNECT_DELAY => attempt = 0,
                Err(e) if !e.is_transient() => {
                    return tracing::debug!("mcp: {}: no stream of the server's own: {e}", self.url.host_str().unwrap_or(""));
                }
                _ => {}
            }
            cursor.received = false;
            if attempt >= MAX_RECONNECTS || !self.pause(attempt, cursor.retry).await {
                return;
            }
            attempt += 1;
        }
    }

    async fn pause(&self, attempt: u32, asked: Option<Duration>) -> bool {
        let delay = asked.unwrap_or_else(|| (RECONNECT_DELAY * 2u32.pow(attempt)).min(MAX_RECONNECT_DELAY));
        tokio::select! {
            () = tokio::time::sleep(delay) => true,
            () = self.closed.cancelled() => false,
        }
    }
}

fn content_type(response: &Response) -> Option<String> {
    let value = response.headers().get(CONTENT_TYPE)?.to_str().ok()?;
    Some(value.split(';').next()?.trim().to_ascii_lowercase())
}

async fn read(mut response: Response, limit: usize) -> Result<(Vec<u8>, bool), Error> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(network)? {
        let room = limit - body.len();
        if chunk.len() > room {
            body.extend_from_slice(&chunk[..room]);
            return Ok((body, true));
        }
        body.extend_from_slice(&chunk);
    }
    Ok((body, false))
}

#[derive(Default)]
struct Events {
    line: Vec<u8>,
    /// The last byte ended a line with a CR: an LF next belongs to it.
    after_cr: bool,
    data: Option<String>,
    kind: Option<String>,
}

impl Events {
    fn feed(&mut self, bytes: &[u8], cursor: &mut Cursor) -> Result<Vec<String>, Error> {
        let mut messages = vec![];
        for &byte in bytes {
            if std::mem::take(&mut self.after_cr) && byte == b'\n' {
                continue;
            }
            if byte != b'\n' && byte != b'\r' {
                self.line.push(byte);
                continue;
            }
            self.after_cr = byte == b'\r';
            let line = std::mem::take(&mut self.line);
            self.line(&String::from_utf8_lossy(&line), cursor, &mut messages);
            if self.data.as_ref().is_some_and(|data| data.len() > MAX_MESSAGE_BYTES) {
                return Err(Error::Other(format!("The server sent an event of over {MAX_MESSAGE_BYTES} bytes")));
            }
        }
        if self.line.len() > MAX_MESSAGE_BYTES {
            return Err(Error::Other(format!("The server sent an event of over {MAX_MESSAGE_BYTES} bytes")));
        }
        Ok(messages)
    }

    fn end(&mut self, cursor: &mut Cursor) -> Vec<String> {
        let mut messages = vec![];
        let line = std::mem::take(&mut self.line);
        if !line.is_empty() {
            self.line(&String::from_utf8_lossy(&line), cursor, &mut messages);
        }
        self.line("", cursor, &mut messages);
        messages
    }

    fn line(&mut self, line: &str, cursor: &mut Cursor, messages: &mut Vec<String>) {
        if line.is_empty() {
            // The event is complete. Only a `message` (the default) is JSON-RPC.
            let (data, kind) = (self.data.take(), self.kind.take());
            messages.extend(data.filter(|_| kind.is_none_or(|kind| kind == "message")));
            return;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "data" => match &mut self.data {
                Some(data) => *data = format!("{data}\n{value}"),
                None => self.data = Some(value.to_owned()),
            },
            "event" => self.kind = Some(value.to_owned()),
            "id" if !value.contains('\0') => cursor.last_event_id = Some(value.to_owned()),
            "retry" => cursor.retry = value.parse().ok().map(Duration::from_millis).or(cursor.retry),
            // A comment (the field is empty), or a field of a newer spec.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_come_apart_however_the_bytes_are_cut() {
        let stream =
            ": hello\r\nid: 7\r\nretry: 250\r\n\r\nevent: message\ndata: {\"a\":\ndata: 1}\n\nevent: other\ndata: no\n\rdata: last";
        for cut in 0..stream.len() {
            let (mut events, mut cursor) = (Events::default(), Cursor::default());
            let mut messages = events.feed(&stream.as_bytes()[..cut], &mut cursor).unwrap();
            messages.extend(events.feed(&stream.as_bytes()[cut..], &mut cursor).unwrap());
            messages.extend(events.end(&mut cursor));
            assert_eq!(messages, ["{\"a\":\n1}", "last"], "cut at {cut}");
            assert_eq!((cursor.last_event_id.as_deref(), cursor.retry), (Some("7"), Some(Duration::from_millis(250))));
        }
    }
}
