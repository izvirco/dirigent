//! Codemode: the `codemode` tool runs a JavaScript script the model wrote, in a QuickJS
//! sandbox, which calls the session's other tools. Only what the script prints reaches the
//! model, so a script can run calls side by side and cut large results down first.
//!
//! The sandbox knows nothing of sessions: it reaches the tools through `ScriptHost`.
//!
//! - `vm`: one script in one VM, on a thread of its own; `prelude.js` is what the VM runs
//!   first, the script's globals.
//! - `declare`: the tools as a script is told of them (identifiers, TypeScript declarations),
//!   and the tool's description.
//! - `search`: `searchTools()`.
//! - `output`: the result as the model reads it.
//! - here: the tool. It runs the script's tool calls and holds its deadline.

mod declare;
mod output;
mod search;
mod vm;

use crate::tool::{Call, Content, Tool, ToolOutput, ToolSpec};
use anyhow::{Context as _, bail};
use async_trait::async_trait;
use base64::Engine;
use output::Item;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use vm::{Event, Script, Vm};

/// What a script's sandbox reaches of the session it runs in.
#[async_trait]
pub trait ScriptHost: Send + Sync {
    /// Every tool a script can call, the ones not declared to the model included
    /// (`ToolSpec::deferred`); not `codemode` itself.
    fn tools(&self) -> Vec<ToolSpec>;

    /// Call tool `name` for a script: `call` is the script's own (`Call::script`). It is
    /// cancelled when the script ends with the call still running (it ended by itself, hit
    /// its deadline, or the run was stopped); a call that has not returned a few seconds
    /// later is dropped. No call outlives its script's `codemode` call.
    async fn call(&self, name: &str, input: Value, call: &Call) -> ToolOutput;

    /// What `describeNamespace(name)` tells of a group of tools (`ToolSpec::namespace`).
    fn namespace(&self, name: &str) -> Option<Namespace>;

    /// The values earlier scripts kept with `store()`, as of this branch of the thread.
    fn stored(&self) -> BTreeMap<String, Value>;

    /// Keep what a script that succeeded stored: by key, `Value::Null` to forget one.
    fn store(&self, values: BTreeMap<String, Value>);
}

/// A group of tools: an MCP server.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Namespace {
    pub name: String,
    pub description: Option<String>,
    /// How to use its tools, as the server says (or dirigent, for a screen's driver).
    pub instructions: Option<String>,
}

/// The `codemode` tool of one session. What a script's result leaves in files (the images it
/// showed, its text when that was cut) goes to the system's temp directory.
pub fn tool(host: Arc<dyn ScriptHost>) -> Arc<dyn Tool> {
    Arc::new(Codemode { host, files: std::env::temp_dir(), slots: &vm::SLOTS })
}

const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 10_000;
/// The longest `timeout_ms`.
const MAX_TIMEOUT_MS: u64 = 2_147_483_647;
/// How long a script waits for a VM when all are taken: scripts may wait on one another (a
/// script's tool call that ends in another thread's script), and this bounds it.
const SLOT_WAIT: Duration = Duration::from_secs(60);
/// How long the calls a script left running get to end what they started, once cancelled.
const WIND_DOWN: Duration = Duration::from_secs(5);
const ABORTED: &str = "Script aborted: Execution aborted";
/// How many tools a failed script's summary names for its failed calls, and for its cancelled ones.
const NAMED_TOOLS: usize = 8;

struct Codemode {
    host: Arc<dyn ScriptHost>,
    /// Where output files go.
    files: PathBuf,
    /// The VMs that may live at once.
    slots: &'static Semaphore,
}

#[async_trait]
impl Tool for Codemode {
    fn spec(&self) -> ToolSpec {
        let description = declare::description(&self.host.tools(), |name| self.host.namespace(name));
        let input = json!({ "type": "object", "properties": { "code": { "type": "string", "description": "Raw JavaScript source." } }, "required": ["code"] });
        ToolSpec {
            snippet: Some("Run JavaScript that calls other tools".into()),
            guidelines: vec![
                "Use codemode to batch independent tool calls (Promise.allSettled), chain them, or filter large output, instead of many separate calls.".into(),
            ],
            ..ToolSpec::new("codemode", &description, input)
        }
    }

