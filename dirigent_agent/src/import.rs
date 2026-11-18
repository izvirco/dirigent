use crate::config::{Compaction, Config, Dirs};
use dirigent_protocol::transcript::{Api, Assistant, Block, Body, Cost, Entry, Notice, Stop, ToolResult, Usage};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Default, PartialEq)]
pub struct Converted {
    pub entries: Vec<Entry>,
    /// The model (its id) and effort the thread was last switched to. Pi kept those as
    /// entries (`model_change`, `thinking_level_change`); we keep them on the thread.
    pub model: Option<String>,
    pub effort: Option<String>,
    /// The entries left out, counted by what they were: `model_change`, `message:system`,
    /// `custom:<type>`, ...
    pub dropped: BTreeMap<String, usize>,
    /// Why an entry could not be read, a line each. Such an entry is a `Body::Unknown` now,
    /// and its thread is one to lock: what the model would go on from is not what it had.
    pub unreadable: Vec<String>,
}

enum Outcome {
    Keep(Body),
    Drop(String),
}

const BRANCH_SUMMARY: &str = "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";

/// A thread's pi entries, in `seq` order and as `service.db` has them (an image is
/// `{type: "image", mimeType, blob}`: its bytes are in the db's blobs), as ours. Ids are
/// kept: pins, forks and asks name entries by them. The tree stays whole: the children of
/// an entry that is left out hang on its nearest ancestor that isn't.
pub fn entries(pi: &[Value]) -> Converted {
    let mut out = Converted::default();
    let mut parents: HashMap<&str, Option<&str>> = HashMap::new();
    let mut moved: HashMap<&str, Option<String>> = HashMap::new();
    let mut replied: Option<String> = None;
    let mut at = 0;
    for (i, e) in pi.iter().enumerate() {
        let s = |k: &str| e.get(k).and_then(Value::as_str);
        let (id, pi_parent) = (s("id"), s("parentId"));
        let parent_id = match pi_parent.and_then(|p| moved.get(p)) {
            Some(kept) => kept.clone(),
            None => pi_parent.map(str::to_owned),
        };
        // A message's own time (unix ms) is when it began; the entry's, when pi wrote it.
        at = (e.pointer("/message/timestamp").and_then(Value::as_i64)).or_else(|| s("timestamp").and_then(iso_ms)).unwrap_or(at);
        let outcome = match id {
            Some(id) => {
                parents.insert(id, pi_parent);
                convert(e, &mut out, &mut replied)
            }
            None => Err("it has no id".into()),
        };
        let body = match outcome {
            Ok(Outcome::Keep(Body::Compaction { summary, first_kept, tokens_before, model, usage })) => {
                let first_kept = first_kept_entry(&first_kept, id.unwrap_or_default(), pi_parent, &parents, &moved);
                Body::Compaction { summary, first_kept, tokens_before, model, usage }
            }
            Ok(Outcome::Keep(body)) => body,
            Ok(Outcome::Drop(what)) => {
                *out.dropped.entry(what).or_default() += 1;
                if let Some(id) = id {
                    moved.insert(id, parent_id);
                }
                continue;
            }
            Err(why) => {
                out.unreadable.push(format!("entry {} ({}): {why}", id.unwrap_or("?"), s("type").unwrap_or("?")));
                Body::Unknown
            }
        };
        let id = id.map(str::to_owned).unwrap_or_else(|| format!("pi-{i}"));
        // Pi timed a reply from its start; an entry's time is when it was written.
        let written = match &body {
            Body::Assistant(a) => at + a.duration_ms,
            _ => at,
        };
        out.entries.push(Entry { seq: out.entries.len() as u64, id, parent_id, at: written, body });
    }
    out
}

fn convert(e: &Value, out: &mut Converted, replied: &mut Option<String>) -> Result<Outcome, String> {
    let s = |k: &str| e.get(k).and_then(Value::as_str);
    let text = |k: &str| s(k).map(str::to_owned).ok_or(format!("it has no {k}"));
    let kind = s("type").ok_or("it has no type")?;
    Ok(Outcome::Keep(match kind {
        "message" => {
            let outcome = message(e.get("message").ok_or("it has no message")?)?;
            if let Outcome::Keep(Body::Assistant(a)) = &outcome {
                *replied = Some(a.model.clone());
            }
            return Ok(outcome);
        }
        "custom_message" => Body::Notice(notice(e)),
        "custom" => match s("customType") {
            Some("codemode-store") => match stored(e.get("data")) {
                Some(values) => Body::Store { values },
                None => return Ok(Outcome::Drop("custom:codemode-store".into())),
            },
            Some("dirigent:compaction_failed") => {
                Body::CompactionFailed { error: e.pointer("/data/error").and_then(Value::as_str).unwrap_or("Compaction failed").to_owned() }
            }
            other => return Ok(Outcome::Drop(format!("custom:{}", other.unwrap_or("?")))),
        },
        "compaction" => Body::Compaction {
            summary: text("summary")?,
            first_kept: text("firstKeptEntryId")?,
            tokens_before: e.get("tokensBefore").and_then(Value::as_u64).unwrap_or(0),
            // Pi doesn't say which model wrote the summary: the one at work then.
            model: replied.clone().or(out.model.clone()).unwrap_or_default(),
            usage: e.get("usage").map(usage).unwrap_or_default(),
        },
        "branch_summary" => Body::Notice(Notice::Info { text: format!("{BRANCH_SUMMARY}{}</summary>", text("summary")?) }),
        "model_change" => {
            out.model = s("modelId").map(str::to_owned).or(out.model.take());
            return Ok(Outcome::Drop(kind.into()));
        }
        "thinking_level_change" => {
            out.effort = s("thinkingLevel").map(str::to_owned).or(out.effort.take());
            return Ok(Outcome::Drop(kind.into()));
        }
        "session_info" | "label" | "usage" => return Ok(Outcome::Drop(kind.into())),
        other => return Err(format!("an entry of a type we don't know, {other:?}")),
    }))
}

