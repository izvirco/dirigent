//! Turns opaque pi entries / events into what the thread view renders: a timeline of
//! user messages, *work* (everything the model did between a prompt and its reply:
//! thinking, interim prose, tool calls), replies, and notices. Anything unrecognised
//! becomes a `Notice`, so pi format changes degrade gracefully instead of breaking the UI.

use dirigent_protocol::PromptMode;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    /// `at` is pi's message timestamp (ms); the next `Work` starts there. `images` are
    /// blob ids (the host keeps image bytes out of entries; see `Dirigent::blobs`).
    User {
        text: String,
        images: Vec<String>,
        at: i64,
    },
    Work(Work),
    /// The reply that ends a turn.
    Assistant(String),
    /// Assistant turn ended with an error (API failure, abort, ...).
    Error(String),
    /// Informational line (warnings, compaction, unknown entries, ...).
    Notice(String),
}

/// One model run: from the prompt to the final reply. Open (`ended == None`) while the
/// agent is still working on it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Work {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub started: i64,
    pub ended: Option<i64>,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Thinking(String),
    /// Prose the model wrote before calling more tools.
    Text(String),
    Tool(ToolCall),
    /// A message the user steered the run with mid-turn. Stays visible under the work line.
    /// `queued` until pi hands it to the model (it drains steers one per turn, after the
    /// turn's tool calls; nothing is persisted or emitted before that).
    Steer {
        text: String,
        queued: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// The argument that identifies the call at a glance (`command`, `path`, ...), one line.
    pub primary: String,
    /// Every argument, `key: value` per line, for the expanded row.
    pub args: String,
    /// The call's own `timeout` argument, in seconds (bash).
    pub timeout: Option<f64>,
    /// Unix ms. Set from the live `tool_execution_start` event or derived from the result.
    pub started: Option<i64>,
    pub ended: Option<i64>,
    pub result: Option<ToolResult>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    pub text: String,
    pub is_error: bool,
}

impl Work {
    pub fn tool_count(&self) -> usize {
        self.steps.iter().filter(|s| matches!(s, Step::Tool(_))).count()
    }

    /// The last prose the model wrote: the interim response shown in the transcript while
    /// the work goes on (until the next one, or the final reply, replaces it). Prose from
    /// before the latest steer is stale — the model hasn't answered the steer yet.
    pub fn latest_text(&self) -> Option<&str> {
        self.steps
            .iter()
            .rev()
            .take_while(|s| !matches!(s, Step::Steer { .. }))
            .find_map(|s| if let Step::Text(t) = s { Some(t.as_str()) } else { None })
    }

    /// `(step index, text, queued)` of every steer, in order.
    pub fn steers(&self) -> impl Iterator<Item = (usize, &str, bool)> {
        self.steps.iter().enumerate().filter_map(|(i, s)| match s {
            Step::Steer { text, queued } => Some((i, text.as_str(), *queued)),
            _ => None,
        })
    }

    /// Duration in ms: to `ended`, else to the last tool's end, else to `now`.
    pub fn duration(&self, now: i64) -> i64 {
        let end = self.ended.or_else(|| self.steps.iter().rev().find_map(|s| if let Step::Tool(t) = s { t.ended } else { None }));
        (end.unwrap_or(now) - self.started).max(0)
    }
}

/// Builds the timeline entry by entry. Tracks the session's model / thinking level so a
/// `Work` can be labelled even before its first assistant message says which model ran.
#[derive(Debug, Default)]
pub struct Timeline {
    pub items: Vec<Item>,
    model: Option<String>,
    effort: Option<String>,
}

