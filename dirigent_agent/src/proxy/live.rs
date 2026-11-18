use super::tests::{dirs, point_at};
use super::*;
use dirigent_protocol::transcript::text_of;
use serde_json::json;

const HAIKU: &str = "claude-haiku-5-5";

fn proxy() -> (tempfile::TempDir, Proxy) {
    let from_env = std::env::var("DIRIGENT_PROXY_URL").ok().zip(std::env::var("DIRIGENT_PROXY_KEY").ok());
    let (url, key) = from_env.unwrap_or_else(|| {
        let real = Dirs { config: std::env::temp_dir(), data: format!("{}/.local/share/dirigent", std::env::var("HOME").unwrap()).into() };
        let proxy = config::Proxy::load(&real).unwrap();
        assert!(!proxy.api_key.is_empty(), "no proxy: set DIRIGENT_PROXY_URL and DIRIGENT_PROXY_KEY");
        (proxy.url, proxy.api_key)
    });
    proxy_at(&url, &key)
}

fn proxy_at(url: &str, key: &str) -> (tempfile::TempDir, Proxy) {
    let (tmp, dirs) = dirs();
    point_at(&dirs, url, key);
    (tmp, Proxy::new(dirs))
}

fn user(text: &str) -> Message {
    Message::User(vec![Part::Text(text.into())])
}

async fn ask(proxy: &Proxy, model: &Model, messages: &[Message], tools: &[ToolSpec], effort: Option<&str>) -> (Reply, Vec<Chunk>) {
    let request =
        Request { model, system: "You are terse.", messages, tools, effort, max_output: Some(2000), cache_key: "dirigent-live-test" };
    let mut chunks = vec![];
    let reply = proxy.stream(&request, &mut |chunk| chunks.push(chunk), &CancellationToken::new()).await;
    assert_eq!(reply.message.error, None, "{:?}", reply.failure);
    (reply, chunks)
}

