//! A live thread: its branch of the history in memory, the messages waiting for the model,
//! its jobs and its tools. The host loads one when a thread has work (`Agent::load`) and
//! drops it when it has been idle for a while; everything in it can be built again from the
//! thread's entries, except what runs (jobs end with their session).
//!
//! What reaches the model comes in one of three ways: a prompt starts a run (`run.rs`); a
//! steer is queued for the run in progress and goes in after its current tool calls; a
//! follow-up is queued for when the run would stop. Notices (a job ended, a sub-agent
//! ended, the user's answers to an ask) take the same ways, and start a run when there is
//! none.

use crate::codemode::{Namespace, ScriptHost};
use crate::mcp::Servers;
use crate::prompt::Prompt as SystemPrompt;
use crate::tool::{Call, Tool, ToolOutput, ToolSpec};
use crate::tools::bash::{AskUser, AsyncBash, Bash};
use crate::tools::computer::{COMPUTER, Computer};
use crate::tools::files::{Edit, Read, Write};
use crate::tools::shell::Jobs;
use crate::tools::{Env, images};
use crate::{Host, Llm, Out};
use anyhow::{Result, anyhow, bail, ensure};
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use dirigent_protocol::transcript::{Block, Body, Entry, Event, Notice, ToolResult, branch};
use dirigent_protocol::{Ask, ImageAttachment, Notify, PromptMode};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

/// The tools a thread has only while their switch is on (`Prompt::tools`); `bash_background`
/// is an option of `async_bash`, not a tool.
const SWITCHED: [&str; 5] = ["async_bash", "ask_user", "dirigent", COMPUTER, CODEMODE];
pub(crate) const CODEMODE: &str = "codemode";
/// How long a request, or a script, waits for MCP servers that are still connecting.
const SERVERS_WAIT: Duration = Duration::from_secs(10);
/// Images a reply links to are shown up to this size.
const MAX_LINKED_IMAGE: u64 = 16 * 1024 * 1024;

/// A thread as its host hands it over to be loaded.
#[derive(Debug, Clone, Default)]
pub struct Thread {
    pub id: String,
    pub cwd: String,
    /// Every entry of the thread, in `seq` order.
    pub entries: Vec<Entry>,
    /// The model (its id) and reasoning effort it last ran with, if it ran.
    pub model: Option<String>,
    pub effort: Option<String>,
    /// The tool switches that are on (`async_bash`, `ask_user`, `dirigent`, `computer`,
    /// `codemode`) and the option `bash_background`.
    pub tools: Vec<String>,
}

/// A message for the thread's model.
#[derive(Debug, Clone, Default)]
pub struct Prompt {
    pub text: String,
    pub images: Vec<ImageAttachment>,
    /// How it reaches a model that is working; one that isn't gets it as a prompt whatever this says.
    pub mode: Option<PromptMode>,
    /// The model and effort from now on; `None` leaves them.
    pub model: Option<String>,
    pub effort: Option<String>,
    /// The tool switches from now on; `None` leaves them.
    pub tools: Option<Vec<String>>,
    /// The id of a user message this one replaces: the thread goes back to just before it.
    pub edit: Option<String>,
}

/// Something waiting for the model.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Queued {
    /// `text` is how the queue names it to clients (`Event::Queue`), and how one is cancelled.
    User {
        text: String,
        content: Vec<Block>,
    },
    Notice(Notice),
}

pub(crate) struct State {
    /// The thread's current branch: what the model sees and new entries extend.
    pub branch: Vec<Entry>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub switches: Vec<String>,
    pub steering: VecDeque<Queued>,
    pub follow_up: VecDeque<Queued>,
    /// The run in progress: what stops it.
    pub run: Option<CancellationToken>,
}

/// One more in `count` for as long as it is held, however that ends: what counts may be
/// dropped midway (a prompt whose sender went, a tool call a stopped run let go of).
pub(crate) struct Counted<'a>(&'a watch::Sender<usize>);

impl<'a> Counted<'a> {
    pub(crate) fn new(count: &'a watch::Sender<usize>) -> Self {
        count.send_modify(|n| *n += 1);
        Self(count)
    }
}

impl Drop for Counted<'_> {
    fn drop(&mut self) {
        self.0.send_modify(|n| *n -= 1);
    }
}

/// A `wait` in progress, one of its session's `waiters` for as long as it is held.
struct Waiting<'a> {
    session: &'a Session,
    waiter: mpsc::UnboundedSender<String>,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.session.waiters.lock().unwrap().retain(|w| !w.same_channel(&self.waiter));
    }
}

