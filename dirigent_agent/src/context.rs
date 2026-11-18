use crate::Llm;
use crate::proxy::{Message, Model, Part, Request};
use anyhow::{Result, anyhow, bail};
use dirigent_protocol::transcript::{Assistant, Block, Body, Entry, Stop, Usage, text_of};
use std::collections::{BTreeSet, HashSet};
use tokio_util::sync::CancellationToken;

const IMAGE_CHARS: usize = 4800;
const SUMMARY_RESULT_CHARS: usize = 2000;

const SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
const SUMMARY_SUFFIX: &str = "\n</summary>";

pub fn window(branch: &[Entry]) -> &[Entry] {
    let Some(at) = branch.iter().rposition(|e| matches!(e.body, Body::Compaction { .. })) else { return branch };
    let Body::Compaction { first_kept, .. } = &branch[at].body else { unreachable!() };
    &branch[branch[..at].iter().position(|e| e.id == *first_kept).unwrap_or(at)..]
}

fn visible(branch: &[Entry]) -> (Option<&str>, Vec<&Entry>) {
    let in_context = |e: &&Entry| matches!(e.body, Body::User { .. } | Body::Assistant(_) | Body::ToolResult(_) | Body::Notice(_));
    let window = window(branch);
    let summary =
        window.iter().rev().find_map(|e| if let Body::Compaction { summary, .. } = &e.body { Some(summary.as_str()) } else { None });
    (summary, window.iter().filter(in_context).collect())
}

pub fn messages(branch: &[Entry], model: &str, images: bool, blob: &mut dyn FnMut(&str) -> Option<(String, String)>) -> Vec<Message> {
    let (summary, entries) = visible(branch);
    let mut parts = |content: &[Block]| -> Vec<Part> {
        let mut out = vec![];
        for block in content {
            match block {
                Block::Text { text, .. } if !text.is_empty() => out.push(Part::Text(text.clone())),
                Block::Image { blob: id, .. } if images => match blob(id) {
                    Some((mime, data)) => out.push(Part::Image { mime, data }),
                    None => out.push(Part::Text("[Image omitted: it is no longer stored.]".into())),
                },
                Block::Image { .. } => out.push(Part::Text("[Image omitted: the current model does not take images.]".into())),
                _ => {}
            }
        }
        out
    };
    let mut out = vec![];
    if let Some(summary) = summary {
        out.push(Message::User(vec![Part::Text(format!("{SUMMARY_PREFIX}{summary}{SUMMARY_SUFFIX}"))]));
    }
    // MUST KILL: rename: whole -> not_failed_or_aborted, so dropping a failed reply and its calls reads from the name
    let whole = |a: &Assistant| !matches!(a.stop, Stop::Error | Stop::Aborted);
    let called: HashSet<&str> = entries
        .iter()
        .filter_map(|e| if let Body::Assistant(a) = &e.body { Some(a) } else { None })
        .filter(|a| whole(a))
        .flat_map(|a| a.content.iter().filter_map(|b| if let Block::ToolCall { id, .. } = b { Some(id.as_str()) } else { None }))
        .collect();
    let mut open: Vec<&str> = vec![];
    let close = |open: &mut Vec<&str>, out: &mut Vec<Message>| {
        for call_id in open.drain(..) {
            let content = vec![Part::Text("No result: the run was interrupted before the tool finished.".into())];
            out.push(Message::ToolResult { call_id: call_id.to_owned(), content, is_error: true });
        }
    };
    for entry in entries {
        match &entry.body {
            Body::ToolResult(r) if called.contains(r.call_id.as_str()) => {
                open.retain(|id| *id != r.call_id);
                let content = match parts(&r.content) {
                    content if content.is_empty() => vec![Part::Text("(no output)".into())],
                    content => content,
                };
                out.push(Message::ToolResult { call_id: r.call_id.clone(), content, is_error: r.is_error });
                continue;
            }
            Body::ToolResult(_) => continue,
            _ => close(&mut open, &mut out),
        }
        match &entry.body {
            Body::User { content } => out.push(Message::User(parts(content))),
            Body::Notice(notice) => out.push(Message::User(vec![Part::Text(notice.text().to_owned())])),
            Body::Assistant(a) if whole(a) && !a.content.is_empty() => {
                open.extend(a.content.iter().filter_map(|b| if let Block::ToolCall { id, .. } = b { Some(id.as_str()) } else { None }));
                let own = |b: &&Block| a.model == model || !matches!(b, Block::Thinking { .. });
                out.push(Message::Assistant { content: a.content.iter().filter(own).cloned().collect(), api: a.api });
            }
            _ => {}
        }
    }
    close(&mut open, &mut out);
    out
}

