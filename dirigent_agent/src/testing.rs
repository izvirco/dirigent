use crate::proxy::{Chunk, Failure, Model, Price, Reply, Request};
use crate::tool::{Call, Tool, ToolOutput, ToolSpec};
use crate::{Host, Llm, Out};
use async_trait::async_trait;
use dirigent_protocol::transcript::{Api, Assistant, Block, Body, Entry, Event, Stop, Usage};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct Deaf;

impl Host for Deaf {
    fn emit(&self, _: &str, _: Out) {}
    fn put_blob(&self, _: &str, _: &[u8]) -> String {
        String::new()
    }
    fn blob(&self, _: &str) -> Option<(String, Vec<u8>)> {
        None
    }
    fn children_busy(&self, _: &str) -> bool {
        false
    }
    fn tools(&self, _: &str) -> Vec<Arc<dyn Tool>> {
        vec![]
    }
}

#[derive(Default)]
pub struct Memory {
    pub outs: Mutex<Vec<Out>>,
    blobs: Mutex<HashMap<String, (String, Vec<u8>)>>,
    pub came: tokio::sync::Notify,
    tools: Vec<Arc<dyn Tool>>,
    pub trip: AtomicBool,
}

impl Host for Memory {
    fn emit(&self, _: &str, out: Out) {
        assert!(!self.trip.swap(false, Ordering::Relaxed), "the host tripped");
        self.outs.lock().unwrap().push(out);
        self.came.notify_waiters();
    }
    fn put_blob(&self, mime: &str, bytes: &[u8]) -> String {
        let id = format!("blob{}", bytes.len());
        self.blobs.lock().unwrap().insert(id.clone(), (mime.to_owned(), bytes.to_vec()));
        id
    }
    fn blob(&self, id: &str) -> Option<(String, Vec<u8>)> {
        self.blobs.lock().unwrap().get(id).cloned()
    }
    fn children_busy(&self, _: &str) -> bool {
        false
    }
    fn tools(&self, _: &str) -> Vec<Arc<dyn Tool>> {
        self.tools.clone()
    }
}

pub struct Does<F>(pub &'static str, pub F);

#[async_trait]
impl<F, Fut> Tool for Does<F>
where
    F: Fn() -> Fut + Send + Sync,
    Fut: Future<Output = ToolOutput> + Send,
{
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(self.0, "", serde_json::json!({ "type": "object" }))
    }

    async fn call(&self, _: Value, _: &Call) -> anyhow::Result<ToolOutput> {
        Ok((self.1)().await)
    }
}

impl Memory {
    pub fn entries(&self) -> Vec<Entry> {
        self.outs.lock().unwrap().iter().filter_map(|o| if let Out::Entry(e) = o { Some(e.clone()) } else { None }).collect()
    }

    pub async fn settled(&self, n: usize) {
        self.until(|outs| outs.iter().filter(|o| matches!(o, Out::Event(Event::RunEnd))).count() >= n).await;
    }

