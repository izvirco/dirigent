use crate::Out;
use crate::config::Config;
use crate::context::{self, Plan};
use crate::proxy::{Chunk, Failure, Model, Reply, Request};
use crate::session::{Counted, Queued, Session, State};
use crate::tool::{Call, Content, Tool, ToolOutput};
use crate::tools::images;
use anyhow::Result;
use dirigent_protocol::transcript::{Api, Assistant, Block, Body, Event, Stop, ToolResult, Usage};
use futures_util::FutureExt;
use serde_json::Value;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

const MAX_RETRIES: u32 = 3;
const RETRY_DELAY: Duration = Duration::from_secs(2);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);
const TOOL_STOP_GRACE: Duration = Duration::from_secs(3);
const BAD_INPUT_CHARS: usize = 300;

pub(crate) async fn run(session: Arc<Session>, cancel: CancellationToken) {
    match AssertUnwindSafe(session.turns(&cancel)).catch_unwind().await {
        Ok(Ok(true)) => return,
        Ok(Ok(false)) => {}
        Ok(Err(e)) => session.close_turn(Some(format!("{e:#}"))),
        Err(panic) => session.close_turn(Some(format!("The agent hit a bug and stopped: {}", crate::panic_message(&*panic)))),
    }
    session.stop();
}

pub(crate) async fn with_retries<Fut>(
    cancel: &CancellationToken,
    mut on_retry: impl FnMut(u32, Duration, &str),
    mut request: impl FnMut() -> Fut,
) -> Reply
where
    Fut: Future<Output = Reply>,
{
    let mut attempt = 0;
    loop {
        let mut reply = request().await;
        let Some(Failure::Transient { retry_after }) = reply.failure else { return reply };
        if attempt >= MAX_RETRIES || cancel.is_cancelled() {
            return reply;
        }
        attempt += 1;
        let delay = retry_after.unwrap_or(RETRY_DELAY * 2u32.pow(attempt - 1)).min(MAX_RETRY_DELAY);
        on_retry(attempt, delay, reply.message.error.as_deref().unwrap_or("the request failed"));
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = cancel.cancelled() => {
                reply.message.stop = Stop::Aborted;
                reply.message.error = None;
                reply.failure = None;
                return reply;
            }
        }
    }
}

impl Session {
    // MUST KILL: type: turns returns a bool for "the run already ended in finish"; an enum (Ended / StoppedShort) says it
    async fn turns(self: &Arc<Self>, cancel: &CancellationToken) -> Result<bool> {
        loop {
            self.take_steers();
            let reply = self.turn(cancel).await?;
            if matches!(reply.stop, Stop::Error | Stop::Aborted) {
                return Ok(false);
            }
            let calls: Vec<(String, String, Value)> = reply
                .content
                .iter()
                .filter_map(|b| {
                    if let Block::ToolCall { id, name, input } = b { Some((id.clone(), name.clone(), input.clone())) } else { None }
                })
                .collect();
            if calls.is_empty() {
                if self.finish() {
                    return Ok(true);
                }
                continue;
            }
            let results = self.call_tools(calls, reply.stop == Stop::MaxTokens, cancel).await;
            {
                let mut state = self.state.lock().unwrap();
                for result in results {
                    self.append(&mut state, Body::ToolResult(result));
                }
            }
            if cancel.is_cancelled() {
                self.close_turn(None);
                return Ok(false);
            }
        }
    }

    fn take_steers(&self) {
        let mut state = self.state.lock().unwrap();
        if state.steering.is_empty() {
            return;
        }
        // MUST KILL: extract: take_steers' emit-the-queue-then-append order into a fn of its own, so clients never see a message twice
        let taken = std::mem::take(&mut state.steering);
        self.emit_queue(&state);
        for queued in taken {
            self.append_queued(&mut state, queued);
        }
    }

    fn append_queued(&self, state: &mut State, queued: Queued) {
        match queued {
            Queued::User { content, .. } => self.append(state, Body::User { content }),
            Queued::Notice(notice) => self.append(state, Body::Notice(notice)),
        };
    }

    fn finish(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        if !state.steering.is_empty() {
            return false;
        }
        if let Some(next) = state.follow_up.pop_front() {
            self.emit_queue(&state);
            self.append_queued(&mut state, next);
            return false;
        }
        state.run = None;
        self.emit(Out::Event(Event::RunEnd));
        true
    }