fn estimate(entry: &Entry) -> u64 {
    let blocks = |content: &[Block]| -> usize {
        let size = |b: &Block| match b {
            Block::Text { text, .. } | Block::Thinking { text, .. } => text.len(),
            Block::ToolCall { name, input, .. } => name.len() + input.to_string().len(),
            Block::Image { .. } => IMAGE_CHARS,
        };
        content.iter().map(size).sum()
    };
    let chars = match &entry.body {
        Body::User { content } => blocks(content),
        Body::Assistant(a) => blocks(&a.content),
        Body::ToolResult(r) => blocks(&r.content),
        Body::Notice(n) => n.text().len(),
        _ => 0,
    };
    chars.div_ceil(4) as u64
}

fn usage(entry: &Entry) -> Option<&Usage> {
    match &entry.body {
        Body::Assistant(a) if !matches!(a.stop, Stop::Error | Stop::Aborted) && a.usage.prompt() + a.usage.output > 0 => Some(&a.usage),
        _ => None,
    }
}

pub fn tokens(branch: &[Entry]) -> u64 {
    let (summary, entries) = visible(branch);
    let after_compaction = branch.iter().rposition(|e| matches!(e.body, Body::Compaction { .. })).map_or(0, |at| at + 1);
    let counted = branch[after_compaction..].iter().rev().find_map(|e| usage(e).map(|u| (e.seq, &e.id, u)));
    match counted {
        Some((_, id, usage)) => {
            let since = entries.iter().skip_while(|e| e.id != *id).skip(1);
            usage.prompt() + usage.output + since.map(|e| estimate(e)).sum::<u64>()
        }
        None => summary.map_or(0, |s| s.len().div_ceil(4) as u64) + entries.iter().map(|e| estimate(e)).sum::<u64>(),
    }
}

#[derive(Debug, PartialEq)]
pub struct Plan<'a> {
    pub first_kept: String,
    older: Vec<&'a Entry>,
    turn_prefix: Vec<&'a Entry>,
    previous: Option<&'a str>,
    pub tokens_before: u64,
}

impl<'a> Plan<'a> {
    // MUST KILL: extract: Plan::new's cut search into a fn that never cuts between a tool call and its result
    pub fn new(branch: &'a [Entry], keep_recent: u64) -> Option<Self> {
        if matches!(branch.last()?.body, Body::Compaction { .. }) {
            return None;
        }
        let (previous, entries) = visible(branch);
        let starts_turn = |e: &Entry| matches!(e.body, Body::User { .. } | Body::Notice(_));
        let cut_points: Vec<usize> = (0..entries.len()).filter(|i| !matches!(entries[*i].body, Body::ToolResult(_))).collect();
        let mut cut = *cut_points.first()?;
        let mut kept = 0;
        for i in (0..entries.len()).rev() {
            kept += estimate(entries[i]);
            if kept >= keep_recent {
                cut = cut_points.iter().copied().find(|c| *c >= i).unwrap_or(*cut_points.last()?);
                break;
            }
        }
        let turn_start = if starts_turn(entries[cut]) { None } else { (0..cut).rev().find(|i| starts_turn(entries[*i])) };
        let older = entries[..turn_start.unwrap_or(cut)].to_vec();
        let turn_prefix = turn_start.map_or(vec![], |start| entries[start..cut].to_vec());
        if older.is_empty() && turn_prefix.is_empty() {
            return None;
        }
        Some(Plan { first_kept: entries[cut].id.clone(), older, turn_prefix, previous, tokens_before: tokens(branch) })
    }