fn message(m: &Value) -> Result<Outcome, String> {
    let s = |k: &str| m.get(k).and_then(Value::as_str);
    let duration_ms = m.get("durationMs").and_then(Value::as_i64).unwrap_or(0);
    Ok(Outcome::Keep(match s("role") {
        Some("user") => Body::User { content: blocks(m.get("content"))? },
        Some("assistant") => {
            let model = s("model").unwrap_or_default().to_owned();
            Body::Assistant(Assistant {
                content: blocks(m.get("content"))?,
                provider: provider(&model, s("provider").unwrap_or_default()),
                model,
                api: if s("api") == Some("anthropic-messages") { Api::Anthropic } else { Api::Codex },
                effort: s("thinkingLevel").map(str::to_owned),
                usage: m.get("usage").map(usage).unwrap_or_default(),
                stop: match s("stopReason") {
                    Some("stop") => Stop::End,
                    Some("toolUse") => Stop::ToolUse,
                    Some("length") => Stop::MaxTokens,
                    Some("error") => Stop::Error,
                    Some("aborted") => Stop::Aborted,
                    other => return Err(format!("a reply that stopped with {:?}", other.unwrap_or_default())),
                },
                error: s("errorMessage").map(str::to_owned),
                duration_ms,
            })
        }
        Some("toolResult") => Body::ToolResult(ToolResult {
            call_id: call_id(s("toolCallId").ok_or("a tool result without its call's id")?),
            tool: s("toolName").unwrap_or_default().to_owned(),
            content: blocks(m.get("content"))?,
            is_error: m.get("isError").and_then(Value::as_bool).unwrap_or(false),
            duration_ms,
        }),
        Some("system") => return Ok(Outcome::Drop("message:system".into())),
        other => return Err(format!("a message of role {:?}", other.unwrap_or_default())),
    }))
}

fn blocks(content: Option<&Value>) -> Result<Vec<Block>, String> {
    match content {
        Some(Value::String(text)) => Ok(vec![Block::text(text.as_str())]),
        Some(Value::Array(blocks)) => blocks.iter().map(block).collect(),
        _ => Err("a message without content".into()),
    }
}

fn block(b: &Value) -> Result<Block, String> {
    let s = |k: &str| b.get(k).and_then(Value::as_str);
    Ok(match s("type") {
        Some("text") => Block::Text {
            text: s("text").unwrap_or_default().to_owned(),
            blobs: b.get("blobs").and_then(|v| serde_json::from_value(v.clone()).ok()).unwrap_or_default(),
        },
        Some("image") => Block::Image {
            mime: s("mimeType").unwrap_or("application/octet-stream").to_owned(),
            blob: s("blob").ok_or("an image whose bytes are not in a blob")?.to_owned(),
        },
        Some("thinking") => Block::Thinking {
            text: s("thinking").unwrap_or_default().to_owned(),
            signature: None,
            redacted: b.get("redacted").and_then(Value::as_bool).unwrap_or(false),
        },
        Some("toolCall") => Block::ToolCall {
            id: call_id(s("id").ok_or("a tool call without an id")?),
            name: s("name").ok_or("a tool call without a name")?.to_owned(),
            input: b.get("arguments").cloned().unwrap_or_else(|| Value::Object(Default::default())),
        },
        other => return Err(format!("a block of type {:?}", other.unwrap_or_default())),
    })
}

/// A tool call's id: pi's is `<call id>|<item id>`, the second half the Responses API's
/// name for the same call.
fn call_id(pi: &str) -> String {
    pi.split('|').next().unwrap_or(pi).to_owned()
}

fn usage(u: &Value) -> Usage {
    let tokens = |k: &str| u.get(k).and_then(Value::as_f64).map_or(0, |n| n as u64);
    let dollars = |k: &str| u.get("cost").and_then(|c| c.get(k)).and_then(Value::as_f64);
    let part = |k: &str| dollars(k).unwrap_or(0.0);
    let mut cost = Cost { input: part("input"), output: part("output"), cache_read: part("cacheRead"), cache_write: part("cacheWrite") };
    // MUST KILL: extract: scale_to_total(cost, total) out of usage, so rescaling the parts to pi's total has a name
    let sum = cost.total();
    if let Some(total) = dollars("total").filter(|total| (total - sum).abs() > 1e-9) {
        let scale = |part: f64| part * total / sum;
        cost = if sum > 0.0 {
            Cost {
                input: scale(cost.input),
                output: scale(cost.output),
                cache_read: scale(cost.cache_read),
                cache_write: scale(cost.cache_write),
            }
        } else {
            Cost { input: total, ..cost }
        };
    }
    Usage { input: tokens("input"), output: tokens("output"), cache_read: tokens("cacheRead"), cache_write: tokens("cacheWrite"), cost }
}

