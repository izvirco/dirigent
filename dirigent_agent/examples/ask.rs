//! Ask a model one thing and print its reply as it streams: the proxy client, end to end.
//!
//! ```sh
//! cargo run -p dirigent_agent --example ask -- "Why is the sky blue?"
//! cargo run -p dirigent_agent --example ask -- --model claude-haiku-5-5 --effort high --system "Be terse." "17 * 23?"
//! cargo run -p dirigent_agent --example ask -- --codex "hi"      # over the `codex` wire, whatever the model
//! cargo run -p dirigent_agent --example ask -- --models          # what the proxy serves
//! ```
//!
//! The proxy is the one `DIRIGENT_PROXY_URL` and `DIRIGENT_PROXY_KEY` name, else the one of
//! this machine's install (`[proxy]` in `~/.config/dirigent/agent.toml`).

use anyhow::{Context, Result};
use dirigent_agent::config::{self, Config, Dirs};
use dirigent_agent::proxy::{Chunk, Message, Model, Part, Proxy, Request};
use dirigent_protocol::transcript::Api;
use std::io::Write;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let (mut model, mut effort, mut system, mut codex, mut list, mut prompt) =
        ("claude-haiku-5-5".to_owned(), None, String::new(), false, false, vec![]);
    while let Some(arg) = args.next() {
        let mut value = || args.next().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--model" => model = value()?,
            "--effort" => effort = Some(value()?),
            "--system" => system = value()?,
            "--codex" => codex = true,
            "--models" => list = true,
            _ => prompt.push(arg),
        }
    }

    // Directories of its own, with an `agent.toml` that names the proxy.
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
    let proxy = Proxy::new(dirs);

    if list {
        for Model { id, name, owner, api, context_window, max_output, efforts, price, .. } in proxy.models().await? {
            let price = format!("${} in, ${} out", price.input, price.output);
            println!("{id}  {name}  ({owner}, {api:?})  {context_window} tokens, {max_output} out  [{}]  {price}", efforts.join(" "));
        }
        return Ok(());
    }

    let mut model = proxy.model(&model).await?;
    if codex {
        model.api = Api::Codex;
    }
    let messages = [Message::User(vec![Part::Text(prompt.join(" "))])];
    let request = Request {
        model: &model,
        system: &system,
        messages: &messages,
        tools: &[],
        effort: effort.as_deref(),
        max_output: None,
        cache_key: "",
    };
    // Thinking is printed dim; a new block starts on a new line.
    let mut block = None;
    let mut on = |chunk: Chunk| {
        let (index, text) = match chunk {
            Chunk::Start => return,
            Chunk::Thinking { index, delta } => (index, format!("\x1b[2m{delta}\x1b[0m")),
            Chunk::Text { index, delta } => (index, delta),
            Chunk::ToolCall { index, name, input, .. } => (index, format!("[{name} {input}]")),
        };
        if block.replace(index).is_some_and(|before| before != index) {
            println!();
        }
        print!("{text}");
        let _ = std::io::stdout().flush();
    };
    let reply = proxy.stream(&request, &mut on, &CancellationToken::new()).await;
    println!();
    let (message, usage) = (&reply.message, &reply.message.usage);
    eprintln!(
        "\x1b[2m{:?}: {} in, {} out, {} read from and {} written to the cache, ${:.6}\x1b[0m",
        message.stop,
        usage.input,
        usage.output,
        usage.cache_read,
        usage.cache_write,
        usage.cost.total()
    );
    if let Some(error) = &message.error {
        anyhow::bail!("{error} ({:?})", reply.failure);
    }
    Ok(())
}