    pub async fn summarize(
        &self,
        llm: &dyn Llm,
        model: &Model,
        effort: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<(String, Usage)> {
        let (previous, mut files) = match self.previous {
            Some(previous) => Files::split(previous),
            None => (String::new(), Files::default()),
        };
        self.older.iter().chain(&self.turn_prefix).for_each(|e| files.note(e));
        let mut usage = Usage::default();
        let history = if self.older.is_empty() {
            if previous.is_empty() { "No prior history.".to_owned() } else { previous }
        } else {
            let mut prompt = format!("<conversation>\n{}\n</conversation>\n\n", serialize(&self.older));
            if previous.is_empty() {
                prompt.push_str(SUMMARY);
            } else {
                prompt.push_str(&format!("<previous-summary>\n{previous}\n</previous-summary>\n\n{UPDATE}"));
            }
            ask(llm, model, &prompt, effort, cancel, &mut usage).await?
        };
        let summary = if self.turn_prefix.is_empty() {
            history
        } else {
            let prompt = format!("# Conversation\n{}\n\n# Instructions\n{TURN_PREFIX}", serialize(&self.turn_prefix));
            let prefix = ask(llm, model, &prompt, effort, cancel, &mut usage).await?;
            format!("{history}\n\n---\n\n**Turn Context (split turn):**\n\n{prefix}")
        };
        Ok((format!("{summary}{}", files.tags()), usage))
    }
}

async fn ask(
    llm: &dyn Llm,
    model: &Model,
    prompt: &str,
    effort: Option<&str>,
    cancel: &CancellationToken,
    usage: &mut Usage,
) -> Result<String> {
    let reply = complete(llm, model, SUMMARIZER, prompt, effort, cancel).await?;
    add(usage, &reply.usage);
    Ok(text_of(&reply.content))
}

fn add(total: &mut Usage, more: &Usage) {
    total.input += more.input;
    total.output += more.output;
    total.cache_read += more.cache_read;
    total.cache_write += more.cache_write;
    total.cost.input += more.cost.input;
    total.cost.output += more.cost.output;
    total.cost.cache_read += more.cost.cache_read;
    total.cost.cache_write += more.cost.cache_write;
}

/// One reply to `text`, without tools: a summary, a title. An error when the request
/// failed or its reply was cut off at the output limit: half a summary is none.
pub async fn complete(
    llm: &dyn Llm,
    model: &Model,
    system: &str,
    text: &str,
    effort: Option<&str>,
    cancel: &CancellationToken,
) -> Result<Assistant> {
    let messages = [Message::User(vec![Part::Text(text.to_owned())])];
    let effort = effort.filter(|e| model.efforts.iter().any(|m| m.as_str() == *e));
    let request = Request { model, system, messages: &messages, tools: &[], effort, max_output: None, cache_key: "" };
    let reply = crate::run::with_retries(cancel, |_, _, _| {}, || async { llm.stream(&request, &mut |_| {}, cancel).await }).await;
    match reply.message.stop {
        Stop::End => Ok(reply.message),
        Stop::MaxTokens => bail!("the reply hit the output limit and is incomplete"),
        Stop::ToolUse => bail!("the model tried to call a tool"),
        Stop::Aborted => bail!("aborted"),
        Stop::Error => Err(anyhow!(reply.message.error.unwrap_or_else(|| "the request failed".into()))),
    }
}

#[derive(Debug, Default, PartialEq)]
struct Files {
    read: BTreeSet<String>,
    modified: BTreeSet<String>,
}

impl Files {
    fn split(summary: &str) -> (String, Files) {
        let mut files = Files::default();
        let mut text = summary.to_owned();
        for (tag, set) in [("read-files", &mut files.read), ("modified-files", &mut files.modified)] {
            let (open, close) = (format!("<{tag}>\n"), format!("\n</{tag}>"));
            let Some(start) = text.rfind(&open) else { continue };
            let Some(end) = text[start..].find(&close).map(|i| start + i) else { continue };
            set.extend(text[start + open.len()..end].lines().map(str::to_owned));
            text.replace_range(start..end + close.len(), "");
        }
        (text.trim_end().to_owned(), files)
    }