pub struct Session {
    pub env: Arc<Env>,
    pub(crate) llm: Arc<dyn Llm>,
    pub(crate) prompt: SystemPrompt,
    /// `read`, `bash`, `edit`, `write`; then the switched ones, the host's among them. By name.
    tools: Vec<(String, Arc<dyn Tool>)>,
    /// The MCP servers, whose tools come and go with their connections.
    pub mcp: Servers,
    /// The `computer` tool, which is one of `tools`: the screens it opened close with the
    /// session, and when its switch goes off.
    computer: Arc<Computer>,
    /// The names of the host's tools among them.
    host_tools: Vec<String>,
    pub jobs: Jobs,
    pub(crate) state: Mutex<State>,
    /// The `wait` calls in progress: each is told what reached the model's queue.
    waiters: Mutex<Vec<mpsc::UnboundedSender<String>>>,
    /// Whether the session takes messages yet: not while it captures the nix dev shell.
    ready: watch::Sender<bool>,
    /// Nothing comes of an unloaded session any more: its thread may be gone.
    unloaded: AtomicBool,
    /// The images of the branch as requests send them, by blob id: mime type and base64.
    /// Those of the part a compaction summarized are forgotten (`keep_images`).
    images: Mutex<HashMap<String, (String, String)>>,
    /// How many calls of the host's tools are in flight (`wait` lets them end first).
    pub(crate) host_calls: watch::Sender<usize>,
    /// How many prompts are on their way in (`prompt`): the session is busy with them.
    prompts: watch::Sender<usize>,
    /// When something last came of the session.
    active: Mutex<std::time::Instant>,
}

impl Session {
    pub(crate) fn new(
        dirs: &crate::config::Dirs,
        host: Arc<dyn Host>,
        llm: Arc<dyn Llm>,
        out: mpsc::UnboundedSender<(String, Out)>,
        thread: Thread,
    ) -> Arc<Self> {
        let cwd = std::path::PathBuf::from(&thread.cwd);
        let env = Arc::new(Env {
            thread_id: thread.id.clone(),
            cwd: cwd.clone(),
            dirs: dirs.clone(),
            host: host.clone(),
            out,
            shell_env: Default::default(),
        });
        let branch = match thread.entries.last() {
            Some(last) => branch(&thread.entries, &last.id).into_iter().cloned().collect(),
            None => vec![],
        };
        let state = State {
            branch,
            model: thread.model,
            effort: thread.effort,
            switches: thread.tools,
            steering: VecDeque::new(),
            follow_up: VecDeque::new(),
            run: None,
        };
        let nix = !cfg!(windows) && cwd.join("flake.nix").exists();
        let warn = {
            let (out, id) = (env.out.clone(), thread.id.clone());
            move |message| drop(out.send((id.clone(), Out::Event(Event::Warning { message }))))
        };
        let mcp = Servers::start(dirs, &cwd, warn);
        let session = Arc::new_cyclic(|this: &Weak<Session>| {
            let mut tools: Vec<Arc<dyn Tool>> = vec![
                Arc::new(Read::new(env.clone())),
                Arc::new(Bash::new(this.clone())),
                Arc::new(Edit::new(env.clone())),
                Arc::new(Write::new(env.clone())),
                Arc::new(AsyncBash::new(this.clone())),
                Arc::new(AskUser::new(this.clone())),
                crate::codemode::tool(Arc::new(Scripts(this.clone()))),
            ];
            let host_tools = host.tools(&thread.id);
            let named = |tools: &[Arc<dyn Tool>]| -> Vec<String> { tools.iter().map(|t| t.spec().name).collect() };
            let host_names = named(&host_tools);
            tools.extend(host_tools);
            let computer = Arc::new(Computer::new(this.clone()));
            tools.push(computer.clone());
            let tools = named(&tools).into_iter().zip(tools).collect();
            Session {
                computer,
                host_tools: host_names,
                active: Mutex::new(std::time::Instant::now()),
                prompt: SystemPrompt::load(dirs, &cwd),
                env,
                llm,
                tools,
                mcp,
                jobs: Jobs::default(),
                state: Mutex::new(state),
                waiters: Mutex::default(),
                ready: watch::channel(!nix).0,
                unloaded: AtomicBool::new(false),
                images: Mutex::default(),
                host_calls: watch::channel(0).0,
                prompts: watch::channel(0).0,
            }
        });
        if nix {
            // Prompts wait for it (`ready`); clients show the thread activating it meanwhile.
            session.emit(Out::NixShell(true));
            let this = session.clone();
            tokio::spawn(async move {
                match nix_env(&this.env.cwd).await {
                    Ok(env) => *this.env.shell_env.write().unwrap() = env,
                    Err(e) => this.warn(format!("nix develop failed: {e}")),
                }
                this.emit(Out::NixShell(false));
                this.ready.send_replace(true);
            });
        }
        session
    }