impl Timeline {
    pub fn from_entries<'a>(entries: impl IntoIterator<Item = &'a Value>) -> Self {
        let mut t = Self::default();
        for e in entries {
            t.push(e);
        }
        t
    }

    /// Fold one persisted entry in. Returns the index of the first item it changed or
    /// created, so the list can remeasure from there.
    pub fn push(&mut self, entry: &Value) -> Option<usize> {
        let kind = entry.get("type").and_then(Value::as_str).unwrap_or("?");
        match kind {
            "message" => self.push_message(entry.get("message")?),
            "thinking_level_change" => {
                self.effort = entry.get("thinkingLevel").and_then(Value::as_str).map(str::to_owned);
                None
            }
            "model_change" => {
                self.model = entry.get("modelId").and_then(Value::as_str).map(str::to_owned);
                None
            }
            "session_info" | "label" | "usage" | "custom" => None,
            "compaction" => Some(self.add(Item::Notice("context compacted".into()))),
            "custom_message" => Some(self.add(Item::Notice(text_of(entry.get("content"))))),
            other => Some(self.add(Item::Notice(format!("unknown entry: {other}")))),
        }
    }

    fn push_message(&mut self, msg: &Value) -> Option<usize> {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("?");
        let at = msg.get("timestamp").and_then(Value::as_i64).unwrap_or(0);
        match role {
            // A user message while a work is open can only be a steer: a fresh prompt waits
            // for the final assistant message, which closes the work.
            "user" => Some(match self.open_work() {
                Some(ix) => {
                    self.work_mut(ix).steps.push(Step::Steer { text: text_of(msg.get("content")), queued: false });
                    ix
                }
                None => self.add(Item::User { text: text_of(msg.get("content")), images: images_of(msg.get("content")), at }),
            }),
            "assistant" => self.push_assistant(msg, at),
            "toolResult" => {
                let id = msg.get("toolCallId").and_then(Value::as_str).unwrap_or("");
                let result = ToolResult {
                    text: text_of(msg.get("content")),
                    is_error: msg.get("isError").and_then(Value::as_bool).unwrap_or(false),
                };
                let duration = msg.get("durationMs").and_then(Value::as_i64);
                let ix = self.open_work()?;
                let Item::Work(w) = &mut self.items[ix] else { return None };
                let tool = w.steps.iter_mut().rev().find_map(|s| match s {
                    Step::Tool(t) if t.id == id => Some(t),
                    _ => None,
                })?;
                tool.ended = Some(at);
                tool.started = tool.started.or(duration.map(|d| at - d));
                tool.result = Some(result);
                Some(ix)
            }
            "system" => None,
            "bashExecution" => Some(self.add(Item::Notice(format!("$ {}", msg.get("command").and_then(Value::as_str).unwrap_or(""))))),
            "compactionSummary" | "branchSummary" => Some(self.add(Item::Notice(text_of(msg.get("summary"))))),
            other => Some(self.add(Item::Notice(format!("unknown message role: {other}")))),
        }
    }

    fn push_assistant(&mut self, msg: &Value, at: i64) -> Option<usize> {
        if let Some(m) = msg.get("model").and_then(Value::as_str) {
            self.model = Some(m.to_owned());
        }
        if let Some(e) = msg.get("thinkingLevel").and_then(Value::as_str) {
            self.effort = Some(e.to_owned());
        }
        let ix = self.open_work().unwrap_or_else(|| self.start_work(at));
        if let Item::Work(w) = &mut self.items[ix] {
            w.model = self.model.clone();
            w.effort = self.effort.clone();
        }
        let blocks = msg.get("content").and_then(Value::as_array).cloned().unwrap_or_default();
        let calls_tools = blocks.iter().any(|b| b.get("type").and_then(Value::as_str) == Some("toolCall"));
        let mut reply = vec![];
        for b in &blocks {
            match step_of(b) {
                Some(Step::Text(t)) if !calls_tools => reply.push(t),
                Some(step) => self.work_mut(ix).steps.push(step),
                None => {}
            }
        }
        let stop = msg.get("stopReason").and_then(Value::as_str);
        let error =
            msg.get("errorMessage").and_then(Value::as_str).map(str::to_owned).or((stop == Some("aborted")).then(|| "aborted".into()));
        if calls_tools && error.is_none() {
            return Some(ix);
        }
        // The turn is over: close the work (drop it if nothing happened) and add the reply.
        self.work_mut(ix).ended = Some(at + msg.get("durationMs").and_then(Value::as_i64).unwrap_or(0));
        if self.work_mut(ix).steps.is_empty() {
            self.items.remove(ix);
        }
        if !reply.is_empty() {
            self.add(Item::Assistant(reply.join("\n")));
        }
        if let Some(e) = error {
            self.add(Item::Error(e));
        }
        Some(ix.min(self.items.len().saturating_sub(1)))
    }

    fn add(&mut self, item: Item) -> usize {
        self.items.push(item);
        self.items.len() - 1
    }

    /// Index of the last item if it is a `Work` still in progress.
    pub fn open_work(&self) -> Option<usize> {
        match self.items.last() {
            Some(Item::Work(w)) if w.ended.is_none() => Some(self.items.len() - 1),
            _ => None,
        }
    }

    /// Start a `Work` at the prompt that triggered it (the last user message), else at `at`.
    fn start_work(&mut self, at: i64) -> usize {
        let started = self.items.iter().rev().find_map(|i| if let Item::User { at, .. } = i { Some(*at) } else { None }).unwrap_or(at);
        self.add(Item::Work(Work { model: self.model.clone(), effort: self.effort.clone(), started, ended: None, steps: vec![] }))
    }

    fn work_mut(&mut self, ix: usize) -> &mut Work {
        match &mut self.items[ix] {
            Item::Work(w) => w,
            _ => unreachable!("not a work item"),
        }
    }
}