    fn note(&mut self, entry: &Entry) {
        let Body::Assistant(a) = &entry.body else { return };
        for block in &a.content {
            let Block::ToolCall { name, input, .. } = block else { continue };
            let Some(path) = input.get("path").and_then(|p| p.as_str()) else { continue };
            match name.as_str() {
                "read" => self.read.insert(path.to_owned()),
                "write" | "edit" => self.modified.insert(path.to_owned()),
                _ => false,
            };
        }
    }

    fn tags(&self) -> String {
        let read: Vec<&str> = self.read.difference(&self.modified).map(String::as_str).collect();
        let modified: Vec<&str> = self.modified.iter().map(String::as_str).collect();
        let mut out = String::new();
        for (tag, files) in [("read-files", read), ("modified-files", modified)] {
            if !files.is_empty() {
                out.push_str(&format!("\n\n<{tag}>\n{}\n</{tag}>", files.join("\n")));
            }
        }
        out
    }
}

fn serialize(entries: &[&Entry]) -> String {
    let mut parts = vec![];
    for entry in entries {
        match &entry.body {
            Body::User { content } => parts.push(format!("[User]: {}", text_of(content))),
            Body::Notice(notice) => parts.push(format!("[User]: {}", notice.text())),
            Body::Assistant(a) => {
                let text = text_of(&a.content);
                if !text.is_empty() {
                    parts.push(format!("[Assistant]: {text}"));
                }
                let calls: Vec<String> = a
                    .content
                    .iter()
                    .filter_map(|b| if let Block::ToolCall { name, input, .. } = b { Some((name, input)) } else { None })
                    .map(|(name, input)| {
                        let args: Vec<String> = input.as_object().into_iter().flatten().map(|(k, v)| format!("{k}={v}")).collect();
                        format!("{name}({})", args.join(", "))
                    })
                    .collect();
                if !calls.is_empty() {
                    parts.push(format!("[Assistant tool calls]: {}", calls.join("; ")));
                }
            }
            Body::ToolResult(r) => {
                let text = text_of(&r.content);
                let cut = match text.char_indices().nth(SUMMARY_RESULT_CHARS) {
                    Some((at, _)) => format!("{}\n\n[... {} more characters truncated]", &text[..at], text[at..].chars().count()),
                    None => text,
                };
                if !cut.is_empty() {
                    parts.push(format!("[Tool result]: {cut}"));
                }
            }
            _ => {}
        }
    }
    parts.join("\n\n")
}

const SUMMARIZER: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI \
assistant, then produce a structured summary following the exact format specified.

Do NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

const SUMMARY: &str = "The messages above are a conversation to summarize. Create a structured context checkpoint summary that another \
LLM will use to continue the work.

Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or \"(none)\" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or \"(none)\" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages.";

const UPDATE: &str = "The messages above are NEW conversation messages to incorporate into the existing summary provided in \
<previous-summary> tags.

Update the existing structured summary with new information. RULES:
- PRESERVE all existing information from the previous summary
- ADD new progress, decisions, and context from the new messages
- UPDATE the Progress section: move items from \"In Progress\" to \"Done\" when completed
- UPDATE \"Next Steps\" based on what was accomplished
- PRESERVE exact file paths, function names, and error messages
- If something is no longer relevant, you may remove it

Use this EXACT format:

## Goal
[Preserve existing goals, add new ones if the task expanded]

## Constraints & Preferences
- [Preserve existing, add new ones discovered]

## Progress
### Done
- [x] [Include previously done items AND newly completed items]

### In Progress
- [ ] [Current work - update based on progress]

### Blocked
- [Current blockers - remove if resolved]

## Key Decisions
- **[Decision]**: [Brief rationale] (preserve all previous, add new)

## Next Steps
1. [Update based on current state]

## Critical Context
- [Preserve important context, add new if needed]

Keep each section concise. Preserve exact file paths, function names, and error messages.";

const TURN_PREFIX: &str = "The messages above are earlier context from an ongoing conversation. Later messages are stored separately and \
do not need to be reconstructed.

Create a concise checkpoint of the user's request and the progress shown above. This checkpoint will be placed before the later \
messages so the conversation can continue with the necessary context.

## Original Request
[What did the user ask for?]

## Progress So Far
- [Key decisions and work completed in these messages]

## Context Needed to Continue
- [Information from these messages needed to understand the later work]

Only summarize information explicitly present above. Do not infer or recreate later messages.";

#[cfg(test)]
mod tests {
    use super::*;
    use dirigent_protocol::transcript::{Api, Notice, ToolResult};
    use serde_json::json;