    /// Hand what came of the session to its host.
    pub(crate) fn emit(&self, out: Out) {
        if !self.unloaded.load(Ordering::Relaxed) {
            *self.active.lock().unwrap() = std::time::Instant::now();
            let _ = self.env.out.send((self.env.thread_id.clone(), out));
        }
    }

    /// How long nothing has come of the session.
    pub(crate) fn idle_for(&self) -> Duration {
        self.active.lock().unwrap().elapsed()
    }

    pub(crate) fn is_host_tool(&self, name: &str) -> bool {
        self.host_tools.iter().any(|t| t == name)
    }

    pub(crate) fn warn(&self, message: String) {
        self.emit(Out::Event(Event::Warning { message }));
    }

    /// Whether tool switch `name` is on.
    pub fn is_on(&self, name: &str) -> bool {
        self.state.lock().unwrap().switches.iter().any(|s| s == name)
    }

    /// The tools the thread has now, but for `codemode` when `scripts` is off: as the
    /// model is told of them, or as a script is (which doesn't call itself).
    fn specs(&self, scripts: bool) -> Vec<ToolSpec> {
        // Not under the state's lock: codemode's spec lists the tools a script can call.
        let switches = self.state.lock().unwrap().switches.clone();
        let on = |name: &str| !SWITCHED.contains(&name) || switches.iter().any(|s| s == name);
        let own = self.tools.iter().filter(|(name, _)| on(name) && (scripts || name != CODEMODE)).map(|(_, tool)| tool.spec());
        own.chain(self.mcp.tools().iter().map(|tool| tool.spec())).collect()
    }

    /// The tool called `name`, if the thread has it now.
    pub(crate) fn tool(&self, name: &str) -> Option<Arc<dyn Tool>> {
        match self.tools.iter().find(|(n, _)| n == name) {
            Some((_, tool)) => (!SWITCHED.contains(&name) || self.is_on(name)).then(|| tool.clone()),
            None => self.mcp.tools().into_iter().find(|tool| tool.spec().name == name),
        }
    }

    /// Wait, but not for long, for the MCP servers a request needs: all of them for a
    /// script (`scripts`), or a thread that calls their tools itself; else those whose
    /// tools the model is always told of.
    pub(crate) async fn servers_ready(&self, scripts: bool) {
        self.mcp.ready(!scripts && self.is_on(CODEMODE), SERVERS_WAIT).await;
    }

    /// The MCP servers whose tools only the thread's scripts call, for the system prompt.
    pub(crate) fn scripted_servers(&self) -> &[(String, Option<String>)] {
        if self.is_on(CODEMODE) { self.mcp.scripted() } else { &[] }
    }

    /// The tools the model is told of: all the thread has, but for those only scripts call
    /// (`ToolSpec::deferred`) while it has codemode to call them with.
    pub(crate) fn declared(&self) -> Vec<ToolSpec> {
        let scripts = self.is_on(CODEMODE);
        self.specs(true).into_iter().filter(|spec| !(scripts && spec.deferred)).collect()
    }

    /// Wait until the session takes messages. An error once it is unloaded.
    async fn ready(&self) -> Result<()> {
        let mut ready = self.ready.subscribe();
        let _ = ready.wait_for(|ready| *ready).await;
        ensure!(!self.unloaded.load(Ordering::Relaxed), "the thread was unloaded");
        Ok(())
    }

    /// Add an entry to the branch and hand it to the host. Under the state's lock, so entries
    /// reach the host in the order they were said.
    pub(crate) fn append(&self, state: &mut State, body: Body) -> Entry {
        let parent_id = state.branch.last().map(|e| e.id.clone());
        // Long enough that no two of a thread are ever the same: nothing checks that they aren't.
        let entry = Entry { seq: 0, id: crate::short_id(16), parent_id, at: crate::now(), body };
        state.branch.push(entry.clone());
        self.emit(Out::Entry(entry.clone()));
        entry
    }