    async fn call(&self, input: Value, call: &Call) -> anyhow::Result<ToolOutput> {
        let started = Instant::now();
        let code = input.get("code").and_then(Value::as_str).context("codemode takes `code`: the JavaScript source, as a string")?;
        let source = Source::parse(code)?;
        let (items, end) = self.run(&source, call, started).await;
        let error = match end {
            Ok(stored) if stored.is_empty() => None,
            Ok(stored) => {
                self.host.store(stored);
                None
            }
            Err(error) => Some(error),
        };
        // Off the async workers: it decodes the images and writes files.
        let (wall, max_tokens, files) = (started.elapsed(), source.max_output_tokens, self.files.clone());
        Ok(tokio::task::spawn_blocking(move || output::render(wall, items, error, max_tokens, &files)).await?)
    }
}

impl Codemode {
    /// Run the script to its end: what it put out, and what it stored, or why it failed (the
    /// text after `Script error:`).
    async fn run(&self, source: &Source<'_>, call: &Call, started: Instant) -> (Vec<Item>, Result<BTreeMap<String, Value>, String>) {
        let never_started = |why: String| (vec![], Err(format!("{why}\n\nThe script did not start.")));
        // The wait for a VM counts toward the script's deadline.
        let slot = tokio::select! {
            biased;
            _ = call.cancel.cancelled() => return never_started(ABORTED.into()),
            slot = self.slots.acquire() => slot.expect("the semaphore is never closed"),
            _ = tokio::time::sleep(source.timeout.map_or(SLOT_WAIT, |timeout| timeout.min(SLOT_WAIT))) => return never_started(match source.timeout {
                Some(timeout) if timeout <= SLOT_WAIT => format!("Script timed out: Execution timed out after {} ms while waiting for a free codemode VM", timeout.as_millis()),
                _ => format!("No codemode VM became free within {} seconds: all that may run at once on this host are held by other scripts. Retry after they finish.", SLOT_WAIT.as_secs()),
            }),
        };
        let tools = self.host.tools();
        let mut namespaces: Vec<&str> = tools.iter().filter_map(|tool| tool.namespace.as_deref()).collect();
        namespaces.sort_unstable();
        namespaces.dedup();
        let namespaces = namespaces.into_iter().filter_map(|name| self.host.namespace(name)).collect();
        let script = Script { code: source.code.into(), tools, namespaces, stored: self.host.stored() };
        let mut vm = match Vm::start(script, slot) {
            Ok(vm) => vm,
            Err(error) => return never_started(format!("Script sandbox failed: {error}")),
        };

        let deadline = async {
            match source.timeout {
                Some(timeout) => tokio::time::sleep_until((started + timeout).into()).await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(deadline);
        // The script's calls end with the run, and with the script.
        let nested = Call { id: call.id.clone(), cancel: call.cancel.child_token(), script: true };
        // A call's id in the VM, its place in `calls`, whether it failed, and what the script gets.
        let mut running: JoinSet<(u32, usize, bool, Result<String, String>)> = JoinSet::new();
        // Every call the script made, with how it went (`summary`).
        let mut calls: Vec<(String, &str)> = vec![];
        let mut items = vec![];
        let end = loop {
            tokio::select! {
                biased;
                _ = call.cancel.cancelled() => break Err(ABORTED.to_string()),
                _ = &mut deadline => break Err(format!("Script timed out: Execution timed out after {} ms", source.timeout.unwrap_or_default().as_millis())),
                event = vm.events.recv() => match event {
                    Some(Event::Output(item)) => items.push(item),
                    Some(Event::Call { id, name, args }) => {
                        let (host, nested, index) = (self.host.clone(), nested.clone(), calls.len());
                        calls.push((name.clone(), "cancelled"));
                        running.spawn(async move {
                            // `tools.name()` without arguments is a call with none. JavaScript's JSON
                            // is not all serde_json's: arguments nested too deep are none it reads.
                            let input = match args.as_deref().map(serde_json::from_str).unwrap_or_else(|| Ok(json!({}))) {
                                Ok(input) => input,
                                Err(error) => return (id, index, true, Err(format!("The arguments could not be read as JSON: {error}"))),
                            };
                            let output = host.call(&name, input, &nested).await;
                            (id, index, output.is_error, script_value(&name, output))
                        });
                    }
                    Some(Event::Done(Ok(writes))) => break parse_writes(&writes).map(|(stored, refused)| {
                        items.extend(refused.into_iter().map(Item::Console));
                        stored
                    }),
                    Some(Event::Done(Err(error))) => break Err(error),
                    Some(Event::Crash(message)) => break Err(format!("Script sandbox failed: {message}")),
                    None => break Err("Script sandbox failed: the VM ended without a result".into()),
                },
                Some(settled) = running.join_next() => match settled {
                    Ok((id, index, failed, result)) => {
                        calls[index].1 = if failed { "failed" } else { "ok" };
                        vm.settle(id, result);
                    }
                    Err(error) => break Err(format!("Script sandbox failed: a tool call broke off: {error}")),
                },
            }
        };
        // A script that fails keeps its partial output: what the VM had put out by now.
        if end.is_err() {
            while let Ok(event) = vm.events.try_recv() {
                if let Event::Output(item) = event {
                    items.push(item);
                }
            }
        }
        // The script is over: the VM stops, and the calls it left running are cancelled. They
        // get a moment to end what they started (`Call::cancel`); one that takes longer is dropped.
        drop(vm);
        nested.cancel.cancel();
        let _ = tokio::time::timeout(WIND_DOWN, async { while running.join_next().await.is_some() {} }).await;

        (items, end.map_err(|error| format!("{error}\n\n{}", summary(&calls))))
    }
}

/// What a failed script's calls (each with `ok`, `failed` or `cancelled`) came to, said after
/// its error: how many of each, and for those that failed or were cancelled which tools'.
/// Short however many calls there were: a result is cut to its budget in the middle, and a
/// list of thousands of calls at its end would leave nothing of the error before it.
fn summary(calls: &[(String, &str)]) -> String {
    if calls.is_empty() {
        return "No tool calls were made.".into();
    }
    let mut parts = vec![];
    for status in ["ok", "failed", "cancelled"] {
        let mut tools: BTreeMap<&str, usize> = BTreeMap::new();
        for (name, _) in calls.iter().filter(|(_, of)| *of == status) {
            *tools.entry(name).or_default() += 1;
        }
        let count: usize = tools.values().sum();
        if count == 0 {
            continue;
        }
        if status == "ok" {
            parts.push(format!("{count} ok"));
            continue;
        }
        let named = |(name, count): (&&str, &usize)| if *count == 1 { name.to_string() } else { format!("{name} ×{count}") };
        let mut names: Vec<String> = tools.iter().take(NAMED_TOOLS).map(named).collect();
        if tools.len() > NAMED_TOOLS {
            names.push(format!("and {} more tools", tools.len() - NAMED_TOOLS));
        }
        parts.push(format!("{count} {status} ({})", names.join(", ")));
    }
    format!("Tool calls made before the failure (they are not undone): {}", parts.join(", "))
}

/// What a script's call resolves to (`Ok`, as JSON) or rejects with: the tool's value when it
/// gave one, also for an error that carries one; else an image block for an output with an
/// image; else its text.
fn script_value(name: &str, output: ToolOutput) -> Result<String, String> {
    let text = output.as_text();
    if let Some(value) = output.value {
        return Ok(value.to_string());
    }
    if output.is_error {
        return Err(if text.is_empty() { format!("Tool \"{name}\" failed") } else { text });
    }
    let image = output.content.iter().find_map(|content| match content {
        Content::Image { mime, data } => {
            Some(json!({ "type": "image", "data": base64::engine::general_purpose::STANDARD.encode(data), "mimeType": mime, "note": text }))
        }
        Content::Text(_) => None,
    });
    Ok(image.unwrap_or(Value::String(text)).to_string())
}

/// What a script stored, from the prelude's `{ key: JSON text, null to forget }`, and a line
/// to tell the model for each key that is not kept, the others being: its JSON is
/// JavaScript's, and not all of that is serde_json's (a value nested too deep, for one).
fn parse_writes(json: &str) -> Result<(BTreeMap<String, Value>, Vec<String>), String> {
    let writes: BTreeMap<String, Option<String>> =
        serde_json::from_str(json).map_err(|error| format!("Script sandbox failed: what the script stored could not be read: {error}"))?;
    let (mut stored, mut refused) = (BTreeMap::new(), vec![]);
    for (key, json) in writes {
        match json.as_deref().map_or(Ok(Value::Null), serde_json::from_str) {
            Ok(value) => {
                stored.insert(key, value);
            }
            Err(error) => refused.push(format!("store({}) was not kept: its value could not be read as JSON: {error}", json!(key))),
        }
    }
    Ok((stored, refused))
}

/// A script as the model wrote it: JavaScript, after an optional first line of options,
/// `// @options: {"max_output_tokens": 2000, "timeout_ms": 30000}`.
#[derive(Debug, PartialEq)]
struct Source<'a> {
    /// From the options line's end on, so the script's lines keep their numbers.
    code: &'a str,
    max_output_tokens: u64,
    /// The deadline of the whole script, the wait for a VM included.
    timeout: Option<Duration>,
}

impl<'a> Source<'a> {
    /// An options line anywhere but first is an error: it would be ignored without notice.
    fn parse(input: &'a str) -> anyhow::Result<Self> {
        const PREFIX: &str = "// @options:";
        const FIELDS: &str = "`max_output_tokens` and `timeout_ms`";
        if input.trim().is_empty() {
            bail!(
                "Expected JavaScript source text (non-empty). Provide JS only, optionally with a first line `{PREFIX} {{\"max_output_tokens\": 1000}}`."
            );
        }
        if let Some(line) = input.split('\n').skip(1).position(|line| line.trim_start().starts_with(PREFIX)) {
            bail!("The {PREFIX} line must be the first line of the input; found one on line {}", line + 2);
        }
        let mut source = Self { code: input, max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS, timeout: None };
        let first = input.split('\n').next().unwrap_or_default();
        let Some(options) = first.trim_start().strip_prefix(PREFIX) else { return Ok(source) };
        source.code = &input[first.len()..];
        if source.code.trim().is_empty() {
            bail!("The @options line must be followed by JavaScript source on subsequent lines");
        }
        let fields = match serde_json::from_str(options.trim()) {
            Ok(Value::Object(fields)) => fields,
            Err(error) if !options.trim().is_empty() => bail!("@options must be valid JSON with supported fields {FIELDS}: {error}"),
            _ => bail!("@options must be a JSON object with supported fields {FIELDS}"),
        };
        for (key, value) in fields {
            // As JavaScript reads numbers: `1e3` is an integer.
            let integer =
                value.as_u64().or_else(|| value.as_f64().filter(|n| n.fract() == 0.0 && (0.0..9e15).contains(n)).map(|n| n as u64));
            match (key.as_str(), integer) {
                ("max_output_tokens", Some(tokens)) => source.max_output_tokens = tokens,
                ("max_output_tokens", None) => bail!("@options field `max_output_tokens` must be a non-negative integer"),
                ("timeout_ms", Some(ms @ 1..=MAX_TIMEOUT_MS)) => source.timeout = Some(Duration::from_millis(ms)),
                ("timeout_ms", _) => bail!("@options field `timeout_ms` must be a positive integer up to {MAX_TIMEOUT_MS}"),
                _ => bail!("@options only supports {FIELDS}; got `{key}`"),
            }
        }
        Ok(source)
    }
}

#[cfg(test)]
mod tests;
