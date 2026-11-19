//! The prompt cache, which makes a long conversation affordable, keeps a request's start up
//! to the breakpoints the request sets (`cache_control`), four at most. CLIProxyAPI (8.0.63,
//! tried from outside) sets its own where a request has none, keeps every one for an hour
//! whatever the request asks, and drops what is over four. The ones set here are thus what a
//! proxy that does none of this needs, and do no harm with one that does.

use super::{Chunk, Error, Message, Part, Partial, Progress, Request, STILL_STREAMING, Wire, sse};
use dirigent_protocol::transcript::{Api, Block, Stop};
use serde_json::{Value, json};
use std::collections::HashMap;

pub(super) const PATH: &str = "/v1/messages";

pub(super) fn headers(request: &Request<'_>) -> Vec<(&'static str, String)> {
    let mut headers = vec![("anthropic-version", "2023-06-01".to_owned())];
    headers.extend(super::session(request).map(|id| ("x-session-id", id.to_owned())));
    headers
}

pub(super) fn body(request: &Request<'_>) -> Value {
    let model = request.model;
    let mut body = json!({
        "model": model.id,
        "max_tokens": request.max_output.unwrap_or(model.max_output),
        "stream": true,
    });
    // The cache keeps a request's start up to a breakpoint, in the order tools, system,
    // messages: one after each part that stays the same from request to request.
    if !request.system.trim().is_empty() {
        let mut system = json!({"type": "text", "text": request.system});
        breakpoint(&mut system);
        body["system"] = json!([system]);
    }
    let mut tools: Vec<Value> = request.tools.iter().map(tool).collect();
    if let Some(last) = tools.last_mut() {
        breakpoint(last);
        body["tools"] = tools.into();
    }
    body["messages"] = messages(request);
    if !model.efforts.is_empty() {
        // The model decides how much to think, within the effort asked for. `summarized`:
        // without it the newer models send their thinking blocks empty.
        body["thinking"] = json!({"type": "adaptive", "display": "summarized"});
        if let Some(effort) = request.effort {
            body["output_config"] = json!({"effort": effort});
        }
    }
    body
}

fn breakpoint(block: &mut Value) {
    block["cache_control"] = json!({"type": "ephemeral"});
}

fn tool(spec: &crate::tool::ToolSpec) -> Value {
    // Only these of a schema's top level: the API turns down a request whose tool has
    // `anyOf` and its like there, which MCP servers' tools do have.
    let mut schema = json!({
        "type": "object",
        "properties": spec.input.get("properties").cloned().unwrap_or_else(|| json!({})),
        "required": spec.input.get("required").cloned().unwrap_or_else(|| json!([])),
    });
    for definitions in ["$defs", "definitions"] {
        if let Some(value) = spec.input.get(definitions) {
            schema[definitions] = value.clone();
        }
    }
    json!({"name": spec.name, "description": spec.description, "input_schema": schema})
}