fn notice(e: &Value) -> Notice {
    let text = match e.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => {
            let texts = blocks.iter().filter_map(|b| b.get("text").and_then(Value::as_str));
            texts.collect::<Vec<_>>().join("\n")
        }
        _ => String::new(),
    };
    let kind = match e.get("customType").and_then(Value::as_str) {
        Some("dirigent:job") => "job",
        Some("dirigent:child") => "child",
        Some("dirigent:answer") => "answer",
        _ => return Notice::Info { text },
    };
    let mut fields = e.get("details").and_then(Value::as_object).cloned().unwrap_or_default();
    fields.insert("kind".into(), kind.into());
    fields.insert("text".into(), text.as_str().into());
    if let Some(error) = fields.remove("error") {
        fields.insert("failed".into(), error);
    }
    serde_json::from_value(Value::Object(fields)).unwrap_or(Notice::Info { text })
}

fn stored(data: Option<&Value>) -> Option<BTreeMap<String, Value>> {
    let set = data?.get("set")?.as_object()?;
    let deleted = data?.get("delete")?.as_array()?.iter().map(|key| Some((key.as_str()?.to_owned(), Value::Null)));
    let mut values: BTreeMap<String, Value> = deleted.collect::<Option<_>>()?;
    values.extend(set.iter().map(|(key, value)| (key.clone(), value.clone())));
    Some(values)
}

fn first_kept_entry(
    first: &str,
    id: &str,
    parent: Option<&str>,
    parents: &HashMap<&str, Option<&str>>,
    moved: &HashMap<&str, Option<String>>,
) -> String {
    let mut path = vec![];
    let mut next = parent;
    while let Some(entry) = next
        && path.len() <= parents.len()
    {
        path.push(entry);
        next = if entry == first { None } else { parents.get(entry).copied().flatten() };
    }
    // Not one of them (the compaction itself: pi's way of keeping nothing): as it is.
    if path.last() != Some(&first) {
        return first.to_owned();
    }
    path.iter().rev().find(|entry| !moved.contains_key(*entry)).copied().unwrap_or(id).to_owned()
}

// MUST KILL: extract: days_from_civil(y, m, d) out of iso_ms, so its era/March-year arithmetic has a name
fn iso_ms(s: &str) -> Option<i64> {
    let (date, time) = s.trim_end_matches('Z').split_once('T')?;
    let number = |n: &str| n.parse::<u16>().ok().map(i64::from);
    let mut date = date.splitn(3, '-').map(number);
    let (y, m, d) = (date.next()??, date.next()??, date.next()??);
    let mut time = time.splitn(3, ':');
    let (h, min) = (number(time.next()?)?, number(time.next()?)?);
    let s = time.next()?.parse::<f64>().ok().filter(|s| (0.0..61.0).contains(s))?;
    let (y, m) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y.div_euclid(400);
    let year = y - era * 400;
    let day = year * 365 + year / 4 - year / 100 + (153 * m + 2) / 5 + d - 1;
    let days = era * 146_097 + day - 719_468;
    Some(((days * 24 + h) * 60 + min) * 60_000 + (s * 1000.0).round() as i64)
}

/// A model's id, from how old dbs and configs name one: `provider/id`
/// (`cliproxyapi/claude-opus-5-5`). There is one provider now, so the id alone says it.
pub fn model_id(s: &str) -> String {
    s.split_once('/').map_or(s, |(_, id)| id).to_owned()
}

/// Who makes `model` (`anthropic`, `openai`, ...), as far as its id tells: what usage is
/// summed under now. `pi` is the provider pi reached it through (`cliproxyapi`), which
/// stays for a model whose id doesn't say.
pub fn provider(model: &str, pi: &str) -> String {
    let o_series = model.strip_prefix('o').is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()));
    let made_by = if model.starts_with("claude") {
        "anthropic"
    } else if model.starts_with("gpt") || model.starts_with("codex") || o_series {
        "openai"
    } else if model.starts_with("gemini") {
        "google"
    } else {
        pi
    };
    made_by.to_owned()
}

#[derive(Debug, Default, PartialEq)]
pub struct Imported {
    /// What it brought over: for the log.
    pub done: Vec<String>,
    /// What it did not, and what to do about that: for the user, as nothing tries again.
    pub left: Vec<String>,
}

