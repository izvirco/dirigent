//! Wire types between `dirigent_desktop` and `dirigent_service` (JSON over WebSocket).
//!
//! Requests carry an `id`, replies carry `reply_to`, pushes have neither.
//! Pi entries and events stay opaque `serde_json::Value`s so this crate is pi-version-agnostic.
//! `release` and `leaderboard` are the other wire: the update api's (`dirigent_api`).

pub mod leaderboard;
pub mod release;

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
    /// Register a directory on this host as a project: an absolute path, or one under `~/`
    /// (this host's home).
    AddProject {
        id: u64,
        path: String,
    },
    /// The subdirectories of a directory on this host (absolute or `~/…`), for the desktop's
    /// add-project browser.
    ListDirs {
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
    /// `workspace`), and the parent is told when it finishes: with what (`report`) and when
    /// (`notify`); both are ignored without a parent.
    /// `name` names it from the start (an agent names the threads it starts); a thread
    /// without one is titled from its first prompt.
    /// `prompt` sends the thread its first prompt in the same request (a `Prompt` in mode
    /// `prompt`), so it is listed running from the start, never idle meanwhile: if it can't
    /// be delivered the thread is deleted again and the request fails, so a client never
    /// ends up with a thread that silently never started. Until it is delivered the thread
    /// takes no other prompt.
    /// `thread_id` (a lowercase, hyphenated uuid) lets the client know the thread's id before
    /// the reply, to open it at once; the host picks one without it. A host from before this
    /// field ignores it: the reply's id is the one.
    CreateThread {
        id: u64,
        #[serde(default)]
        thread_id: Option<String>,
        project_id: String,
        #[serde(default)]
        workspace: bool,
        #[serde(default)]
        base: Option<String>,
        #[serde(default)]
        parent: Option<String>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        report: Report,
        #[serde(default)]
        notify: Notify,
        #[serde(default)]
        prompt: Option<FirstPrompt>,
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
    /// Rewrite one of the thread's user messages (`entry_id`, a pi `message` entry with role
    /// `user`) and run from there: the conversation goes back to just before it and `text`
    /// is sent in its place, as a `Prompt` in mode `prompt` with these fields would be. The
    /// old message and everything after it stay in the thread's history, on a branch of
    /// pi's session tree no client shows (see `Entry`). Refused while the thread runs. Not a
    /// field of `Prompt`: a host from before this refuses it instead of appending the text.
    EditMessage {
        id: u64,
        thread_id: String,
        entry_id: String,
        text: String,
        model: Option<String>,
        thinking_level: Option<String>,
        #[serde(default)]
        images: Vec<ImageAttachment>,
        #[serde(default)]
        tools: Option<Vec<String>>,
    },
    /// Start a new thread from the thread's history up to and including `entry_id` (that
    /// entry and its ancestors: the branch it is on). The fork is a top-level thread in
    /// the same project and directory, sharing its source's workspace without owning it,
    /// as a sub-agent does; it remembers where it came from (`ThreadSummary::forked_from`).
    /// `fork_id` is the new thread's id, as `CreateThread::thread_id`. Replies `ThreadCreated`.
    ForkThread {
        id: u64,
        thread_id: String,
        entry_id: String,
        #[serde(default)]
        fork_id: Option<String>,
    },
    /// The user's answers to an `ask_user` call (see `ThreadSummary::asks`): one list per
    /// question with the picked options and / or typed text, or `None` when dismissed.
    /// The host hands them to the model, starting a turn if it is idle. `silent` only drops
    /// the ask and tells the model nothing: the user answers in a message of their own (the
    /// form's `Insert`).
    Answer {
        id: u64,
        thread_id: String,
        ask_id: String,
        answers: Option<Vec<Vec<String>>>,
        #[serde(default)]
        silent: bool,
    },
    /// Name `text` the way a thread's first prompt would be named (Settings › Thread titles' `Test`).
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
    /// Name the thread by hand. Every client sees it, as it does a generated title.
    RenameThread {
        id: u64,
        thread_id: String,
        name: String,
    },
    /// Title the thread again from what it has become: its user messages and latest reply,
    /// not just its first prompt. `model` (`provider/id`) and `reasoning` are the `[title]`
    /// settings; the host has the prompt. Replies `Title`; every client sees the new name.
    RetitleThread {
        id: u64,
        thread_id: String,
        model: String,
        reasoning: String,
    },
    /// Snooze: out of the sidebar until the thread's next run (or until woken by hand).
    SnoozeThread {
        id: u64,
        thread_id: String,
        snoozed: bool,
    },
    /// Keep the thread on top of its status group in every client's lists.
    PinThread {
        id: u64,
        thread_id: String,
        pinned: bool,
    },
    /// Pin one of the thread's messages (by the pi entry id it came from; see
    /// `ThreadSummary::pinned_entries`).
    PinMessage {
        id: u64,
        thread_id: String,
        entry_id: String,
        pinned: bool,
    },
    /// The user looked at the thread as of `updated_at` (unread tracking, shared by every
    /// client; see `ThreadSummary::seen_at`). Only moves `seen_at` forward.
    MarkSeen {
        id: u64,
        thread_id: String,
        updated_at: i64,
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
    /// Permanently remove a thread and its entries. Refused while a `CreateThread` is still
    /// starting it: that request deletes it itself if it can't start.
    DeleteThread {
        id: u64,
        thread_id: String,
    },
    /// Models this host has credentials for.
    ListModels {
        id: u64,
    },
    /// The model providers this host's pi knows, and how each is set up (Settings › Providers).
    ListProviders {
        id: u64,
    },
    /// Sign in to a model provider on this host, as pi's `/login` does: with an account
    /// (`Oauth`) or a key (`ApiKey`: or whatever setup the provider asks for). Its steps
    /// come to this connection only, as `LoginStep`s under this request's id; the prompts
    /// among them are answered with `LoginAnswer`. The reply (`Ok`, or `Error`) ends it.
    /// The connection dropping cancels it.
    Login {
        id: u64,
        provider: String,
        method: AuthMethod,
    },
    /// The answer to prompt `prompt` of the login started by request `login` (see
    /// `LoginStep::Prompt`).
    LoginAnswer {
        id: u64,
        login: u64,
        prompt: u32,
        value: String,
    },
    /// Stop the login started by request `login`; its reply is an `Error`.
    LoginCancel {
        id: u64,
        login: u64,
    },
    /// Remove the credential a `Login` stored for the provider. Environment variables and
    /// pi's `models.json` stay as they are.
    Logout {
        id: u64,
        provider: String,
    },
    /// The other hosts this host's `hosts.toml` lists (what its agents can reach).
    ListHosts {
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
        /// Random, made once per service (its `service.db`): what the leaderboard counts a
        /// service's usage under, whichever desktops upload it. Empty from older services.
        #[serde(default)]
        service_id: String,
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
    /// Reply to `ListDirs`: the subdirectories (dot ones included), sorted by name.
    Dirs {
        reply_to: u64,
        dirs: Vec<Dir>,
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
        thread: Box<ThreadSummary>,
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
    /// Reply to `ListProviders`: by name; `auth_path` is the file logins are saved to.
    Providers {
        reply_to: u64,
        providers: Vec<ProviderInfo>,
        auth_path: String,
    },
    /// A step of the login started by request `login` (`ClientMessage::Login`), to the
    /// connection that started it.
    LoginStep {
        login: u64,
        step: LoginStep,
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
    /// Reply to `GenerateTitle` and `RetitleThread`: the title and what producing it cost.
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
    Hosts {
        reply_to: u64,
        hosts: Vec<HostInfo>,
    },
}

/// How to sign in to a model provider: with an account, or a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    Oauth,
    ApiKey,
}

/// A model provider and how it is set up on a host (see `ClientMessage::ListProviders`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
    /// Signing in with an account is offered: what that is ("Anthropic (Claude Pro/Max)").
    #[serde(default)]
    pub oauth: Option<String>,
    /// A key (or the provider's other setup: a profile, an account id) can be entered: what
    /// it is ("Anthropic API key").
    #[serde(default)]
    pub api_key: Option<String>,
    /// Where its credentials come from now ("Signed in", "From ANTHROPIC_API_KEY"); `None`:
    /// it has none, so it offers no models.
    #[serde(default)]
    pub status: Option<String>,
    /// What a `Login` stored for it: what `Logout` removes.
    #[serde(default)]
    pub stored: Option<AuthMethod>,
}

/// A step of a login (`ServerMessage::LoginStep`): pi's prompts and notices, in order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LoginStep {
    /// Sign in at `url` in a browser.
    AuthUrl {
        url: String,
        #[serde(default)]
        instructions: Option<String>,
    },
    /// Enter `user_code` at `verification_uri`; the login goes on once that is done.
    DeviceCode {
        user_code: String,
        verification_uri: String,
    },
    Info {
        message: String,
        #[serde(default)]
        links: Vec<AuthLink>,
    },
    Progress {
        message: String,
    },
    /// A question, answered with `ClientMessage::LoginAnswer { prompt: n }`: the text typed
    /// for `Text` / `Secret` / `ManualCode`, the picked option's `id` for `Select`.
    Prompt {
        n: u32,
        kind: PromptKind,
        message: String,
        #[serde(default)]
        placeholder: Option<String>,
        #[serde(default)]
        options: Vec<AuthOption>,
    },
    /// Prompt `n` needs no answer any more (the browser sign-in got there first).
    PromptClosed {
        n: u32,
    },
    /// A step a newer host knows: nothing to show, rather than the whole message dropped.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptKind {
    Text,
    /// Text not to show (a key).
    Secret,
    Select,
    /// The code or redirect url the provider shows after a browser sign-in, pasted: the way
    /// in when the browser can't reach the host (a remote one).
    ManualCode,
    /// A kind a newer host knows: answered as text.
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthLink {
    pub url: String,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthOption {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// A host in a `hosts.toml`: its name there and its normalized `ws://` url.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostInfo {
    pub name: String,
    pub url: String,
}

/// What a parent is told when its sub-agent's run ends: the child's final reply, or only
/// that it finished (the parent reads the reply if it wants it).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Report {
    #[default]
    Reply,
    Done,
}

/// When a notice reaches a model: after its current tool calls (a steer), or once its
/// turn has ended (a follow-up). Either way an idle model gets a new turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Notify {
    #[default]
    Asap,
    WhenIdle,
}

impl Notify {
    /// The runtime's name for it.
    pub fn as_str(self) -> &'static str {
        match self {
            Notify::Asap => "asap",
            Notify::WhenIdle => "when_idle",
        }
    }
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

/// A thread's first prompt, sent along with `CreateThread`: the fields of a `Prompt`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FirstPrompt {
    pub text: String,
    pub model: Option<String>,
    pub thinking_level: Option<String>,
    #[serde(default)]
    pub images: Vec<ImageAttachment>,
    #[serde(default)]
    pub title: Option<TitleGen>,
    #[serde(default)]
    pub tools: Option<Vec<String>>,
}

/// An image sent with a prompt: base64 bytes and their mime type (`image/png`, ...). `n` is
/// the `N` of the `[image-N]` token that names it in the text. The host saves the image
/// as a file and links the token to it, so the model can use the file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageAttachment {
    pub mime_type: String,
    pub data: String,
    #[serde(default)]
    pub n: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Dir {
    pub name: String,
    /// It holds a `.git` or `.jj`: likely a project.
    pub repo: bool,
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

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
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
    /// Snoozed and not run since (see `ClientMessage::SnoozeThread`).
    #[serde(default)]
    pub snoozed: bool,
    /// On top of its status group in the lists (`ClientMessage::PinThread`).
    #[serde(default)]
    pub pinned: bool,
    /// `updated_at` as of the user's last look (`ClientMessage::MarkSeen`); a finished
    /// thread updated since, or never looked at, is unread.
    #[serde(default)]
    pub seen_at: Option<i64>,
    /// Pi entry ids of the pinned messages (`ClientMessage::PinMessage`).
    #[serde(default)]
    pub pinned_entries: Vec<String>,
    /// Unix millis of the last `Prompt` that started a turn (mode `prompt`): when the
    /// thread was last given a task. Orders a parent's children.
    #[serde(default)]
    pub prompted_at: Option<i64>,
    /// `async_bash` jobs running in its live session. Runtime state, never stored: a thread
    /// whose turn ended with jobs running is waiting on them (they wake it when they end).
    #[serde(default)]
    pub jobs: u32,
    /// `async_bash` jobs it left running (`notify: "never"`: dev servers, watchers), not in
    /// `jobs`. Nothing waits on them, so the thread can finish; archiving it stops them.
    /// Runtime state, never stored.
    #[serde(default)]
    pub free_jobs: u32,
    /// What the thread's own agent has consumed so far (its sub-agents not included, nor,
    /// in a fork, what its source spent on the history it copied).
    #[serde(default)]
    pub spend: Spend,
    /// The thread this one was forked from (`ClientMessage::ForkThread`), and the entry it
    /// was forked at, if it is a fork.
    #[serde(default)]
    pub forked_from: Option<ForkedFrom>,
    /// The step its host is setting it up with, before it can take a message. Runtime
    /// state, never stored.
    #[serde(default)]
    pub setup: Option<Setup>,
    /// Unix millis its host started compacting the thread's context, while it does (pi's
    /// `compaction_start` to `compaction_end`). Runtime state, never stored.
    #[serde(default)]
    pub compacting_since: Option<i64>,
}

/// Where a fork came from (`ThreadSummary::forked_from`): its source thread and the last
/// entry it copied.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForkedFrom {
    pub thread_id: String,
    pub entry_id: String,
}