async fn calls_a_tool_and_reads_its_result(proxy: &Proxy, model: &Model) {
    let add = ToolSpec::new(
        "add",
        "Add two numbers.",
        json!({"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}, "required": ["a", "b"]}),
    );
    let mut messages = vec![user("Use the add tool to add 17 and 25, then tell me the sum.")];
    let (reply, chunks) = ask(proxy, model, &messages, std::slice::from_ref(&add), Some("low")).await;
    assert_eq!((reply.message.stop, reply.message.api, reply.message.model.as_str()), (Stop::ToolUse, model.api, HAIKU));
    let calls: Vec<_> = reply
        .message
        .content
        .iter()
        .filter_map(|block| if let Block::ToolCall { id, name, input } = block { Some((id, name, input)) } else { None })
        .collect();
    let [(id, name, input)] = calls.as_slice() else { panic!("one call: {:?}", reply.message.content) };
    assert_eq!((name.as_str(), input["a"].as_f64(), input["b"].as_f64()), ("add", Some(17.0), Some(25.0)));
    assert_eq!(chunks[0], Chunk::Start);
    assert_eq!(chunks.iter().filter(|chunk| matches!(chunk, Chunk::ToolCall { .. })).count(), 1);
    let usage = reply.message.usage;
    assert!(usage.prompt() > 0 && usage.output > 0 && usage.cost.total() > 0.0, "{usage:?}");

    let call_id = id.to_string();
    messages.push(Message::Assistant { content: reply.message.content, api: reply.message.api });
    messages.push(Message::ToolResult { call_id, content: vec![Part::Text("42".into())], is_error: false });
    let (reply, chunks) = ask(proxy, model, &messages, std::slice::from_ref(&add), Some("low")).await;
    assert_eq!(reply.message.stop, Stop::End);
    let text = text_of(&reply.message.content);
    assert!(text.contains("42"), "{text}");
    let streamed: String =
        chunks.iter().filter_map(|chunk| if let Chunk::Text { delta, .. } = chunk { Some(delta.as_str()) } else { None }).collect();
    assert_eq!(streamed, text);
}

async fn thinks_aloud_and_takes_it_back(proxy: &Proxy, model: &Model) {
    let puzzle = "A farmer has 17 pens with 23 sheep each, sells 40 sheep, then splits the rest evenly among 3 children. \
                  How many per child, how many left over? Reason carefully, answer in one line.";
    let mut messages = vec![user(puzzle)];
    let (reply, chunks) = ask(proxy, model, &messages, &[], Some("high")).await;
    assert_eq!(reply.message.stop, Stop::End);
    let thinking: Vec<_> = reply
        .message
        .content
        .iter()
        .filter_map(|block| if let Block::Thinking { text, signature, .. } = block { Some((text, signature)) } else { None })
        .collect();
    assert!(
        !thinking.is_empty() && thinking.iter().all(|(_, signature)| signature.is_some()),
        "signed thinking: {:?}",
        reply.message.content
    );
    let streamed: String =
        chunks.iter().filter_map(|chunk| if let Chunk::Thinking { delta, .. } = chunk { Some(delta.as_str()) } else { None }).collect();
    assert!(!streamed.trim().is_empty(), "the thinking's text streams");
    assert!(text_of(&reply.message.content).contains("117"));

    messages.push(Message::Assistant { content: reply.message.content, api: reply.message.api });
    messages.push(user("And with 4 children? One line."));
    let (reply, _) = ask(proxy, model, &messages, &[], Some("high")).await;
    assert_eq!(reply.message.stop, Stop::End);
    assert!(text_of(&reply.message.content).contains("87"), "{:?}", reply.message.content);
}

async fn reads_its_past_from_the_cache(proxy: &Proxy, model: &Model) {
    let rules: Vec<String> = (0..150).map(|i| format!("Rule {i} of run {}: the answer to question {i} is {}.", now_ms(), i * 7)).collect();
    let system = format!("You are terse.\n{}", rules.join("\n"));
    let tools = [ToolSpec::new(
        "bash",
        "Run a shell command.",
        json!({"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}),
    )];
    let cache_key = format!("dirigent-live-test-{}", now_ms());
    let mut messages = vec![user("Run `echo hi`.")];
    let mut usages = vec![];
    for step in 0..3 {
        let request = Request {
            model,
            system: &system,
            messages: &messages,
            tools: &tools,
            effort: Some("low"),
            max_output: Some(300),
            cache_key: &cache_key,
        };
        let reply = proxy.stream(&request, &mut |_| {}, &CancellationToken::new()).await;
        assert_eq!(reply.message.error, None);
        eprintln!("{:?} request {step}: {:?}", model.api, reply.message.usage);
        usages.push(reply.message.usage);
        if step == 0 {
            let call = Block::ToolCall { id: "toolu_01A".into(), name: "bash".into(), input: json!({"command": "echo hi"}) };
            messages.push(Message::Assistant { content: vec![call], api: model.api });
            messages.push(Message::ToolResult { call_id: "toolu_01A".into(), content: vec![Part::Text("hi".into())], is_error: false });
        } else {
            messages.push(Message::Assistant { content: vec![Block::text("It printed hi.")], api: model.api });
            messages.push(user("Say OK."));
        }
    }
    let [first, second, third] = usages.as_slice() else { unreachable!() };
    assert!(first.prompt() > 3000 && first.cache_read < first.prompt() / 2, "a new prompt is not read from the cache: {first:?}");
    assert!(second.cache_read > first.prompt() * 9 / 10 && second.input + second.cache_write < 200, "{second:?}");
    assert!(third.cache_read > second.cache_read && third.input + third.cache_write < 200, "{third:?}");
}

#[tokio::test]
#[ignore = "needs the proxy: see testing.md"]
async fn the_proxy_lists_its_models_with_prices() {
    let (tmp, proxy) = proxy();
    let models = proxy.models().await.unwrap();
    let haiku = models.iter().find(|model| model.id == HAIKU).expect("the proxy serves claude-haiku-5-5");
    assert_eq!((haiku.api, haiku.owner.as_str(), haiku.images), (Api::Anthropic, "anthropic", true));
    assert!(
        haiku.efforts.iter().any(|effort| effort == "medium") && haiku.context_window >= 200_000 && haiku.max_output >= 8_000,
        "{haiku:?}"
    );
    assert!(haiku.price.input > 0.0 && haiku.price.output > haiku.price.input && haiku.price.cache_read > 0.0, "{:?}", haiku.price);
    assert!(tmp.path().join("data/models.json").exists() && tmp.path().join("data/prices.json").exists());
    assert_eq!(proxy.model(HAIKU).await.unwrap(), *haiku);
    assert!(proxy.model("claude-nope-9").await.unwrap_err().to_string().contains("does not serve"));
}

#[tokio::test]
#[ignore = "needs the proxy: see testing.md"]
async fn the_anthropic_wire_carries_a_conversation() {
    let (_tmp, proxy) = proxy();
    let model = proxy.model(HAIKU).await.unwrap();
    assert_eq!(model.api, Api::Anthropic);
    calls_a_tool_and_reads_its_result(&proxy, &model).await;
    thinks_aloud_and_takes_it_back(&proxy, &model).await;
    reads_its_past_from_the_cache(&proxy, &model).await;
}

#[tokio::test]
#[ignore = "needs the proxy: see testing.md"]
async fn the_codex_wire_carries_a_conversation() {
    let (_tmp, proxy) = proxy();
    let model = Model { api: Api::Codex, ..proxy.model(HAIKU).await.unwrap() };
    calls_a_tool_and_reads_its_result(&proxy, &model).await;
    thinks_aloud_and_takes_it_back(&proxy, &model).await;
    reads_its_past_from_the_cache(&proxy, &model).await;
}

#[tokio::test]
#[ignore = "needs the proxy: see testing.md"]
async fn a_completion_and_an_abort() {
    let (_tmp, proxy) = proxy();
    let model = proxy.model(HAIKU).await.unwrap();
    let never = CancellationToken::new();
    let reply = crate::complete(&proxy, &model, "Answer with one word.", "The capital of France?", Some("low"), &never).await.unwrap();
    assert!(text_of(&reply.content).contains("Paris"), "{reply:?}");
    assert!(reply.usage.cost.total() > 0.0);

    let stop = CancellationToken::new();
    let mut streamed = String::new();
    let mut on = |chunk| {
        if let Chunk::Text { delta, .. } = chunk {
            streamed.push_str(&delta);
            stop.cancel();
        }
    };
    let messages = [user("Count from 1 to 300, one number per line.")];
    let request =
        Request { model: &model, system: "", messages: &messages, tools: &[], effort: Some("low"), max_output: Some(2000), cache_key: "" };
    let reply = proxy.stream(&request, &mut on, &stop).await;
    assert_eq!((reply.message.stop, &reply.message.error, reply.failure), (Stop::Aborted, &None, None));
    assert!(!streamed.is_empty() && text_of(&reply.message.content) == streamed && !streamed.contains("300"), "{streamed}");
}

#[tokio::test]
#[ignore = "needs the proxy: see testing.md"]
async fn failures_are_told_apart() {
    let (_tmp, proxy) = proxy();
    let haiku = proxy.model(HAIKU).await.unwrap();
    let never = CancellationToken::new();
    let failure = async |proxy: &Proxy, model: &Model, text: &str| {
        let messages = [user(text)];
        let request = Request { model, system: "", messages: &messages, tools: &[], effort: None, max_output: Some(16), cache_key: "" };
        let reply = proxy.stream(&request, &mut |_| {}, &never).await;
        assert_eq!(reply.message.stop, Stop::Error);
        (reply.failure.unwrap(), reply.message.error.unwrap())
    };

    let url = config::Proxy::load(&proxy.inner.dirs).unwrap().url;
    let (_tmp, stranger) = proxy_at(&url, "not-a-key");
    let (kind, message) = failure(&stranger, &haiku, "hi").await;
    assert_eq!(kind, Failure::Unauthorized, "{message}");
    assert!(message.contains("Settings › Proxy"), "{message}");
    assert_eq!(stranger.models().await.unwrap_err().downcast_ref::<Error>().unwrap().failure, Failure::Unauthorized);

    for api in [Api::Anthropic, Api::Codex] {
        let (kind, message) = failure(&proxy, &Model { id: "claude-nope-9".into(), api, ..haiku.clone() }, "hi").await;
        assert_eq!(kind, Failure::Other, "{message}");
    }

    let small = proxy.model("claude-haiku-4-5-20251001").await.unwrap();
    let long = "word zeta ".repeat(150_000);
    for api in [Api::Anthropic, Api::Codex] {
        let (kind, message) = failure(&proxy, &Model { api, ..small.clone() }, &long).await;
        assert_eq!(kind, Failure::Overflow, "{message}");
    }
}