fn messages(request: &Request<'_>) -> Value {
    let images = request.model.images;
    let mut turns: Vec<(&str, Vec<Value>)> = vec![];
    // What follows a user's turn joins it: tool results and what the user said meanwhile are
    // one turn to the API.
    fn user<'a>(turns: &'a mut Vec<(&'static str, Vec<Value>)>) -> &'a mut Vec<Value> {
        if turns.last().is_none_or(|(role, _)| *role != "user") {
            turns.push(("user", vec![]));
        }
        &mut turns.last_mut().expect("just pushed").1
    }
    for message in request.messages {
        match message {
            Message::User(parts) => {
                let blocks = blocks(parts, images);
                if !blocks.is_empty() {
                    user(&mut turns).extend(blocks);
                }
            }
            Message::ToolResult { call_id, content, is_error } => {
                let mut blocks = blocks(content, images);
                // The API takes only text in a failed call's result: its images go after the results.
                let mut after = vec![];
                if *is_error && blocks.iter().any(|block| block["type"] == "image") {
                    after.push(json!({"type": "text", "text": format!("Images of the failed tool call {call_id}:")}));
                    after.extend(blocks.extract_if(.., |block| block["type"] == "image"));
                    blocks.push(json!({"type": "text", "text": "(images follow the tool results)"}));
                }
                let content = match blocks.as_slice() {
                    [] => json!("(no output)"),
                    blocks if blocks.iter().all(|block| block["type"] == "text") => {
                        blocks.iter().filter_map(|block| block["text"].as_str()).collect::<Vec<_>>().join("\n").into()
                    }
                    _ => blocks.into(),
                };
                let mut result = json!({"type": "tool_result", "tool_use_id": super::call_id(call_id), "content": content});
                if *is_error {
                    result["is_error"] = true.into();
                }
                // Results open their turn: the API wants them before anything else in it.
                let turn = user(&mut turns);
                let results = turn.iter().take_while(|block| block["type"] == "tool_result").count();
                turn.insert(results, result);
                turn.extend(after);
            }
            Message::Assistant { content, api } => {
                let blocks: Vec<Value> = content.iter().filter_map(|block| assistant(block, *api)).collect();
                if !blocks.is_empty() {
                    turns.push(("assistant", blocks));
                }
            }
        }
    }
    for (_, blocks) in turns.iter_mut().rev().filter(|(role, _)| *role == "user").take(2) {
        if let Some(last) = blocks.last_mut() {
            breakpoint(last);
        }
    }
    turns.into_iter().map(|(role, blocks)| json!({"role": role, "content": blocks})).collect()
}

/// A user's or a tool result's parts as blocks. Text that is empty is left out: the API
/// takes no empty block.
fn blocks(parts: &[Part], images: bool) -> Vec<Value> {
    let block = |part: &Part| match part {
        Part::Text(text) if text.trim().is_empty() => None,
        Part::Text(text) => Some(json!({"type": "text", "text": text})),
        Part::Image { mime, data } if images => {
            Some(json!({"type": "image", "source": {"type": "base64", "media_type": mime, "data": data}}))
        }
        Part::Image { .. } => Some(json!({"type": "text", "text": super::NO_IMAGES})),
    };
    parts.iter().filter_map(block).collect()
}

fn assistant(block: &Block, api: Api) -> Option<Value> {
    match block {
        Block::Text { text, .. } if !text.trim().is_empty() => Some(json!({"type": "text", "text": text})),
        // Thinking goes back as it came, signed, or not at all: the API checks the signature.
        Block::Thinking { text, signature: Some(signature), redacted } if api == Api::Anthropic && !signature.is_empty() => {
            Some(match redacted {
                true => json!({"type": "redacted_thinking", "data": signature}),
                false => json!({"type": "thinking", "thinking": text, "signature": signature}),
            })
        }
        Block::ToolCall { id, name, input } => {
            // The API takes an object only: a call that had none (`Reply`) goes back without input.
            let input = if input.is_object() { input.clone() } else { json!({}) };
            Some(json!({"type": "tool_use", "id": super::call_id(id), "name": name, "input": input}))
        }
        _ => None,
    }
}

#[derive(Default)]
pub(super) struct Stream {
    partial: Partial,
    open: HashMap<u64, Open>,
}

struct Open {
    at: usize,
    input: String,
}

impl Wire for Stream {
    fn event(&mut self, event: sse::Event, on: &mut (dyn FnMut(Chunk) + Send)) -> Result<Progress, Error> {
        if event.data.is_empty() {
            return Ok(Progress::Continue);
        }
        let data: Value =
            serde_json::from_str(&event.data).map_err(|e| Error::transient(format!("The proxy sent an event that is not JSON ({e})")))?;
        let index = data["index"].as_u64().unwrap_or(0);
        match data["type"].as_str().or(event.name.as_deref()).unwrap_or_default() {
            "message_start" => self.usage(&data["message"]["usage"]),
            "content_block_start" => {
                let block = &data["content_block"];
                let text = |key: &str| block[key].as_str().unwrap_or_default().to_owned();
                let at = self.partial.content.len();
                let block = match block["type"].as_str().unwrap_or_default() {
                    "text" => Block::text(text("text")),
                    "thinking" => Block::Thinking {
                        text: text("thinking"),
                        signature: Some(text("signature")).filter(|s| !s.is_empty()),
                        redacted: false,
                    },
                    // Reasoning the provider keeps to itself: all of it is the data to send back.
                    "redacted_thinking" => Block::Thinking { text: String::new(), signature: Some(text("data")), redacted: true },
                    "tool_use" => Block::ToolCall { id: text("id"), name: text("name"), input: STILL_STREAMING },
                    _ => return Ok(Progress::Continue),
                };
                match &block {
                    Block::Text { text, .. } if !text.is_empty() => on(Chunk::Text { index: at, delta: text.clone() }),
                    Block::Thinking { text, .. } if !text.is_empty() => on(Chunk::Thinking { index: at, delta: text.clone() }),
                    _ => {}
                }
                self.partial.content.push(block);
                self.open.insert(index, Open { at, input: String::new() });
            }
            "content_block_delta" => {
                let Some(open) = self.open.get_mut(&index) else { return Ok(Progress::Continue) };
                let delta = &data["delta"];
                let piece = |key: &str| delta[key].as_str().unwrap_or_default();
                match (delta["type"].as_str().unwrap_or_default(), &mut self.partial.content[open.at]) {
                    ("text_delta", Block::Text { text, .. }) => {
                        text.push_str(piece("text"));
                        on(Chunk::Text { index: open.at, delta: piece("text").to_owned() });
                    }
                    ("thinking_delta", Block::Thinking { text, .. }) => {
                        text.push_str(piece("thinking"));
                        on(Chunk::Thinking { index: open.at, delta: piece("thinking").to_owned() });
                    }
                    ("signature_delta", Block::Thinking { signature, .. }) => {
                        signature.get_or_insert_default().push_str(piece("signature"))
                    }
                    ("input_json_delta", Block::ToolCall { .. }) => open.input.push_str(piece("partial_json")),
                    _ => {}
                }
            }
            "content_block_stop" => {
                let Some(open) = self.open.remove(&index) else { return Ok(Progress::Continue) };
                if let Block::ToolCall { id, name, input } = &mut self.partial.content[open.at] {
                    *input = super::call_input(&open.input);
                    on(Chunk::ToolCall { index: open.at, id: id.clone(), name: name.clone(), input: input.clone() });
                }
            }
            "message_delta" => {
                self.usage(&data["usage"]);
                let delta = &data["delta"];
                self.partial.stop = match delta["stop_reason"].as_str() {
                    None => self.partial.stop,
                    Some("max_tokens") => Some(Stop::MaxTokens),
                    Some("tool_use") => Some(Stop::ToolUse),
                    Some("end_turn" | "stop_sequence" | "pause_turn") => Some(Stop::End),
                    Some("refusal") => {
                        let why = delta["stop_details"]["explanation"].as_str().unwrap_or("The model refused to answer.");
                        return Err(Error::other(why));
                    }
                    Some("model_context_window_exceeded") => {
                        return Err(Error {
                            failure: super::Failure::Overflow,
                            message: "The reply ran into the end of the model's context window.".into(),
                        });
                    }
                    Some(other) => {
                        tracing::warn!("anthropic: unknown stop reason {other}");
                        Some(Stop::End)
                    }
                };
            }
            "message_stop" => return Ok(Progress::Complete),
            "error" => return Err(Error::in_stream(&data)),
            _ if !data["error"].is_null() => return Err(Error::in_stream(&data)),
            _ => {}
        }
        Ok(Progress::Continue)
    }

    fn take(&mut self) -> Partial {
        std::mem::take(&mut self.partial)
    }
}

impl Stream {
    /// Take the counts `usage` has: the stream's first event has the prompt's, a later one
    /// the reply's.
    fn usage(&mut self, usage: &Value) {
        let take = |count: &mut u64, value: &Value| {
            if let Some(tokens) = value.as_u64() {
                *count = tokens;
            }
        };
        let Partial { usage: counts, cache_write_1h, .. } = &mut self.partial;
        take(&mut counts.input, &usage["input_tokens"]);
        take(&mut counts.output, &usage["output_tokens"]);
        take(&mut counts.cache_read, &usage["cache_read_input_tokens"]);
        take(&mut counts.cache_write, &usage["cache_creation_input_tokens"]);
        take(cache_write_1h, &usage["cache_creation"]["ephemeral_1h_input_tokens"]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::tests::{model, replay, request_parts};
    use crate::proxy::{Failure, Model};
    use dirigent_protocol::transcript::Usage;

    fn claude() -> Model {
        model("claude-haiku-5-5", Api::Anthropic)
    }

    #[test]
    fn a_request_is_what_the_api_takes() {
        let (system, messages, tools) = request_parts();
        let model = claude();
        let request = Request {
            model: &model,
            system,
            messages: &messages,
            tools: &tools,
            effort: Some("high"),
            max_output: None,
            cache_key: "thread-1",
        };
        let cached = json!({"type": "ephemeral"});
        assert_eq!(
            body(&request),
            json!({
                "model": "claude-haiku-5-5",
                "max_tokens": 128000,
                "stream": true,
                "system": [{"type": "text", "text": "Be terse.", "cache_control": cached}],
                "tools": [
                    {"name": "bash", "description": "Run a command.",
                     "input_schema": {"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}},
                    {"name": "mcp__docs__find", "description": "Find.",
                     "input_schema": {"type": "object", "properties": {}, "required": [], "$defs": {"q": {"type": "string"}}}, "cache_control": cached},
                ],
                "messages": [
                    {"role": "user", "content": [
                        {"type": "text", "text": "Look at this."},
                        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aW1n"}},
                    ]},
                    {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": "Two calls.", "signature": "sig"},
                        {"type": "redacted_thinking", "data": "opaque"},
                        {"type": "text", "text": "Running both."},
                        {"type": "tool_use", "id": "call_1", "name": "bash", "input": {"command": "ls"}},
                        {"type": "tool_use", "id": "call_2", "name": "bash", "input": {}},
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "call_1", "content": "a\nb"},
                        {"type": "tool_result", "tool_use_id": "call_2", "content": "boom\n(images follow the tool results)", "is_error": true},
                        {"type": "text", "text": "Images of the failed tool call call_2:"},
                        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "c2hvdA=="}},
                        {"type": "text", "text": "And then?", "cache_control": cached},
                    ]},
                    {"role": "assistant", "content": [{"type": "text", "text": "Done."}]},
                    {"role": "user", "content": [{"type": "text", "text": "Thanks.", "cache_control": cached}]},
                ],
                "thinking": {"type": "adaptive", "display": "summarized"},
                "output_config": {"effort": "high"},
            })
        );
        assert_eq!(headers(&request), [("anthropic-version", "2023-06-01".to_owned()), ("x-session-id", "thread-1".to_owned())]);

        let plain = Model { efforts: vec![], images: false, ..claude() };
        let request = Request {
            model: &plain,
            system: "",
            messages: &messages[..1],
            tools: &[],
            effort: Some("high"),
            max_output: Some(100),
            cache_key: "",
        };
        assert_eq!(
            body(&request),
            json!({
                "model": "claude-haiku-5-5",
                "max_tokens": 100,
                "stream": true,
                "messages": [{"role": "user", "content": [
                    {"type": "text", "text": "Look at this."},
                    {"type": "text", "text": "(image omitted: the model does not take images)", "cache_control": cached},
                ]}],
            })
        );
        assert_eq!(headers(&request).len(), 1);
    }

    #[test]
    fn breakpoints_stay_within_the_four_the_api_allows() {
        let (system, _, tools) = request_parts();
        let model = claude();
        let turn = |text: &str| {
            [Message::User(vec![Part::Text(text.into())]), Message::Assistant { content: vec![Block::text("ok")], api: Api::Anthropic }]
        };
        let mut messages: Vec<Message> = ["one", "two", "three", "four"].iter().flat_map(|text| turn(text)).collect();
        messages.push(Message::User(vec![Part::Text("five".into())]));
        let request = Request { model: &model, system, messages: &messages, tools: &tools, effort: None, max_output: None, cache_key: "" };
        let body = body(&request);
        assert_eq!(body.to_string().matches("cache_control").count(), 4);
        let marked: Vec<&str> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["content"][0]["cache_control"].is_object())
            .map(|m| m["content"][0]["text"].as_str().unwrap())
            .collect();
        assert_eq!(marked, ["four", "five"]);
    }

    #[test]
    fn a_recorded_stream_becomes_the_reply() {
        let (partial, chunks, outcome) = replay(Stream::default(), include_str!("fixtures/anthropic_tools.sse"));
        assert_eq!(outcome, Ok(Progress::Complete));
        let thinking = "I'm weighing whether \"bash\" or \"read\" comes first alphabetically, and the tool names are zoo_bash and narrow_read, \
                        with \"bash\" landing ahead. The order doesn't really matter, so I just need to issue both calls together in one block.\n\n";
        let text = "Alphabetically, \"bash\" comes before \"read,\" so the bash call goes first. Both calls are independent, so I'm running them together.";
        let (bash, read) = ("toolu_01X7XGwb3oqsCFGLzg8KVBCi", "toolu_01LB45y32WUz2JMR2jm9JUxG");
        assert_eq!(
            partial.content,
            [
                Block::Thinking {
                    text: thinking.into(),
                    signature: Some("CAQSnQcKEAgTGAI4AUIIdGhpbmtpbmcSDAYxTRls".into()),
                    redacted: false
                },
                Block::text(text),
                Block::ToolCall { id: bash.into(), name: "bash".into(), input: json!({"command": "echo hi"}) },
                Block::ToolCall { id: read.into(), name: "read".into(), input: json!({"path": "/etc/hostname"}) },
            ]
        );
        assert_eq!(partial.usage, Usage { input: 2, output: 280, cache_read: 627, cache_write: 25, ..Usage::default() });
        assert_eq!((partial.stop, partial.cache_write_1h), (Some(Stop::ToolUse), 25));
        let streamed = |of: fn(&Chunk) -> Option<&str>| chunks.iter().filter_map(of).collect::<String>();
        assert_eq!(streamed(|c| if let Chunk::Thinking { index: 0, delta } = c { Some(delta) } else { None }), thinking);
        assert_eq!(streamed(|c| if let Chunk::Text { index: 1, delta } = c { Some(delta) } else { None }), text);
        let calls: Vec<&Chunk> = chunks.iter().filter(|c| matches!(c, Chunk::ToolCall { .. })).collect();
        assert_eq!(
            calls,
            [
                &Chunk::ToolCall { index: 2, id: bash.into(), name: "bash".into(), input: json!({"command": "echo hi"}) },
                &Chunk::ToolCall { index: 3, id: read.into(), name: "read".into(), input: json!({"path": "/etc/hostname"}) },
            ]
        );
    }

    #[test]
    fn a_stream_that_breaks_keeps_what_came() {
        let cut = concat!(
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"bash\",\"input\":{}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\": \\\"ec\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"},\"usage\":{\"output_tokens\":9}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let (partial, chunks, outcome) = replay(Stream::default(), cut);
        assert_eq!((outcome, partial.stop), (Ok(Progress::Complete), Some(Stop::MaxTokens)));
        let input = json!("{\"command\": \"ec");
        assert_eq!(partial.content, [Block::ToolCall { id: "t1".into(), name: "bash".into(), input: input.clone() }]);
        assert_eq!(chunks, [Chunk::ToolCall { index: 0, id: "t1".into(), name: "bash".into(), input }]);
        let open = &cut[..cut.find("event: content_block_stop").unwrap()];
        assert!(
            matches!(replay(Stream::default(), open).0.content.as_slice(), [Block::ToolCall { input, .. }] if *input == STILL_STREAMING)
        );

        let overloaded = concat!(
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Half a\"}}\n\n",
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
        );
        let (partial, _, outcome) = replay(Stream::default(), overloaded);
        assert_eq!(
            outcome,
            Err(Error { failure: Failure::Transient { retry_after: None }, message: "Overloaded (overloaded_error)".into() })
        );
        assert_eq!(partial.content, [Block::text("Half a")]);

        let refusal = "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"refusal\",\"stop_details\":{\"explanation\":\"Not this.\"}}}\n\n";
        assert_eq!(replay(Stream::default(), refusal).2, Err(Error { failure: Failure::Other, message: "Not this.".into() }));
        let garbage = "data: {\"type\":\"content_block_st\n\n";
        assert!(matches!(replay(Stream::default(), garbage).2, Err(Error { failure: Failure::Transient { .. }, .. })));
    }
}
