//! Wire types between `dirigent_desktop` and `dirigent_service` (JSON over WebSocket).
//!
//! Requests carry an `id`, replies carry `reply_to`, pushes have neither.
//! Pi entries and events stay opaque `serde_json::Value`s so this crate is pi-version-agnostic.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptMode {
    /// Start a run (queued as follow-up if one is already running).
    Prompt,
    /// Inject after the current assistant turn.
    Steer,
    /// Deliver after the current run finishes.
    FollowUp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    ListProjects {
        id: u64,
    },
    /// Register a directory on this host as a project. The user is responsible for it existing.
    AddProject {
        id: u64,
        path: String,
    },
    /// Soft delete; the project's threads stay and become orphans.
    RemoveProject {
        id: u64,
        project_id: String,
    },
    ListThreads {
        id: u64,
    },
    GetThread {
        id: u64,
        thread_id: String,
    },
    /// `workspace` starts the thread in an isolated git worktree / jj workspace of the
    /// project instead of its directory, checked out at `base` (anything git / jj resolve:
    /// a branch, bookmark, commit, revset; default `HEAD` / the parents of `@`).
    /// `parent` makes it a sub-agent of that thread: it starts in the parent's cwd (unless
    /// `workspace`), and the parent is told when it finishes. `run` ties it to a workflow run.
    CreateThread {
        id: u64,
        project_id: String,
        #[serde(default)]
        workspace: bool,
        #[serde(default)]
        base: Option<String>,
        #[serde(default)]
        parent: Option<String>,
        #[serde(default)]
        run: Option<String>,
    },
    /// Move a thread into its own workspace at `base` (or recreate one that went missing,
    /// where its branch is resumed and `base` is ignored). Refused while the thread runs
    /// or the project's working copy is dirty.
    MoveToWorkspace {
        id: u64,
        thread_id: String,
        #[serde(default)]
        base: Option<String>,
    },
    /// Take the thread back to its project directory. `detach` leaves the workspace on
    /// disk (the thread just forgets it); otherwise it is removed, uncommitted changes
    /// included. The git branch stays either way.
    RemoveWorkspace {
        id: u64,
        thread_id: String,
        detach: bool,
    },
    /// Version control state of a directory on this host (see `ServerMessage::VcsStatus`).
    VcsStatus {
        id: u64,
        path: String,
    },
    /// `model` ("provider/id") and `thinking_level` switch the thread before sending, if they differ.
    /// `images` are attached to the message; pi resizes them to the model's limits.
    /// `title` (sent with a thread's first prompt) asks the host to name the thread from `text`.
    /// `tools` names the dirigent tools (`async_bash`, `ask_user`) the model gets from now on;
    /// the host remembers them for the thread. `None` leaves them as they are.
    Prompt {
        id: u64,
        thread_id: String,
        text: String,
        mode: PromptMode,
        model: Option<String>,
        thinking_level: Option<String>,
        #[serde(default)]
        images: Vec<ImageAttachment>,
        #[serde(default)]
        title: Option<TitleGen>,
        #[serde(default)]
        tools: Option<Vec<String>>,
    },
    /// The user's answers to an `ask_user` call (see `ThreadSummary::asks`): one list per
    /// question with the picked options and / or typed text, or `None` when dismissed.
    /// The host hands them to the model, starting a turn if it is idle.
    Answer {
        id: u64,
        thread_id: String,
        ask_id: String,
        answers: Option<Vec<Vec<String>>>,
    },
    /// Name `text` the way a thread's first prompt would be named (the settings page's `Test`).
    GenerateTitle {
        id: u64,
        title: TitleGen,
        text: String,
    },
    Abort {
        id: u64,
        thread_id: String,
    },
    /// Drop one steer / follow-up (by text) that pi has queued but not yet handed to the model.
    CancelQueued {
        id: u64,
        thread_id: String,
        text: String,
    },
    /// Hide a finished thread from the inbox (or bring it back). It stays replayable.
    ArchiveThread {
        id: u64,
        thread_id: String,
        archived: bool,
    },
    /// Snooze: out of the sidebar until the thread's next run (or until woken by hand).
    SnoozeThread {
        id: u64,
        thread_id: String,
        snoozed: bool,
    },
    /// Ask every client to open the thread (an agent calling the user's attention to it).
    FocusThread {
        id: u64,
        thread_id: String,
    },
    /// Full-text search over every thread's messages; best hit per thread, best first.
    SearchThreads {
        id: u64,
        query: String,
        #[serde(default = "default_limit")]
        limit: u64,
    },
    /// The workflows a project has (`.dirigent/workflows/*.ts`), described by running them.
    ListWorkflows {
        id: u64,
        project_id: String,
    },
    /// Start a workflow with `input`; `parent` is the thread that asked for it and is told
    /// when it ends.
    RunWorkflow {
        id: u64,
        project_id: String,
        name: String,
        #[serde(default)]
        input: Value,
        #[serde(default)]
        parent: Option<String>,
    },
    ListRuns {
        id: u64,
    },
    /// One run with its steps and log.
    GetRun {
        id: u64,
        run_id: String,
    },
    /// A workflow run reporting what it did (from the runner's SDK).
    ReportStep {
        id: u64,
        run_id: String,
        step: Value,
    },
    AbortRun {
        id: u64,
        run_id: String,
    },
    /// Permanently remove a thread and its entries.
    DeleteThread {
        id: u64,
        thread_id: String,
    },
    /// Models this host has credentials for.
    ListModels {
        id: u64,
    },
    /// The bytes behind an image block's `blob` id (see `ServerMessage::Entry`).
    GetBlob {
        id: u64,
        blob_id: String,
    },
    /// Token / cost usage of every assistant message with `since <= timestamp < until`,
    /// summed per `bucket_ms`-wide slot (counted from `since`) and per provider + model.
    ListUsage {
        id: u64,
        since: i64,
        until: i64,
        bucket_ms: i64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Hello {
        host_name: String,
        version: String,
    },
    /// Reply to `ListProjects`, or pushed (`reply_to: None`) whenever a project changes.
    Projects {
        reply_to: Option<u64>,
        projects: Vec<Project>,
    },
    ProjectAdded {
        reply_to: u64,
        project: Project,
    },
    /// Reply to `ListThreads`, or pushed (`reply_to: None`) whenever a thread changes.
    Threads {
        reply_to: Option<u64>,
        threads: Vec<ThreadSummary>,
    },
    Thread {
        reply_to: u64,
        thread_id: String,
        header: Value,
        entries: Vec<Entry>,
    },
    ThreadCreated {
        reply_to: u64,
        thread: ThreadSummary,
    },
    Ok {
        reply_to: u64,
    },
    Error {
        reply_to: Option<u64>,
        message: String,
    },
    /// Pushed to all clients whenever an entry is persisted for a thread. Image blocks in
    /// entries carry `blob` (a content hash) instead of `data`; fetch them with `GetBlob`.
    Entry {
        thread_id: String,
        entry: Entry,
    },
    /// Pi `AgentSessionEvent`, forwarded as JSON.
    Event {
        thread_id: String,
        event: Value,
    },
    Models {
        reply_to: u64,
        models: Vec<ModelInfo>,
    },
    /// Base64 bytes of a blob.
    Blob {
        reply_to: u64,
        blob_id: String,
        mime_type: String,
        data: String,
    },
    Usage {
        reply_to: u64,
        buckets: Vec<UsageBucket>,
    },
    /// Reply to `VcsStatus`: `vcs` is `None` for a directory under neither git nor jj (or
    /// one that doesn't exist); `dirty` means uncommitted changes (for jj: `@` is not
    /// empty); `branch` is the git branch or the jj change of `@`; `refs` are the names a
    /// workspace can start from (git branches and tags, jj bookmarks).
    VcsStatus {
        reply_to: u64,
        path: String,
        vcs: Option<Vcs>,
        dirty: bool,
        branch: Option<String>,
        #[serde(default)]
        refs: Vec<String>,
    },
    /// Reply to `GenerateTitle`: the title and what producing it cost.
    Title {
        reply_to: u64,
        title: String,
        model: String,
        input: u64,
        output: u64,
        cost: f64,
        duration_ms: i64,
    },
    /// Pushed on `FocusThread`: open this thread.
    Focus {
        thread_id: String,
    },
    Search {
        reply_to: u64,
        hits: Vec<SearchHit>,
    },
    Workflows {
        reply_to: u64,
        project_id: String,
        workflows: Vec<Workflow>,
    },
    /// Reply to `ListRuns`, or pushed (`reply_to: None`) when a run starts or ends.
    Runs {
        reply_to: Option<u64>,
        runs: Vec<WorkflowRun>,
    },
    RunStarted {
        reply_to: u64,
        run: WorkflowRun,
    },
    /// Reply to `GetRun`.
    Run {
        reply_to: u64,
        run: WorkflowRun,
        steps: Vec<RunStep>,
        log: String,
    },
    /// Pushed whenever a run reports a step.
    Step {
        run_id: String,
        step: RunStep,
    },
}