    /// Give the model a message. Returns once it has it: its run started, or the message is
    /// queued behind the run in progress. An error means it has nothing of it (the model
    /// isn't served, the thread runs and can't be edited), and why.
    pub async fn prompt(self: &Arc<Self>, prompt: Prompt) -> Result<()> {
        // Until the model has it, or it is refused: the session is not idle meanwhile (`busy`).
        let _on_its_way = Counted::new(&self.prompts);
        self.ready().await?;
        *self.active.lock().unwrap() = std::time::Instant::now();
        // No model, no message: asked before anything of it is kept.
        let model = prompt.model.clone().or_else(|| self.state.lock().unwrap().model.clone()).ok_or_else(|| anyhow!("no model picked"))?;
        self.llm.model(&model).await?;
        let (text, content) = self.attach(&prompt.text, &prompt.images).await;
        let mut state = self.state.lock().unwrap();
        ensure!(!self.unloaded.load(Ordering::Relaxed), "the thread was unloaded");
        ensure!(prompt.edit.is_none() || state.run.is_none(), "the thread is running; stop it first");
        state.model = Some(model);
        state.effort = prompt.effort.or(state.effort.take());
        if let Some(tools) = prompt.tools {
            // `computer` going off also closes the screens the thread has open.
            if !tools.iter().any(|tool| tool == COMPUTER) {
                let (computer, mcp) = (self.computer.clone(), self.mcp.clone());
                tokio::spawn(async move { computer.close_all(&mcp).await });
            }
            state.switches = tools;
        }
        if state.run.is_some() {
            let steer = prompt.mode == Some(PromptMode::Steer);
            let queue = if steer { &mut state.steering } else { &mut state.follow_up };
            queue.push_back(Queued::User { text, content });
            self.emit_queue(&state);
            if steer {
                // The model is meant to see it now, not after a `wait` times out.
                self.wake("a message came in");
            }
            return Ok(());
        }
        if let Some(edit) = &prompt.edit {
            let at = state.branch.iter().position(|e| e.id == *edit && matches!(e.body, Body::User { .. }));
            state.branch.truncate(at.ok_or_else(|| anyhow!("not a user message: {edit}"))?);
        }
        self.append(&mut state, Body::User { content });
        self.start(&mut state);
        Ok(())
    }

    /// Begin a run on the branch as it is. Not in a session that was unloaded: its thread
    /// may be gone, and nobody would hear of the run.
    fn start(self: &Arc<Self>, state: &mut State) {
        if self.unloaded.load(Ordering::Relaxed) {
            return;
        }
        let cancel = CancellationToken::new();
        state.run = Some(cancel.clone());
        self.emit(Out::Event(Event::RunStart));
        tokio::spawn(crate::run::run(self.clone(), cancel));
    }

    /// Tell the model something nobody typed: queued for the run in progress (after its
    /// current tool calls, or `WhenIdle` once it would stop), else it starts a run.
    pub async fn notify(self: &Arc<Self>, notice: Notice, when: Notify) -> Result<()> {
        self.ready().await?;
        ensure!(self.state.lock().unwrap().model.is_some(), "the thread has no model yet");
        ensure!(self.deliver(notice, when), "the thread was unloaded");
        Ok(())
    }

    /// `notify` from inside the session, which is ready and has run. `false`: the session
    /// was unloaded (a job of it may end only then), and nothing comes of the notice.
    pub(crate) fn deliver(self: &Arc<Self>, notice: Notice, when: Notify) -> bool {
        let mut state = self.state.lock().unwrap();
        if self.unloaded.load(Ordering::Relaxed) {
            return false;
        }
        if state.run.is_none() {
            self.append(&mut state, Body::Notice(notice));
            self.start(&mut state);
            return true;
        }
        // A model that waits has no work to interrupt: every notice reaches it now.
        if when == Notify::Asap || !self.waiters.lock().unwrap().is_empty() {
            let line = first_line(notice.text());
            state.steering.push_back(Queued::Notice(notice));
            self.wake(&line);
        } else {
            state.follow_up.push_back(Queued::Notice(notice));
        }
        true
    }

    /// The user's answers to an `ask_user` call (one list per question, the picked options
    /// and / or typed text), or `None` when they dismissed it: tell the model.
    pub async fn answer(self: &Arc<Self>, ask: Ask, answers: Option<Vec<Vec<String>>>) -> Result<()> {
        let text = match &answers {
            // Each answer on a line of its own: joined, a typed "Kiwi, sliced" would read as two picks.
            Some(answers) => {
                let lines: Vec<String> = ask
                    .questions
                    .iter()
                    .enumerate()
                    .map(|(i, q)| {
                        let got: Vec<String> = answers.get(i).into_iter().flatten().map(|a| format!("  → {a}")).collect();
                        format!("- {}\n{}", q.text, if got.is_empty() { "  (no answer)".into() } else { got.join("\n") })
                    })
                    .collect();
                format!("The user answered ask {}:\n{}", ask.id, lines.join("\n"))
            }
            None => format!("The user dismissed ask {} without answering.", ask.id),
        };
        self.notify(Notice::Answer { text, ask_id: ask.id, questions: ask.questions, answers }, Notify::Asap).await
    }