    // MUST KILL: rename: stop -> end_run_keeping_notices, it drops queued messages but keeps queued notices as entries
    fn stop(&self) {
        let mut state = self.state.lock().unwrap();
        let state = &mut *state;
        let waiting: Vec<Queued> = state.steering.drain(..).chain(state.follow_up.drain(..)).collect();
        self.emit_queue(state);
        for notice in waiting.into_iter().filter(|q| matches!(q, Queued::Notice(_))) {
            self.append_queued(state, notice);
        }
        state.run = None;
        self.emit(Out::Event(Event::RunEnd));
    }

    // MUST KILL: rename: close_turn -> append_stand_in_reply, it writes an Assistant entry in place of the reply that didn't come
    fn close_turn(&self, error: Option<String>) {
        let mut state = self.state.lock().unwrap();
        let closing = Assistant {
            content: vec![],
            model: state.model.clone().unwrap_or_default(),
            provider: String::new(),
            api: Api::Anthropic,
            effort: None,
            usage: Usage::default(),
            stop: if error.is_some() { Stop::Error } else { Stop::Aborted },
            error,
            duration_ms: 0,
        };
        self.append(&mut state, Body::Assistant(closing));
    }

    fn window(&self) -> Vec<dirigent_protocol::transcript::Entry> {
        context::window(&self.state.lock().unwrap().branch).to_vec()
    }

    async fn turn(&self, cancel: &CancellationToken) -> Result<Assistant> {
        let (model, effort) = {
            let state = self.state.lock().unwrap();
            (state.model.clone().unwrap_or_default(), state.effort.clone())
        };
        let model = self.llm.model(&model).await?;
        let config = Config::load(&self.env.dirs)?;
        let effort = effort.filter(|e| model.efforts.contains(e));
        let limit = model.context_window.saturating_sub(config.reserve(&model.id));
        let mut compacted = context::tokens(&self.window()) > limit && self.compact(&model, effort.as_deref(), &config, cancel).await;
        loop {
            self.servers_ready(false).await;
            let specs = self.declared();
            let system = self.prompt.system(&specs, self.scripted_servers());
            let messages = context::messages(&self.window(), &model.id, model.images, &mut |blob| self.image(blob));
            let request = Request {
                model: &model,
                system: &system,
                messages: &messages,
                tools: &specs,
                effort: effort.as_deref(),
                max_output: None,
                cache_key: &self.env.thread_id,
            };
            let started = Instant::now();
            let retry = |attempt: u32, delay: Duration, error: &str| {
                self.emit(Out::Event(Event::Retry { attempt, delay_ms: delay.as_millis() as u64, error: error.to_owned() }));
            };
            let reply = with_retries(cancel, retry, || async {
                let mut on_chunk = |chunk| {
                    self.emit(Out::Event(match chunk {
                        Chunk::Start => Event::MessageStart,
                        Chunk::Text { index, delta } => Event::Text { index, delta },
                        Chunk::Thinking { index, delta } => Event::Thinking { index, delta },
                        Chunk::ToolCall { index, id, name, input } => Event::ToolCall { index, id, name, input },
                    }));
                };
                self.llm.stream(&request, &mut on_chunk, cancel).await
            })
            .await;
            if reply.failure == Some(Failure::Overflow) && !compacted {
                compacted = true;
                if self.compact(&model, effort.as_deref(), &config, cancel).await {
                    continue;
                }
            }
            let mut message = reply.message;
            message.model = model.id.clone();
            message.provider = model.owner.clone();
            message.api = model.api;
            message.effort = effort.clone();
            message.duration_ms = started.elapsed().as_millis() as i64;
            self.link_images(&mut message.content);
            self.append(&mut self.state.lock().unwrap(), Body::Assistant(message.clone()));
            return Ok(message);
        }
    }

