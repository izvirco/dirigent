//! One script in one QuickJS VM, on a thread of its own: a script may spin, and must never
//! hold a tokio worker. The thread makes the VM, runs the script until it waits on its tool
//! calls, and sleeps until one is settled. All that leaves the VM goes out as `Event`s, in
//! order, so whoever runs the script has its output even when the VM never comes back.
//!
//! The VM has `MEMORY_BYTES` of heap (past it the script gets `InternalError: out of memory`,
//! or `null` when not even that error fits: `out_of_memory`) and `STACK_BYTES` of stack
//! (`RangeError`). It is stopped through QuickJS's interrupt handler, which the engine asks
//! every ten thousand calls and loop turns: a `while (true) {}` ends at once, a loop of long
//! built-ins (`JSON.parse` of megabytes) only after that many of them. A thread cannot be
//! killed, so a stopped VM's thread goes on until then, with its place among the VMs
//! (`SLOTS`); who ran the script does not wait for it.

use super::Namespace;
use super::declare::{identifier, sample};
use super::output::Item;
use super::search::{Document, rank};
use crate::tool::ToolSpec;
use rquickjs::context::EvalOptions;
use rquickjs::function::Rest;
use rquickjs::{Context, Ctx, Function, Module, Object, Runtime, Value as JsValue};
use serde_json::{Value, json};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::{Semaphore, SemaphorePermit};

/// What makes the globals of a script, on top of the one function that leaves the VM.
const PRELUDE: &str = include_str!("prelude.js");

const MEMORY_BYTES: usize = 256 * 1024 * 1024;
/// Some 2000 calls deep, as far as pi's VM went.
const STACK_BYTES: usize = 4 * 1024 * 1024;
/// The thread's own stack: QuickJS checks its limit only now and then, native code runs past it.
const THREAD_STACK_BYTES: usize = 2 * STACK_BYTES;

/// What is said of a script that threw `null`: QuickJS does when it has no memory left for
/// the error that would say so, which is how a script that fills the heap bit by bit ends.
fn out_of_memory() -> String {
    format!("Error: null was thrown: the script most likely ran out of memory (it has {} MB)", MEMORY_BYTES / (1024 * 1024))
}

/// The VMs that may live at once in this process, whatever threads they are of: each can
/// take `MEMORY_BYTES`, and a thread.
pub static SLOTS: Semaphore = Semaphore::const_new(32);

/// What a VM runs, and what its script can find out about the tools.
pub struct Script {
    /// The body of an async function.
    pub code: String,
    pub tools: Vec<ToolSpec>,
    /// What is known of the tools' namespaces.
    pub namespaces: Vec<Namespace>,
    /// What `load()` reads.
    pub stored: BTreeMap<String, Value>,
}

/// What leaves a VM.
pub enum Event {
    Output(Item),
    /// The script called a tool, with the JSON of its arguments: `Vm::settle` answers by `id`.
    Call {
        id: u32,
        name: String,
        args: Option<String>,
    },
    /// The script's end: the JSON of what it stored (`{ key: JSON text, null to forget }`), or
    /// its error. Nothing follows it.
    Done(Result<String, String>),
    /// The VM failed, not the script.
    Crash(String),
}

/// A running VM. Dropping it stops the script.
pub struct Vm {
    pub events: UnboundedReceiver<Event>,
    /// A call's id, and the JSON of what it resolves to or the message it rejects with.
    results: Sender<(u32, Result<String, String>)>,
    interrupt: Arc<AtomicBool>,
}

impl Vm {
    /// `slot` is the VM's place among those that may live at once: held until its thread
    /// ends, so a VM that was stopped while a built-in still computes keeps counting.
    pub fn start(script: Script, slot: SemaphorePermit<'static>) -> std::io::Result<Self> {
        let (events_out, events) = unbounded_channel();
        let (results, results_in) = channel();
        let interrupt = Arc::new(AtomicBool::new(false));
        let stop = interrupt.clone();
        std::thread::Builder::new().name("codemode".into()).stack_size(THREAD_STACK_BYTES).spawn(move || {
            let _slot = slot;
            // A stopped VM fails with the interrupt: nobody listens any more.
            if let Err(message) = run(script, &events_out, &results_in, &stop)
                && !stop.load(Ordering::Relaxed)
            {
                let _ = events_out.send(Event::Crash(message));
            }
        })?;
        Ok(Self { events, results, interrupt })
    }

    /// Settle the script's call `id`.
    pub fn settle(&self, id: u32, result: Result<String, String>) {
        let _ = self.results.send((id, result));
    }
}