/// Bring what pi's agent dir (`<config dir>/agent`) holds into what takes its place, each
/// only where ours isn't there yet (so this never undoes an edit): `cliproxyapi.json` as
/// `config`'s `[proxy]`, `settings.json`'s compaction settings as its `[compaction]` and
/// `[models.<id>]`, `AGENTS.md` and `skills/` as the config dir's own. Not the MCP
/// servers: `mcp` imports those. The pi dir is left as it is.
///
/// As far as it goes: what can't be brought over is a line for the user (`Imported::left`)
/// and the rest still comes. `config` is what `agent.toml` holds, and the caller's to
/// save: the file is what says the import was done.
pub fn agent_dir(pi: &Path, dirs: &Dirs, config: &mut Config) -> Imported {
    let mut imported = Imported::default();
    let (notes, left) = (&mut imported.done, &mut imported.left);
    let before = config.clone();
    let proxy = if config.proxy.url.is_empty() { read_json(&pi.join("cliproxyapi.json"), left) } else { None };
    if let Some(proxy) = proxy
        && let Some(url) = proxy.get("baseUrl").and_then(Value::as_str)
    {
        config.proxy.url = url.trim_end_matches('/').to_owned();
        config.proxy.key = proxy.get("apiKey").and_then(Value::as_str).unwrap_or_default().to_owned();
        notes.push(format!("the proxy is {} (from cliproxyapi.json)", config.proxy.url));
    }
    let settings = read_json(&pi.join("settings.json"), left);
    if let Some(theirs) = settings.as_ref().and_then(|s| s.get("compaction")) {
        let tokens = |of: &Value, key: &str| of.get(key).and_then(Value::as_u64);
        // MUST KILL: type: make Config.compaction an Option, so agent_dir needn't treat "equal to the defaults" as "not set"
        if config.compaction == Compaction::default() {
            config.compaction.reserve = tokens(theirs, "reserveTokens").unwrap_or(config.compaction.reserve);
            config.compaction.keep_recent = tokens(theirs, "keepRecentTokens").unwrap_or(config.compaction.keep_recent);
        }
        for (model, over) in theirs.get("modelOverrides").and_then(Value::as_object).into_iter().flatten() {
            if let Some(reserve) = tokens(over, "reserveTokens") {
                config.models.entry(model_id(model)).or_default().reserve.get_or_insert(reserve);
            }
        }
        if config.compaction != before.compaction || config.models != before.models {
            notes.push("compaction is set as in pi's settings.json".into());
        }
    }
    let _ = std::fs::create_dir_all(&dirs.config);
    for name in ["AGENTS.md", "skills"] {
        let (from, to) = (pi.join(name), dirs.config.join(name));
        // `symlink_metadata`: a link that leads nowhere is there too.
        if from.symlink_metadata().is_err() || to.symlink_metadata().is_ok() {
            continue;
        }
        let staging = dirs.config.join(format!("{name}.new"));
        remove(&staging);
        if let Err(e) = copy(&from, &staging, true).and_then(|()| std::fs::rename(&staging, &to)) {
            remove(&staging);
            left.push(format!("{} was not copied ({e}): copy it to {} yourself", from.display(), to.display()));
            continue;
        }
        notes.push(format!("{name} copied from {}", from.display()));
    }
    let append = pi.join("APPEND_SYSTEM.md");
    if append.exists() {
        let ours = dirs.config.join("AGENTS.md");
        left.push(format!("{} is not carried over: move what it says into {}", append.display(), ours.display()));
    }
    imported
}

fn read_json(path: &Path, notes: &mut Vec<String>) -> Option<Value> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            notes.push(format!("{} not read: {e}", path.display()));
            return None;
        }
    };
    serde_json::from_str::<Value>(&text).map_err(|e| notes.push(format!("{} not read: {e}", path.display()))).ok()
}

// MUST KILL: extract: copy's unix symlink branch into a #[cfg(unix)] fn, so this allow(unused_variables) can go
#[cfg_attr(not(unix), allow(unused_variables))]
fn copy(from: &Path, to: &Path, at_top: bool) -> std::io::Result<()> {
    if cfg!(unix) && from.symlink_metadata()?.is_symlink() {
        let target = std::fs::read_link(from)?;
        let target = if at_top && target.is_relative() {
            let led_to = from.parent().unwrap_or(Path::new("")).canonicalize()?.join(target);
            let mut tidy = PathBuf::new();
            for part in led_to.components() {
                match part {
                    Component::CurDir => {}
                    Component::ParentDir => drop(tidy.pop()),
                    part => tidy.push(part),
                }
            }
            tidy
        } else {
            target
        };
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, to)?;
    } else if from.is_dir() {
        std::fs::create_dir(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy(&entry.path(), &to.join(entry.file_name()), false)?;
        }
    } else {
        std::fs::copy(from, to)?;
    }
    Ok(())
}