    /// The user stopped the run. The jobs the model waits on go with it: left running, the
    /// end of one would start a new run, and the user would have to stop that too.
    pub fn abort(&self) {
        if let Some(run) = &self.state.lock().unwrap().run {
            run.cancel();
        }
        self.jobs.kill(true);
        self.report_jobs();
    }

    /// Drop one queued steer or follow-up, by its text.
    pub fn cancel_queued(&self, text: &str) {
        let mut state = self.state.lock().unwrap();
        let state = &mut *state;
        // A client's echo of a message lacks the image links its text got here (`attach`).
        let is = |q: &Queued| matches!(q, Queued::User { text: t, .. } if t == text || t.starts_with(&format!("{text}\n\n[image-")));
        for queue in [&mut state.steering, &mut state.follow_up] {
            if let Some(at) = queue.iter().position(is) {
                queue.remove(at);
                break;
            }
        }
        self.emit_queue(state);
    }

    /// Tell clients which of the user's messages wait.
    pub(crate) fn emit_queue(&self, state: &State) {
        let texts = |queue: &VecDeque<Queued>| -> Vec<String> {
            queue.iter().filter_map(|q| if let Queued::User { text, .. } = q { Some(text.clone()) } else { None }).collect()
        };
        self.emit(Out::Event(Event::Queue { steering: texts(&state.steering), follow_up: texts(&state.follow_up) }));
    }

    /// Drop the session: its run stops where it is, its jobs are killed, the ones left
    /// running too, and its servers and screens close. Nothing more comes of it.
    pub(crate) fn unload(self: &Arc<Self>) {
        self.end(false);
        let this = self.clone();
        tokio::spawn(async move { this.close().await });
    }

    /// What `unload` does at once: nothing more comes of the session, its run stops, its
    /// jobs are killed. `now`: with no grace, and none starts any more (`Jobs::kill_now`),
    /// for a host that is about to exit.
    pub(crate) fn end(&self, now: bool) {
        {
            // Under the lock runs start under: one is either refused or stopped here.
            let state = self.state.lock().unwrap();
            self.unloaded.store(true, Ordering::Relaxed);
            if let Some(run) = &state.run {
                run.cancel();
            }
        }
        if now {
            self.jobs.kill_now()
        } else {
            self.jobs.kill(false)
        }
        // Whoever waits for it to get ready finds it gone.
        self.ready.send_replace(true);
    }

    /// Close what a session that ended holds: its MCP servers and its screens.
    pub(crate) async fn close(&self) {
        // The servers first: none is added after that, so no screen opens once they are closed.
        self.mcp.close().await;
        self.computer.close_all(&self.mcp).await;
    }

    /// Whether the session does anything that unloading would end: a run, jobs, or a
    /// message on its way to the model (a prompt being taken in, or anything that waits
    /// for the session to get ready).
    pub fn busy(&self) -> bool {
        let taking_in = *self.prompts.borrow() > 0 || !*self.ready.borrow();
        taking_in || self.state.lock().unwrap().run.is_some() || self.jobs.running() != (0, 0)
    }

    /// Tell the host how many jobs run. Called wherever that changes.
    pub(crate) fn report_jobs(&self) {
        let (running, free) = self.jobs.running();
        self.emit(Out::Jobs { running, free });
    }

    /// End every `wait` in progress: `why` reached the model's queue.
    fn wake(&self, why: &str) {
        for waiter in self.waiters.lock().unwrap().iter() {
            let _ = waiter.send(why.to_owned());
        }
    }