impl Drop for Vm {
    /// The engine sees the flag at its next poll; a thread waiting on a result wakes because
    /// `results` is gone.
    fn drop(&mut self) {
        self.interrupt.store(true, Ordering::Relaxed);
    }
}

fn run(
    script: Script,
    events: &UnboundedSender<Event>,
    results: &Receiver<(u32, Result<String, String>)>,
    interrupt: &Arc<AtomicBool>,
) -> Result<(), String> {
    let runtime = Runtime::new().map_err(|e| e.to_string())?;
    runtime.set_memory_limit(MEMORY_BYTES);
    runtime.set_max_stack_size(STACK_BYTES);
    let stop = interrupt.clone();
    runtime.set_interrupt_handler(Some(Box::new(move || stop.load(Ordering::Relaxed))));
    let context = Context::full(&runtime).map_err(|e| e.to_string())?;
    context.with(|ctx| drive(&ctx, script, events, results, interrupt).map_err(|error| explain(&ctx, error)))
}

/// Run the script to its end: its code and the promise jobs it queues, then, while it waits
/// on tool calls, their results as they come.
fn drive<'js>(
    ctx: &Ctx<'js>,
    script: Script,
    events: &UnboundedSender<Event>,
    results: &Receiver<(u32, Result<String, String>)>,
    interrupt: &AtomicBool,
) -> rquickjs::Result<()> {
    let Script { code, tools, namespaces, stored } = script;
    let stored: serde_json::Map<String, Value> =
        stored.into_iter().filter(|(_, v)| !v.is_null()).map(|(key, v)| (key, v.to_string().into())).collect();
    let catalog = Catalog { tools, namespaces };
    let tools_json = catalog.tools_json();
    let done = Rc::new(Cell::new(false));

    // `prelude.js` is a module: its default export makes the globals and hands back what
    // drives the script.
    let bridge = Function::new(ctx.clone(), bridge(events.clone(), catalog, done.clone()))?;
    let (prelude, loaded) = Module::declare(ctx.clone(), "codemode-prelude.js", PRELUDE)?.eval()?;
    loaded.finish::<()>()?;
    let prelude: Function = prelude.get("default")?;
    let api: Object = prelude.call((bridge, tools_json, Value::Object(stored).to_string(), out_of_memory()))?;
    let (settle, start, stalled): (Function, Function, Function) = (api.get("settle")?, api.get("run")?, api.get("stalled")?);

    // The script is the body of an async function, not in strict mode (as pi ran it). The
    // wrapper shares the script's first line, so the lines errors name are the script's.
    let mut options = EvalOptions::default();
    (options.strict, options.filename) = (false, Some("codemode.js".into()));
    let main: Function = match ctx.eval_with_options(format!("(async (tools, console) => {{{code}\n}})"), options) {
        Ok(main) => main,
        Err(error) => {
            let _ = events.send(Event::Done(Err(explain(ctx, error))));
            return Ok(());
        }
    };
    start.call::<_, ()>((main,))?;
    loop {
        // The promise jobs, until none is left: `while (true) await null` always leaves one,
        // so a stopped VM runs no more of them.
        while !interrupt.load(Ordering::Relaxed) && ctx.execute_pending_job() {}
        if done.get() || interrupt.load(Ordering::Relaxed) {
            return Ok(());
        }
        // An exception nobody was handed: the engine had no memory left to queue what follows
        // a promise (the script's own, once it has awaited), and says so nowhere else. What
        // waits for that promise never runs: the script would only look stalled.
        if ctx.has_exception() {
            let _ = events.send(Event::Done(Err(explain(ctx, rquickjs::Error::Exception))));
            return Ok(());
        }
        if stalled.call::<_, bool>(())? {
            return Ok(());
        }
        let Ok((id, result)) = results.recv() else { return Ok(()) };
        let (ok, payload) = match result {
            Ok(json) => (true, json),
            Err(message) => (false, message),
        };
        settle.call::<_, ()>((id, ok, payload))?;
    }
}