/// A step in setting a thread up (`ThreadSummary::setup`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Setup {
    /// Its isolated workspace is being made (`CreateThread::workspace`).
    Workspace,
    /// Its session is capturing the project's nix dev shell (`nix develop`); prompts wait
    /// for it. Every load of a session in a project with a `flake.nix` does this (not on Windows).
    NixShell,
}

/// Tokens and dollars summed over a thread's assistant messages, as pi reports them.
#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Spend {
    /// Every prompt token: fresh, cache read and cache write.
    pub input: u64,
    pub output: u64,
    /// The part of `input` read from cache.
    pub cached: u64,
    /// Dollars, as pi prices them.
    pub cost: f64,
}

impl std::ops::AddAssign for Spend {
    fn add_assign(&mut self, o: Self) {
        self.input += o.input;
        self.output += o.output;
        self.cached += o.cached;
        self.cost += o.cost;
    }
}

impl ThreadSummary {
    pub fn running(&self) -> bool {
        self.running_since.is_some()
    }

    /// The model is waiting on the user.
    pub fn asking(&self) -> bool {
        !self.asks.is_empty()
    }

    /// Not spawned by another thread: what the desktop's lists show.
    /// Sub-agents are reached through their parent's orchestration panel.
    pub fn top_level(&self) -> bool {
        self.parent_id.is_none()
    }
}

