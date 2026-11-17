//! Turns a thread's entries and events (`dirigent_protocol::transcript`) into what the
//! thread view renders: a timeline of user messages, *work* (everything the model did
//! between a prompt and its reply: thinking, interim prose, tool calls), replies, and
//! notices. An entry of a kind this build doesn't know (a newer host's) becomes a `Notice`.

use crate::markdown;
use dirigent_protocol::PromptMode;
use dirigent_protocol::transcript::{self, Assistant, Block, Body, Entry, Event, Notice, Stop, text_of};
use serde_json::Value;
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    /// `at` is when it was sent (unix ms); the next `Work` starts there. `images` are
    /// blob ids (the host keeps image bytes out of entries; see `Dirigent::blobs`).
    User {
        text: String,
        images: Vec<String>,
        at: i64,
    },
    Work(Work),
    /// The reply that ends a turn.
    Assistant(String),
    /// Assistant turn ended with an error (API failure, abort, ...), or compacting the
    /// context failed.
    Error(String),
    /// The context was compacted here (its own item so the ruler can mark it).
    Compaction,
    /// Informational line (warnings, unknown entries, ...).
    Notice(String),
    /// A sub-agent of this thread finished (`Notice::Child`) between turns: `text` is the
    /// notice line, the thread is a click away.
    Child {
        thread_id: String,
        text: String,
    },
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
    /// `queued` until the agent hands it to the model (after the turn's tool calls; nothing
    /// is persisted before that). `images`: blob ids of
    /// its attachments, like `Item::User`'s (none while queued: echoes are text only).
    Steer {
        text: String,
        images: Vec<String>,
        queued: bool,
    },
    /// Something that reached the model mid-run on its own: an `async_bash` job ending.
    Notice(String),
    /// A request of the run re-billed what the cache should have held (`CacheBaseline`).
    /// Shown only in the log, not in the transcript.
    CacheMiss(String),
    /// A sub-agent started (the model's `dirigent` spawn) or finished (`Notice::Child`)
    /// during the run. Shown under the work line like `Item::Child`, and in the log.
    Child {
        thread_id: String,
        text: String,
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
    /// A `dirigent` spawn: the sub-agent's name. Its thread id comes with the result,
    /// which then adds a `Step::Child` to the work.
    pub spawned: Option<String>,
    /// Unix ms. Set from the live `Event::ToolStart` or derived from the result.
    pub started: Option<i64>,
    pub ended: Option<i64>,
    pub result: Option<ToolResult>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ToolResult {
    pub text: String,
    pub is_error: bool,
    /// Blob ids of the images in the result (a `read` of an image file), in order.
    pub images: Vec<String>,
}

impl Work {
    pub fn tool_count(&self) -> usize {
        self.steps.iter().filter(|s| matches!(s, Step::Tool(_))).count()
    }

    /// The last prose the model wrote: the interim response shown in the transcript while
    /// the work goes on (until the next one, or the final reply, replaces it). Prose from
    /// before the latest steer the model has is stale — it hasn't answered that steer yet.
    /// A queued steer hides nothing: the model writes on without it.
    pub fn latest_text(&self) -> Option<&str> {
        self.steps
            .iter()
            .rev()
            .take_while(|s| !matches!(s, Step::Steer { queued: false, .. }))
            .find_map(|s| if let Step::Text(t) = s { Some(t.as_str()) } else { None })
    }

    /// `(step index, text, images, queued)` of every steer, in order.
    pub fn steers(&self) -> impl Iterator<Item = (usize, &str, &[String], bool)> {
        self.steps.iter().enumerate().filter_map(|(i, s)| match s {
            Step::Steer { text, images, queued } => Some((i, text.as_str(), images.as_slice(), *queued)),
            _ => None,
        })
    }

    /// Duration in ms of a finished work: to `ended`, else (a run that never got its final
    /// message: its host died under it) to the last thing that happened in it, a tool's
    /// end or start, else nothing. The run in progress uses `now - started` directly, so
    /// its clock keeps moving between tool calls.
    pub fn duration(&self) -> i64 {
        let end =
            self.ended.or_else(|| self.steps.iter().rev().find_map(|s| if let Step::Tool(t) = s { t.ended.or(t.started) } else { None }));
        (end.unwrap_or(self.started) - self.started).max(0)
    }
}

/// What a thread cost so far, summed over its requests: the model's replies and the
/// summaries its compactions wrote (the context chip's hover card). `input` is everything
/// fed to the model (fresh + cache read + cache write); `cached` is the part read from cache.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Stats {
    pub input: u64,
    pub output: u64,
    pub cached: u64,
    pub cost: f64,
    pub user_messages: usize,
    pub assistant_messages: usize,
    pub compactions: usize,
}

impl Stats {
    pub fn of<'a>(entries: impl IntoIterator<Item = &'a Entry>) -> Self {
        let mut s = Self::default();
        for e in entries {
            let usage = match &e.body {
                Body::User { .. } => {
                    s.user_messages += 1;
                    continue;
                }
                Body::Assistant(a) => {
                    s.assistant_messages += 1;
                    &a.usage
                }
                Body::Compaction { usage, .. } => {
                    s.compactions += 1;
                    usage
                }
                _ => continue,
            };
            s.input += usage.prompt();
            s.cached += usage.cache_read;
            s.output += usage.output;
            s.cost += usage.cost.total();
        }
        s
    }

    /// Share of the prompt tokens that came from cache, in percent.
    pub fn cached_pct(&self) -> u64 {
        if self.input == 0 { 0 } else { (self.cached as f64 / self.input as f64 * 100.).round() as u64 }
    }
}

/// The noise floor: a miss this small is cache breakpoint granularity.
const MISS_FLOOR: i64 = 1024;
/// Idle gaps longer than this (Anthropic's default cache TTL) are named as the likely cause.
const CACHE_TTL_MS: i64 = 5 * 60 * 1000;

/// Spots prompt-cache misses: every entry of the branch goes through `observe`, which
/// compares each reply's prompt with the previous request's. What the previous one had
/// cached and this one didn't read from the cache was billed again.
#[derive(Debug, Default)]
struct CacheBaseline {
    prev: Option<Request>,
    /// $ per cache-read token per model, from its last reply that read the cache: what a
    /// full miss should have paid.
    read_rate: std::collections::HashMap<String, f64>,
}

/// The request a miss is measured against.
#[derive(Debug)]
struct Request {
    /// Prompt tokens: fresh input + cache read + cache write.
    tokens: u64,
    model: String,
    /// When it started (ms).
    at: i64,
    /// The thread reported cache activity by then (a zero-cache turn only counts after that,
    /// so models without caching never show a miss).
    reported: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct CacheMiss {
    /// Prompt tokens the previous request had cached but this one didn't read from cache.
    tokens: u64,
    /// Extra $ paid against a full hit.
    cost: f64,
    idle_ms: i64,
    model_changed: bool,
}

impl CacheBaseline {
    fn observe(&mut self, entry: &Entry) -> Option<CacheMiss> {
        match &entry.body {
            // The context legitimately changed: the next prompt is new content, not re-billed.
            Body::Compaction { .. } => self.prev = None,
            Body::Assistant(a) => return self.assistant(a, entry.at - a.duration_ms),
            _ => {}
        }
        None
    }