    pub async fn until(&self, enough: impl Fn(&[Out]) -> bool) {
        let wait = async {
            loop {
                let came = self.came.notified();
                if enough(&self.outs.lock().unwrap()) {
                    return;
                }
                came.await;
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(20), wait).await.expect("it never came");
    }

    pub fn transcript(&self) -> Vec<String> {
        self.entries().iter().map(line).collect()
    }
}

pub fn line(entry: &Entry) -> String {
    let blocks = |content: &[Block]| -> String {
        let block = |b: &Block| match b {
            Block::Text { text, .. } => text.clone(),
            Block::Thinking { .. } => "(thinking)".into(),
            Block::ToolCall { name, input, .. } => format!("{name}({input})"),
            Block::Image { .. } => "(image)".into(),
        };
        content.iter().map(block).collect::<Vec<_>>().join(" ")
    };
    match &entry.body {
        Body::User { content } => format!("user: {}", blocks(content)),
        Body::Assistant(a) => match &a.error {
            Some(error) => format!("assistant: {} [{error}]", blocks(&a.content)).replace(":  [", ": ["),
            None if a.stop == Stop::Aborted => format!("assistant: {} [aborted]", blocks(&a.content)).replace(":  [", ": ["),
            None => format!("assistant: {}", blocks(&a.content)),
        },
        Body::ToolResult(r) => format!("result{}: {}", if r.is_error { "!" } else { "" }, blocks(&r.content)),
        Body::Notice(n) => format!("notice: {}", n.text()),
        Body::Compaction { summary, first_kept, .. } => format!("compaction: {summary} (kept from {first_kept})"),
        Body::CompactionFailed { error } => error.clone(),
        Body::Store { values } => format!("store: {}", serde_json::json!(values)),
        Body::Unknown => "unknown".into(),
    }
}

pub enum Step {
    Say(Vec<Block>),
    Fail(Failure, &'static str),
    Hang,
    Panic(&'static str),
}

pub fn text(text: &str) -> Block {
    Block::text(text)
}

pub fn call(id: &str, name: &str, input: Value) -> Block {
    Block::ToolCall { id: id.into(), name: name.into(), input }
}

pub struct Script {
    steps: Mutex<VecDeque<Step>>,
    pub requests: Mutex<Vec<(String, String, Vec<String>)>>,
    pub context_window: u64,
}

impl Script {
    pub fn new(steps: Vec<Step>) -> Arc<Self> {
        Arc::new(Self { steps: Mutex::new(steps.into()), requests: Mutex::default(), context_window: 1_000_000 })
    }

    pub fn with_window(steps: Vec<Step>, context_window: u64) -> Arc<Self> {
        Arc::new(Self { steps: Mutex::new(steps.into()), requests: Mutex::default(), context_window })
    }
}

#[async_trait]
impl Llm for Script {
    async fn model(&self, id: &str) -> anyhow::Result<Model> {
        anyhow::ensure!(id != "unserved", "{id} is not a model the proxy serves");
        if id == "slow" {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        Ok(Model {
            id: id.to_owned(),
            name: id.to_owned(),
            owner: "test".into(),
            api: Api::Anthropic,
            context_window: self.context_window,
            max_output: 1000,
            efforts: vec!["low".into(), "high".into()],
            images: true,
            price: Price::default(),
        })
    }

    async fn stream(&self, request: &Request<'_>, on: &mut (dyn FnMut(Chunk) + Send), cancel: &CancellationToken) -> Reply {
        let tools = request.tools.iter().map(|tool| tool.name.clone()).collect();
        self.requests.lock().unwrap().push((format!("{:?}", request.messages), request.system.to_owned(), tools));
        let step = self.steps.lock().unwrap().pop_front().expect("the model was asked more often than scripted");
        let message = |content: Vec<Block>, stop: Stop, error: Option<&str>| Assistant {
            content,
            model: request.model.id.clone(),
            provider: String::new(),
            api: Api::Anthropic,
            effort: None,
            usage: Usage { input: 100, output: 10, ..Usage::default() },
            stop,
            error: error.map(str::to_owned),
            duration_ms: 0,
        };
        match step {
            Step::Say(content) => {
                on(Chunk::Start);
                for (index, block) in content.iter().enumerate() {
                    match block {
                        Block::Text { text, .. } => on(Chunk::Text { index, delta: text.clone() }),
                        Block::ToolCall { id, name, input } => {
                            on(Chunk::ToolCall { index, id: id.clone(), name: name.clone(), input: input.clone() })
                        }
                        _ => {}
                    }
                }
                let stop = if content.iter().any(|b| matches!(b, Block::ToolCall { .. })) { Stop::ToolUse } else { Stop::End };
                Reply { message: message(content, stop, None), failure: None }
            }
            Step::Fail(failure, error) => Reply { message: message(vec![], Stop::Error, Some(error)), failure: Some(failure) },
            Step::Hang => {
                on(Chunk::Start);
                cancel.cancelled().await;
                Reply { message: message(vec![text("half a")], Stop::Aborted, None), failure: None }
            }
            Step::Panic(said) => panic!("{said}"),
        }
    }
}

pub fn agent(dir: &std::path::Path, script: Arc<Script>, tools: Vec<Arc<dyn Tool>>) -> (Arc<Memory>, Arc<crate::Agent>) {
    let host = Arc::new(Memory { tools, ..Memory::default() });
    let dirs = crate::config::Dirs { config: dir.join("config"), data: dir.join("data") };
    (host.clone(), crate::Agent::new(dirs, host, script))
}

pub fn thread(dir: &std::path::Path) -> crate::session::Thread {
    crate::session::Thread {
        id: "thread".into(),
        cwd: dir.to_string_lossy().into_owned(),
        model: Some("model".into()),
        tools: vec!["async_bash".into(), "ask_user".into()],
        ..Default::default()
    }
}

pub fn session(dir: &std::path::Path, script: Arc<Script>) -> (Arc<Memory>, Arc<crate::session::Session>) {
    session_with(dir, script, vec![])
}

pub fn session_with(dir: &std::path::Path, script: Arc<Script>, tools: Vec<Arc<dyn Tool>>) -> (Arc<Memory>, Arc<crate::session::Session>) {
    let (host, agent) = agent(dir, script, tools);
    (host, agent.load(thread(dir)))
}

pub fn prompt(text: &str) -> crate::session::Prompt {
    crate::session::Prompt { text: text.into(), ..Default::default() }
}