    /// `async_bash wait`: block until something reaches the model's queue (a notice of a job,
    /// a sub-agent or an ask, or a steer), `timeout` passes or the run is stopped, and say
    /// which. The notice itself is queued and follows the tool's result. Returns at once
    /// when something is queued already, or when nothing could ever come.
    pub(crate) async fn wait(&self, timeout: Duration, cancel: &CancellationToken) -> String {
        // Registered before anything is looked at: what comes in meanwhile wakes it. Until
        // the wait ends, or is let go of (a stopped run drops a call that takes too long):
        // a waiter left behind would make a steer of every later notice (`deliver`).
        let (waiter, mut woken) = mpsc::unbounded_channel();
        self.waiters.lock().unwrap().push(waiter.clone());
        let _waiting = Waiting { session: self, waiter };
        let started = std::time::Instant::now();
        let waited = || crate::tools::shell::duration(started.elapsed().as_millis() as i64);
        let outcome = async {
            // The model calls tools side by side: a sub-agent spawned in the same batch counts.
            let mut host_calls = self.host_calls.subscribe();
            let _ = host_calls.wait_for(|n| *n == 0).await;
            let busy = !self.jobs.awaited().is_empty() || self.env.host.children_busy(&self.env.thread_id);
            let (now, later) = {
                let state = self.state.lock().unwrap();
                let line = |q: &Queued| match q {
                    Queued::Notice(n) => first_line(n.text()),
                    Queued::User { .. } => "a message came in".to_owned(),
                };
                let now: Vec<String> = state.steering.iter().map(line).collect();
                let later: Vec<String> = state.follow_up.iter().filter(|q| matches!(q, Queued::Notice(_))).map(line).collect();
                (now, later)
            };
            if !now.is_empty() || !later.is_empty() {
                let mut out = vec![];
                if !now.is_empty() {
                    out.push("Already queued for you (in full in the next message):".to_owned());
                    out.extend(now);
                }
                if !later.is_empty() {
                    out.push("Queued for when your turn ends (end it to get them):".to_owned());
                    out.extend(later);
                }
                return out.join("\n");
            }
            if !busy {
                return "Nothing to wait for: no jobs or sub-agents running that would wake you.".to_owned();
            }
            let why = woken.recv().await.unwrap_or_default();
            format!("Woke after {} on this (in full in the next message):\n{why}", waited())
        };
        // The stop and the timeout end it wherever it is, also while the host's calls go on.
        tokio::select! {
            // What is queued already is said even to a wait of no time at all.
            biased;
            text = outcome => text,
            _ = cancel.cancelled() => format!("wait stopped after {}", waited()),
            _ = tokio::time::sleep(timeout) => {
                let running: Vec<String> = self.jobs.awaited().iter().map(|j| j.describe(false)).collect();
                let still = if running.is_empty() { String::new() } else { format!(" Still running:\n{}", running.join("\n")) };
                format!("Nothing came in {}.{still}", waited())
            }
        }
    }

    /// A message's text and images as an entry's content. Each image is made fit to send
    /// (`images::process`), kept as a blob, and saved as a file under
    /// `<data dir>/attachments/` that the text then names: a markdown reference definition
    /// per image, `[image-N]: /path`, in the images' order, so the model sees which file each
    /// `[image-N]` is and can use it. Returns the text as the model reads it, and the content.
    async fn attach(&self, text: &str, attachments: &[ImageAttachment]) -> (String, Vec<Block>) {
        if attachments.is_empty() {
            return (text.to_owned(), vec![Block::text(text)]);
        }
        // The `N`s the text names, for images that come without theirs.
        let mut tokens: Vec<u32> = text.split("[image-").skip(1).filter_map(|rest| rest.split_once(']')?.0.parse().ok()).collect();
        tokens.sort_unstable();
        tokens.dedup();
        let dir = self.env.dirs.data.join("attachments");
        let (mut refs, mut notes, mut blocks) = (vec![], vec![], vec![]);
        for (i, attachment) in attachments.iter().enumerate() {
            let Ok(bytes) = BASE64.decode(&attachment.data) else {
                notes.push("[Image omitted: it did not arrive whole.]".to_owned());
                continue;
            };
            let mime = attachment.mime_type.clone();
            let original = bytes.clone();
            let image = match tokio::task::spawn_blocking(move || images::process(&bytes, &mime)).await {
                Ok(Ok(image)) => image,
                Ok(Err(why)) => {
                    notes.push(why);
                    continue;
                }
                Err(e) => {
                    notes.push(format!("[Image omitted: {e}.]"));
                    continue;
                }
            };
            let blob = self.env.host.put_blob(&image.mime, &image.data);
            // The file is the image as it was sent, whatever was made of it for the model.
            let ext = match attachment.mime_type.as_str() {
                "image/jpeg" => "jpg",
                "image/svg+xml" => "svg",
                other => other.rsplit('/').next().unwrap_or("bin"),
            };
            let path = dir.join(format!("{blob}.{ext}"));
            match std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, &original)) {
                Ok(()) => {
                    let n = attachment.n.or(tokens.get(i).copied()).unwrap_or(i as u32 + 1);
                    let path = path.to_string_lossy();
                    // A destination with spaces or parens (a Windows user dir) must be in <>.
                    let path = if path.contains([' ', '(', ')']) { format!("<{path}>") } else { path.into_owned() };
                    refs.push(format!("[image-{n}]: {path}"));
                }
                Err(e) => self.warn(format!("saving an attached image failed: {e}")),
            }
            notes.extend(image.notes);
            blocks.push(Block::Image { mime: image.mime, blob });
        }
        let paragraphs = [text.to_owned(), refs.join("\n"), notes.join("\n")];
        let text = paragraphs.into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join("\n\n");
        blocks.insert(0, Block::text(&text));
        (text, blocks)
    }

    /// An image's bytes as a request sends them: its mime type and base64.
    pub(crate) fn image(&self, blob: &str) -> Option<(String, String)> {
        // Not held while the host is asked: a host that panics there doesn't take the lock with it.
        if let Some(image) = self.images.lock().unwrap().get(blob) {
            return Some(image.clone());
        }
        let (mime, bytes) = self.env.host.blob(blob)?;
        let image = (mime, BASE64.encode(bytes));
        self.images.lock().unwrap().insert(blob.to_owned(), image.clone());
        Some(image)
    }

    /// Forget the images that are in none of `entries`, the part of the branch the model
    /// still sees: after a compaction, no request sends the others again.
    pub(crate) fn keep_images(&self, entries: &[Entry]) {
        let mut seen = HashSet::new();
        for entry in entries {
            let (Body::User { content } | Body::ToolResult(ToolResult { content, .. })) = &entry.body else { continue };
            seen.extend(content.iter().filter_map(|block| if let Block::Image { blob, .. } = block { Some(blob.as_str()) } else { None }));
        }
        self.images.lock().unwrap().retain(|blob, _| seen.contains(blob.as_str()));
    }

    /// For every markdown image in a reply's text that links to an image file on this
    /// machine (absolute, or relative to the thread's directory), keep the file as a blob
    /// and record it on the text block (`Block::Text::blobs`): what lets a client anywhere
    /// show it. The text stays as the model wrote it.
    pub(crate) fn link_images(&self, content: &mut [Block]) {
        for block in content {
            let Block::Text { text, blobs } = block else { continue };
            for target in dirigent_protocol::images::image_targets(text) {
                let target = &text[target];
                if blobs.contains_key(target) {
                    continue;
                }
                if let Some((mime, bytes)) = linked_image(&self.env.cwd, target) {
                    blobs.insert(target.to_owned(), self.env.host.put_blob(mime, &bytes));
                }
            }
        }
    }

    /// The values codemode scripts kept (`Body::Store`), as of the branch.
    pub(crate) fn stored(&self) -> BTreeMap<String, serde_json::Value> {
        let mut values = BTreeMap::new();
        for entry in &self.state.lock().unwrap().branch {
            let Body::Store { values: changed } = &entry.body else { continue };
            for (key, value) in changed {
                match value {
                    serde_json::Value::Null => values.remove(key),
                    value => values.insert(key.clone(), value.clone()),
                };
            }
        }
        values
    }
}