fn remove(path: &Path) {
    if path.symlink_metadata().is_ok_and(|m| m.is_dir()) {
        let _ = std::fs::remove_dir_all(path);
    } else {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ModelConfig, Proxy};
    use dirigent_protocol::Question;
    use dirigent_protocol::transcript::branch;
    use serde_json::json;

    fn pi(id: &str, parent: Option<&str>, mut fields: Value) -> Value {
        fields["id"] = id.into();
        fields["parentId"] = parent.into();
        fields["timestamp"] = "2026-01-02T03:04:05.500Z".into();
        fields
    }

    fn message(id: &str, parent: Option<&str>, message: Value) -> Value {
        pi(id, parent, json!({"type": "message", "message": message}))
    }

    fn ids(entries: &[Entry]) -> Vec<(&str, Option<&str>)> {
        entries.iter().map(|e| (e.id.as_str(), e.parent_id.as_deref())).collect()
    }

    #[test]
    fn messages_become_typed() {
        let got = entries(&[
            message("s", None, json!({"role":"system","content":"","sections":{"preamble":"You are"},"timestamp":1})),
            message("u1", Some("s"), json!({"role":"user","content":"hi","timestamp":1000})),
            message(
                "u2",
                Some("u1"),
                json!({"role":"user","timestamp":2000,"content":[{"type":"text","text":"see"},{"type":"image","mimeType":"image/png","blob":"ab"}]}),
            ),
            message(
                "a",
                Some("u2"),
                json!({"role":"assistant","api":"cliproxyapi-codex-responses","provider":"cliproxyapi","model":"claude-opus-5-5",
                    "thinkingLevel":"high","stopReason":"toolUse","timestamp":3000,"durationMs":700,"responseId":"r",
                    "usage":{"input":1,"output":2,"cacheRead":3,"cacheWrite":4,"totalTokens":10,
                        "cost":{"input":0.5,"output":0.25,"cacheRead":0.125,"cacheWrite":0.0,"total":0.875}},
                    "content":[
                        {"type":"thinking","thinking":"hm","thinkingSignature":"sig"},
                        {"type":"text","text":"![shot](a.png)","textSignature":"t","blobs":{"a.png":"cd"}},
                        {"type":"toolCall","id":"call_1|fc_1","name":"edit","arguments":{"path":"a.rs","edits":[]}}]}),
            ),
            message(
                "r",
                Some("a"),
                json!({"role":"toolResult","toolCallId":"call_1|fc_1","toolName":"edit","isError":true,"timestamp":4000,"durationMs":30,
                    "details":{"diff":"…"},"content":[{"type":"text","text":"no such file"}]}),
            ),
            message(
                "b",
                Some("r"),
                json!({"role":"assistant","api":"anthropic-messages","provider":"anthropic","model":"claude-opus-5-5",
                    "stopReason":"aborted","errorMessage":"Request was aborted","timestamp":5000,"content":[]}),
            ),
        ]);
        let user = |content| Body::User { content };
        let want = [
            ("u1", None, 1000, user(vec![Block::text("hi")])),
            ("u2", Some("u1"), 2000, user(vec![Block::text("see"), Block::Image { mime: "image/png".into(), blob: "ab".into() }])),
            (
                "a",
                Some("u2"),
                3700,
                Body::Assistant(Assistant {
                    content: vec![
                        Block::Thinking { text: "hm".into(), signature: None, redacted: false },
                        Block::Text { text: "![shot](a.png)".into(), blobs: [("a.png".to_owned(), "cd".to_owned())].into() },
                        Block::ToolCall { id: "call_1".into(), name: "edit".into(), input: json!({"path":"a.rs","edits":[]}) },
                    ],
                    model: "claude-opus-5-5".into(),
                    provider: "anthropic".into(),
                    api: Api::Codex,
                    effort: Some("high".into()),
                    usage: Usage {
                        input: 1,
                        output: 2,
                        cache_read: 3,
                        cache_write: 4,
                        cost: Cost { input: 0.5, output: 0.25, cache_read: 0.125, cache_write: 0.0 },
                    },
                    stop: Stop::ToolUse,
                    error: None,
                    duration_ms: 700,
                }),
            ),
            (
                "r",
                Some("a"),
                4000,
                Body::ToolResult(ToolResult {
                    call_id: "call_1".into(),
                    tool: "edit".into(),
                    content: vec![Block::text("no such file")],
                    is_error: true,
                    duration_ms: 30,
                }),
            ),
            (
                "b",
                Some("r"),
                5000,
                Body::Assistant(Assistant {
                    content: vec![],
                    model: "claude-opus-5-5".into(),
                    provider: "anthropic".into(),
                    api: Api::Anthropic,
                    effort: None,
                    usage: Usage::default(),
                    stop: Stop::Aborted,
                    error: Some("Request was aborted".into()),
                    duration_ms: 0,
                }),
            ),
        ];
        let want: Vec<Entry> = want
            .into_iter()
            .enumerate()
            .map(|(seq, (id, parent, at, body))| Entry { seq: seq as u64, id: id.into(), parent_id: parent.map(str::to_owned), at, body })
            .collect();
        assert_eq!(got.entries, want);
        assert_eq!(got.dropped, [("message:system".to_owned(), 1)].into());
        assert!(got.unreadable.is_empty());
    }

    #[test]
    fn a_cost_adds_up_to_pis_total() {
        let cost = |cost: Value| usage(&json!({"input": 1, "cost": cost})).cost;
        assert_eq!(cost(json!({"input":1.0,"output":3.0,"cacheRead":0.0,"cacheWrite":0.0,"total":2.0})).output, 1.5);
        assert_eq!(cost(json!({"total":2.0})), Cost { input: 2.0, ..Cost::default() });
        assert_eq!(cost(json!({"input":1.0,"output":3.0})).total(), 4.0);
    }

    #[test]
    fn notices_keep_what_clients_show() {
        let custom = |id: &str, kind: &str, details: Value| {
            let mut e = pi(id, None, json!({"type":"custom_message","customType":kind,"content":"said","display":true}));
            if !details.is_null() {
                e["details"] = details;
            }
            e
        };
        let question = json!({"text":"Db?","options":["pg"],"allow_custom":true,"multi":false});
        let got = entries(&[
            custom(
                "j",
                "dirigent:job",
                json!({"job_id":"x1","command":"cargo check","exit_code":1,"duration_ms":12000,"log":"/l","key":"job-x1"}),
            ),
            custom("c", "dirigent:child", json!({"thread_id":"kid","name":"Review","reply":null,"error":true,"asking":false})),
            custom("a", "dirigent:answer", json!({"ask_id":"q1","questions":[question],"answers":[["pg"]],"key":"ask-q1"})),
            custom("d", "dirigent:answer", json!({"ask_id":"q2","questions":[],"answers":null})),
            custom("w", "dirigent:workspace", Value::Null),
            custom("o", "other-extension", json!({"x":1})),
            custom("broken", "dirigent:job", json!({"job_id":7})),
            pi("s", None, json!({"type":"branch_summary","fromId":"x","summary":"tried A"})),
        ]);
        let text = || "said".to_owned();
        let questions = vec![Question { text: "Db?".into(), options: vec!["pg".into()], allow_custom: true, multi: false }];
        let want = [
            Notice::Job {
                text: text(),
                job_id: "x1".into(),
                command: "cargo check".into(),
                exit_code: 1,
                duration_ms: 12000,
                log: "/l".into(),
            },
            Notice::Child { text: text(), thread_id: "kid".into(), name: "Review".into(), asking: false, failed: true },
            Notice::Answer { text: text(), ask_id: "q1".into(), questions, answers: Some(vec![vec!["pg".into()]]) },
            Notice::Answer { text: text(), ask_id: "q2".into(), questions: vec![], answers: None },
            Notice::Info { text: text() },
            Notice::Info { text: text() },
            Notice::Info { text: text() },
            Notice::Info { text: format!("{BRANCH_SUMMARY}tried A</summary>") },
        ];
        assert_eq!(got.entries.iter().map(|e| e.body.clone()).collect::<Vec<_>>(), want.map(Body::Notice));
        assert_eq!(got.entries[0].at, 1_767_323_045_500);
        assert!(got.unreadable.is_empty() && got.dropped.is_empty());
    }

    #[test]
    fn the_rest_is_kept_on_the_thread_or_dropped() {
        let got = entries(&[
            pi("m1", None, json!({"type":"model_change","provider":"anthropic","modelId":"claude-opus-5-5"})),
            pi("t1", Some("m1"), json!({"type":"thinking_level_change","thinkingLevel":"high"})),
            pi(
                "st",
                Some("t1"),
                json!({"type":"custom","customType":"codemode-store","data":{"set":{"n":4,"gone":1},"delete":["old","gone"]}}),
            ),
            pi("bad", Some("st"), json!({"type":"custom","customType":"codemode-store","data":{"set":{}}})),
            pi("cf", Some("bad"), json!({"type":"custom","customType":"dirigent:compaction_failed","data":{"error":"529"}})),
            pi("x", Some("cf"), json!({"type":"custom","customType":"pi.virtual-model-state","data":{}})),
            pi("n", Some("x"), json!({"type":"session_info","name":"A title"})),
            pi("l", Some("n"), json!({"type":"label","targetId":"st","label":"here"})),
            pi("w", Some("l"), json!({"type":"usage","kind":"cache_warm","usage":{"cacheRead":5}})),
            pi("m2", Some("w"), json!({"type":"model_change","provider":"cliproxyapi","modelId":"gpt-9"})),
            pi("t2", Some("m2"), json!({"type":"thinking_level_change","thinkingLevel":"low"})),
        ]);
        let stored = [("gone", json!(1)), ("n", json!(4)), ("old", Value::Null)].map(|(k, v)| (k.to_owned(), v));
        assert_eq!(
            got.entries.iter().map(|e| e.body.clone()).collect::<Vec<_>>(),
            [Body::Store { values: stored.into() }, Body::CompactionFailed { error: "529".into() }]
        );
        assert_eq!(ids(&got.entries), [("st", None), ("cf", Some("st"))]);
        assert_eq!((got.model.as_deref(), got.effort.as_deref()), (Some("gpt-9"), Some("low")));
        let dropped = [
            ("custom:codemode-store", 1),
            ("custom:pi.virtual-model-state", 1),
            ("label", 1),
            ("model_change", 2),
            ("session_info", 1),
            ("thinking_level_change", 2),
            ("usage", 1),
        ];
        assert_eq!(got.dropped, dropped.map(|(what, n)| (what.to_owned(), n)).into());
        assert!(got.unreadable.is_empty());
    }

    #[test]
    fn the_tree_stays_whole() {
        let user = |id: &str, parent: Option<&str>| message(id, parent, json!({"role":"user","content":id,"timestamp":1}));
        let system = |id: &str, parent: &str| message(id, Some(parent), json!({"role":"system","content":"","timestamp":1}));
        let got = entries(&[
            pi("m", None, json!({"type":"model_change","provider":"p","modelId":"x"})),
            pi("t", Some("m"), json!({"type":"thinking_level_change","thinkingLevel":"low"})),
            user("u1", Some("t")),
            system("s1", "u1"),
            pi("n", Some("s1"), json!({"type":"session_info","name":"A title"})),
            user("u2", Some("n")),
            system("s2", "n"),
            user("u3", Some("s2")),
        ]);
        assert_eq!(ids(&got.entries), [("u1", None), ("u2", Some("u1")), ("u3", Some("u1"))]);
        assert_eq!(got.entries.iter().map(|e| e.seq).collect::<Vec<_>>(), [0, 1, 2]);
        assert_eq!(branch(&got.entries, "u3").iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["u1", "u3"]);
    }

    #[test]
    fn a_compaction_keeps_from_an_entry_that_is_still_there() {
        let user = |id: &str, parent: &str| message(id, Some(parent), json!({"role":"user","content":id,"timestamp":1}));
        let reply =
            message("a", Some("m"), json!({"role":"assistant","model":"gpt-9","provider":"cliproxyapi","stopReason":"stop","content":[]}));
        let compaction = |id: &str, parent: &str, first: &str| {
            let usage = json!({"input":10,"output":2,"cost":{"input":1.0,"output":0.5,"total":1.5}});
            pi(
                id,
                Some(parent),
                json!({"type":"compaction","summary":"so far","firstKeptEntryId":first,"tokensBefore":900,"usage":usage,"details":{}}),
            )
        };
        let got = entries(&[
            pi("m", None, json!({"type":"model_change","provider":"p","modelId":"x"})),
            reply,
            message("s", Some("a"), json!({"role":"system","content":"","timestamp":1})),
            user("u", "s"),
            compaction("c1", "u", "u"),
            compaction("c2", "c1", "s"),
            pi("l", Some("c2"), json!({"type":"label","targetId":"u","label":"x"})),
            compaction("c3", "l", "l"),
            compaction("c4", "c3", "c4"),
        ]);
        let first_kept = |e: &Entry| match &e.body {
            Body::Compaction { first_kept, .. } => Some(first_kept.clone()),
            _ => None,
        };
        assert_eq!(got.entries.iter().filter_map(first_kept).collect::<Vec<_>>(), ["u", "u", "c3", "c4"]);
        let usage = Usage { input: 10, output: 2, cost: Cost { input: 1.0, output: 0.5, ..Cost::default() }, ..Usage::default() };
        let want = Body::Compaction { summary: "so far".into(), first_kept: "u".into(), tokens_before: 900, model: "gpt-9".into(), usage };
        assert_eq!(got.entries[2].body, want);
    }

    #[test]
    fn an_entry_that_cant_be_read_locks_the_thread() {
        let got = entries(&[
            message("u", None, json!({"role":"user","content":"hi","timestamp":7})),
            message("b", Some("u"), json!({"role":"bashExecution","command":"ls","output":"","timestamp":8})),
            message("i", Some("b"), json!({"role":"user","content":[{"type":"image","mimeType":"image/png","data":"AQID"}]})),
            pi("e", Some("i"), json!({"type":"context_edit","targetId":"u","replacement":null})),
            json!("not an entry"),
            message("a", Some("e"), json!({"role":"assistant","model":"m","stopReason":"deferred","content":[]})),
        ]);
        assert_eq!(
            ids(&got.entries),
            [("u", None), ("b", Some("u")), ("i", Some("b")), ("e", Some("i")), ("pi-4", None), ("a", Some("e"))]
        );
        assert!(got.entries[1..].iter().all(|e| e.body == Body::Unknown));
        assert_eq!(
            got.entries.iter().map(|e| e.at).collect::<Vec<_>>(),
            [7, 8, 1_767_323_045_500, 1_767_323_045_500, 1_767_323_045_500, 1_767_323_045_500]
        );
        assert_eq!(got.unreadable.len(), 5);
        assert_eq!(got.unreadable[0], "entry b (message): a message of role \"bashExecution\"");
    }

    #[test]
    fn models_are_named_by_id_and_maker() {
        assert_eq!(model_id("cliproxyapi/claude-opus-5-5"), "claude-opus-5-5");
        assert_eq!(model_id("anthropic/claude-opus-5-5"), "claude-opus-5-5");
        assert_eq!(model_id("claude-opus-5-5"), "claude-opus-5-5");
        let made_by = |model: &str| provider(model, "cliproxyapi");
        assert_eq!(made_by("claude-fable-5-2"), "anthropic");
        assert_eq!([made_by("gpt-5.5"), made_by("o4-mini"), made_by("codex-mini")], ["openai", "openai", "openai"]);
        assert_eq!(made_by("gemini-3-pro"), "google");
        assert_eq!([made_by("opus"), made_by("qwen3")], ["cliproxyapi", "cliproxyapi"]);
        assert_eq!(iso_ms("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(iso_ms("2024-02-29T23:59:59.999Z"), Some(1_709_251_199_999));
        assert_eq!([iso_ms("yesterday"), iso_ms("2024-02-29T23:59:1e300Z")], [None, None]);
    }

    #[cfg(unix)]
    #[test]
    fn a_relative_link_leads_where_it_did_without_pis_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs { config: tmp.path().join("config"), data: tmp.path().join("data") };
        let pi = dirs.config.join("agent");
        std::fs::create_dir_all(&pi).unwrap();
        std::fs::create_dir_all(tmp.path().join("dotfiles")).unwrap();
        std::fs::write(tmp.path().join("dotfiles/AGENTS.md"), "Mine.").unwrap();
        std::os::unix::fs::symlink("../../dotfiles/AGENTS.md", pi.join("AGENTS.md")).unwrap();
        assert_eq!(agent_dir(&pi, &dirs, &mut Config::default()).left, Vec::<String>::new());
        std::fs::remove_dir_all(&pi).unwrap();
        assert_eq!(std::fs::read_to_string(dirs.config.join("AGENTS.md")).unwrap(), "Mine.");
    }

    #[test]
    fn the_agent_dir_comes_along_once() {
        let tmp = tempfile::tempdir().unwrap();
        let pi = tmp.path().join("config/agent");
        let dirs = Dirs { config: tmp.path().join("config"), data: tmp.path().join("data") };
        std::fs::create_dir_all(pi.join("skills/commit")).unwrap();
        std::fs::write(
            pi.join("cliproxyapi.json"),
            r#"{"baseUrl":"http://127.0.0.1:8317/","apiKey":"sk-secret","providerId":"cliproxyapi"}"#,
        )
        .unwrap();
        let settings = json!({"theme":"dark","compaction":{"reserveTokens":30000,"modelOverrides":{
            "cliproxyapi/claude-opus-5-5":{"reserveTokens":350000},"anthropic/claude-opus-5-5":{"reserveTokens":1},"x/y":{"keepRecentTokens":5}}}});
        std::fs::write(pi.join("settings.json"), settings.to_string()).unwrap();
        std::fs::write(pi.join("mcp.json"), "{}").unwrap();
        std::fs::write(pi.join("APPEND_SYSTEM.md"), "Be brief.").unwrap();
        std::fs::write(pi.join("skills/commit/SKILL.md"), "how").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/nix/store/x-AGENTS.md", pi.join("AGENTS.md")).unwrap();
            std::os::unix::fs::symlink("SKILL.md", pi.join("skills/commit/alias.md")).unwrap();
        }

        let import = |pi: &Path| {
            let mut imported = Imported::default();
            let saved = Config::update(&dirs, |config| {
                imported = agent_dir(pi, &dirs, config);
                Ok(())
            });
            saved.map(|()| imported).unwrap()
        };
        let Imported { done, left } = import(&pi);
        let config = Config::load(&dirs).unwrap();
        assert_eq!(config.proxy, Proxy { url: "http://127.0.0.1:8317".into(), key: "sk-secret".into() });
        assert_eq!(config.compaction, Compaction { reserve: 30000, keep_recent: 20000 });
        assert_eq!(config.models, [("claude-opus-5-5".to_owned(), ModelConfig { reserve: Some(350_000) })].into());
        assert_eq!(std::fs::read_to_string(dirs.config.join("skills/commit/SKILL.md")).unwrap(), "how");
        #[cfg(unix)]
        {
            assert_eq!(std::fs::read_link(dirs.config.join("AGENTS.md")).unwrap(), Path::new("/nix/store/x-AGENTS.md"));
            assert_eq!(std::fs::read_link(dirs.config.join("skills/commit/alias.md")).unwrap(), Path::new("SKILL.md"));
        }
        assert!(!dirs.config.join("mcp.json").exists() && !dirs.config.join("skills.new").exists());
        assert_eq!(left.len(), 1, "{left:?}");
        assert!(left[0].contains("APPEND_SYSTEM.md is not carried over"), "{left:?}");
        assert!(done.iter().chain(&left).all(|n| !n.contains("sk-secret")), "{done:?} {left:?}");

        Config::update(&dirs, |c| {
            c.proxy.url = "http://proxy:1".into();
            c.compaction.reserve = 5;
            Ok(())
        })
        .unwrap();
        std::fs::write(dirs.config.join("skills/commit/SKILL.md"), "mine").unwrap();
        let again = import(&pi);
        assert_eq!((again.done.len(), again.left.len()), (0, 1), "{again:?}");
        let config = Config::load(&dirs).unwrap();
        assert_eq!((config.proxy.url.as_str(), config.compaction.reserve), ("http://proxy:1", 5));
        assert_eq!(std::fs::read_to_string(dirs.config.join("skills/commit/SKILL.md")).unwrap(), "mine");
        assert_eq!(std::fs::read_to_string(pi.join("skills/commit/SKILL.md")).unwrap(), "how");
        assert_eq!(import(&tmp.path().join("nowhere")), Imported::default());
    }

    #[cfg(unix)]
    #[test]
    fn what_does_not_come_along_is_left_to_the_user() {
        let tmp = tempfile::tempdir().unwrap();
        let pi = tmp.path().join("config/agent");
        let dirs = Dirs { config: tmp.path().join("config"), data: tmp.path().join("data") };
        std::fs::create_dir_all(pi.join("skills/commit")).unwrap();
        std::fs::write(pi.join("AGENTS.md"), "Be brief.").unwrap();
        std::fs::write(pi.join("cliproxyapi.json"), r#"{"baseUrl":"http://127.0.0.1:8317","apiKey":"k"}"#).unwrap();
        std::fs::write(pi.join("settings.json"), "{ not json").unwrap();
        let _listening = std::os::unix::net::UnixListener::bind(pi.join("skills/commit/socket")).unwrap();

        let mut config = Config::default();
        let Imported { done, left } = agent_dir(&pi, &dirs, &mut config);
        assert_eq!(config.proxy.url, "http://127.0.0.1:8317");
        assert_eq!(std::fs::read_to_string(dirs.config.join("AGENTS.md")).unwrap(), "Be brief.");
        assert_eq!(done.len(), 2, "{done:?}");
        assert!(!dirs.config.join("skills").exists() && !dirs.config.join("skills.new").exists());
        let (settings, skills) = (pi.join("settings.json"), pi.join("skills"));
        assert_eq!(left.len(), 2, "{left:?}");
        assert!(left[0].starts_with(&format!("{} not read: ", settings.display())), "{left:?}");
        assert!(left[1].starts_with(&format!("{} was not copied (", skills.display())), "{left:?}");
        assert!(left[1].ends_with(&format!("): copy it to {} yourself", dirs.config.join("skills").display())), "{left:?}");
    }
}