    fn entry(id: &str, body: Body) -> Entry {
        Entry { seq: 0, id: id.into(), parent_id: None, at: 0, body }
    }

    fn user(id: &str, text: &str) -> Entry {
        entry(id, Body::User { content: vec![Block::text(text)] })
    }

    fn assistant(id: &str, content: Vec<Block>, stop: Stop, prompt: u64) -> Entry {
        let usage = Usage { input: prompt, output: 10, ..Usage::default() };
        let a = Assistant {
            content,
            model: "m".into(),
            provider: "p".into(),
            api: Api::Anthropic,
            effort: None,
            usage,
            stop,
            error: None,
            duration_ms: 0,
        };
        entry(id, Body::Assistant(a))
    }

    fn call(id: &str, path: &str) -> Block {
        Block::ToolCall { id: id.into(), name: "read".into(), input: json!({ "path": path }) }
    }

    fn result(id: &str, call_id: &str, text: &str) -> Entry {
        let r =
            ToolResult { call_id: call_id.into(), tool: "read".into(), content: vec![Block::text(text)], is_error: false, duration_ms: 0 };
        entry(id, Body::ToolResult(r))
    }

    fn kinds(messages: &[Message]) -> String {
        let kind = |m: &Message| match m {
            Message::User(_) => "user".to_owned(),
            Message::Assistant { .. } => "assistant".to_owned(),
            Message::ToolResult { call_id, is_error, .. } => format!("result:{call_id}{}", if *is_error { "!" } else { "" }),
        };
        messages.iter().map(kind).collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn a_request_is_whole_whatever_the_history_went_through() {
        let branch = [
            user("u1", "go"),
            assistant("a1", vec![call("c1", "a"), call("c2", "b")], Stop::ToolUse, 100),
            result("r1", "c1", "A"),
            user("u2", "still there?"),
            assistant("a2", vec![Block::text("half"), call("c3", "c")], Stop::Aborted, 0),
            result("r3", "c3", "C"),
            entry("n", Body::Notice(Notice::Info { text: "moved".into() })),
            entry("s", Body::Store { values: Default::default() }),
        ];
        let messages = messages(&branch, "m", true, &mut |_| None);
        assert_eq!(kinds(&messages), "user assistant result:c1 result:c2! user user");
    }

    #[test]
    fn thinking_goes_back_only_to_the_model_that_thought_it() {
        let thought = |id: &str, model: &str| {
            let thinking = Block::Thinking { text: "hm".into(), signature: Some("sig".into()), redacted: false };
            let mut entry = assistant(id, vec![thinking, Block::text("so")], Stop::End, 10);
            if let Body::Assistant(a) = &mut entry.body {
                a.model = model.into();
            }
            entry
        };
        let branch = [user("u1", "go"), thought("a1", "other"), user("u2", "and"), thought("a2", "m")];
        let kept: Vec<usize> = messages(&branch, "m", true, &mut |_| None)
            .iter()
            .filter_map(|m| if let Message::Assistant { content, .. } = m { Some(content) } else { None })
            .map(|content| content.iter().filter(|b| matches!(b, Block::Thinking { .. })).count())
            .collect();
        assert_eq!(kept, [0, 1]);
    }

    #[test]
    fn a_compaction_stands_for_what_came_before_its_first_kept_entry() {
        let compaction =
            Body::Compaction { summary: "S".into(), first_kept: "u2".into(), tokens_before: 9, model: "m".into(), usage: Usage::default() };
        let branch = [user("u1", "old"), user("u2", "kept"), entry("c", compaction), user("u3", "new")];
        let messages = messages(&branch, "m", true, &mut |_| None);
        let texts: Vec<&str> = messages
            .iter()
            .map(|m| if let Message::User(p) = m { if let Part::Text(t) = &p[0] { t.as_str() } else { "" } } else { "" })
            .collect();
        assert_eq!(texts, [&format!("{SUMMARY_PREFIX}S{SUMMARY_SUFFIX}"), "kept", "new"]);
        assert_eq!(tokens(&branch), 3);
    }

    #[test]
    fn the_context_is_as_big_as_the_last_request_said_plus_what_came_since() {
        let mut branch =
            vec![user("u1", "go"), assistant("a1", vec![call("c1", "a")], Stop::ToolUse, 1000), result("r1", "c1", &"x".repeat(400))];
        assert_eq!(tokens(&branch), 1000 + 10 + 100);
        branch.push(assistant("a2", vec![], Stop::Error, 0));
        assert_eq!(tokens(&branch), 1000 + 10 + 100);
    }

    #[test]
    fn a_plan_cuts_between_turns_or_inside_one_too_long_to_keep() {
        let big = "x".repeat(4000);
        let branch = [
            user("u1", "first"),
            assistant("a1", vec![Block::text("done")], Stop::End, 10),
            user("u2", "second"),
            assistant("a2", vec![call("c1", "f")], Stop::ToolUse, 10),
            result("r1", "c1", &big),
            assistant("a3", vec![call("c2", "g")], Stop::ToolUse, 10),
            result("r2", "c2", &big),
        ];
        let ids = |entries: &[&Entry]| entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>().join(" ");
        assert_eq!(Plan::new(&branch, 1_000_000), None);
        let plan = Plan::new(&branch, 500).unwrap();
        assert_eq!((plan.first_kept.as_str(), ids(&plan.older), ids(&plan.turn_prefix)), ("a3", "u1 a1".into(), "u2 a2 r1".into()));
        let plan = Plan::new(&branch, 2009).unwrap();
        assert_eq!((plan.first_kept.as_str(), ids(&plan.older), plan.turn_prefix.len()), ("u2", "u1 a1".into(), 0));
    }

    #[test]
    fn a_summary_carries_the_files_of_the_ones_before_it() {
        let (text, mut files) =
            Files::split("## Goal\nx\n\n<read-files>\na.rs\nb.rs\n</read-files>\n\n<modified-files>\nc.rs\n</modified-files>");
        assert_eq!(text, "## Goal\nx");
        files.note(&assistant(
            "a",
            vec![Block::ToolCall { id: "c".into(), name: "edit".into(), input: json!({ "path": "a.rs" }) }],
            Stop::ToolUse,
            1,
        ));
        assert_eq!(files.tags(), "\n\n<read-files>\nb.rs\n</read-files>\n\n<modified-files>\na.rs\nc.rs\n</modified-files>");
    }
}
