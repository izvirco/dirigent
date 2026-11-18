use super::{Chunk, Error, Message, Part, Partial, Request, Wire, sse};
use dirigent_protocol::transcript::{Api, Block, Stop};
use serde_json::{Value, json};
use std::collections::HashMap;

pub(super) const PATH: &str = "/backend-api/codex/responses";

pub(super) fn headers(request: &Request<'_>) -> Vec<(&'static str, String)> {
    let mut headers = vec![("openai-beta", "responses=experimental".to_owned()), ("originator", "dirigent".to_owned())];
    // How the Codex CLI names a conversation, next to the body's `prompt_cache_key`.
    headers.extend(super::session(request).map(|id| ("session_id", id.to_owned())));
    headers
}

pub(super) fn body(request: &Request<'_>) -> Value {
    let model = request.model;
    let mut body = json!({
        "model": model.id,
        "store": false,
        "stream": true,
        // The backend takes no request without instructions.
        "instructions": if request.system.trim().is_empty() { "You are a helpful assistant." } else { request.system },
        "input": input(request),
        "tool_choice": "auto",
        "parallel_tool_calls": true,
        "text": {"verbosity": "low"},
    });
    let tools: Vec<Value> = request.tools.iter().map(tool).collect();
    if !tools.is_empty() {
        body["tools"] = tools.into();
    }
    if !model.efforts.is_empty() {
        // `summary`: the thinking that streams is the provider's summary of it.
        body["reasoning"] = match request.effort {
            Some(effort) => json!({"effort": effort, "summary": "auto"}),
            None => json!({"summary": "auto"}),
        };
        // The reasoning itself, encrypted, to send back with the history.
        body["include"] = json!(["reasoning.encrypted_content"]);
    }
    if let Some(id) = super::session(request) {
        // The provider's limit for it is 64 characters.
        body["prompt_cache_key"] = id.chars().take(64).collect::<String>().into();
    }
    if let Some(max) = request.max_output {
        body["max_output_tokens"] = max.into();
    }
    body
}

fn tool(spec: &crate::tool::ToolSpec) -> Value {
    json!({"type": "function", "name": spec.name, "description": spec.description, "parameters": spec.input, "strict": false})
}

fn input(request: &Request<'_>) -> Vec<Value> {
    let images = request.model.images;
    let mut items = vec![];
    for message in request.messages {
        match message {
            Message::User(parts) => {
                let content = content(parts, images);
                if !content.is_empty() {
                    items.push(json!({"type": "message", "role": "user", "content": content}));
                }
            }
            Message::Assistant { content, api } => {
                let from = items.len();
                items.extend(content.iter().filter_map(|block| assistant(block, *api)));
                // The provider takes no reasoning without the item it led to: what a reply
                // that was cut short ends with.
                while items.len() > from && items[items.len() - 1]["type"] == "reasoning" {
                    items.pop();
                }
            }
            Message::ToolResult { call_id, content: parts, .. } => {
                let content = content(parts, images);
                let output = match content.as_slice() {
                    [] => json!("(no output)"),
                    content if content.iter().all(|part| part["type"] == "input_text") => {
                        content.iter().filter_map(|part| part["text"].as_str()).collect::<Vec<_>>().join("\n").into()
                    }
                    _ => content.into(),
                };
                items.push(json!({"type": "function_call_output", "call_id": super::call_id(call_id), "output": output}));
            }
        }
    }
    items
}

fn content(parts: &[Part], images: bool) -> Vec<Value> {
    let part = |part: &Part| match part {
        Part::Text(text) if text.trim().is_empty() => None,
        Part::Text(text) => Some(json!({"type": "input_text", "text": text})),
        Part::Image { mime, data } if images => {
            Some(json!({"type": "input_image", "detail": "auto", "image_url": format!("data:{mime};base64,{data}")}))
        }
        Part::Image { .. } => Some(json!({"type": "input_text", "text": super::NO_IMAGES})),
    };
    parts.iter().filter_map(part).collect()
}

fn assistant(block: &Block, api: Api) -> Option<Value> {
    match block {
        Block::Text { text, .. } if !text.trim().is_empty() => {
            Some(json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]}))
        }
        Block::Thinking { signature: Some(item), .. } if api == Api::Codex => {
            serde_json::from_str::<Value>(item).ok().filter(Value::is_object)
        }
        Block::ToolCall { id, name, input } => {
            // The API takes an object's JSON only: a call that had none (`Reply`) goes back without arguments.
            let arguments = if input.is_object() { input.to_string() } else { "{}".into() };
            Some(json!({"type": "function_call", "call_id": super::call_id(id), "name": name, "arguments": arguments}))
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
    arguments: String,
}