/// The prelude's `bridge(kind, ...)`: see `prelude.js` for what each kind carries.
fn bridge<'js>(
    events: UnboundedSender<Event>,
    catalog: Catalog,
    done: Rc<Cell<bool>>,
) -> impl Fn(String, Rest<JsValue<'js>>) -> Option<String> + 'js {
    move |kind, Rest(args)| {
        let text = |i: usize| args.get(i).and_then(JsValue::as_string).and_then(|s| s.to_string().ok());
        let number = |i: usize| args.get(i).and_then(JsValue::as_number).unwrap_or_default();
        let event = match kind.as_str() {
            "call" => Event::Call { id: number(0) as u32, name: text(1).unwrap_or_default(), args: text(2) },
            "output" => Event::Output(match text(0).as_deref() {
                Some("image") => Item::Image { data: text(1).unwrap_or_default(), mime: text(2).unwrap_or_default() },
                Some("console") => Item::Console(text(1).unwrap_or_default()),
                _ => Item::Text(text(1).unwrap_or_default()),
            }),
            "done" => {
                done.set(true);
                let payload = text(1).unwrap_or_default();
                Event::Done(if args.first().and_then(JsValue::as_bool) == Some(true) { Ok(payload) } else { Err(payload) })
            }
            "search" => return Some(catalog.search(&text(0).unwrap_or_default(), number(1) as usize, text(2).as_deref())),
            "namespace" => return catalog.namespace(&text(0).unwrap_or_default()),
            _ => return None,
        };
        let _ = events.send(event);
        None
    }
}

/// A failed evaluation as text: the exception as `Name: message` over its stack.
fn explain(ctx: &Ctx<'_>, error: rquickjs::Error) -> String {
    if !error.is_exception() {
        return error.to_string();
    }
    let thrown = ctx.catch();
    if thrown.is_null() {
        return out_of_memory();
    }
    let Some(object) = thrown.as_object() else {
        return thrown.get::<rquickjs::Coerced<String>>().map(|text| text.0).unwrap_or_else(|_| "an exception".into());
    };
    let field = |name: &str| object.get::<_, Option<rquickjs::Coerced<String>>>(name).ok().flatten().map(|text| text.0).unwrap_or_default();
    let (name, message, stack) = (field("name"), field("message"), field("stack"));
    let head = if message.is_empty() { name } else { format!("{name}: {message}") };
    format!("{head}\n{stack}").trim_end().to_string()
}

/// What a script can find out about the tools beyond `ALL_TOOLS`.
struct Catalog {
    tools: Vec<ToolSpec>,
    namespaces: Vec<Namespace>,
}

impl Catalog {
    /// `[{ name, jsName, description }]` for the prelude: every tool with what a script reads of it.
    fn tools_json(&self) -> String {
        let tools =
            self.tools.iter().map(|tool| json!({ "name": tool.name, "jsName": identifier(&tool.name), "description": sample(tool) }));
        Value::Array(tools.collect()).to_string()
    }

    /// The tools of the namespace a script names: by its name, the same as an identifier
    /// (`dev-radius` and `dev_radius`), or an MCP server's own name (`radius` for its
    /// `mcp__radius`), which is what a user calls it.
    fn of<'a>(&'a self, namespace: &str) -> impl Iterator<Item = &'a ToolSpec> {
        let asked = identifier(namespace);
        let matches = move |name: &str| {
            let name = identifier(name);
            name == asked || name.strip_prefix("mcp__") == Some(asked.as_str())
        };
        self.tools.iter().filter(move |tool| tool.namespace.as_deref().is_some_and(&matches))
    }

    fn known(&self, namespace: &str) -> Option<&Namespace> {
        self.namespaces.iter().find(|known| known.name == namespace)
    }

    /// `searchTools()`: the JSON list of the best tools' names.
    fn search(&self, query: &str, limit: usize, namespace: Option<&str>) -> String {
        let tools: Vec<&ToolSpec> = match namespace.filter(|namespace| !namespace.is_empty()) {
            Some(namespace) => self.of(namespace).collect(),
            None => self.tools.iter().collect(),
        };
        let documents: Vec<Document> =
            tools.iter().map(|tool| Document::new(tool, tool.namespace.as_deref().and_then(|n| self.known(n)))).collect();
        json!(rank(query, &documents, limit)).to_string()
    }

    /// `describeNamespace()`: the JSON of `{ name, description?, instructions?, tools }`.
    fn namespace(&self, query: &str) -> Option<String> {
        let tools: Vec<&ToolSpec> = self.of(query).collect();
        let name = tools.first()?.namespace.as_deref()?;
        let mut about = serde_json::Map::new();
        about.insert("name".into(), name.into());
        let known = self.known(name);
        for (key, text) in
            [("description", known.and_then(|n| n.description.as_ref())), ("instructions", known.and_then(|n| n.instructions.as_ref()))]
        {
            if let Some(text) = text.filter(|text| !text.is_empty()) {
                about.insert(key.into(), text.as_str().into());
            }
        }
        about.insert("tools".into(), tools.iter().map(|tool| identifier(&tool.name)).collect::<Vec<_>>().into());
        Some(Value::Object(about).to_string())
    }
}