/// A session as its codemode scripts reach it.
struct Scripts(Weak<Session>);

#[async_trait]
impl ScriptHost for Scripts {
    fn tools(&self) -> Vec<ToolSpec> {
        self.0.upgrade().map(|session| session.specs(false)).unwrap_or_default()
    }

    async fn call(&self, name: &str, input: serde_json::Value, call: &Call) -> ToolOutput {
        let Some(session) = self.0.upgrade() else { return ToolOutput::error("the thread was unloaded") };
        match session.tool(name).filter(|_| name != CODEMODE) {
            Some(tool) => session.call_tool(&tool, name, input, call).await,
            None => ToolOutput::error(format!("Tool {name} not found")),
        }
    }

    fn namespace(&self, name: &str) -> Option<Namespace> {
        self.0.upgrade()?.mcp.namespace(name)
    }

    fn stored(&self) -> BTreeMap<String, serde_json::Value> {
        self.0.upgrade().map(|session| session.stored()).unwrap_or_default()
    }

    fn store(&self, values: BTreeMap<String, serde_json::Value>) {
        if let Some(session) = self.0.upgrade() {
            session.append(&mut session.state.lock().unwrap(), Body::Store { values });
        }
    }
}

/// The image file a reply links to as `target`, if it is one to show: its mime type and bytes.
fn linked_image(cwd: &Path, target: &str) -> Option<(&'static str, Vec<u8>)> {
    if target.is_empty() || target.contains("://") || target.starts_with("blob:") || target.starts_with("data:") {
        return None;
    }
    let path = cwd.join(target);
    let mime = match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => return None,
    };
    if std::fs::metadata(&path).ok()?.len() > MAX_LINKED_IMAGE {
        return None;
    }
    Some((mime, std::fs::read(&path).ok()?))
}

/// A notice's first line, for `wait` to say what woke it.
fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or("");
    match line.char_indices().nth(200) {
        Some((at, _)) => format!("{}…", &line[..at]),
        None => line.to_owned(),
    }
}