    /// `at`: when the request started.
    fn assistant(&mut self, a: &Assistant, at: i64) -> Option<CacheMiss> {
        let u = &a.usage;
        let (input, read, write, tokens) = (u.input, u.cache_read, u.cache_write, u.prompt());
        if read > 0 {
            self.read_rate.insert(a.model.clone(), u.cost.cache_read / read as f64);
        }
        let miss = self.prev.as_ref().filter(|p| tokens > 0 && (read + write > 0 || p.reported)).and_then(|p| {
            let missed = p.tokens.min(tokens) as i64 - read as i64;
            (missed > MISS_FLOOR).then(|| {
                // Missed tokens land in input or cache write: the paid rate is theirs.
                let paid = input + write;
                let paid_rate = if paid > 0 { (u.cost.input + u.cost.cache_write) / paid as f64 } else { 0. };
                let read_rate = self.read_rate.get(&a.model).copied().unwrap_or(0.);
                CacheMiss {
                    tokens: missed as u64,
                    cost: missed as f64 * (paid_rate - read_rate).max(0.),
                    idle_ms: (at - p.at).max(0),
                    model_changed: a.model != p.model,
                }
            })
        });
        // A request with no prompt (an error before the call) leaves the baseline alone.
        if tokens > 0 {
            let reported = read + write > 0 || self.prev.as_ref().is_some_and(|p| p.reported);
            self.prev = Some(Request { tokens, model: a.model.clone(), at, reported });
        }
        miss
    }
}

impl CacheMiss {
    /// The line for the log; `None` for a miss too small to show.
    fn notice(&self) -> Option<String> {
        if self.tokens < 20_000 && self.cost < 0.1 {
            return None;
        }
        let label = if self.model_changed {
            "cache miss after model switch".to_owned()
        } else if self.idle_ms >= CACHE_TTL_MS {
            format!("cache miss after {}m idle", (self.idle_ms as f64 / 60_000.).round())
        } else {
            "cache miss".to_owned()
        };
        let cost = if self.cost >= 0.01 { format!(" (~${:.2})", self.cost) } else { String::new() };
        Some(format!("{label}: {} tokens re-billed{cost}", crate::theme::kilo(self.tokens)))
    }
}

/// Builds the timeline entry by entry, from the entries of a thread's current branch.
#[derive(Debug, Default)]
pub struct Timeline {
    pub items: Vec<Item>,
    /// Per item, the id of the entry that created it: what a message pin names
    /// (`ThreadSummary::pinned_entries`), stable across clients unlike an index.
    ids: Vec<String>,
    /// Id of the entry being folded in (`push`), for `add`.
    current: String,
    cache: CacheBaseline,
}

impl Timeline {
    pub fn from_entries<'a>(entries: impl IntoIterator<Item = &'a Entry>) -> Self {
        let mut t = Self::default();
        for e in entries {
            t.push(e);
        }
        t
    }

    /// Fold one persisted entry in. Returns the index of the first item it changed or
    /// created, so the list can remeasure from there.
    pub fn push(&mut self, entry: &Entry) -> Option<usize> {
        self.current = entry.id.clone();
        let miss = self.cache.observe(entry).and_then(|m| m.notice());
        match &entry.body {
            Body::User { content } => Some(self.said(text_of(content), images_of(content), entry.at)),
            Body::Assistant(a) => Some(self.push_assistant(a, entry.at, miss)),
            Body::ToolResult(r) => self.push_result(r, entry.at),
            Body::Notice(notice) => Some(self.push_notice(notice, entry.at)),
            Body::Compaction { .. } => Some(self.cut(Item::Compaction, entry.at)),
            Body::CompactionFailed { error } => Some(self.cut(Item::Error(error.clone()), entry.at)),
            Body::Store { .. } => None,
            Body::Unknown => Some(self.add(Item::Notice("an entry this version of dirigent doesn't know".into()))),
        }
    }

    /// A compaction, done or failed, as its own `item`. Mid-run, the work so far ends there
    /// (`at`): the model's next message starts another (`start_work`). Returns that work's
    /// index, as it changed too, else the item's.
    fn cut(&mut self, item: Item, at: i64) -> usize {
        let work = self.open_work();
        if let Some(ix) = work {
            self.work_mut(ix).ended = Some(at);
        }
        let added = self.add(item);
        work.unwrap_or(added)
    }

    /// The user's words: a message of its own, or a steer inside the work in progress (a
    /// fresh prompt waits for the final assistant message, which closes the work).
    fn said(&mut self, text: String, images: Vec<String>, at: i64) -> usize {
        match self.open_work() {
            Some(ix) => {
                self.work_mut(ix).steps.push(Step::Steer { text, images, queued: false });
                ix
            }
            None => self.add(Item::User { text, images, at }),
        }
    }

    /// A notice is a user turn the user didn't type: the answers to an `ask_user` form read
    /// as the user's words (a bubble, or a steer inside a running work); a job or a
    /// sub-agent ending, and what the host says, are a line (a step of a running work).
    fn push_notice(&mut self, notice: &Notice, at: i64) -> usize {
        let text = match notice {
            Notice::Answer { questions, answers, .. } => {
                let answers = answers.as_ref().map(|a| a.iter().map(Vec::as_slice));
                return self.said(answer_markdown(questions.iter().map(|q| q.text.as_str()), answers), vec![], at);
            }
            Notice::Child { thread_id, name, asking, failed, .. } => {
                let what = match (asking, failed) {
                    (true, _) => "is asking the user",
                    (false, true) => "stopped without a reply",
                    (false, false) => "finished",
                };
                let (thread_id, text) = (thread_id.clone(), format!("sub-agent \"{name}\" {what}"));
                return match self.open_work() {
                    Some(ix) => {
                        self.work_mut(ix).steps.push(Step::Child { thread_id, text });
                        ix
                    }
                    None => self.add(Item::Child { thread_id, text }),
                };
            }
            // `job cargo check · exit 1 · 12s`
            Notice::Job { command, exit_code, duration_ms, .. } => {
                format!("job {} · exit {exit_code} · {}", command.lines().next().unwrap_or(""), crate::thread::duration(*duration_ms))
            }
            Notice::Info { text } => text.clone(),
        };
        match self.open_work() {
            Some(ix) => {
                self.work_mut(ix).steps.push(Step::Notice(text));
                ix
            }
            None => self.add(Item::Notice(text)),
        }
    }