impl Wire for Stream {
    fn event(&mut self, event: sse::Event, on: &mut (dyn FnMut(Chunk) + Send)) -> Result<bool, Error> {
        if event.data.is_empty() || event.data == "[DONE]" {
            return Ok(false);
        }
        let data: Value =
            serde_json::from_str(&event.data).map_err(|e| Error::transient(format!("The proxy sent an event that is not JSON ({e})")))?;
        let index = data["output_index"].as_u64().unwrap_or(0);
        let delta = data["delta"].as_str().unwrap_or_default();
        match data["type"].as_str().or(event.name.as_deref()).unwrap_or_default() {
            "response.output_item.added" => {
                self.open(index, &data["item"]);
            }
            // A summary comes in parts: paragraphs of the thinking.
            "response.reasoning_summary_part.added" => {
                if let Some((at, Block::Thinking { text, .. })) = self.block(index)
                    && !text.is_empty()
                {
                    text.push_str("\n\n");
                    on(Chunk::Thinking { index: at, delta: "\n\n".into() });
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some((at, Block::Thinking { text, .. })) = self.block(index)
                    && !delta.is_empty()
                {
                    text.push_str(delta);
                    on(Chunk::Thinking { index: at, delta: delta.into() });
                }
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                if let Some((at, Block::Text { text, .. })) = self.block(index)
                    && !delta.is_empty()
                {
                    text.push_str(delta);
                    on(Chunk::Text { index: at, delta: delta.into() });
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(open) = self.open.get_mut(&index) {
                    open.arguments.push_str(delta);
                }
            }
            "response.output_item.done" => self.close(index, &data["item"], on),
            "response.completed" | "response.incomplete" | "response.done" => {
                let response = &data["response"];
                self.usage(&response["usage"]);
                let reason = response["incomplete_details"]["reason"].as_str();
                self.partial.stop = match (response["status"].as_str(), reason) {
                    (Some("incomplete"), Some("max_output_tokens")) => Some(Stop::MaxTokens),
                    (Some("incomplete"), reason) => {
                        return Err(Error::other(format!("The reply was cut short: {}", reason.unwrap_or("no reason given"))));
                    }
                    (Some("failed" | "cancelled"), _) => return Err(Error::in_stream(&data)),
                    _ => Some(Stop::End),
                };
                return Ok(true);
            }
            "response.failed" | "error" => return Err(Error::in_stream(&data)),
            _ if !data["error"].is_null() => return Err(Error::in_stream(&data)),
            _ => {}
        }
        Ok(false)
    }

    fn take(&mut self) -> Partial {
        std::mem::take(&mut self.partial)
    }
}

impl Stream {
    fn block(&mut self, index: u64) -> Option<(usize, &mut Block)> {
        let at = self.open.get(&index)?.at;
        Some((at, &mut self.partial.content[at]))
    }

    fn open(&mut self, index: u64, item: &Value) -> bool {
        let text = |key: &str| item[key].as_str().unwrap_or_default().to_owned();
        let block = match item["type"].as_str().unwrap_or_default() {
            "reasoning" => Block::Thinking { text: String::new(), signature: None, redacted: false },
            "message" => Block::text(""),
            "function_call" => Block::ToolCall { id: text("call_id"), name: text("name"), input: Value::Null },
            _ => return false,
        };
        self.open.insert(index, Open { at: self.partial.content.len(), arguments: text("arguments") });
        self.partial.content.push(block);
        true
    }

    fn close(&mut self, index: u64, item: &Value, on: &mut (dyn FnMut(Chunk) + Send)) {
        // A provider may send an item whole, without the events that open and fill it.
        if !self.open.contains_key(&index) && !self.open(index, item) {
            return;
        }
        let Some(open) = self.open.remove(&index) else { return };
        let texts = |parts: &Value, between: &str| {
            let texts = parts.as_array().into_iter().flatten().filter_map(|part| part["text"].as_str().or(part["refusal"].as_str()));
            texts.collect::<Vec<_>>().join(between)
        };
        match &mut self.partial.content[open.at] {
            Block::Thinking { text, signature, .. } => {
                let whole =
                    Some(texts(&item["summary"], "\n\n")).filter(|s| !s.is_empty()).unwrap_or_else(|| texts(&item["content"], "\n\n"));
                amend(text, whole, |delta| on(Chunk::Thinking { index: open.at, delta }));
                // MUST KILL: type: a reasoning item's whole JSON is stashed in Block::Thinking.signature; give it a field of its own
                *signature = Some(item.to_string());
            }
            Block::Text { text, .. } => amend(text, texts(&item["content"], ""), |delta| on(Chunk::Text { index: open.at, delta })),
            Block::ToolCall { id, name, input } => {
                let arguments = item["arguments"].as_str().filter(|arguments| !arguments.is_empty()).unwrap_or(&open.arguments);
                *input = super::call_input(arguments);
                on(Chunk::ToolCall { index: open.at, id: id.clone(), name: name.clone(), input: input.clone() });
            }
            Block::Image { .. } => {}
        }
    }

    fn usage(&mut self, usage: &Value) {
        let count = |value: &Value| value.as_u64().unwrap_or(0);
        let (read, written) =
            (count(&usage["input_tokens_details"]["cached_tokens"]), count(&usage["input_tokens_details"]["cache_write_tokens"]));
        let counts = &mut self.partial.usage;
        // The input count has the cached tokens in it.
        counts.input = count(&usage["input_tokens"]).saturating_sub(read + written);
        counts.output = count(&usage["output_tokens"]);
        counts.cache_read = read;
        counts.cache_write = written;
    }
}

fn amend(text: &mut String, whole: String, mut on: impl FnMut(String)) {
    if whole.is_empty() || whole == *text {
        return;
    }
    if let Some(rest) = whole.strip_prefix(text.as_str()) {
        on(rest.to_owned());
    }
    *text = whole;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::tests::{model, replay, request_parts};
    use crate::proxy::{Failure, Model};
    use dirigent_protocol::transcript::Usage;

    fn gpt() -> Model {
        model("gpt-5.5", Api::Codex)
    }

    #[test]
    fn a_request_is_what_the_api_takes() {
        let (system, messages, tools) = request_parts();
        let model = gpt();
        let request = Request {
            model: &model,
            system,
            messages: &messages,
            tools: &tools,
            effort: Some("high"),
            max_output: None,
            cache_key: "thread-1",
        };
        assert_eq!(
            body(&request),
            json!({
                "model": "gpt-5.5",
                "store": false,
                "stream": true,
                "instructions": "Be terse.",
                "input": [
                    {"type": "message", "role": "user", "content": [
                        {"type": "input_text", "text": "Look at this."},
                        {"type": "input_image", "detail": "auto", "image_url": "data:image/png;base64,aW1n"},
                    ]},
                    {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Running both."}]},
                    {"type": "function_call", "call_id": "call_1", "name": "bash", "arguments": "{\"command\":\"ls\"}"},
                    {"type": "function_call", "call_id": "call_2", "name": "bash", "arguments": "{}"},
                    {"type": "function_call_output", "call_id": "call_1", "output": "a\nb"},
                    {"type": "function_call_output", "call_id": "call_2", "output": [
                        {"type": "input_text", "text": "boom"},
                        {"type": "input_image", "detail": "auto", "image_url": "data:image/png;base64,c2hvdA=="},
                    ]},
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "And then?"}]},
                    {"id": "rs_1", "type": "reasoning", "encrypted_content": "enc", "summary": [{"type": "summary_text", "text": "Thought."}]},
                    {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Done."}]},
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Thanks."}]},
                ],
                "tool_choice": "auto",
                "parallel_tool_calls": true,
                "text": {"verbosity": "low"},
                "tools": [
                    {"type": "function", "name": "bash", "description": "Run a command.", "strict": false,
                     "parameters": {"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}},
                    {"type": "function", "name": "mcp__docs__find", "description": "Find.", "strict": false,
                     "parameters": {"anyOf": [{"type": "object"}], "$defs": {"q": {"type": "string"}}}},
                ],
                "reasoning": {"effort": "high", "summary": "auto"},
                "include": ["reasoning.encrypted_content"],
                "prompt_cache_key": "thread-1",
            })
        );
        assert_eq!(headers(&request)[2], ("session_id", "thread-1".to_owned()));

        let plain = Model { efforts: vec![], images: false, ..gpt() };
        let request = Request {
            model: &plain,
            system: " ",
            messages: &messages[..1],
            tools: &[],
            effort: Some("high"),
            max_output: Some(100),
            cache_key: "",
        };
        assert_eq!(
            body(&request),
            json!({
                "model": "gpt-5.5",
                "store": false,
                "stream": true,
                "instructions": "You are a helpful assistant.",
                "input": [{"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "Look at this."},
                    {"type": "input_text", "text": "(image omitted: the model does not take images)"},
                ]}],
                "tool_choice": "auto",
                "parallel_tool_calls": true,
                "text": {"verbosity": "low"},
                "max_output_tokens": 100,
            })
        );
        assert_eq!(headers(&request).len(), 2);
    }

    #[test]
    fn a_recorded_stream_becomes_the_reply() {
        let (partial, chunks, outcome) = replay(Stream::default(), include_str!("fixtures/codex_tools.sse"));
        assert_eq!(outcome, Ok(true));
        let thinking = "I'm checking which comes first alphabetically, \"bash\" or \"read,\" and it's \"bash.\" I'll make both calls, \
                        the echo and reading the hostname file, together in one block and keep the reply brief.\n\n";
        let text = "Alphabetically, \"bash\" comes before \"read,\" so I'll run the bash command first and the read together.";
        let (bash, read) = ("toolu_01GPdJN7qYf7mNwqvpCrEz7s", "toolu_01Dqi7ZrYJfCpyKVyJWJhd3p");
        let item = json!({
            "id": "rs_msg_011Ch4xATvjVRrBaLNVxiXQ3_0", "type": "reasoning", "status": "completed",
            "encrypted_content": "CAQSngcKEAgTGAI4AUIIdGhpbmtpbmcSDK2uKbKR", "summary": [{"type": "summary_text", "text": thinking}],
        });
        assert_eq!(
            partial.content,
            [
                Block::Thinking { text: thinking.into(), signature: Some(item.to_string()), redacted: false },
                Block::text(text),
                Block::ToolCall { id: bash.into(), name: "bash".into(), input: json!({"command": "echo hi"}) },
                Block::ToolCall { id: read.into(), name: "read".into(), input: json!({"path": "/etc/hostname"}) },
            ]
        );
        assert_eq!(partial.usage, Usage { input: 2, output: 274, cache_read: 652, cache_write: 0, ..Usage::default() });
        assert_eq!(partial.stop, Some(Stop::End));
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
        let model = gpt();
        let messages = [Message::Assistant { content: partial.content, api: Api::Codex }];
        let request = Request { model: &model, system: "", messages: &messages, tools: &[], effort: None, max_output: None, cache_key: "" };
        assert_eq!(body(&request)["input"][0], item);
    }

    #[test]
    fn a_stream_that_breaks_keeps_what_came() {
        let start = concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"content\":[]}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"Half a\"}\n\n",
        );
        let cut = "data: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\
                   \"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\n";
        let (partial, _, outcome) = replay(Stream::default(), &format!("{start}{cut}"));
        assert_eq!((outcome, partial.stop, partial.usage.input), (Ok(true), Some(Stop::MaxTokens), 10));
        assert_eq!(partial.content, [Block::text("Half a")]);

        let failed = "data: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":{\"code\":\"server_error\",\"message\":\"The server had an error\"}}}\n\n";
        let (partial, _, outcome) = replay(Stream::default(), &format!("{start}{failed}"));
        assert_eq!(
            outcome,
            Err(Error { failure: Failure::Transient { retry_after: None }, message: "The server had an error (server_error)".into() })
        );
        assert_eq!(partial.content, [Block::text("Half a")]);
        let error = "data: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"code\":\"context_length_exceeded\",\"message\":\"Your input exceeds the context window of this model.\"}}\n\n";
        assert_eq!(replay(Stream::default(), error).2.unwrap_err().failure, Failure::Overflow);

        let whole = "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"bash\",\"arguments\":\"{\\\"command\\\":\\\"ls\\\"}\"}}\n\n";
        let (partial, chunks, _) = replay(Stream::default(), whole);
        let call = Block::ToolCall { id: "c1".into(), name: "bash".into(), input: json!({"command": "ls"}) };
        assert_eq!(partial.content, [call]);
        assert_eq!(chunks, [Chunk::ToolCall { index: 0, id: "c1".into(), name: "bash".into(), input: json!({"command": "ls"}) }]);

        let call = |arguments: &str| {
            let item = json!({"type": "function_call", "call_id": "c1", "name": "bash", "arguments": arguments});
            let done = json!({"type": "response.output_item.done", "output_index": 0, "item": item});
            replay(Stream::default(), &format!("data: {done}\n\n")).0.content
        };
        let bash = |input: Value| [Block::ToolCall { id: "c1".into(), name: "bash".into(), input }];
        assert_eq!(call("{\"command\": \"cd C:\\src\nls\"}"), bash(json!({"command": "cd C:\\src\nls"})));
        assert_eq!(call("{\"command\": "), bash(json!("{\"command\": ")));
    }
}
