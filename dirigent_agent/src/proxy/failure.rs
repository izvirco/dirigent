use super::Failure;
use serde_json::Value;
use std::time::Duration;

/// Why a request failed. `Proxy::stream` puts it into the `Reply`; the methods that return
/// an `anyhow::Error` have it as that error's cause (`downcast_ref`).
#[derive(Debug, Clone, PartialEq)]
pub struct Error {
    pub failure: Failure,
    /// For the user to read.
    pub message: String,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

const SETTINGS: &str = "under Settings › Proxy";

impl Error {
    pub(super) fn transient(message: impl Into<String>) -> Self {
        Self { failure: Failure::Transient { retry_after: None }, message: message.into() }
    }

    pub(super) fn other(message: impl Into<String>) -> Self {
        Self { failure: Failure::Other, message: message.into() }
    }

    pub(super) fn not_set_up() -> Self {
        let message = format!("No model proxy is set up. Enter your CLIProxyAPI's address and key {SETTINGS}.");
        Self { failure: Failure::Unauthorized, message }
    }

    pub(super) fn not_a_proxy() -> Self {
        Self::other(format!("The proxy's address answers, but not as a CLIProxyAPI does. Check it {SETTINGS}."))
    }

    pub(super) fn unreachable(url: &str, error: &reqwest::Error) -> Self {
        Self::transient(format!("Could not reach the proxy at {url}: {}", cause(error)))
    }

    pub(super) fn http(status: u16, retry_after: Option<Duration>, body: &str) -> Self {
        let words = serde_json::from_str::<Value>(body).ok().and_then(|json| words(&json)).unwrap_or_else(|| short(body));
        let words = if words.is_empty() { "no details".into() } else { words };
        match classify(Some(status), body) {
            Failure::Unauthorized => {
                let message = format!("The proxy turned the request down: {words} (HTTP {status}). Check its address and key {SETTINGS}.");
                Self { failure: Failure::Unauthorized, message }
            }
            Failure::Transient { .. } => Self { failure: Failure::Transient { retry_after }, message: format!("{words} (HTTP {status})") },
            failure => Self { failure, message: format!("{words} (HTTP {status})") },
        }
    }