    /// A tool's result goes to its call, in the work in progress.
    fn push_result(&mut self, r: &transcript::ToolResult, at: i64) -> Option<usize> {
        let result = ToolResult { text: text_of(&r.content), is_error: r.is_error, images: images_of(&r.content) };
        let ix = self.open_work()?;
        let w = self.work_mut(ix);
        let tool = w.steps.iter_mut().rev().find_map(|s| match s {
            Step::Tool(t) if t.id == r.call_id => Some(t),
            _ => None,
        })?;
        tool.ended = Some(at);
        if r.duration_ms > 0 {
            tool.started = Some(at - r.duration_ms);
        }
        let spawned = tool
            .spawned
            .clone()
            .zip(spawned_id(&result))
            .map(|(name, thread_id)| Step::Child { thread_id, text: format!("sub-agent \"{name}\" started") });
        tool.result = Some(result);
        w.steps.extend(spawned);
        Some(ix)
    }

    /// `at`: when the reply ended. `miss`: the cache-miss notice it comes with.
    fn push_assistant(&mut self, a: &Assistant, at: i64, miss: Option<String>) -> usize {
        let ix = self.open_work().unwrap_or_else(|| self.start_work(at - a.duration_ms));
        let w = self.work_mut(ix);
        w.model = Some(a.model.clone());
        w.effort = a.effort.clone();
        let calls_tools = a.content.iter().any(|b| matches!(b, Block::ToolCall { .. }));
        let mut reply = vec![];
        for b in &a.content {
            match step_of(b) {
                Some(Step::Text(t)) if !calls_tools => reply.push(t),
                // The call starts when the message ends (its result may refine that).
                Some(Step::Tool(t)) => w.steps.push(Step::Tool(ToolCall { started: Some(at), ..t })),
                Some(step) => w.steps.push(step),
                None => {}
            }
        }
        let error = a.error.clone().or(match a.stop {
            Stop::Aborted => Some("aborted".into()),
            Stop::Error => Some("the request failed".into()),
            _ => None,
        });
        // A failed request's miss isn't shown; it still moved the baseline.
        let miss = miss.filter(|_| error.is_none());
        // Misses go in the work, for the log; one keeps a turn's otherwise empty work.
        w.steps.extend(miss.map(Step::CacheMiss));
        if calls_tools && error.is_none() {
            return ix;
        }
        // The turn is over: close the work (drop it if nothing happened) and add the reply.
        w.ended = Some(at);
        if w.steps.is_empty() {
            self.items.remove(ix);
            self.ids.remove(ix);
        }
        if !reply.is_empty() {
            self.add(Item::Assistant(reply.join("\n")));
        }
        if let Some(e) = error {
            self.add(Item::Error(e));
        }
        ix.min(self.items.len().saturating_sub(1))
    }

    fn add(&mut self, item: Item) -> usize {
        self.items.push(item);
        self.ids.push(self.current.clone());
        self.items.len() - 1
    }

    /// The id of the entry item `ix` came from.
    pub fn entry_id(&self, ix: usize) -> Option<&str> {
        self.ids.get(ix).map(String::as_str)
    }

    /// Index of the last item if it is a `Work` still in progress.
    pub fn open_work(&self) -> Option<usize> {
        match self.items.last() {
            Some(Item::Work(w)) if w.ended.is_none() => Some(self.items.len() - 1),
            _ => None,
        }
    }

    /// `event` is the model starting a message with no work open: the first one of a new
    /// work, which `push_assistant` starts once the message is persisted.
    pub fn begins_work(&self, event: &Event) -> bool {
        self.open_work().is_none() && *event == Event::MessageStart
    }

    /// When the user prompted the run whose work starts now: their last message, as long as
    /// only lines came since. Anything else after it is what that message's run made, so
    /// this run is another, which a notice woke (a job or a sub-agent ending), or the same
    /// one going on after a compaction (`cut`): neither started at the message.
    pub fn prompted_at(&self) -> Option<i64> {
        let prompted = self.items.iter().rev().find_map(|i| match i {
            Item::User { at, .. } => Some(Some(*at).filter(|a| *a > 0)),
            Item::Notice(_) | Item::Child { .. } => None,
            Item::Work(_) | Item::Assistant(_) | Item::Error(_) | Item::Compaction => Some(None),
        });
        prompted.flatten()
    }

    /// Start a `Work` at the message that prompted it (`prompted_at`), else at `at`, the
    /// start of the model's first message in it.
    fn start_work(&mut self, at: i64) -> usize {
        self.add(Item::Work(Work { started: self.prompted_at().unwrap_or(at), ..Work::default() }))
    }

    fn work_mut(&mut self, ix: usize) -> &mut Work {
        match &mut self.items[ix] {
            Item::Work(w) => w,
            _ => unreachable!("not a work item"),
        }
    }
}

/// A content block of an assistant message as a work step. The images a text links to
/// become `blob:` links (`markdown::link_blobs`), for markdown to draw.
fn step_of(b: &Block) -> Option<Step> {
    Some(match b {
        Block::Text { text, blobs } if blobs.is_empty() => Step::Text(text.clone()),
        Block::Text { text, blobs } => Step::Text(markdown::link_blobs(text, blobs)),
        Block::Thinking { redacted: true, .. } => Step::Thinking("(redacted)".into()),
        // Reasoning the provider keeps to itself leaves a block without text.
        Block::Thinking { text, .. } if text.is_empty() => return None,
        Block::Thinking { text, .. } => Step::Thinking(text.clone()),
        Block::ToolCall { id, name, input } => Step::Tool(tool_call(id, name, input)),
        Block::Image { .. } => return None,
    })
}

fn tool_call(id: &str, name: &str, input: &Value) -> ToolCall {
    // `ask_user`'s arguments are all nested: its first question stands for the call.
    let primary = match name {
        "ask_user" => args_primary(input.pointer("/questions/0")),
        _ => args_primary(Some(input)),
    };
    let arg = |k: &str| input.get(k).and_then(Value::as_str);
    ToolCall {
        id: id.to_owned(),
        name: name.to_owned(),
        primary,
        args: args_multiline(Some(input)),
        timeout: input.get("timeout").and_then(Value::as_f64),
        spawned: (name == "dirigent" && arg("action") == Some("spawn")).then(|| arg("name").unwrap_or("untitled").to_owned()),
        ..Default::default()
    }
}

