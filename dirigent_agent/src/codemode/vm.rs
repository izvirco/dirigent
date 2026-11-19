use super::Namespace;
use super::declare::{identifier, sample};
use super::output::Item;
use super::search::{Document, rank};
use crate::tool::ToolSpec;
use rquickjs::context::EvalOptions;
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

const PRELUDE: &str = include_str!("prelude.js");

const MEMORY_BYTES: usize = 256 * 1024 * 1024;
const STACK_BYTES: usize = 4 * 1024 * 1024;
/// The thread's own stack: QuickJS checks its limit only now and then, native code runs past it.
const THREAD_STACK_BYTES: usize = 2 * STACK_BYTES;

/// What is said of a script that threw `null`: QuickJS does when it has no memory left for
/// the error that would say so, which is how a script that fills the heap bit by bit ends.
fn out_of_memory() -> String {
    format!("Error: null was thrown: the script most likely ran out of memory (it has {} MB)", MEMORY_BYTES / (1024 * 1024))
}

pub static SLOTS: Semaphore = Semaphore::const_new(32);

pub struct Script {
    pub code: String,
    pub tools: Vec<ToolSpec>,
    pub namespaces: Vec<Namespace>,
    pub stored: BTreeMap<String, Value>,
}

pub enum Event {
    Output(Item),
    Call { id: u32, name: String, args: Option<String> },
    Done(Result<String, String>),
    Crash(String),
}

pub struct Vm {
    pub events: UnboundedReceiver<Event>,
    results: Sender<(u32, Result<String, String>)>,
    interrupt: Arc<AtomicBool>,
}

impl Vm {
    pub fn start(script: Script, slot: SemaphorePermit<'static>) -> std::io::Result<Self> {
        let (events_out, events) = unbounded_channel();
        let (results, results_in) = channel();
        let interrupt = Arc::new(AtomicBool::new(false));
        let stop = interrupt.clone();
        std::thread::Builder::new().name("codemode".into()).stack_size(THREAD_STACK_BYTES).spawn(move || {
            let _slot = slot;
            if let Err(message) = run(script, &events_out, &results_in, &stop)
                && !stop.load(Ordering::Relaxed)
            {
                let _ = events_out.send(Event::Crash(message));
            }
        })?;
        Ok(Self { events, results, interrupt })
    }

    pub fn settle(&self, id: u32, result: Result<String, String>) {
        let _ = self.results.send((id, result));
    }
}

impl Drop for Vm {
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

    let host = host(ctx, events.clone(), catalog, done.clone())?;
    let (prelude, loaded) = Module::declare(ctx.clone(), "codemode-prelude.js", PRELUDE)?.eval()?;
    loaded.finish::<()>()?;
    let prelude: Function = prelude.get("default")?;
    let api: Object = prelude.call((host, tools_json, Value::Object(stored).to_string(), out_of_memory()))?;
    let (settle, start, stalled): (Function, Function, Function) = (api.get("settle")?, api.get("run")?, api.get("stalled")?);

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

fn host<'js>(ctx: &Ctx<'js>, events: UnboundedSender<Event>, catalog: Catalog, done: Rc<Cell<bool>>) -> rquickjs::Result<Object<'js>> {
    let host = Object::new(ctx.clone())?;
    let out = events.clone();
    let call = move |id: u32, name: JsValue<'js>, args: JsValue<'js>| {
        let _ = out.send(Event::Call { id, name: text(name).unwrap_or_default(), args: text(args) });
    };
    host.set("call", Function::new(ctx.clone(), call)?)?;
    let out = events.clone();
    let output = move |kind: JsValue<'js>, data: JsValue<'js>, mime: JsValue<'js>| {
        let item = match text(kind).as_deref() {
            Some("image") => Item::Image { data: text(data).unwrap_or_default(), mime: text(mime).unwrap_or_default() },
            Some("console") => Item::Console(text(data).unwrap_or_default()),
            _ => Item::Text(text(data).unwrap_or_default()),
        };
        let _ = out.send(Event::Output(item));
    };
    host.set("output", Function::new(ctx.clone(), output)?)?;
    let finish = move |ok: bool, payload: JsValue<'js>| {
        done.set(true);
        let payload = text(payload).unwrap_or_default();
        let _ = events.send(Event::Done(if ok { Ok(payload) } else { Err(payload) }));
    };
    host.set("done", Function::new(ctx.clone(), finish)?)?;
    let catalog = Rc::new(catalog);
    let searched = catalog.clone();
    let search = move |query: JsValue<'js>, limit: f64, namespace: JsValue<'js>| {
        searched.search(&text(query).unwrap_or_default(), limit as usize, text(namespace).as_deref())
    };
    host.set("search", Function::new(ctx.clone(), search)?)?;
    host.set("namespace", Function::new(ctx.clone(), move |name: JsValue<'js>| catalog.namespace(&text(name).unwrap_or_default()))?)?;
    Ok(host)
}

fn text(value: JsValue<'_>) -> Option<String> {
    value.as_string().and_then(|text| text.to_string().ok())
}

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

struct Catalog {
    tools: Vec<ToolSpec>,
    namespaces: Vec<Namespace>,
}

impl Catalog {
    fn tools_json(&self) -> String {
        let tools =
            self.tools.iter().map(|tool| json!({ "name": tool.name, "jsName": identifier(&tool.name), "description": sample(tool) }));
        Value::Array(tools.collect()).to_string()
    }

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

    fn search(&self, query: &str, limit: usize, namespace: Option<&str>) -> String {
        let tools: Vec<&ToolSpec> = match namespace.filter(|namespace| !namespace.is_empty()) {
            Some(namespace) => self.of(namespace).collect(),
            None => self.tools.iter().collect(),
        };
        let documents: Vec<Document> =
            tools.iter().map(|tool| Document::new(tool, tool.namespace.as_deref().and_then(|n| self.known(n)))).collect();
        json!(rank(query, &documents, limit)).to_string()
    }

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