    async fn compact(&self, model: &Model, effort: Option<&str>, config: &Config, cancel: &CancellationToken) -> bool {
        let window = self.window();
        let Some(plan) = Plan::new(&window, config.compaction.keep_recent) else { return false };
        self.emit(Out::Event(Event::CompactionStart));
        let summary = plan.summarize(&*self.llm, model, effort, cancel).await;
        let done = summary.is_ok();
        match summary {
            Ok((summary, usage)) => {
                let body = Body::Compaction {
                    summary,
                    first_kept: plan.first_kept,
                    tokens_before: plan.tokens_before,
                    model: model.id.clone(),
                    usage,
                };
                self.append(&mut self.state.lock().unwrap(), body);
                self.keep_images(&self.window());
            }
            Err(_) if cancel.is_cancelled() => {}
            Err(e) => {
                self.append(&mut self.state.lock().unwrap(), Body::CompactionFailed { error: format!("Compaction failed: {e:#}") });
            }
        }
        self.emit(Out::Event(Event::CompactionEnd));
        done
    }

    async fn call_tools(&self, calls: Vec<(String, String, Value)>, cut_off: bool, cancel: &CancellationToken) -> Vec<ToolResult> {
        futures_util::future::join_all(calls.into_iter().map(|call| self.answer_call(call, cut_off, cancel))).await
    }

    async fn answer_call(&self, (id, name, input): (String, String, Value), cut_off: bool, cancel: &CancellationToken) -> ToolResult {
        self.emit(Out::Event(Event::ToolStart { id: id.clone() }));
        let started = Instant::now();
        let output = match self.tool(&name) {
            _ if cut_off => ToolOutput::error(format!(
                "Tool call \"{name}\" was not executed: the response hit the output token limit, so its arguments may be \
                 truncated. Re-issue the tool call with complete arguments."
            )),
            None => ToolOutput::error(format!("Tool {name} not found")),
            Some(_) if !input.is_object() => {
                let came = input.as_str().map_or_else(|| input.to_string(), str::to_owned);
                ToolOutput::error(format!(
                    "Tool call \"{name}\" was not executed: its arguments were not valid JSON. Re-issue the tool call with valid JSON \
                     arguments.\n\nReceived: {}",
                    came.chars().take(BAD_INPUT_CHARS).collect::<String>()
                ))
            }
            Some(tool) => self.call_tool(&tool, &name, input, &Call { id: id.clone(), cancel: cancel.clone(), script: false }).await,
        };
        self.emit(Out::Event(Event::ToolEnd { id: id.clone() }));
        ToolResult {
            call_id: id,
            tool: name,
            content: self.blocks(output.content).await,
            is_error: output.is_error,
            duration_ms: started.elapsed().as_millis() as i64,
        }
    }

    async fn blocks(&self, content: Vec<Content>) -> Vec<Block> {
        let mut blocks = vec![];
        for content in content {
            let (mime, data) = match content {
                Content::Text(text) => {
                    blocks.push(Block::text(text));
                    continue;
                }
                Content::Image { mime, data } => (mime, data),
            };
            match tokio::task::spawn_blocking(move || images::process(&data, &mime)).await {
                Ok(Ok(image)) => {
                    blocks.push(Block::Image { blob: self.env.host.put_blob(&image.mime, &image.data), mime: image.mime });
                    blocks.extend(image.notes.into_iter().map(Block::text));
                }
                Ok(Err(why)) => blocks.push(Block::text(why)),
                Err(e) => blocks.push(Block::text(format!("[Image omitted: {e}.]"))),
            }
        }
        blocks
    }

