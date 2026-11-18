use anyhow::{Context, Result};
use dirigent_agent::config::{self, Config, Dirs};
use dirigent_agent::proxy::Proxy;
use dirigent_agent::session::{Prompt, Thread};
use dirigent_agent::tool::Tool;
use dirigent_agent::{Agent, Host, Out};
use dirigent_protocol::transcript::{Body, Event, text_of};
use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Printer {
    blobs: Mutex<HashMap<String, (String, Vec<u8>)>>,
    ended: tokio::sync::Notify,
}

impl Host for Printer {
    fn emit(&self, _thread_id: &str, out: Out) {
        match out {
            Out::Event(Event::Text { delta, .. }) => print!("{delta}"),
            Out::Event(Event::Thinking { delta, .. }) => print!("\x1b[2m{delta}\x1b[0m"),
            Out::Event(Event::ToolCall { name, input, .. }) => println!("\n\x1b[36m→ {name} {input}\x1b[0m"),
            Out::Event(Event::Retry { attempt, delay_ms, error }) => println!("\n(try {attempt} in {delay_ms} ms: {error})"),
            Out::Event(Event::Warning { message }) => println!("\n(warning: {message})"),
            Out::Event(Event::CompactionStart) => println!("\n(compacting)"),
            Out::Event(Event::RunEnd) => self.ended.notify_one(),
            Out::Entry(entry) => match entry.body {
                Body::ToolResult(result) => {
                    let text = text_of(&result.content);
                    let (shown, more) = (text.lines().take(8).collect::<Vec<_>>().join("\n"), text.lines().count().saturating_sub(8));
                    let more = if more > 0 { format!("\n… {more} more lines") } else { String::new() };
                    println!("\x1b[2m← {}{shown}{more}\x1b[0m", if result.is_error { "error: " } else { "" });
                }
                Body::Assistant(reply) => {
                    let (usage, error) = (reply.usage, reply.error.map(|e| format!(" {e}")).unwrap_or_default());
                    println!(
                        "\n\x1b[2m[{:?}{error}: {} in ({} cached, {} written), {} out, ${:.4}, {} ms]\x1b[0m",
                        reply.stop,
                        usage.prompt(),
                        usage.cache_read,
                        usage.cache_write,
                        usage.output,
                        usage.cost.total(),
                        reply.duration_ms
                    );
                }
                Body::Notice(notice) => println!("\n(notice: {})", notice.text()),
                Body::Compaction { summary, .. } => println!("\n(compacted to: {summary})"),
                Body::CompactionFailed { error } => println!("\n(compaction failed: {error})"),
                _ => {}
            },
            _ => {}
        }
        let _ = std::io::stdout().flush();
    }

    fn put_blob(&self, mime: &str, bytes: &[u8]) -> String {
        let mut blobs = self.blobs.lock().unwrap();
        let id = format!("blob-{}", blobs.len());
        blobs.insert(id.clone(), (mime.to_owned(), bytes.to_vec()));
        id
    }

    fn blob(&self, id: &str) -> Option<(String, Vec<u8>)> {
        self.blobs.lock().unwrap().get(id).cloned()
    }

    fn children_busy(&self, _thread_id: &str) -> bool {
        false
    }

    fn tools(&self, _thread_id: &str) -> Vec<Arc<dyn Tool>> {
        vec![]
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let (mut model, mut effort, mut cwd, mut tools, mut text) =
        ("claude-haiku-5-5".to_owned(), None, std::env::current_dir()?, vec![], vec![]);
    while let Some(arg) = args.next() {
        let mut value = || args.next().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--model" => model = value()?,
            "--effort" => effort = Some(value()?),
            "--cwd" => cwd = value()?.into(),
            "--tools" => tools = value()?.split(',').map(str::to_owned).collect(),
            _ => text.push(arg),
        }
    }
    anyhow::ensure!(!text.is_empty(), "what to ask?");

    let tmp = tempfile::tempdir()?;
    let dirs = Dirs { config: tmp.path().join("config"), data: tmp.path().join("data") };
    let proxy = match (std::env::var("DIRIGENT_PROXY_URL"), std::env::var("DIRIGENT_PROXY_KEY")) {
        (Ok(url), Ok(key)) => config::Proxy { url, key },
        _ => {
            let real = Dirs { config: format!("{}/.config/dirigent", std::env::var("HOME")?).into(), data: dirs.data.clone() };
            Config::load(&real)?.proxy
        }
    };
    anyhow::ensure!(!proxy.url.is_empty(), "no proxy: set DIRIGENT_PROXY_URL and DIRIGENT_PROXY_KEY");
    Config::update(&dirs, |config| {
        config.proxy = proxy;
        Ok(())
    })?;

    let host = Arc::new(Printer::default());
    let agent = Agent::new(dirs.clone(), host.clone(), Arc::new(Proxy::new(dirs)));
    let thread = Thread { id: "run".into(), cwd: cwd.to_string_lossy().into_owned(), ..Thread::default() };
    let session = agent.load(thread);
    session.prompt(Prompt { text: text.join(" "), model: Some(model), effort, tools: Some(tools), ..Prompt::default() }).await?;
    host.ended.notified().await;
    agent.unload("run");
    Ok(())
}