/// The name an unnamed thread gets from its first prompt: the first non-blank line, cut to
/// 80 chars.
/// Set by the service when the prompt arrives (and by the desktop, optimistically, for its
/// own drafts); shown until the generated title lands, or if that fails.
pub fn prompt_title(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    Some(line.chars().take(80).collect())
}

/// One pi session entry. A session is a tree: each entry is a child of the one before it
/// (`parent_id`), except where an `EditMessage` branched off an earlier entry. The branch
/// that counts, what the model sees and clients show, is the last entry's and its
/// ancestors (pi's leaf); older branches stay in the history.
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

/// The branch of `entries` (a thread's, in `seq` order) that ends at the entry `leaf`: it
/// and its ancestors, oldest first; empty without such an entry. With the last entry as
/// `leaf`, the thread's current branch: all of it, unless an edit branched it.
pub fn branch<'a>(entries: &'a [Entry], leaf: &str) -> Vec<&'a Entry> {
    let Some(end) = entries.iter().rposition(|e| e.id == leaf) else { return vec![] };
    // A parent is appended before its children, so nothing past `leaf` is on its branch.
    let entries = &entries[..=end];
    // No edit up to there: each entry is the child of the one before.
    if entries.windows(2).all(|w| w[1].parent_id.as_deref() == Some(w[0].id.as_str())) {
        return entries.iter().collect();
    }
    let by_id: std::collections::HashMap<&str, &Entry> = entries.iter().map(|e| (e.id.as_str(), e)).collect();
    let mut branch = vec![&entries[end]];
    while let Some(parent) = branch[branch.len() - 1].parent_id.as_deref().and_then(|p| by_id.get(p))
        && branch.len() <= entries.len()
    {
        branch.push(parent);
    }
    branch.reverse();
    branch
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
        roundtrip(ClientMessage::ListDirs { id: 1, path: "~/".into() });
        roundtrip(ClientMessage::RemoveProject { id: 1, project_id: "p".into() });
        roundtrip(ClientMessage::ListThreads { id: 1 });
        roundtrip(ClientMessage::GetThread { id: 2, thread_id: "t".into() });
        roundtrip(ClientMessage::CreateThread {
            id: 3,
            thread_id: Some("01a182e2-9989-7681-9439-c15c752508f6".into()),
            project_id: "p".into(),
            workspace: true,
            base: Some("main~2".into()),
            parent: Some("t0".into()),
            name: Some("Review".into()),
            report: Report::Done,
            notify: Notify::WhenIdle,
            prompt: Some(FirstPrompt {
                text: "hi".into(),
                model: Some("a/b".into()),
                thinking_level: Some("low".into()),
                images: vec![ImageAttachment { mime_type: "image/png".into(), data: "AAAA".into(), n: Some(1) }],
                title: Some(TitleGen { model: "a/c".into(), reasoning: "low".into(), prompt: "name it".into() }),
                tools: None,
            }),
        });
        roundtrip(ClientMessage::ListHosts { id: 20 });
        roundtrip(ClientMessage::SnoozeThread { id: 17, thread_id: "t".into(), snoozed: true });
        roundtrip(ClientMessage::RenameThread { id: 24, thread_id: "t".into(), name: "Fix it".into() });
        roundtrip(ClientMessage::RetitleThread { id: 25, thread_id: "t".into(), model: "a/c".into(), reasoning: "low".into() });
        roundtrip(ClientMessage::PinThread { id: 21, thread_id: "t".into(), pinned: true });
        roundtrip(ClientMessage::PinMessage { id: 22, thread_id: "t".into(), entry_id: "e".into(), pinned: false });
        roundtrip(ClientMessage::MarkSeen { id: 23, thread_id: "t".into(), updated_at: 5 });
        roundtrip(ClientMessage::FocusThread { id: 18, thread_id: "t".into() });
        roundtrip(ClientMessage::SearchThreads { id: 19, query: "sqlite".into(), limit: 5 });
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
            images: vec![ImageAttachment { mime_type: "image/png".into(), data: "AAAA".into(), n: None }],
            title: Some(TitleGen { model: "a/c".into(), reasoning: "low".into(), prompt: "name it".into() }),
            tools: Some(vec!["async_bash".into()]),
        });
        roundtrip(ClientMessage::Answer {
            id: 13,
            thread_id: "t".into(),
            ask_id: "a1".into(),
            answers: Some(vec![vec!["postgres".into()], vec![]]),
            silent: false,
        });
        roundtrip(ClientMessage::Answer { id: 14, thread_id: "t".into(), ask_id: "a2".into(), answers: None, silent: true });
        roundtrip(ClientMessage::GenerateTitle {
            id: 12,
            title: TitleGen { model: "a/c".into(), reasoning: "low".into(), prompt: "name it".into() },
            text: "hi".into(),
        });
        roundtrip(ClientMessage::EditMessage {
            id: 26,
            thread_id: "t".into(),
            entry_id: "e".into(),
            text: "hi again".into(),
            model: Some("a/b".into()),
            thinking_level: Some("low".into()),
            images: vec![ImageAttachment { mime_type: "image/png".into(), data: "AAAA".into(), n: Some(1) }],
            tools: Some(vec![]),
        });
        roundtrip(ClientMessage::ForkThread { id: 27, thread_id: "t".into(), entry_id: "e".into(), fork_id: Some("f".into()) });
        roundtrip(ClientMessage::Abort { id: 5, thread_id: "t".into() });
        roundtrip(ClientMessage::CancelQueued { id: 9, thread_id: "t".into(), text: "x".into() });
        roundtrip(ClientMessage::ArchiveThread { id: 7, thread_id: "t".into(), archived: true });
        roundtrip(ClientMessage::DeleteThread { id: 8, thread_id: "t".into() });
        roundtrip(ClientMessage::ListModels { id: 6 });
        roundtrip(ClientMessage::ListProviders { id: 26 });
        roundtrip(ClientMessage::Login { id: 27, provider: "anthropic".into(), method: AuthMethod::ApiKey });
        roundtrip(ClientMessage::LoginAnswer { id: 28, login: 27, prompt: 1, value: "sk-x".into() });
        roundtrip(ClientMessage::LoginCancel { id: 29, login: 27 });
        roundtrip(ClientMessage::Logout { id: 30, provider: "anthropic".into() });
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
        assert_eq!(
            c,
            ClientMessage::CreateThread {
                id: 1,
                thread_id: None,
                project_id: "p".into(),
                workspace: false,
                base: None,
                parent: None,
                name: None,
                report: Report::Reply,
                notify: Notify::Asap,
                prompt: None,
            }
        );
        let s: ClientMessage = serde_json::from_value(json!({"type":"search_threads","id":1,"query":"x"})).unwrap();
        assert_eq!(s, ClientMessage::SearchThreads { id: 1, query: "x".into(), limit: 20 });
        assert!(!t.snoozed);
        assert_eq!(t.parent_id, None);
        assert_eq!(t.prompted_at, None);
        assert_eq!((t.jobs, t.free_jobs), (0, 0));
        assert_eq!(t.spend, Spend::default());
        assert_eq!(t.setup, None);
        assert_eq!(t.forked_from, None);
        assert!(!t.pinned && t.seen_at.is_none() && t.pinned_entries.is_empty());
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
            snoozed: true,
            pinned: true,
            seen_at: Some(2),
            pinned_entries: vec!["e1".into()],
            prompted_at: Some(4),
            jobs: 2,
            free_jobs: 1,
            spend: Spend { input: 10, output: 2, cached: 8, cost: 0.5 },
            forked_from: Some(ForkedFrom { thread_id: "t1".into(), entry_id: "e1".into() }),
            setup: Some(Setup::NixShell),
            compacting_since: Some(6),
        };
        assert!(t.asking());
        roundtrip(ServerMessage::Focus { thread_id: "t".into() });
        roundtrip(ServerMessage::Hosts { reply_to: 1, hosts: vec![HostInfo { name: "h".into(), url: "ws://h:7777/ws".into() }] });
        assert_eq!(serde_json::to_value(Notify::WhenIdle).unwrap(), json!(Notify::WhenIdle.as_str()));
        roundtrip(ServerMessage::Search { reply_to: 1, hits: vec![SearchHit { thread_id: "t".into(), snippet: "…sqlite…".into() }] });
        let p = Project { id: "p".into(), path: "/x".into(), origin: Some("github.com/a/b".into()), created_at: 1 };
        let m =
            ModelInfo { provider: "a".into(), id: "b".into(), name: "B".into(), context_window: 1, thinking_levels: vec!["low".into()] };
        assert_eq!(m.key(), "a/b");
        roundtrip(ServerMessage::Projects { reply_to: None, projects: vec![p.clone()] });
        roundtrip(ServerMessage::ProjectAdded { reply_to: 1, project: p });
        roundtrip(ServerMessage::Dirs { reply_to: 1, dirs: vec![Dir { name: "work".into(), repo: true }] });
        roundtrip(ServerMessage::Models { reply_to: 1, models: vec![m] });
        roundtrip(ServerMessage::Providers {
            reply_to: 2,
            providers: vec![ProviderInfo {
                id: "anthropic".into(),
                name: "Anthropic".into(),
                oauth: Some("Anthropic (Claude Pro/Max)".into()),
                api_key: Some("Anthropic API key".into()),
                status: Some("Signed in".into()),
                stored: Some(AuthMethod::Oauth),
            }],
            auth_path: "/home/x/.pi/agent/auth.json".into(),
        });
        // As the runtime writes them.
        let step: LoginStep = serde_json::from_value(json!({"type":"prompt","n":2,"kind":"manual_code","message":"Paste it"})).unwrap();
        assert_eq!(
            step,
            LoginStep::Prompt { n: 2, kind: PromptKind::ManualCode, message: "Paste it".into(), placeholder: None, options: vec![] }
        );
        // What a newer host may send.
        let newer: LoginStep = serde_json::from_value(json!({"type":"qr_code","data":"x"})).unwrap();
        assert_eq!(newer, LoginStep::Unknown);
        let newer: LoginStep = serde_json::from_value(json!({"type":"prompt","n":3,"kind":"date","message":"m"})).unwrap();
        assert!(matches!(newer, LoginStep::Prompt { kind: PromptKind::Other, .. }));
        for step in [
            step,
            LoginStep::AuthUrl { url: "https://x".into(), instructions: None },
            LoginStep::DeviceCode { user_code: "AB-12".into(), verification_uri: "https://x/device".into() },
            LoginStep::Info { message: "m".into(), links: vec![AuthLink { url: "https://x".into(), label: None }] },
            LoginStep::Progress { message: "p".into() },
            LoginStep::PromptClosed { n: 2 },
        ] {
            roundtrip(ServerMessage::LoginStep { login: 27, step });
        }
        let e = Entry::from_pi(0, json!({"type":"message","id":"a","parentId":null,"message":{"role":"user","content":"hi"}}));
        assert_eq!(e.kind, "message");
        assert_eq!(e.parent_id, None);
        roundtrip(ServerMessage::Hello { host_name: "h".into(), version: "0".into(), service_id: "s".into() });
        roundtrip(ServerMessage::Threads { reply_to: None, threads: vec![t.clone()] });
        roundtrip(ServerMessage::Thread {
            reply_to: 1,
            thread_id: "t".into(),
            header: json!({"type":"session"}),
            entries: vec![e.clone()],
        });
        roundtrip(ServerMessage::ThreadCreated { reply_to: 2, thread: Box::new(t) });
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

    #[test]
    fn branches_follow_parent_links() {
        let e = |seq, id: &str, parent: Option<&str>| Entry::from_pi(seq, json!({"type":"message","id":id,"parentId":parent}));
        let ids = |b: Vec<&Entry>| b.iter().map(|e| e.id.clone()).collect::<Vec<_>>();
        // a ← b ← c, then an edit of c: d is b's child too, e follows d.
        let entries = [e(0, "a", None), e(1, "b", Some("a")), e(2, "c", Some("b")), e(3, "d", Some("b")), e(4, "e", Some("d"))];
        assert_eq!(ids(branch(&entries, "e")), ["a", "b", "d", "e"]);
        assert_eq!(ids(branch(&entries, "c")), ["a", "b", "c"]);
        assert_eq!(ids(branch(&entries[..3], "c")), ["a", "b", "c"]);
        assert!(branch(&entries, "x").is_empty());
    }
}