/// The question in bold, the answer under it (a list when there are several, so a typed
/// "Kiwi, sliced" doesn't read as two picks), per question; or the one-line dismissal.
/// Shared by the persisted entry and the echo sent ahead of it, so the two look alike.
fn answer_markdown<'a>(questions: impl Iterator<Item = &'a str>, answers: Option<impl Iterator<Item = &'a [String]>>) -> String {
    let Some(mut answers) = answers else { return "Dismissed the questions.".into() };
    questions
        .map(|text| {
            let picked = answers.next().unwrap_or(&[]);
            match picked {
                [] => format!("**{text}**\\\n_no answer_"),
                // A backslash before the newline is markdown's hard break.
                [one] => format!("**{text}**\\\n{one}"),
                many => format!("**{text}**\n{}", many.iter().map(|a| format!("- {a}")).collect::<Vec<_>>().join("\n")),
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// An ask as a draft reply for the composer (the form's `Insert into composer`): each question quoted,
/// the answer so far under it (after a blank line, or markdown folds it into the quote).
/// Returns the text and where the cursor goes: the first missing answer, else the end.
pub fn draft_reply(questions: &[dirigent_protocol::Question], answers: &[Vec<String>]) -> (String, usize) {
    let mut text = String::new();
    let mut cursor = None;
    for (i, q) in questions.iter().enumerate() {
        if i > 0 {
            text.push_str("\n\n");
        }
        let quoted: Vec<String> = q.text.lines().map(|l| format!("> {l}").trim_end().to_owned()).collect();
        text.push_str(&quoted.join("\n"));
        text.push_str("\n\n");
        let answer = answers.get(i).map(|a| a.join(", ")).unwrap_or_default();
        if answer.is_empty() {
            cursor.get_or_insert(text.len());
        }
        text.push_str(&answer);
    }
    let end = text.len();
    (text, cursor.unwrap_or(end))
}

/// The new thread's id in a successful `dirigent` spawn's result (`Spawned sub-agent thread <id>. …`).
fn spawned_id(result: &ToolResult) -> Option<String> {
    if result.is_error {
        return None;
    }
    let rest = result.text.strip_prefix("Spawned sub-agent thread ")?;
    Some(rest.split('.').next()?.to_owned())
}

/// A user message's text without the `[image-N]: <path>` lines its host appended to tell
/// the model which file each attached image is (`dirigent_agent`'s `Session::attach`), and
/// the `N`s of those lines: one per image of the message, in order. Empty for a message
/// without images, or one sent before hosts saved them. The agent's notes for the model
/// come after them when it resized, converted or dropped an image (`[Image: original …]`);
/// those go too.
pub fn split_image_refs(text: &str) -> (&str, Vec<u32>) {
    let mut end = text.len();
    while let Some(at) = text[..end].rfind("\n\n") {
        let para = &text[at + 2..end];
        let ns: Option<Vec<u32>> = para.lines().map(|l| l.strip_prefix("[image-")?.split_once("]: ")?.0.parse().ok()).collect();
        match ns {
            Some(ns) if !ns.is_empty() => return (&text[..at], ns),
            _ if para.lines().all(|l| l.starts_with("[Image")) => end = at,
            _ => break,
        }
    }
    (text, vec![])
}

/// Blob ids of the images in a message's content, in order.
fn images_of(content: &[Block]) -> Vec<String> {
    let blobs = content.iter().filter_map(|b| match b {
        Block::Image { blob, .. } => Some(blob.clone()),
        _ => None,
    });
    blobs.collect()
}

/// Blob ids of the `blob:<id>` links in markdown text (a reply showing a local image, see
/// `step_of`). Ids are sha256 hex, so the scan stops at the first other byte.
pub fn blob_links(text: &str) -> impl Iterator<Item = &str> {
    text.match_indices("](blob:").filter_map(|(i, m)| {
        let rest = &text[i + m.len()..];
        let n = rest.bytes().take_while(u8::is_ascii_hexdigit).count();
        (n == 64).then(|| &rest[..n])
    })
}

/// Every blob the timeline shows: user attachments (steers' too), images its replies and
/// interim prose link to, and the images in the results of the tool calls that are open in
/// the log (`expanded`: work item and step index). Not those of the closed ones: an agent
/// using the computer takes a screenshot every few seconds, and nobody sees them there.
pub fn blobs_shown(items: &[Item], expanded: &HashSet<(usize, usize)>) -> Vec<String> {
    let mut out = vec![];
    for (i, item) in items.iter().enumerate() {
        match item {
            Item::User { images, .. } => out.extend(images.iter().cloned()),
            Item::Assistant(text) => out.extend(blob_links(text).map(str::to_owned)),
            Item::Work(w) => {
                for (j, step) in w.steps.iter().enumerate() {
                    match step {
                        Step::Text(text) => out.extend(blob_links(text).map(str::to_owned)),
                        Step::Tool(t) if expanded.contains(&(i, j)) => out.extend(t.result.iter().flat_map(|r| r.images.iter().cloned())),
                        Step::Steer { images, .. } => out.extend(images.iter().cloned()),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    out
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
/// follow-up echoes are replaced by the agent's own queues on every `Event::Queue`. Answers
/// to `ask_user` are steers too, but the queues list only what the user typed, so they echo
/// in `answers` (by ask id) until their `Notice::Answer` entry lands (`confirm_answer`).
///
/// Text streams in word by word but is shown a line at a time (`markdown::committed`), so
/// a delta only counts as a visible change once it completes a line.
#[derive(Debug, Default, Clone)]
pub struct Streaming {
    pub steps: Vec<Step>,
    /// (tool call id, started at unix ms)
    pub running_tools: Vec<(String, i64)>,
    pub echo: Vec<(String, PromptMode)>,
    /// (ask id, the answers as `answer_markdown` renders them)
    pub answers: Vec<(String, String)>,
}

impl Streaming {
    /// The user answered an ask: show it as a pending steer until the model has it.
    pub fn echo_answer(&mut self, ask_id: &str, questions: &[dirigent_protocol::Question], answers: Option<&[Vec<String>]>) {
        let text = answer_markdown(questions.iter().map(|q| q.text.as_str()), answers.map(|a| a.iter().map(|v| v.as_slice())));
        self.answers.push((ask_id.to_owned(), text));
    }

    /// The entry of the answers to ask `id` landed: the echo goes.
    pub fn confirm_answer(&mut self, id: &str) {
        self.answers.retain(|(a, _)| a != id);
    }

    /// A persisted user message started a run: drop the oldest prompt echo. Without one,
    /// the message was sent as a steer or follow-up to a model that turned out idle, and
    /// its host prompted it instead: that echo goes, found by its
    /// text (the host's has the image links the echo lacks, `split_image_refs`).
    pub fn confirm_prompt(&mut self, text: &str) {
        let i = self
            .echo
            .iter()
            .position(|(_, m)| *m == PromptMode::Prompt)
            .or_else(|| self.echo.iter().position(|(t, _)| split_image_refs(text).0 == t));
        if let Some(i) = i {
            self.echo.remove(i);
        }
    }

    /// Queued steers, oldest first.
    pub fn queued_steers(&self) -> impl Iterator<Item = &str> {
        self.echo.iter().filter_map(|(t, m)| (*m == PromptMode::Steer).then_some(t.as_str()))
    }

    /// What the run in progress adds to its work's persisted steps: the message streaming
    /// right now (text only up to its last completed line), then the steers still waiting
    /// (typed, or answers on their way) as queued steps. Those go last, where they will
    /// land: the model gets a steer after the streaming message and its tool calls.
    pub fn live_steps(&self) -> impl Iterator<Item = Step> {
        let streamed = self.steps.iter().filter_map(|s| match s {
            Step::Text(x) => x.contains('\n').then(|| Step::Text(markdown::committed(x).to_owned())),
            Step::Thinking(x) => x.contains('\n').then(|| Step::Thinking(markdown::committed(x).to_owned())),
            tool => Some(tool.clone()),
        });
        let queued = self.queued_steers().chain(self.answers.iter().map(|(_, text)| text.as_str()));
        streamed.chain(queued.map(|text| Step::Steer { text: text.to_owned(), images: vec![], queued: true }))
    }

    /// Apply one event of the thread. Returns true if anything visible changed.
    pub fn apply(&mut self, event: &Event, now: i64) -> bool {
        match event {
            Event::MessageStart => self.steps.clear(),
            Event::Text { index, delta } => return self.append(*index, delta, Step::Text),
            Event::Thinking { index, delta } => return self.append(*index, delta, Step::Thinking),
            Event::ToolCall { index, id, name, input } => self.set(*index, Step::Tool(tool_call(id, name, input))),
            Event::ToolStart { id } => self.running_tools.push((id.clone(), now)),
            Event::ToolEnd { id } => self.running_tools.retain(|(i, _)| i != id),
            // The agent's queues are the truth for steers and follow-ups (it says them when a
            // message is queued or cancelled, and before it hands one to the model). A prompt
            // it had to queue (sent as the run started) shows up here as a follow-up, so its
            // prompt echo goes too (the agent's text has the image links the echo lacks:
            // `split_image_refs`). Its texts are kept whole: a steer is cancelled by its text.
            Event::Queue { steering, follow_up } => {
                self.echo.retain(|(t, m)| *m == PromptMode::Prompt && !follow_up.iter().any(|f| split_image_refs(f).0 == t));
                self.echo.extend(steering.iter().map(|t| (t.clone(), PromptMode::Steer)));
                self.echo.extend(follow_up.iter().map(|t| (t.clone(), PromptMode::FollowUp)));
            }
            // An abort strands whatever was still queued.
            Event::RunEnd => self.settle(),
            _ => return false,
        }
        true
    }

    /// The run is over (`RunEnd`, or its host lists the thread idle without one):
    /// nothing is streaming, running or queued any more.
    pub fn settle(&mut self) {
        self.steps.clear();
        self.running_tools.clear();
        self.echo.clear();
        self.answers.clear();
    }

    pub fn running_since(&self, id: &str) -> Option<i64> {
        self.running_tools.iter().find(|(i, _)| i == id).map(|(_, t)| *t)
    }

    /// Returns true when the delta completed a line (the only time the view changes).
    fn append(&mut self, idx: usize, delta: &str, make: impl FnOnce(String) -> Step) -> bool {
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
    use dirigent_protocol::transcript::{Api, Cost, Usage};
    use serde_json::json;

    fn entry(at: i64, body: Body) -> Entry {
        Entry { seq: 0, id: String::new(), parent_id: None, at, body }
    }

    fn user(at: i64, text: &str) -> Entry {
        entry(at, Body::User { content: vec![Block::text(text)] })
    }

    /// A reply of model `m` that ended at `at`.
    fn assistant(at: i64, stop: Stop, content: Vec<Block>) -> Entry {
        let a = Assistant {
            content,
            model: "m".into(),
            provider: "anthropic".into(),
            api: Api::Anthropic,
            effort: Some("high".into()),
            usage: Usage::default(),
            stop,
            error: None,
            duration_ms: 0,
        };
        entry(at, Body::Assistant(a))
    }

    fn call(id: &str, name: &str, input: Value) -> Block {
        Block::ToolCall { id: id.into(), name: name.into(), input }
    }

    fn result(at: i64, call_id: &str, content: Vec<Block>, is_error: bool) -> Entry {
        entry(
            at,
            Body::ToolResult(transcript::ToolResult { call_id: call_id.into(), tool: String::new(), content, is_error, duration_ms: 0 }),
        )
    }

    fn notice(at: i64, notice: Notice) -> Entry {
        entry(at, Body::Notice(notice))
    }

    fn thinking(text: &str) -> Block {
        Block::Thinking { text: text.into(), signature: None, redacted: false }
    }

    fn compaction() -> Entry {
        entry(
            0,
            Body::Compaction {
                summary: "..".into(),
                first_kept: String::new(),
                tokens_before: 0,
                model: "m".into(),
                usage: Usage::default(),
            },
        )
    }

    /// A run that never got its final message stops its clock at its last tool, not at now.
    #[test]
    fn dead_run_duration_stops_at_its_last_tool() {
        let tool = |started, ended| Step::Tool(ToolCall { started, ended, ..Default::default() });
        let w =
            Work { started: 1000, ended: None, steps: vec![tool(Some(1500), Some(2000)), tool(Some(4000), None)], ..Default::default() };
        assert_eq!(w.duration(), 3000);
        assert_eq!(Work { started: 1000, ..Default::default() }.duration(), 0);
        assert_eq!(Work { started: 1000, ended: Some(9000), steps: w.steps.clone(), ..Default::default() }.duration(), 8000);
    }

    #[test]
    fn turn_becomes_user_work_reply() {
        let mut bash = result(2000, "c1", vec![Block::text("a\nb")], false);
        if let Body::ToolResult(r) = &mut bash.body {
            r.duration_ms = 300;
        }
        let entries = [
            user(1000, "hi"),
            assistant(
                1500,
                Stop::ToolUse,
                vec![
                    thinking("hmm"),
                    thinking(""),
                    Block::text("looking"),
                    call("c1", "bash", json!({"command":"ls -la\nnext","timeout":60})),
                ],
            ),
            bash,
            assistant(3000, Stop::End, vec![Block::text("done")]),
        ];
        let t = Timeline::from_entries(&entries);
        assert_eq!(t.items.len(), 3);
        assert_eq!(t.items[0], Item::User { text: "hi".into(), images: vec![], at: 1000 });
        let Item::Work(w) = &t.items[1] else { panic!() };
        assert_eq!((w.model.as_deref(), w.effort.as_deref(), w.started, w.ended), (Some("m"), Some("high"), 1000, Some(3000)));
        assert_eq!(w.tool_count(), 1);
        // Thinking without text is not a step.
        assert_eq!(w.steps[0], Step::Thinking("hmm".into()));
        assert_eq!(w.steps[1], Step::Text("looking".into()));
        let Step::Tool(c) = &w.steps[2] else { panic!() };
        assert_eq!((c.primary.as_str(), c.timeout, c.started, c.ended), ("ls -la…", Some(60.), Some(1700), Some(2000)));
        assert_eq!(c.result.as_ref().map(|r| r.text.as_str()), Some("a\nb"));
        assert_eq!(t.items[2], Item::Assistant("done".into()));
    }

    /// A `dirigent` spawn's result adds a `started` child step after the call; a failed
    /// spawn doesn't.
    #[test]
    fn spawn_adds_a_child_step() {
        let spawn =
            |id: &str| assistant(1, Stop::ToolUse, vec![call(id, "dirigent", json!({"action":"spawn","name":"Review","task":"look"}))]);
        let entries = [
            spawn("c1"),
            result(2, "c1", vec![Block::text("Spawned sub-agent thread kid. You will get a message when it finishes.")], false),
            spawn("c2"),
            result(2, "c2", vec![Block::text("no project to spawn in")], true),
        ];
        let t = Timeline::from_entries(&entries);
        let Item::Work(w) = &t.items[0] else { panic!() };
        assert_eq!(w.steps.len(), 3);
        assert_eq!(w.steps[1], Step::Child { thread_id: "kid".into(), text: "sub-agent \"Review\" started".into() });
        assert!(matches!(&w.steps[2], Step::Tool(c) if c.spawned.as_deref() == Some("Review")));
    }

    #[test]
    fn steer_stays_inside_the_work() {
        let bash = |id: &str| call(id, "bash", json!({}));
        let steer = entry(
            4,
            Body::User { content: vec![Block::text("no, other way"), Block::Image { mime: "image/png".into(), blob: "abc".into() }] },
        );
        let entries = [
            user(1, "hi"),
            assistant(2, Stop::ToolUse, vec![Block::text("before"), bash("c1")]),
            result(3, "c1", vec![], false),
            steer,
            assistant(5, Stop::ToolUse, vec![Block::text("after"), bash("c2")]),
            result(6, "c2", vec![], false),
            assistant(7, Stop::End, vec![Block::text("done")]),
            user(8, "next prompt"),
        ];
        let mut t = Timeline::default();
        let changed: Vec<_> = entries.iter().map(|e| t.push(e)).collect();
        // One work for the whole run; the steer changed item 1, not a new one.
        assert_eq!(changed[3], Some(1));
        let Item::Work(w) = &t.items[1] else { panic!() };
        assert_eq!(w.steers().collect::<Vec<_>>(), vec![(2, "no, other way", &["abc".to_owned()][..], false)]);
        assert_eq!(blobs_shown(&t.items, &HashSet::new()), ["abc"]);
        assert_eq!(w.tool_count(), 2);
        assert_eq!(t.items[2], Item::Assistant("done".into()));
        assert_eq!(t.items[3], Item::User { text: "next prompt".into(), images: vec![], at: 8 });
        // Prose from before a steer is stale once the model has it, until it writes again.
        let mut w = w.clone();
        assert_eq!(w.latest_text(), Some("after"));
        w.steps.push(Step::Steer { text: "again".into(), images: vec![], queued: true });
        assert_eq!(w.latest_text(), Some("after"));
        w.steps.push(Step::Steer { text: "again".into(), images: vec![], queued: false });
        assert_eq!(w.latest_text(), None);
    }

    /// A run woken by a job starts a new work with the model's first message, and at that
    /// message, not back at the user's (whose run is over); the messages after it go into
    /// that work.
    #[test]
    fn model_writing_with_no_work_open_begins_a_work() {
        let job = Notice::Info { text: "job done".into() };
        let waiting = assistant(2, Stop::End, vec![thinking("hm"), Block::text("waiting on the job")]);
        let mut t = Timeline::from_entries(&[user(1, "go"), waiting, notice(300, job)]);
        assert!(t.begins_work(&Event::MessageStart));
        assert!(!t.begins_work(&Event::RunStart));
        assert_eq!(t.prompted_at(), None);
        let mut woken = assistant(400, Stop::ToolUse, vec![call("c", "bash", json!({}))]);
        if let Body::Assistant(a) = &mut woken.body {
            a.duration_ms = 50;
        }
        t.push(&woken);
        assert!(!t.begins_work(&Event::MessageStart));
        let started: Vec<i64> = t.items.iter().filter_map(|i| if let Item::Work(w) = i { Some(w.started) } else { None }).collect();
        assert_eq!(started, [1, 350]);
        // A line between the user's message and the model's first leaves the run the user's.
        let t = Timeline::from_entries(&[user(1, "go"), notice(2, Notice::Info { text: "moved".into() })]);
        assert_eq!(t.prompted_at(), Some(1));
    }

    /// A compaction in the middle of a run, done or failed, closes the work so far (which
    /// the list is told changed); the next one starts at the model's next message, not back
    /// at the prompt.
    #[test]
    fn compaction_splits_a_run_in_two_works() {
        let mut after = assistant(50, Stop::End, vec![thinking("so"), Block::text("done")]);
        if let Body::Assistant(a) = &mut after.body {
            a.duration_ms = 10;
        }
        let failed = entry(0, Body::CompactionFailed { error: "Compaction failed: 529".into() });
        for (cut, item) in [(compaction(), Item::Compaction), (failed, Item::Error("Compaction failed: 529".into()))] {
            let run = [user(1, "go"), assistant(20, Stop::ToolUse, vec![call("c", "bash", json!({}))]), result(25, "c", vec![], false)];
            let mut t = Timeline::from_entries(&run);
            assert_eq!(t.push(&Entry { at: 30, ..cut }), Some(1));
            t.push(&after);
            let works: Vec<_> =
                t.items.iter().filter_map(|i| if let Item::Work(w) = i { Some((w.started, w.ended)) } else { None }).collect();
            assert_eq!(works, [(1, Some(30)), (40, Some(50))]);
            assert_eq!(t.items[2], item);
        }
    }

    /// Queued steers ride below the message that is streaming, not above it.
    #[test]
    fn queued_steers_follow_the_streaming_message() {
        let s = Streaming {
            steps: vec![Step::Thinking("hm\nhalf a line".into()), Step::Text("no line yet".into())],
            echo: vec![("wait".into(), PromptMode::Steer), ("later".into(), PromptMode::FollowUp)],
            answers: vec![("ask".into(), "pg".into())],
            ..Default::default()
        };
        let steer = |text: &str| Step::Steer { text: text.into(), images: vec![], queued: true };
        assert_eq!(s.live_steps().collect::<Vec<_>>(), vec![Step::Thinking("hm\n".into()), steer("wait"), steer("pg")]);
    }

    #[test]
    fn the_queue_owns_steer_and_follow_up_echoes() {
        let queue = |steering: &[&str], follow_up: &[&str]| {
            let texts = |list: &[&str]| list.iter().map(|t| (*t).to_owned()).collect();
            Event::Queue { steering: texts(steering), follow_up: texts(follow_up) }
        };
        let echo = vec![("p".into(), PromptMode::Prompt), ("s1".into(), PromptMode::Steer), ("s2".into(), PromptMode::Steer)];
        let mut s = Streaming { echo, ..Streaming::default() };
        // s1 was delivered (or cancelled); s2 remains, in the agent's order.
        assert!(s.apply(&queue(&["s2"], &["f"]), 0));
        assert_eq!(s.echo, vec![("p".into(), PromptMode::Prompt), ("s2".into(), PromptMode::Steer), ("f".into(), PromptMode::FollowUp)]);
        assert_eq!(s.queued_steers().collect::<Vec<_>>(), vec!["s2"]);
        s.confirm_prompt("p");
        assert_eq!(s.echo.len(), 2);
        // A steer the host prompted instead (the model was idle) is claimed by its text.
        s.confirm_prompt("s2\n\n[image-1]: /a.png");
        assert_eq!(s.echo, vec![("f".into(), PromptMode::FollowUp)]);
        // A prompt the agent queued as a follow-up is no longer a prompt.
        s.echo.push(("late".into(), PromptMode::Prompt));
        s.apply(&queue(&[], &["late"]), 0);
        assert_eq!(s.echo, vec![("late".into(), PromptMode::FollowUp)]);
        // Also when the host linked its images to files.
        s.echo = vec![("see [image-1]".into(), PromptMode::Prompt)];
        s.apply(&queue(&[], &["see [image-1]\n\n[image-1]: /a.png"]), 0);
        assert_eq!(s.echo, vec![("see [image-1]\n\n[image-1]: /a.png".into(), PromptMode::FollowUp)]);
        s.apply(&Event::RunEnd, 0);
        assert!(s.echo.is_empty());
    }

    #[test]
    fn notices_are_answers_or_lines() {
        let q = |text: &str| dirigent_protocol::Question { text: text.into(), options: vec![], allow_custom: false, multi: false };
        let picks = |list: &[&str]| list.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>();
        let answer = notice(
            500,
            Notice::Answer {
                text: "raw".into(),
                ask_id: "a".into(),
                questions: vec![q("Db?"), q("Tests?"), q("Fruit?")],
                answers: Some(vec![picks(&["pg"]), vec![], picks(&["Apple", "Kiwi, sliced"])]),
            },
        );
        let job = notice(
            500,
            Notice::Job {
                text: "raw".into(),
                job_id: "j".into(),
                command: "cargo check\n".into(),
                exit_code: 1,
                duration_ms: 12000,
                log: "/l".into(),
            },
        );
        let asks = assistant(900, Stop::ToolUse, vec![call("c", "ask_user", json!({"questions":[{"text":"Db?"}]}))]);
        // Idle: the answer is a user turn whose time starts the next work; the job a notice.
        let t = Timeline::from_entries([&job, &answer, &asks]);
        assert_eq!(t.items[0], Item::Notice("job cargo check · exit 1 · 12s".into()));
        assert_eq!(
            t.items[1],
            Item::User {
                text: "**Db?**\\\npg\n\n**Tests?**\\\n_no answer_\n\n**Fruit?**\n- Apple\n- Kiwi, sliced".into(),
                images: vec![],
                at: 500
            }
        );
        let Item::Work(w) = &t.items[2] else { panic!() };
        assert_eq!(w.started, 500);
        let Step::Tool(c) = &w.steps[0] else { panic!() };
        assert_eq!(c.primary, "Db?");
        // Mid-run: both go inside the work.
        let t = Timeline::from_entries([&asks, &job, &answer]);
        let Item::Work(w) = &t.items[0] else { panic!() };
        assert_eq!(w.steps[1], Step::Notice("job cargo check · exit 1 · 12s".into()));
        assert!(matches!(&w.steps[2], Step::Steer { queued: false, .. }));
        // A sub-agent's end is a link to it when idle, a child step of the work mid-run.
        let child = |asking, failed| {
            notice(0, Notice::Child { text: "raw".into(), thread_id: "kid".into(), name: "Review".into(), asking, failed })
        };
        let t = Timeline::from_entries([&child(false, false)]);
        assert_eq!(t.items[0], Item::Child { thread_id: "kid".into(), text: "sub-agent \"Review\" finished".into() });
        let t = Timeline::from_entries([&asks, &child(false, true)]);
        let Item::Work(w) = &t.items[0] else { panic!() };
        assert_eq!(w.steps[1], Step::Child { thread_id: "kid".into(), text: "sub-agent \"Review\" stopped without a reply".into() });
        let dismissed = notice(0, Notice::Answer { text: "raw".into(), ask_id: "a".into(), questions: vec![], answers: None });
        assert_eq!(
            Timeline::from_entries([&dismissed]).items[0],
            Item::User { text: "Dismissed the questions.".into(), images: vec![], at: 0 }
        );
    }

    #[test]
    fn draft_reply_quotes_questions_and_lands_on_the_first_gap() {
        let q = |text: &str| dirigent_protocol::Question { text: text.into(), options: vec![], allow_custom: false, multi: false };
        let (text, cursor) =
            draft_reply(&[q("Db?"), q("Two\n\nlines?"), q("Fruit?")], &[vec!["pg".into()], vec![], vec!["Apple".into(), "Kiwi".into()]]);
        assert_eq!(text, "> Db?\n\npg\n\n> Two\n>\n> lines?\n\n\n\n> Fruit?\n\nApple, Kiwi");
        assert_eq!(&text[..cursor], "> Db?\n\npg\n\n> Two\n>\n> lines?\n\n");
        let (text, cursor) = draft_reply(&[q("Db?")], &[vec!["pg".into()]]);
        assert_eq!(cursor, text.len());
    }

    /// A reply that used `input` fresh, `read` cached and `write` newly cached prompt tokens.
    fn used(mut e: Entry, input: u64, read: u64, write: u64, output: u64, cost: Cost) -> Entry {
        if let Body::Assistant(a) = &mut e.body {
            a.usage = Usage { input, output, cache_read: read, cache_write: write, cost };
        }
        e
    }

    #[test]
    fn stats_sum_what_the_requests_used() {
        let reply = |input, read, write, output, cost| {
            used(assistant(0, Stop::End, vec![]), input, read, write, output, Cost { output: cost, ..Cost::default() })
        };
        let mut summary = compaction();
        if let Body::Compaction { usage, .. } = &mut summary.body {
            *usage = Usage { input: 100, output: 5, cost: Cost { input: 0.25, ..Cost::default() }, ..Usage::default() };
        }
        let entries =
            [user(0, "hi"), reply(2, 90, 8, 10, 0.5), result(0, "c", vec![], false), reply(0, 95, 5, 20, 0.25), summary, user(0, "more")];
        let s = Stats::of(&entries);
        assert_eq!((s.input, s.output, s.cached, s.cost), (300, 35, 185, 1.));
        assert_eq!((s.user_messages, s.assistant_messages, s.compactions), (2, 2, 1));
        assert_eq!(s.cached_pct(), 62);
    }

    /// Misses are counted against the previous request, reset by a compaction, hidden on
    /// failed requests, priced at the model's last read rate; they go in the work (for the
    /// log), never in the transcript.
    #[test]
    fn cache_misses() {
        // Anthropic-ish rates per token: input 5e-6, cache read 0.5e-6, cache write 6.25e-6.
        let msg = |at: i64, model: &str, input: u64, read: u64, write: u64, stop: Stop| {
            let content = if stop == Stop::ToolUse { vec![call("c", "bash", json!({}))] } else { vec![Block::text("ok")] };
            let cost =
                Cost { input: input as f64 * 5e-6, cache_read: read as f64 * 0.5e-6, cache_write: write as f64 * 6.25e-6, output: 0. };
            let mut e = used(assistant(at, stop, content), input, read, write, 10, cost);
            if let Body::Assistant(a) = &mut e.body {
                a.model = model.into();
            }
            e
        };
        let min = 60_000;
        let entries = [
            user(0, "go"),
            msg(0, "m", 100, 0, 50_000, Stop::ToolUse),
            result(0, "c", vec![], false),
            // Mid-run: 40k of the 50k cached came back as fresh input.
            msg(min, "m", 40_000, 10_000, 0, Stop::ToolUse),
            result(0, "c", vec![], false),
            // Full miss after 7 minutes, at the read rate the model last paid.
            msg(8 * min, "m", 0, 0, 60_000, Stop::End),
            user(0, "go"),
            // Small (under 20k and $0.10): counted, not shown.
            msg(9 * min, "m", 0, 50_000, 12_000, Stop::End),
            user(0, "go"),
            // A model switch, but aborted: not shown.
            msg(10 * min, "other", 62_000, 0, 0, Stop::Aborted),
            compaction(),
            user(0, "go"),
            // Nothing to compare with after the compaction.
            msg(11 * min, "m", 0, 0, 30_000, Stop::End),
            user(0, "go"),
            msg(12 * min, "m", 0, 0, 30_000, Stop::End),
        ];
        let t = Timeline::from_entries(&entries);
        let misses: Vec<_> = t
            .items
            .iter()
            .flat_map(|i| match i {
                Item::Work(w) => w.steps.iter().filter_map(|s| if let Step::CacheMiss(m) = s { Some(m.as_str()) } else { None }).collect(),
                _ => vec![],
            })
            .collect();
        assert_eq!(
            misses,
            [
                "cache miss: 40k tokens re-billed (~$0.18)",
                "cache miss after 7m idle: 50k tokens re-billed (~$0.29)",
                "cache miss: 30k tokens re-billed (~$0.17)",
            ]
        );
        // A turn-ending miss keeps its otherwise empty work, before the reply.
        let ix = t.items.iter().rposition(|i| matches!(i, Item::Work(_))).unwrap();
        let Item::Work(w) = &t.items[ix] else { unreachable!() };
        assert_eq!(w.steps, [Step::CacheMiss("cache miss: 30k tokens re-billed (~$0.17)".into())]);
        assert_eq!(t.items[ix + 1], Item::Assistant("ok".into()));
    }

    #[test]
    fn image_refs_split_off() {
        let text = "see [image-2] and [image-1]\n\nfoo\n\n[image-1]: /d/a.png\n[image-2]: <C:\\My Docs\\b.png>";
        assert_eq!(split_image_refs(text), ("see [image-2] and [image-1]\n\nfoo", vec![1, 2]));
        // A last paragraph that isn't all definitions is the user's.
        let typed = "x\n\n[image-1]: mine\nand more";
        assert_eq!(split_image_refs(typed), (typed, vec![]));
        // The agent's notes on a resized image come after the links.
        let resized = "see [image-1]\n\n[image-1]: /a.png\n\n[Image: original 4000x3000, displayed at 2000x1500.]";
        assert_eq!(split_image_refs(resized), ("see [image-1]", vec![1]));
        assert_eq!(split_image_refs("plain"), ("plain", vec![]));
    }

    #[test]
    fn images_read_and_shown() {
        let (id, old) = ("a".repeat(64), "b".repeat(64));
        let read = vec![Block::text("Read image file [image/png]"), Block::Image { mime: "image/png".into(), blob: "read1".into() }];
        // A link the host stored a blob for, and one that was written as a blob link.
        let shows = Block::Text {
            text: format!("Here it is:\n\n![home]( /s/home.png)\n![old](blob:{old})\n\nand not ![x](blob:short)"),
            blobs: [("/s/home.png".to_owned(), id.clone())].into(),
        };
        let entries = [
            user(1, "look"),
            assistant(2, Stop::ToolUse, vec![call("c1", "read", json!({"path":"shot.png"}))]),
            result(3, "c1", read, false),
            assistant(4, Stop::End, vec![shows]),
        ];
        let t = Timeline::from_entries(&entries);
        let Item::Work(w) = &t.items[1] else { panic!() };
        let Step::Tool(c) = &w.steps[0] else { panic!() };
        assert_eq!(c.result.as_ref().unwrap().images, vec!["read1".to_owned()]);
        // The image the model read shows once its tool call is open in the log.
        assert_eq!(blobs_shown(&t.items, &HashSet::new()), vec![id.clone(), old.clone()]);
        assert_eq!(blobs_shown(&t.items, &HashSet::from([(1, 0)])), vec!["read1".to_owned(), id.clone(), old.clone()]);
    }

    #[test]
    fn plain_reply_has_no_work() {
        let entries = [
            user(1, "hi"),
            assistant(2, Stop::End, vec![Block::text("yo")]),
            compaction(),
            entry(0, Body::CompactionFailed { error: "Compaction failed: 529".into() }),
            entry(0, Body::Store { values: Default::default() }),
            entry(0, Body::Unknown),
        ];
        let t = Timeline::from_entries(&entries);
        assert_eq!(t.items[1], Item::Assistant("yo".into()));
        assert_eq!(t.items[2], Item::Compaction);
        assert_eq!(t.items[3], Item::Error("Compaction failed: 529".into()));
        assert!(matches!(&t.items[4], Item::Notice(_)));
        assert_eq!(t.items.len(), 5);
    }

    /// Items keep the id of the entry they came from, through a work dropped for being
    /// empty (an assistant message that only replied).
    #[test]
    fn items_carry_their_entry_id() {
        let id = |id: &str, e: Entry| Entry { id: id.into(), ..e };
        let entries = [id("u1", user(1, "hi")), id("a1", assistant(2, Stop::End, vec![Block::text("hello")])), id("c1", compaction())];
        let t = Timeline::from_entries(&entries);
        assert_eq!(t.items.len(), 3);
        assert_eq!((t.entry_id(0), t.entry_id(1), t.entry_id(2), t.entry_id(3)), (Some("u1"), Some("a1"), Some("c1"), None));
    }

    #[test]
    fn streaming_accumulates_deltas() {
        let mut s = Streaming::default();
        assert!(s.apply(&Event::MessageStart, 0));
        let delta = |d: &str| Event::Text { index: 0, delta: d.into() };
        // Only a delta that completes a line is a visible change.
        assert!(!s.apply(&delta("he"), 0));
        assert!(s.apply(&delta("llo\n"), 0));
        assert!(!s.apply(&delta(""), 0));
        s.steps.clear();
        s.apply(&delta("he"), 0);
        s.apply(&delta("llo"), 0);
        s.apply(&Event::Thinking { index: 1, delta: "t".into() }, 0);
        assert!(s.apply(&Event::ToolCall { index: 2, id: "c".into(), name: "bash".into(), input: json!({"command":"ls"}) }, 0));
        assert_eq!(s.steps[..2], [Step::Text("hello".into()), Step::Thinking("t".into())]);
        assert!(matches!(&s.steps[2], Step::Tool(c) if c.primary == "ls"));
        s.apply(&Event::ToolStart { id: "c".into() }, 42);
        assert_eq!(s.running_since("c"), Some(42));
        s.apply(&Event::ToolEnd { id: "c".into() }, 50);
        assert!(s.running_tools.is_empty());
        s.apply(&Event::RunEnd, 0);
        assert!(s.steps.is_empty());
    }
}