fn default_limit() -> u64 {
    20
}

/// One thread matching a `SearchThreads` query, with the best matching passage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    pub thread_id: String,
    pub snippet: String,
}

/// A workflow script as its project declares it (see the runtime's `workflow.ts`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workflow {
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub input: Vec<Param>,
    /// The script could not be loaded / has no valid default export.
    #[serde(default)]
    pub error: Option<String>,
}

/// One declared workflow input: `kind` is `string`, `number` or `boolean`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Param {
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub default: Option<Value>,
}

/// A workflow run: a `bun` process the host started for a project's script.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowRun {
    pub id: String,
    pub project_id: Option<String>,
    pub name: String,
    pub input: Value,
    /// The thread that started it, told when it ends.
    pub parent_id: Option<String>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub exit_code: Option<i64>,
    pub steps: u64,
    /// The steps the script takes against placeholder results (`Dirigent.plan`), taken
    /// before it started: what the run is expected to do, filled in by its real steps.
    #[serde(default)]
    pub plan: Vec<Value>,
}

impl WorkflowRun {
    pub fn running(&self) -> bool {
        self.ended_at.is_none()
    }
}

/// What a run reported (see the SDK's `step` kinds: `spawn`, `send`, `wait`, `log`, ...).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunStep {
    pub seq: u64,
    pub at: i64,
    pub json: Value,
}