/// A content block of an assistant message as a work step.
fn step_of(b: &Value) -> Option<Step> {
    Some(match b.get("type")?.as_str()? {
        "text" => Step::Text(b.get("text")?.as_str()?.to_owned()),
        "thinking" => Step::Thinking(b.get("thinking").and_then(Value::as_str).unwrap_or("(redacted)").to_owned()),
        "toolCall" => {
            let args = b.get("arguments");
            Step::Tool(ToolCall {
                id: b.get("id").and_then(Value::as_str).unwrap_or("").to_owned(),
                name: b.get("name").and_then(Value::as_str).unwrap_or("?").to_owned(),
                primary: args_primary(args),
                args: args_multiline(args),
                timeout: args.and_then(|a| a.get("timeout")).and_then(Value::as_f64),
                ..Default::default()
            })
        }
        _ => return None,
    })
}

/// Text of a `string | (TextContent | ImageContent)[]` content field (text blocks only).
pub fn text_of(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Blob ids of the image blocks in a content field, in order.
pub fn images_of(content: Option<&Value>) -> Vec<String> {
    let Some(Value::Array(blocks)) = content else { return vec![] };
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("image"))
        .filter_map(|b| b.get("blob").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

/// The first string argument's first line (`command` for bash, `path` for read/edit, ...).
fn args_primary(args: Option<&Value>) -> String {
    let Some(Value::Object(map)) = args else { return String::new() };
    let Some(s) = map.values().find_map(Value::as_str) else { return String::new() };
    let first = s.lines().next().unwrap_or("");
    let short: String = first.chars().take(120).collect();
    if short.len() < s.len() { format!("{short}…") } else { short }
}

fn args_multiline(args: Option<&Value>) -> String {
    let Some(Value::Object(map)) = args else { return String::new() };
    map.iter()
        .filter(|(_, v)| !v.is_null())
        .map(|(k, v)| match v {
            Value::String(s) => format!("{k}: {s}"),
            other => format!("{k}: {other}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// In-flight state built from streaming events: the current assistant message's steps
/// (replaced by the persisted entry when it lands), tools executing right now, and the
/// optimistic echoes of sent messages with the mode each was sent in, oldest first. A
/// prompt echo lives until its persisted user entry lands (`confirm_prompt`); steer and
/// follow-up echoes are replaced by pi's own queues on every `queue_update`.
///
/// Text streams in word by word but is shown a line at a time (`markdown::committed`), so
/// a delta only counts as a visible change once it completes a line.
#[derive(Debug, Default, Clone)]
pub struct Streaming {
    pub steps: Vec<Step>,
    /// (tool call id, started at unix ms)
    pub running_tools: Vec<(String, i64)>,
    pub echo: Vec<(String, PromptMode)>,
}

impl Streaming {
    /// A persisted user message started a run: drop the oldest prompt echo.
    pub fn confirm_prompt(&mut self) {
        if let Some(i) = self.echo.iter().position(|(_, m)| *m == PromptMode::Prompt) {
            self.echo.remove(i);
        }
    }

    /// Queued steers, oldest first.
    pub fn queued_steers(&self) -> impl Iterator<Item = &str> {
        self.echo.iter().filter_map(|(t, m)| (*m == PromptMode::Steer).then_some(t.as_str()))
    }

    /// Apply one pi `AgentSessionEvent`. Returns true if anything visible changed.
    pub fn apply(&mut self, event: &Value, now: i64) -> bool {
        let ty = event.get("type").and_then(Value::as_str).unwrap_or("");
        match ty {
            "message_start" => {
                if event.pointer("/message/role").and_then(Value::as_str) == Some("assistant") {
                    self.steps.clear();
                    return true;
                }
                false
            }
            "message_update" => {
                let Some(ev) = event.get("assistantMessageEvent") else { return false };
                let idx = ev.get("contentIndex").and_then(Value::as_u64).unwrap_or(0) as usize;
                match ev.get("type").and_then(Value::as_str) {
                    Some("text_delta") => self.append(idx, ev.get("delta"), Step::Text),
                    Some("thinking_delta") => self.append(idx, ev.get("delta"), Step::Thinking),
                    Some("toolcall_end") => {
                        if let Some(s) = ev.get("toolCall").and_then(step_of) {
                            self.set(idx, s);
                        }
                        true
                    }
                    _ => false,
                }
            }
            "tool_execution_start" => {
                let id = event.get("toolCallId").and_then(Value::as_str).unwrap_or("").to_owned();
                self.running_tools.push((id, now));
                true
            }
            "tool_execution_end" => {
                let id = event.get("toolCallId").and_then(Value::as_str).unwrap_or("");
                self.running_tools.retain(|(i, _)| i != id);
                true
            }
            // Pi's queues are the truth for steers and follow-ups (it emits this before
            // delivering one, and after a cancel). A prompt pi had to queue (sent as the run
            // started) shows up here as a follow-up, so its prompt echo goes too.
            "queue_update" => {
                let list = |k| event.get(k).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str);
                let (steering, follow_up): (Vec<_>, Vec<_>) = (list("steering").collect(), list("followUp").collect());
                self.echo.retain(|(t, m)| *m == PromptMode::Prompt && !follow_up.contains(&t.as_str()));
                self.echo.extend(steering.into_iter().map(|t| (t.to_owned(), PromptMode::Steer)));
                self.echo.extend(follow_up.into_iter().map(|t| (t.to_owned(), PromptMode::FollowUp)));
                true
            }
            // The run is over; an abort strands whatever pi still had queued.
            "agent_settled" => {
                self.steps.clear();
                self.running_tools.clear();
                self.echo.clear();
                true
            }
            _ => false,
        }
    }

    pub fn running_since(&self, id: &str) -> Option<i64> {
        self.running_tools.iter().find(|(i, _)| i == id).map(|(_, t)| *t)
    }

    /// What the model is doing right now, for the work line: the running tool (named from
    /// `steps`, the run's persisted and streamed steps) and its elapsed time, else the kind
    /// of step being streamed.
    pub fn activity(&self, steps: &[Step], now: i64) -> Option<String> {
        if let Some((id, since)) = self.running_tools.last() {
            let name = steps.iter().rev().find_map(|s| match s {
                Step::Tool(t) if &t.id == id => Some(t.name.as_str()),
                _ => None,
            });
            return Some(format!("{} {}", name.unwrap_or("tool"), crate::thread::duration(now - since)));
        }
        match self.steps.last()? {
            Step::Thinking(_) => Some("thinking".into()),
            Step::Text(_) => Some("writing".into()),
            Step::Tool(_) | Step::Steer { .. } => None,
        }
    }

    /// Returns true when the delta completed a line (the only time the view changes).
    fn append(&mut self, idx: usize, delta: Option<&Value>, make: impl FnOnce(String) -> Step) -> bool {
        let delta = delta.and_then(Value::as_str).unwrap_or("");
        match self.steps.get_mut(idx) {
            Some(Step::Text(s) | Step::Thinking(s)) => s.push_str(delta),
            _ => self.set(idx, make(delta.to_owned())),
        }
        delta.contains('\n')
    }

    fn set(&mut self, idx: usize, step: Step) {
        while self.steps.len() <= idx {
            self.steps.push(Step::Text(String::new()));
        }
        self.steps[idx] = step;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn turn_becomes_user_work_reply() {
        let entries = [
            json!({"type":"message","message":{"role":"user","content":"hi","timestamp":1000}}),
            json!({"type":"message","message":{"role":"assistant","model":"m","thinkingLevel":"high","timestamp":1500,"stopReason":"toolUse","content":[
                {"type":"thinking","thinking":"hmm"},
                {"type":"text","text":"looking"},
                {"type":"toolCall","id":"c1","name":"bash","arguments":{"command":"ls -la\nnext","timeout":60}}
            ]}}),
            json!({"type":"message","message":{"role":"toolResult","toolCallId":"c1","toolName":"bash","timestamp":2000,"durationMs":300,"content":[{"type":"text","text":"a\nb"}],"isError":false}}),
            json!({"type":"message","message":{"role":"assistant","model":"m","timestamp":2500,"durationMs":500,"stopReason":"stop","content":[{"type":"text","text":"done"}]}}),
        ];
        let t = Timeline::from_entries(entries.iter());
        assert_eq!(t.items.len(), 3);
        assert_eq!(t.items[0], Item::User { text: "hi".into(), images: vec![], at: 1000 });
        let Item::Work(w) = &t.items[1] else { panic!() };
        assert_eq!((w.model.as_deref(), w.effort.as_deref(), w.started, w.ended), (Some("m"), Some("high"), 1000, Some(3000)));
        assert_eq!(w.tool_count(), 1);
        assert_eq!(w.steps[0], Step::Thinking("hmm".into()));
        assert_eq!(w.steps[1], Step::Text("looking".into()));
        let Step::Tool(c) = &w.steps[2] else { panic!() };
        assert_eq!((c.primary.as_str(), c.timeout, c.started, c.ended), ("ls -la…", Some(60.), Some(1700), Some(2000)));
        assert_eq!(c.result.as_ref().map(|r| r.text.as_str()), Some("a\nb"));
        assert_eq!(t.items[2], Item::Assistant("done".into()));
    }

    #[test]
    fn steer_stays_inside_the_work() {
        let assistant = |at: i64, stop: &str, content: Value| json!({"type":"message","message":{"role":"assistant","model":"m","timestamp":at,"stopReason":stop,"content":content}});
        let entries = [
            json!({"type":"message","message":{"role":"user","content":"hi","timestamp":1}}),
            assistant(2, "toolUse", json!([{"type":"text","text":"before"},{"type":"toolCall","id":"c1","name":"bash","arguments":{}}])),
            json!({"type":"message","message":{"role":"toolResult","toolCallId":"c1","timestamp":3,"content":""}}),
            json!({"type":"message","message":{"role":"user","content":"no, other way","timestamp":4}}),
            assistant(5, "toolUse", json!([{"type":"text","text":"after"},{"type":"toolCall","id":"c2","name":"bash","arguments":{}}])),
            json!({"type":"message","message":{"role":"toolResult","toolCallId":"c2","timestamp":6,"content":""}}),
            assistant(7, "stop", json!([{"type":"text","text":"done"}])),
            json!({"type":"message","message":{"role":"user","content":"next prompt","timestamp":8}}),
        ];
        let mut t = Timeline::default();
        let changed: Vec<_> = entries.iter().map(|e| t.push(e)).collect();
        // One work for the whole run; the steer changed item 1, not a new one.
        assert_eq!(changed[3], Some(1));
        let Item::Work(w) = &t.items[1] else { panic!() };
        assert_eq!(w.steers().collect::<Vec<_>>(), vec![(2, "no, other way", false)]);
        assert_eq!(w.tool_count(), 2);
        assert_eq!(t.items[2], Item::Assistant("done".into()));
        assert_eq!(t.items[3], Item::User { text: "next prompt".into(), images: vec![], at: 8 });
        // Prose from before the steer is stale until the model writes again.
        let mut w = w.clone();
        assert_eq!(w.latest_text(), Some("after"));
        w.steps.push(Step::Steer { text: "again".into(), queued: true });
        assert_eq!(w.latest_text(), None);
    }

    #[test]
    fn queue_update_owns_steer_and_follow_up_echoes() {
        let mut s = Streaming::default();
        s.echo = vec![("p".into(), PromptMode::Prompt), ("s1".into(), PromptMode::Steer), ("s2".into(), PromptMode::Steer)];
        // s1 was delivered (or cancelled); s2 remains, in pi's order.
        assert!(s.apply(&json!({"type":"queue_update","steering":["s2"],"followUp":["f"]}), 0));
        assert_eq!(s.echo, vec![("p".into(), PromptMode::Prompt), ("s2".into(), PromptMode::Steer), ("f".into(), PromptMode::FollowUp)]);
        assert_eq!(s.queued_steers().collect::<Vec<_>>(), vec!["s2"]);
        s.confirm_prompt();
        assert_eq!(s.echo.len(), 2);
        // A prompt pi queued as a follow-up is no longer a prompt.
        s.echo.push(("late".into(), PromptMode::Prompt));
        s.apply(&json!({"type":"queue_update","steering":[],"followUp":["late"]}), 0);
        assert_eq!(s.echo, vec![("late".into(), PromptMode::FollowUp)]);
        s.apply(&json!({"type":"agent_settled"}), 0);
        assert!(s.echo.is_empty());
    }

    #[test]
    fn user_message_with_images() {
        let e = json!({"type":"message","message":{"role":"user","timestamp":1,"content":[
            {"type":"text","text":"see [image-1]"},{"type":"image","mimeType":"image/png","blob":"abc"}]}});
        let t = Timeline::from_entries([&e]);
        assert_eq!(t.items[0], Item::User { text: "see [image-1]".into(), images: vec!["abc".into()], at: 1 });
    }

    #[test]
    fn plain_reply_has_no_work() {
        let entries = [
            json!({"type":"message","message":{"role":"user","content":"hi","timestamp":1}}),
            json!({"type":"message","message":{"role":"assistant","model":"m","timestamp":2,"stopReason":"stop","content":[{"type":"text","text":"yo"}]}}),
            json!({"type":"wat"}),
        ];
        let t = Timeline::from_entries(entries.iter());
        assert_eq!(t.items[1], Item::Assistant("yo".into()));
        assert_eq!(t.items[2], Item::Notice("unknown entry: wat".into()));
    }

    #[test]
    fn streaming_accumulates_deltas() {
        let mut s = Streaming::default();
        assert!(s.apply(&json!({"type":"message_start","message":{"role":"assistant"}}), 0));
        let delta = |d: &str| json!({"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":d}});
        // Only a delta that completes a line is a visible change.
        assert!(!s.apply(&delta("he"), 0));
        assert!(s.apply(&delta("llo\n"), 0));
        assert!(!s.apply(&delta(""), 0));
        s.steps.clear();
        s.apply(&delta("he"), 0);
        s.apply(&delta("llo"), 0);
        s.apply(&json!({"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","contentIndex":1,"delta":"t"}}), 0);
        assert_eq!(s.steps, vec![Step::Text("hello".into()), Step::Thinking("t".into())]);
        s.apply(&json!({"type":"tool_execution_start","toolCallId":"c","toolName":"bash"}), 42);
        assert_eq!(s.running_since("c"), Some(42));
        s.apply(&json!({"type":"tool_execution_end","toolCallId":"c"}), 50);
        assert!(s.running_tools.is_empty());
        s.apply(&json!({"type":"agent_settled"}), 0);
        assert!(s.steps.is_empty());
    }

    #[test]
    fn activity_names_the_running_tool() {
        let tool = Step::Tool(ToolCall { id: "c".into(), name: "bash".into(), ..Default::default() });
        let w = Work { steps: vec![Step::Text("a".into()), tool], ..Default::default() };
        let mut s = Streaming::default();
        assert_eq!(s.activity(&w.steps, 0), None);
        s.running_tools.push(("c".into(), 1000));
        assert_eq!(s.activity(&w.steps, 3500).as_deref(), Some("bash 2.5s"));
        s.running_tools.clear();
        s.steps.push(Step::Thinking("t".into()));
        assert_eq!(s.activity(&w.steps, 0).as_deref(), Some("thinking"));
        assert_eq!(w.latest_text(), Some("a"));
    }
}