/// The environment of the project's nix dev shell: `nix develop <cwd> --command env -0`.
async fn nix_env(cwd: &Path) -> Result<HashMap<String, String>> {
    let out = tokio::process::Command::new("nix").arg("develop").arg(cwd).args(["--command", "env", "-0"]).current_dir(cwd).output().await;
    let out = match out {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!("flake.nix found but nix is not on PATH"),
        out => out?,
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("{}", stderr.trim().lines().last().unwrap_or("it failed"));
    }
    let vars = out.stdout.split(|b| *b == 0).filter_map(|kv| {
        let kv = String::from_utf8_lossy(kv);
        let (key, value) = kv.split_once('=')?;
        (!key.is_empty()).then(|| (key.to_owned(), value.to_owned()))
    });
    Ok(vars.collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Does, Script, Step, call, prompt, session, session_with, text};
    use serde_json::json;

    /// `wait` ends at its timeout and at a stop also while a call of the host's goes on
    /// (which it lets end before it looks for sub-agents); one let go of midway is told of
    /// nothing any more; and a host's call that was let go of no longer holds one up.
    #[tokio::test]
    async fn a_wait_is_not_held_by_the_hosts_calls_and_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let hang = Does("hang", std::future::pending::<ToolOutput>);
        let (_, session) = session_with(dir.path(), Script::new(vec![]), vec![Arc::new(hang)]);
        let calling = tokio::spawn({
            let session = session.clone();
            let call = Call { id: "c".into(), cancel: CancellationToken::new(), script: true };
            async move { session.call_tool(&session.tool("hang").unwrap(), "hang", json!({}), &call).await }
        });
        session.host_calls.subscribe().wait_for(|n| *n == 1).await.unwrap();
        let (soon, late) = (Duration::from_millis(50), Duration::from_secs(60));
        let end = |timeout, cancel: CancellationToken| {
            let session = session.clone();
            async move { tokio::time::timeout(Duration::from_secs(5), session.wait(timeout, &cancel)).await.expect("the wait hangs") }
        };
        assert!(end(soon, CancellationToken::new()).await.starts_with("Nothing came in "));
        let stopped = CancellationToken::new();
        stopped.cancel();
        assert!(end(late, stopped).await.starts_with("wait stopped after "));
        assert!(tokio::time::timeout(soon, session.wait(late, &CancellationToken::new())).await.is_err());
        assert!(session.waiters.lock().unwrap().is_empty());
        // The call goes with the script that made it, as when a stopped run drops that.
        calling.abort();
        let _ = calling.await;
        assert_eq!(end(late, CancellationToken::new()).await, "Nothing to wait for: no jobs or sub-agents running that would wake you.");
    }

    #[tokio::test]
    async fn nothing_starts_in_a_session_that_was_unloaded() {
        let dir = tempfile::tempdir().unwrap();
        let (host, session) = session(dir.path(), Script::new(vec![]));
        session.unload();
        // As a job of it would, that ends only now.
        assert!(!session.deliver(Notice::Info { text: "late".into() }, Notify::Asap));
        assert_eq!(session.prompt(prompt("go")).await.unwrap_err().to_string(), "the thread was unloaded");
        assert!(!session.busy() && host.outs.lock().unwrap().is_empty());
    }

    /// The images a compaction summarized away are no longer kept for requests to send.
    #[tokio::test]
    async fn a_compaction_forgets_the_images_it_summarized() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::write(dir.path().join("config/agent.toml"), "[compaction]\nreserve = 50\nkeep_recent = 1\n").unwrap();
        // Every scripted reply says its request was 110 tokens: over the 100 this window leaves.
        let steps = vec![
            Step::Say(vec![call("c1", "bash", json!({ "command": "echo out" }))]),
            Step::Say(vec![text("a picture was shown")]),
            Step::Say(vec![text("done")]),
        ];
        let script = Script::with_window(steps, 150);
        let (host, session) = session(dir.path(), script.clone());
        let mut png = std::io::Cursor::new(vec![]);
        image::DynamicImage::new_rgb8(2, 2).write_to(&mut png, image::ImageFormat::Png).unwrap();
        let image = ImageAttachment { mime_type: "image/png".into(), data: BASE64.encode(png.into_inner()), n: None };
        session.prompt(Prompt { images: vec![image], ..prompt("look") }).await.unwrap();
        host.settled(1).await;
        // The first request sent the image; the compaction that followed summarized its message.
        let requests = script.requests.lock().unwrap();
        assert!(requests[0].0.contains("Image {") && !requests[2].0.contains("Image {"), "{requests:#?}");
        assert!(session.images.lock().unwrap().is_empty());
    }
}