/// A version control system a directory is under. Jj wins over git in a colocated repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Vcs {
    Git,
    Jj,
}

/// Questions the model put to the user with `ask_user`, waiting for an answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ask {
    pub id: String,
    pub questions: Vec<Question>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Question {
    pub text: String,
    /// Choices; empty means a free-text answer.
    #[serde(default)]
    pub options: Vec<String>,
    /// A text field next to the options.
    #[serde(default)]
    pub allow_custom: bool,
    /// Several options may be picked.
    #[serde(default)]
    pub multi: bool,
}

/// How to name a thread from its first prompt: the model (`provider/id`), pi thinking level
/// and system prompt. Comes from the desktop's `config.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TitleGen {
    pub model: String,
    pub reasoning: String,
    pub prompt: String,
}

/// Usage summed over one time slot for one model (see `ClientMessage::ListUsage`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageBucket {
    /// Unix millis the slot starts at: `since + n * bucket_ms`.
    pub start: i64,
    pub provider: String,
    pub model: String,
    /// Fresh input tokens; `cache_read` / `cache_write` are the rest of the prompt.
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// Dollars, as pi prices them.
    pub cost: f64,
    /// Assistant messages summed into this bucket.
    pub messages: u64,
}

/// An image sent with a prompt: base64 bytes and their mime type (`image/png`, ...).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageAttachment {
    pub mime_type: String,
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub path: String,
    /// Normalized git `origin` URL, if the directory is a git repo. Used by the desktop to auto-link.
    pub origin: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub context_window: u64,
    pub thinking_levels: Vec<String>,
}