    pub(crate) async fn call_tool(&self, tool: &Arc<dyn Tool>, name: &str, input: Value, call: &Call) -> ToolOutput {
        let _host_call = self.is_host_tool(name).then(|| Counted::new(&self.host_calls));
        if name == crate::session::CODEMODE {
            self.servers_ready(true).await;
        }
        let stopped = async {
            call.cancel.cancelled().await;
            tokio::time::sleep(TOOL_STOP_GRACE).await;
        };
        tokio::select! {
            output = AssertUnwindSafe(tool.call(input, call)).catch_unwind() => match output {
                Ok(output) => output.unwrap_or_else(|e| ToolOutput::error(format!("{e:#}"))),
                Err(panic) => ToolOutput::error(format!("Tool {name} hit a bug in the agent: {}", crate::panic_message(&*panic))),
            },
            _ = stopped => ToolOutput::error("Operation aborted"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Prompt;
    use crate::testing::{Does, Script, Step, call, line, prompt, session, session_with, text};
    use dirigent_protocol::transcript::Notice;
    use dirigent_protocol::{Notify, PromptMode};
    use serde_json::json;

    fn bash(id: &str, command: &str) -> Block {
        call(id, "bash", json!({ "command": command }))
    }

    #[tokio::test]
    async fn a_run_goes_on_until_a_reply_calls_no_tool() {
        let dir = tempfile::tempdir().unwrap();
        let script = Script::new(vec![
            Step::Say(vec![text("looking"), bash("c1", "echo one"), bash("c2", "echo two; exit 3"), call("c3", "nope", json!({}))]),
            Step::Say(vec![text("done")]),
        ]);
        let (host, session) = session(dir.path(), script.clone());
        session.prompt(prompt("go")).await.unwrap();
        host.settled(1).await;
        assert_eq!(
            host.transcript(),
            [
                "user: go",
                r#"assistant: looking bash({"command":"echo one"}) bash({"command":"echo two; exit 3"}) nope({})"#,
                "result: one\n",
                "result!: two\n\n\nCommand exited with code 3",
                "result!: Tool nope not found",
                "assistant: done",
            ]
        );
        let entries = host.entries();
        assert!(entries.windows(2).all(|w| w[1].parent_id.as_deref() == Some(w[0].id.as_str())));
        assert!(script.requests.lock().unwrap()[1].0.contains("Command exited with code 3"));
        let events: Vec<Out> =
            host.outs.lock().unwrap().iter().filter(|o| matches!(o, Out::Event(Event::RunStart | Event::RunEnd))).cloned().collect();
        assert_eq!(events, [Out::Event(Event::RunStart), Out::Event(Event::RunEnd)]);
    }

    #[tokio::test]
    async fn what_comes_in_during_a_run_waits_its_turn() {
        let dir = tempfile::tempdir().unwrap();
        let script = Script::new(vec![
            Step::Say(vec![bash("c1", "sleep 0.3")]),
            Step::Say(vec![text("steered")]),
            Step::Say(vec![text("followed up")]),
            Step::Say(vec![text("noticed")]),
        ]);
        let (host, session) = session(dir.path(), script);
        session.prompt(prompt("go")).await.unwrap();
        host.until(|outs| outs.iter().any(|o| matches!(o, Out::Event(Event::ToolStart { .. })))).await;
        session.prompt(Prompt { mode: Some(PromptMode::FollowUp), ..prompt("and then?") }).await.unwrap();
        session.prompt(Prompt { mode: Some(PromptMode::Steer), ..prompt("this way") }).await.unwrap();
        session.deliver(Notice::Info { text: "by the way".into() }, Notify::Asap);
        host.settled(1).await;
        session.deliver(Notice::Info { text: "wake up".into() }, Notify::WhenIdle);
        host.settled(2).await;
        assert_eq!(
            host.transcript(),
            [
                "user: go",
                r#"assistant: bash({"command":"sleep 0.3"})"#,
                "result: (no output)",
                "user: this way",
                "notice: by the way",
                "assistant: steered",
                "user: and then?",
                "assistant: followed up",
                "notice: wake up",
                "assistant: noticed",
            ]
        );
        let order: Vec<String> = host
            .outs
            .lock()
            .unwrap()
            .iter()
            .filter_map(|o| match o {
                Out::Event(Event::Queue { steering, follow_up }) => Some(format!("queue: {} {}", steering.len(), follow_up.len())),
                Out::Entry(entry) if matches!(entry.body, Body::User { .. }) => Some(line(entry)),
                _ => None,
            })
            .collect();
        assert_eq!(order, ["user: go", "queue: 0 1", "queue: 1 1", "queue: 0 1", "user: this way", "queue: 0 0", "user: and then?"]);
    }

    #[tokio::test]
    async fn a_stopped_run_keeps_what_it_had_and_drops_what_waited() {
        let dir = tempfile::tempdir().unwrap();
        let script = Script::new(vec![Step::Hang, Step::Say(vec![text("fresh")])]);
        let (host, session) = session(dir.path(), script.clone());
        session.prompt(prompt("go")).await.unwrap();
        session.prompt(Prompt { mode: Some(PromptMode::FollowUp), ..prompt("never mind") }).await.unwrap();
        session.abort();
        host.settled(1).await;
        let refused = session.prompt(Prompt { model: Some("unserved".into()), ..prompt("x") }).await;
        assert_eq!(refused.unwrap_err().to_string(), "unserved is not a model the proxy serves");
        session.prompt(prompt("again")).await.unwrap();
        host.settled(2).await;
        assert_eq!(host.transcript(), ["user: go", "assistant: half a [aborted]", "user: again", "assistant: fresh"]);
        assert!(!script.requests.lock().unwrap()[1].0.contains("half a"));
    }

    #[tokio::test]
    async fn a_run_stopped_while_a_tool_runs_closes_its_turn() {
        let dir = tempfile::tempdir().unwrap();
        let script = Script::new(vec![Step::Say(vec![bash("c1", "sleep 30")]), Step::Say(vec![text("fresh")])]);
        let (host, session) = session(dir.path(), script.clone());
        session.prompt(prompt("go")).await.unwrap();
        host.until(|outs| outs.iter().any(|o| matches!(o, Out::Event(Event::ToolStart { .. })))).await;
        session.abort();
        host.settled(1).await;
        session.prompt(prompt("again")).await.unwrap();
        host.settled(2).await;
        assert_eq!(
            host.transcript(),
            [
                "user: go",
                r#"assistant: bash({"command":"sleep 30"})"#,
                "result!: Command aborted",
                "assistant: [aborted]",
                "user: again",
                "assistant: fresh"
            ]
        );
        let request = &script.requests.lock().unwrap()[1].0;
        assert!(request.contains("Command aborted") && request.matches("Assistant {").count() == 1, "{request}");
    }

    #[tokio::test]
    async fn a_notice_that_waited_for_a_stopped_run_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let script = Script::new(vec![Step::Hang, Step::Say(vec![text("fresh")])]);
        let (host, session) = session(dir.path(), script.clone());
        session.prompt(prompt("go")).await.unwrap();
        session.deliver(Notice::Info { text: "the tests passed".into() }, Notify::WhenIdle);
        session.abort();
        host.settled(1).await;
        assert_eq!(host.transcript(), ["user: go", "assistant: half a [aborted]", "notice: the tests passed"]);
        assert!(!session.busy());
        session.prompt(prompt("again")).await.unwrap();
        host.settled(2).await;
        assert_eq!(host.transcript()[3..], ["user: again", "assistant: fresh"]);
        assert!(script.requests.lock().unwrap()[1].0.contains("the tests passed"));
    }

    #[tokio::test]
    async fn a_panic_fails_the_run_and_a_panicking_tool_only_its_call() {
        let dir = tempfile::tempdir().unwrap();
        let script = Script::new(vec![
            Step::Say(vec![call("c1", "boom", json!({}))]),
            Step::Panic("the reply made no sense"),
            Step::Say(vec![text("fresh")]),
        ]);
        let boom = Does("boom", || async { panic!("no such key") });
        let (host, session) = session_with(dir.path(), script, vec![Arc::new(boom)]);
        session.prompt(prompt("go")).await.unwrap();
        host.settled(1).await;
        assert!(!session.busy());
        session.prompt(prompt("again")).await.unwrap();
        host.settled(2).await;
        assert_eq!(
            host.transcript(),
            [
                "user: go",
                "assistant: boom({})",
                "result!: Tool boom hit a bug in the agent: no such key",
                "assistant: [The agent hit a bug and stopped: the reply made no sense]",
                "user: again",
                "assistant: fresh"
            ]
        );
    }

    #[tokio::test]
    async fn the_images_of_a_tool_result_are_made_fit_to_send() {
        let dir = tempfile::tempdir().unwrap();
        let pictures = Does("pictures", || async {
            let mut png = std::io::Cursor::new(vec![]);
            image::DynamicImage::new_rgb8(4000, 1000).write_to(&mut png, image::ImageFormat::Png).unwrap();
            let big = Content::Image { mime: "image/png".into(), data: png.into_inner() };
            let odd = Content::Image { mime: "image/tiff".into(), data: b"II*".to_vec() };
            ToolOutput { content: vec![big, odd], ..ToolOutput::default() }
        });
        let script = Script::new(vec![Step::Say(vec![call("c1", "pictures", json!({}))]), Step::Say(vec![text("seen")])]);
        let (host, session) = session_with(dir.path(), script, vec![Arc::new(pictures)]);
        session.prompt(prompt("go")).await.unwrap();
        host.settled(1).await;
        let Body::ToolResult(result) = &host.entries()[2].body else { panic!("no result") };
        let [Block::Image { blob, .. }, Block::Text { text: scaled, .. }, Block::Text { text: omitted, .. }] = result.content.as_slice()
        else {
            panic!("{:?}", result.content)
        };
        let sent = image::load_from_memory(&crate::Host::blob(&*host, blob).unwrap().1).unwrap();
        assert_eq!((sent.width(), sent.height()), (2000, 500));
        assert!(scaled.starts_with("[Image: original 4000x1000, displayed at 2000x500."), "{scaled}");
        assert_eq!(omitted, "[Image omitted: could not be converted to a supported inline image format.]");
    }

    #[tokio::test]
    async fn a_call_whose_arguments_are_no_json_is_answered_and_not_run() {
        let dir = tempfile::tempdir().unwrap();
        let came = format!(r#"{{"command": "touch ran; echo {}"#, "x".repeat(400));
        let script = Script::new(vec![Step::Say(vec![call("c1", "bash", json!(came))]), Step::Say(vec![text("done")])]);
        let (host, session) = session(dir.path(), script);
        session.prompt(prompt("go")).await.unwrap();
        host.settled(1).await;
        let answer = format!(
            "result!: Tool call \"bash\" was not executed: its arguments were not valid JSON. Re-issue the tool call with valid JSON \
             arguments.\n\nReceived: {}",
            &came[..300]
        );
        assert_eq!(host.transcript()[2], answer);
        assert!(!dir.path().join("ran").exists());
    }

    #[tokio::test]
    async fn a_passing_failure_is_tried_again_and_a_lasting_one_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let soon = Failure::Transient { retry_after: Some(Duration::from_millis(1)) };
        let script =
            Script::new(vec![Step::Fail(soon, "overloaded"), Step::Say(vec![text("there")]), Step::Fail(Failure::Other, "no such thing")]);
        let (host, session) = session(dir.path(), script);
        session.prompt(prompt("go")).await.unwrap();
        host.settled(1).await;
        session.prompt(prompt("more")).await.unwrap();
        host.settled(2).await;
        assert_eq!(host.transcript(), ["user: go", "assistant: there", "user: more", "assistant: [no such thing]"]);
        let retries = host.outs.lock().unwrap().iter().filter(|o| matches!(o, Out::Event(Event::Retry { attempt: 1, .. }))).count();
        assert_eq!(retries, 1);
    }

    #[tokio::test]
    async fn a_context_near_the_window_is_compacted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::write(dir.path().join("config/agent.toml"), "[compaction]\nreserve = 50\nkeep_recent = 1\n").unwrap();
        let script = Script::with_window(
            vec![Step::Say(vec![bash("c1", "echo out")]), Step::Say(vec![text("what was asked")]), Step::Say(vec![text("done")])],
            150,
        );
        let (host, session) = session(dir.path(), script.clone());
        session.prompt(prompt("the first question")).await.unwrap();
        host.settled(1).await;
        let transcript = host.transcript();
        let kept = &host.entries()[1].id;
        assert_eq!(transcript.len(), 5, "{transcript:#?}");
        assert_eq!(
            transcript[3],
            format!("compaction: No prior history.\n\n---\n\n**Turn Context (split turn):**\n\nwhat was asked (kept from {kept})")
        );
        let requests = script.requests.lock().unwrap();
        assert!(requests[1].1.contains("context summarization assistant") && requests[1].0.contains("[User]: the first question"));
        assert!(requests[2].0.contains("compacted into the following summary") && !requests[2].0.contains("the first question"));
        assert!(requests[2].0.contains("echo out"));
    }

    #[tokio::test]
    async fn a_failed_compaction_is_told_and_the_run_goes_on() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::write(dir.path().join("config/agent.toml"), "[compaction]\nreserve = 50\nkeep_recent = 1\n").unwrap();
        let steps = vec![Step::Say(vec![bash("c1", "echo out")]), Step::Fail(Failure::Other, "overloaded"), Step::Say(vec![text("done")])];
        let script = Script::with_window(steps, 150);
        let (host, session) = session(dir.path(), script.clone());
        session.prompt(prompt("the first question")).await.unwrap();
        host.settled(1).await;
        assert_eq!(host.transcript()[3..], ["Compaction failed: overloaded", "assistant: done"]);
        assert!(script.requests.lock().unwrap()[2].0.contains("the first question"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn mcp_tools_are_the_models_or_its_scripts() {
        use crate::mcp::tests::{STDIO_SERVER, sh};
        let dir = tempfile::tempdir().unwrap();
        let dirs = crate::config::Dirs { config: dir.path().join("config"), data: dir.path().join("data") };
        Config::update(&dirs, |config| {
            config.mcp.insert("files".into(), sh(STDIO_SERVER));
            Ok(())
        })
        .unwrap();
        let script = Script::new(vec![Step::Say(vec![text("a")]), Step::Say(vec![text("b")])]);
        let (host, session) = session(dir.path(), script.clone());
        session.prompt(prompt("go")).await.unwrap();
        host.settled(1).await;
        session.prompt(Prompt { tools: Some(vec!["codemode".into()]), ..prompt("again") }).await.unwrap();
        host.settled(2).await;
        let requests = script.requests.lock().unwrap();
        let declared = |request: usize| requests[request].2.iter().any(|name| name == "mcp__files__echo");
        assert!(declared(0) && !requests[0].1.contains("<mcp_servers>"), "{:?}", requests[0].2);
        assert!(!declared(1) && requests[1].1.contains("\n- mcp__files\n"), "{:?}", requests[1].2);
    }

    #[tokio::test]
    async fn an_edit_takes_the_thread_back_to_before_the_message() {
        let dir = tempfile::tempdir().unwrap();
        let script = Script::new(vec![Step::Say(vec![text("one")]), Step::Say(vec![text("two")]), Step::Say(vec![text("three")])]);
        let (host, session) = session(dir.path(), script.clone());
        session.prompt(prompt("first")).await.unwrap();
        host.settled(1).await;
        session.prompt(prompt("second")).await.unwrap();
        host.settled(2).await;
        let second = host.entries()[2].clone();
        assert!(session.prompt(Prompt { edit: Some(host.entries()[1].id.clone()), ..prompt("x") }).await.is_err());
        session.prompt(Prompt { edit: Some(second.id.clone()), ..prompt("second, reworded") }).await.unwrap();
        host.settled(3).await;
        let entries = host.entries();
        assert_eq!(entries[4].parent_id, second.parent_id);
        let last = &script.requests.lock().unwrap()[2].0;
        assert!(last.contains("first") && last.contains("second, reworded") && !last.contains("two"));
    }

    #[tokio::test]
    async fn scripts_call_tools_and_keep_values() {
        let dir = tempfile::tempdir().unwrap();
        let first = r#"const out = await tools.bash({ command: "echo hi" }); store("seen", out.output.trim()); return out.exit_code;"#;
        let script = Script::new(vec![
            Step::Say(vec![call("c1", "codemode", json!({ "code": first }))]),
            Step::Say(vec![call("c2", "codemode", json!({ "code": "return load('seen') + '!'" }))]),
            Step::Say(vec![text("done")]),
        ]);
        let (host, session) = session(dir.path(), script.clone());
        session.prompt(Prompt { tools: Some(vec!["codemode".into()]), ..prompt("go") }).await.unwrap();
        host.settled(1).await;
        let transcript = host.transcript();
        assert_eq!(transcript.len(), 7, "{transcript:#?}");
        assert_eq!(transcript[2], r#"store: {"seen":"hi"}"#);
        assert!(transcript[3].starts_with("result: Script completed") && transcript[3].ends_with("\n0"), "{}", transcript[3]);
        assert!(transcript[5].ends_with("\nhi!"), "{}", transcript[5]);
        let requests = script.requests.lock().unwrap();
        assert!(requests[0].2.iter().any(|t| t == "codemode"), "{:?}", requests[0].2);
        assert!(requests[1].0.ends_with(r#"Output:\n0")], is_error: false }]"#), "{}", requests[1].0);
    }
}