    pub(super) fn in_stream(event: &Value) -> Self {
        let message = words(event).unwrap_or_else(|| short(&event.to_string()));
        // What the error says of itself decides, not the event around it: a failed response
        // comes with the request's `instructions` and `tools`, which have words of their own
        // (a tool with a `timeout`, a prompt about a "context window exceeded").
        let said = match [&event["error"], &event["response"]["error"]].into_iter().find(|error| !error.is_null()) {
            Some(Value::String(message)) => message.clone(),
            Some(error) => ["message", "detail", "code", "type"].iter().filter_map(|key| error[key].as_str()).collect::<Vec<_>>().join(" "),
            None => String::new(),
        };
        let said = if said.is_empty() { event.to_string() } else { said };
        Self { failure: classify(None, &said), message }
    }
}

/// The innermost reason of `error`: reqwest's own words only say that a request failed.
pub(super) fn cause(error: &dyn std::error::Error) -> String {
    let mut innermost = error;
    while let Some(source) = innermost.source() {
        innermost = source;
    }
    innermost.to_string()
}

fn words(json: &Value) -> Option<String> {
    let error = [&json["error"], &json["response"]["error"], json].into_iter().find(|v| !v.is_null())?;
    let message = match error {
        Value::String(message) => message.as_str(),
        _ => error["message"].as_str().or(error["detail"].as_str())?,
    };
    let kind = error["code"].as_str().or(error["type"].as_str()).filter(|kind| !matches!(*kind, "error" | "invalid_request_error"));
    Some(match kind {
        Some(kind) if !message.contains(kind) => format!("{message} ({kind})"),
        _ => message.to_owned(),
    })
}

fn short(text: &str) -> String {
    let text = text.trim();
    match text.char_indices().nth(300) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

const OVERFLOW: &[&str] = &[
    "prompt is too long", // Anthropic: "prompt is too long: 213462 tokens > 200000 maximum"
    "prompt too long",
    "request_too_large",          // Anthropic, by bytes
    "exceed context limit",       // Anthropic: "input length and `max_tokens` exceed context limit"
    "context_window_exceeded",    // Anthropic's stop reason; "model_context_window_exceeded"
    "exceeds the context window", // OpenAI: "Your input exceeds the context window of this model"
    "context_length_exceeded",    // OpenAI's code
    "context length exceeded",
    "maximum context length", // OpenAI-compatible: "exceeds the model's maximum context length"
    "context window exceeded",
    "exceeds the maximum number of tokens", // Google
    "maximum prompt length",                // xAI
    "too many tokens",
    "token limit exceeded",
];

// MUST KILL: rename: SPENT -> QUOTA_EXHAUSTED, an account that fails again whatever the status
const SPENT: &[&str] = &[
    "insufficient_quota",
    "quota exceeded",
    "usage limit",
    "usage_limit",
    "usage_not_included",
    "out of budget",
    "billing",
    "credit balance",
];

const TRANSIENT: &[&str] = &[
    "overloaded",
    "rate limit",
    "rate_limit",
    "too many requests",
    "server_error",
    "server error",
    "api_error",
    "internal error",
    "internal_error",
    "service unavailable",
    "service_unavailable",
    "temporarily unavailable",
    "at capacity",
    "high demand",
    "server_busy",
    "timeout",
    "timed out",
    "connection", // the proxy's "closed network connection"
    "network",
    "disconnected", // "stream disconnected before completion"
    "upstream",
    "try again",
    "retry your request",
];

pub(super) fn classify(status: Option<u16>, text: &str) -> Failure {
    let text = text.to_lowercase();
    let says = |words: &[&str]| words.iter().any(|word| text.contains(word));
    // A rate limit, by its status or its words, is not a context that is too long, though it
    // may read like one: "Too many tokens, please wait".
    let rate_limited = status == Some(429) || says(&["rate limit", "rate_limit", "too many requests"]);
    if status == Some(413) || (!rate_limited && says(OVERFLOW)) {
        return Failure::Overflow;
    }
    if matches!(status, Some(401 | 403)) {
        return Failure::Unauthorized;
    }
    if says(SPENT) {
        return Failure::Other;
    }
    let transient = match status {
        Some(status) => matches!(status, 408 | 429 | 500..=599),
        None => says(TRANSIENT),
    };
    if transient { Failure::Transient { retry_after: None } } else { Failure::Other }
}

pub(super) fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let wait = |name: &str, per_second: f64| {
        let number = headers.get(name)?.to_str().ok()?.trim().parse::<f64>().ok()?;
        Duration::try_from_secs_f64(number / per_second).ok()
    };
    wait("retry-after-ms", 1000.0).or_else(|| wait("retry-after", 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TRANSIENT: Failure = Failure::Transient { retry_after: None };

    #[test]
    fn failures_are_told_apart() {
        let key = Error::http(401, None, r#"{"error":"Invalid API key"}"#);
        assert_eq!(key.failure, Failure::Unauthorized);
        assert_eq!(
            key.message,
            "The proxy turned the request down: Invalid API key (HTTP 401). Check its address and key under Settings › Proxy."
        );
        let long =
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 750116 tokens > 200000 maximum"}}"#;
        let long = Error::http(400, None, long);
        assert_eq!(
            (long.failure, long.message.as_str()),
            (Failure::Overflow, "prompt is too long: 750116 tokens > 200000 maximum (HTTP 400)")
        );
        let unknown = r#"{"error":{"code":"model_not_found","message":"unknown provider for model x","param":"model","type":"invalid_request_error"}}"#;
        let unknown = Error::http(400, None, unknown);
        assert_eq!(
            (unknown.failure, unknown.message.as_str()),
            (Failure::Other, "unknown provider for model x (model_not_found) (HTTP 400)")
        );

        let wait = Some(Duration::from_secs(7));
        for status in [408, 429, 500, 502, 503, 504, 529] {
            assert_eq!(Error::http(status, wait, "").failure, Failure::Transient { retry_after: wait }, "{status}");
        }
        assert_eq!(
            Error::http(529, None, r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#).message,
            "Overloaded (overloaded_error) (HTTP 529)"
        );
        assert_eq!(Error::http(403, None, "").failure, Failure::Unauthorized);
        assert_eq!(Error::http(413, None, "").failure, Failure::Overflow);
        assert_eq!(Error::http(400, None, "<html>bad gateway timeout</html>").failure, Failure::Other);
        assert_eq!(
            classify(Some(429), r#"{"error":{"code":"insufficient_quota","message":"You exceeded your current quota"}}"#),
            Failure::Other
        );
        assert_eq!(classify(Some(429), "ThrottlingException: Too many tokens, rate limit reached"), TRANSIENT);
        assert_eq!(classify(Some(429), "Too many tokens, please wait before trying again."), TRANSIENT);
        assert_eq!(classify(Some(400), "Too many tokens, please wait before trying again."), Failure::Overflow);

        for text in [
            "input length and `max_tokens` exceed context limit: 197202 + 21333 > 200000",
            "Your input exceeds the context window of this model. Please adjust your input and try again.",
            r#"{"error":{"code":"context_length_exceeded","message":"..."}}"#,
            "This model's maximum context length is 131072 tokens",
        ] {
            assert_eq!(classify(Some(400), text), Failure::Overflow, "{text}");
            assert_eq!(classify(None, text), Failure::Overflow, "{text}");
        }
    }

    #[test]
    fn errors_in_a_stream_are_told_by_their_words() {
        let overloaded = Error::in_stream(&json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}));
        assert_eq!((overloaded.failure, overloaded.message.as_str()), (TRANSIENT, "Overloaded (overloaded_error)"));
        let failed = json!({"type":"response.failed","response":{"status":"failed","error":{"code":"server_error","message":"The server had an error"}}});
        assert_eq!(Error::in_stream(&failed), Error { failure: TRANSIENT, message: "The server had an error (server_error)".into() });
        let closed = json!({"type":"error","message":"read tcp 10.0.0.1:443: use of closed network connection"});
        assert_eq!(Error::in_stream(&closed).failure, TRANSIENT);
        let refused =
            json!({"type":"error","error":{"type":"invalid_request_error","message":"tools.0.name: String should match pattern"}});
        assert_eq!(
            Error::in_stream(&refused),
            Error { failure: Failure::Other, message: "tools.0.name: String should match pattern".into() }
        );

        let echo = |error: Value| {
            json!({"type": "response.failed", "response": {
                "status": "failed",
                "error": error,
                "instructions": "When you see 'context window exceeded', compact. The server is overloaded with rules.",
                "tools": [{"type": "function", "name": "bash", "parameters": {"properties": {"timeout": {"type": "number"}}}}],
            }})
        };
        let invalid = Error::in_stream(&echo(json!({"code": "invalid_prompt", "message": "The prompt was flagged"})));
        assert_eq!(invalid, Error { failure: Failure::Other, message: "The prompt was flagged (invalid_prompt)".into() });
        assert_eq!(Error::in_stream(&echo(json!({"code": "server_error", "message": "Oops"}))).failure, TRANSIENT);
        assert_eq!(
            Error::in_stream(&echo(json!({"type": "invalid_request_error", "code": "context_length_exceeded"}))).failure,
            Failure::Overflow
        );
    }

    #[test]
    fn the_wait_is_read_from_either_header() {
        let headers = |pairs: &[(&'static str, &'static str)]| {
            pairs.iter().map(|(name, value)| (reqwest::header::HeaderName::from_static(name), value.parse().unwrap())).collect()
        };
        assert_eq!(retry_after(&headers(&[("retry-after", "12")])), Some(Duration::from_secs(12)));
        assert_eq!(retry_after(&headers(&[("retry-after", "30"), ("retry-after-ms", "1500")])), Some(Duration::from_millis(1500)));
        assert_eq!(retry_after(&headers(&[("retry-after", "Wed, 21 Oct 2026 07:28:00 GMT")])), None);
        assert_eq!(retry_after(&headers(&[("retry-after", "1e30")])), None);
        assert_eq!(retry_after(&headers(&[("retry-after", "3"), ("retry-after-ms", "-1")])), Some(Duration::from_secs(3)));
        assert_eq!(retry_after(&headers(&[])), None);
    }
}