impl ModelInfo {
    /// `provider/id`, the form used everywhere a model is referenced by string.
    pub fn key(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadSummary {
    pub id: String,
    /// Missing when the project was removed (orphan thread).
    pub project_id: Option<String>,
    pub cwd: String,
    pub name: Option<String>,
    /// Current `provider/id` and thinking level, from the latest change entries.
    pub model: Option<String>,
    pub thinking_level: Option<String>,
    /// Unix millis.
    pub created_at: i64,
    pub updated_at: i64,
    pub entry_count: u64,
    /// Unix millis the current run started, while an agent is working on the thread.
    pub running_since: Option<i64>,
    /// How long the last finished run took, in millis.
    pub last_run_ms: Option<i64>,
    pub archived: bool,
    /// `ask_user` calls waiting for the user, oldest first.
    #[serde(default)]
    pub asks: Vec<Ask>,
    /// The thread's isolated workspace (`cwd` points into it while attached).
    #[serde(default)]
    pub workspace: Option<String>,
    /// The workspace directory is gone (removed outside dirigent); prompts would fail.
    #[serde(default)]
    pub workspace_missing: bool,
    /// The thread that spawned this one as a sub-agent.
    #[serde(default)]
    pub parent_id: Option<String>,
    /// The workflow run that spawned this one.
    #[serde(default)]
    pub run_id: Option<String>,
    /// Snoozed and not run since (see `ClientMessage::SnoozeThread`).
    #[serde(default)]
    pub snoozed: bool,
}

impl ThreadSummary {
    pub fn running(&self) -> bool {
        self.running_since.is_some()
    }

    /// The model is waiting on the user.
    pub fn asking(&self) -> bool {
        !self.asks.is_empty()
    }

    /// Not spawned by another thread or a workflow run: what the desktop's lists show.
    /// Sub-agents are reached through their parent's orchestration panel.
    pub fn top_level(&self) -> bool {
        self.parent_id.is_none() && self.run_id.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// Append order within the thread, starting at 0.
    pub seq: u64,
    pub id: String,
    pub parent_id: Option<String>,
    /// Pi entry `type` (`message`, `model_change`, ...).
    pub kind: String,
    /// The full pi entry.
    pub json: Value,
}

impl Entry {
    /// Build from a raw pi session entry; `seq` is assigned by storage.
    pub fn from_pi(seq: u64, json: Value) -> Self {
        let s = |k: &str| json.get(k).and_then(Value::as_str).map(str::to_owned);
        Self { seq, id: s("id").unwrap_or_default(), parent_id: s("parentId"), kind: s("type").unwrap_or_default(), json }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn roundtrip<T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug>(v: T) {
        let s = serde_json::to_string(&v).unwrap();
        assert_eq!(serde_json::from_str::<T>(&s).unwrap(), v, "{s}");
    }

    #[test]
    fn client_messages() {
        roundtrip(ClientMessage::ListProjects { id: 1 });
        roundtrip(ClientMessage::AddProject { id: 1, path: "/x".into() });
        roundtrip(ClientMessage::RemoveProject { id: 1, project_id: "p".into() });
        roundtrip(ClientMessage::ListThreads { id: 1 });
        roundtrip(ClientMessage::GetThread { id: 2, thread_id: "t".into() });
        roundtrip(ClientMessage::CreateThread {
            id: 3,
            project_id: "p".into(),
            workspace: true,
            base: Some("main~2".into()),
            parent: Some("t0".into()),
            run: None,
        });
        roundtrip(ClientMessage::SnoozeThread { id: 17, thread_id: "t".into(), snoozed: true });
        roundtrip(ClientMessage::FocusThread { id: 18, thread_id: "t".into() });
        roundtrip(ClientMessage::SearchThreads { id: 19, query: "sqlite".into(), limit: 5 });
        roundtrip(ClientMessage::ListWorkflows { id: 20, project_id: "p".into() });
        roundtrip(ClientMessage::RunWorkflow {
            id: 21,
            project_id: "p".into(),
            name: "review".into(),
            input: json!({"pr": 4}),
            parent: None,
        });
        roundtrip(ClientMessage::ListRuns { id: 22 });
        roundtrip(ClientMessage::GetRun { id: 23, run_id: "r".into() });
        roundtrip(ClientMessage::ReportStep { id: 24, run_id: "r".into(), step: json!({"kind": "log", "text": "hi"}) });
        roundtrip(ClientMessage::AbortRun { id: 25, run_id: "r".into() });
        roundtrip(ClientMessage::MoveToWorkspace { id: 14, thread_id: "t".into(), base: None });
        roundtrip(ClientMessage::RemoveWorkspace { id: 15, thread_id: "t".into(), detach: true });
        roundtrip(ClientMessage::VcsStatus { id: 16, path: "/x".into() });
        roundtrip(ClientMessage::Prompt {
            id: 4,
            thread_id: "t".into(),
            text: "hi".into(),
            mode: PromptMode::FollowUp,
            model: Some("a/b".into()),
            thinking_level: None,
            images: vec![ImageAttachment { mime_type: "image/png".into(), data: "AAAA".into() }],
            title: Some(TitleGen { model: "a/c".into(), reasoning: "low".into(), prompt: "name it".into() }),
            tools: Some(vec!["async_bash".into()]),
        });
        roundtrip(ClientMessage::Answer {
            id: 13,
            thread_id: "t".into(),
            ask_id: "a1".into(),
            answers: Some(vec![vec!["postgres".into()], vec![]]),
        });
        roundtrip(ClientMessage::GenerateTitle {
            id: 12,
            title: TitleGen { model: "a/c".into(), reasoning: "low".into(), prompt: "name it".into() },
            text: "hi".into(),
        });
        roundtrip(ClientMessage::Abort { id: 5, thread_id: "t".into() });
        roundtrip(ClientMessage::CancelQueued { id: 9, thread_id: "t".into(), text: "x".into() });
        roundtrip(ClientMessage::ArchiveThread { id: 7, thread_id: "t".into(), archived: true });
        roundtrip(ClientMessage::DeleteThread { id: 8, thread_id: "t".into() });
        roundtrip(ClientMessage::ListModels { id: 6 });
        roundtrip(ClientMessage::GetBlob { id: 10, blob_id: "abc".into() });
        roundtrip(ClientMessage::ListUsage { id: 11, since: 0, until: 86_400_000, bucket_ms: 3_600_000 });
        assert_eq!(
            serde_json::to_value(ClientMessage::Prompt {
                id: 4,
                thread_id: "t".into(),
                text: "hi".into(),
                mode: PromptMode::Steer,
                model: None,
                thinking_level: None,
                images: vec![],
                title: None,
                tools: None,
            })
            .unwrap(),
            json!({"type":"prompt","id":4,"thread_id":"t","text":"hi","mode":"steer","model":null,"thinking_level":null,"images":[],"title":null,"tools":null})
        );
    }

    /// Fields added after a host or client shipped are optional on the wire, so the two
    /// keep talking across an upgrade.
    #[test]
    fn added_fields_are_optional() {
        let t: ThreadSummary = serde_json::from_value(json!({
            "id":"t","project_id":null,"cwd":"/x","name":null,"model":null,"thinking_level":null,
            "created_at":1,"updated_at":2,"entry_count":0,"running_since":null,"last_run_ms":null,"archived":false
        }))
        .unwrap();
        assert!(!t.asking());
        assert_eq!(t.workspace, None);
        let c: ClientMessage = serde_json::from_value(json!({"type":"create_thread","id":1,"project_id":"p"})).unwrap();
        assert_eq!(c, ClientMessage::CreateThread { id: 1, project_id: "p".into(), workspace: false, base: None, parent: None, run: None });
        let s: ClientMessage = serde_json::from_value(json!({"type":"search_threads","id":1,"query":"x"})).unwrap();
        assert_eq!(s, ClientMessage::SearchThreads { id: 1, query: "x".into(), limit: 20 });
        assert!(!t.snoozed);
        assert_eq!(t.parent_id, None);
        let q: Question = serde_json::from_value(json!({"text":"db?"})).unwrap();
        assert_eq!(q, Question { text: "db?".into(), options: vec![], allow_custom: false, multi: false });
    }

    #[test]
    fn server_messages() {
        let t = ThreadSummary {
            id: "t".into(),
            project_id: Some("p".into()),
            cwd: "/x".into(),
            name: None,
            model: None,
            thinking_level: None,
            created_at: 1,
            updated_at: 2,
            entry_count: 3,
            running_since: Some(5),
            last_run_ms: Some(7),
            archived: false,
            asks: vec![Ask {
                id: "a1".into(),
                questions: vec![Question { text: "db?".into(), options: vec!["pg".into()], allow_custom: true, multi: false }],
            }],
            workspace: Some("/ws/t".into()),
            workspace_missing: false,
            parent_id: Some("t0".into()),
            run_id: None,
            snoozed: true,
        };
        assert!(t.asking());
        let run = WorkflowRun {
            id: "r".into(),
            project_id: Some("p".into()),
            name: "review".into(),
            input: json!({"pr": 4}),
            parent_id: None,
            started_at: 1,
            ended_at: None,
            exit_code: None,
            steps: 2,
            plan: vec![json!({"kind":"start"})],
        };
        assert!(run.running());
        roundtrip(ServerMessage::Focus { thread_id: "t".into() });
        roundtrip(ServerMessage::Search { reply_to: 1, hits: vec![SearchHit { thread_id: "t".into(), snippet: "…sqlite…".into() }] });
        roundtrip(ServerMessage::Workflows {
            reply_to: 1,
            project_id: "p".into(),
            workflows: vec![Workflow {
                name: "review".into(),
                path: "/x/.dirigent/workflows/review.ts".into(),
                description: "d".into(),
                input: vec![Param { name: "pr".into(), kind: "number".into(), description: "".into(), default: None }],
                error: None,
            }],
        });
        roundtrip(ServerMessage::Runs { reply_to: None, runs: vec![run.clone()] });
        roundtrip(ServerMessage::RunStarted { reply_to: 2, run: run.clone() });
        let step = RunStep { seq: 0, at: 5, json: json!({"kind":"log","text":"hi"}) };
        roundtrip(ServerMessage::Run { reply_to: 3, run, steps: vec![step.clone()], log: "out".into() });
        roundtrip(ServerMessage::Step { run_id: "r".into(), step });
        let p = Project { id: "p".into(), path: "/x".into(), origin: Some("github.com/a/b".into()), created_at: 1 };
        let m =
            ModelInfo { provider: "a".into(), id: "b".into(), name: "B".into(), context_window: 1, thinking_levels: vec!["low".into()] };
        assert_eq!(m.key(), "a/b");
        roundtrip(ServerMessage::Projects { reply_to: None, projects: vec![p.clone()] });
        roundtrip(ServerMessage::ProjectAdded { reply_to: 1, project: p });
        roundtrip(ServerMessage::Models { reply_to: 1, models: vec![m] });
        let e = Entry::from_pi(0, json!({"type":"message","id":"a","parentId":null,"message":{"role":"user","content":"hi"}}));
        assert_eq!(e.kind, "message");
        assert_eq!(e.parent_id, None);
        roundtrip(ServerMessage::Hello { host_name: "h".into(), version: "0".into() });
        roundtrip(ServerMessage::Threads { reply_to: None, threads: vec![t.clone()] });
        roundtrip(ServerMessage::Thread {
            reply_to: 1,
            thread_id: "t".into(),
            header: json!({"type":"session"}),
            entries: vec![e.clone()],
        });
        roundtrip(ServerMessage::ThreadCreated { reply_to: 2, thread: t });
        roundtrip(ServerMessage::Title {
            reply_to: 3,
            title: "Fix it".into(),
            model: "a/c".into(),
            input: 1,
            output: 2,
            cost: 0.5,
            duration_ms: 9,
        });
        roundtrip(ServerMessage::Ok { reply_to: 3 });
        roundtrip(ServerMessage::Error { reply_to: Some(4), message: "boom".into() });
        roundtrip(ServerMessage::Entry { thread_id: "t".into(), entry: e });
        roundtrip(ServerMessage::Event { thread_id: "t".into(), event: json!({"type":"agent_start"}) });
        roundtrip(ServerMessage::Blob { reply_to: 5, blob_id: "abc".into(), mime_type: "image/png".into(), data: "AAAA".into() });
        roundtrip(ServerMessage::VcsStatus {
            reply_to: 7,
            path: "/x".into(),
            vcs: Some(Vcs::Jj),
            dirty: true,
            branch: Some("main".into()),
            refs: vec!["main".into()],
        });
        assert_eq!(serde_json::to_value(Vcs::Jj).unwrap(), json!("jj"));
        roundtrip(ServerMessage::Usage {
            reply_to: 6,
            buckets: vec![UsageBucket {
                start: 0,
                provider: "anthropic".into(),
                model: "m".into(),
                input: 1,
                output: 2,
                cache_read: 3,
                cache_write: 4,
                cost: 0.5,
                messages: 1,
            }],
        });
    }
}
