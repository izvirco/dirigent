//! Wire types between `dirigent_desktop` and `dirigent_service` (JSON over WebSocket).
//!
//! Requests carry an `id`, replies carry `reply_to`, pushes have neither.
//! A thread's entries and events are `transcript`'s: the format the agent writes, the host
//! stores and clients draw.
//! `release` and `leaderboard` are the other wire: the update api's (`dirigent_api`).
//!
//! A host and a client talk only when they speak the same `PROTOCOL`: the host says its
//! own in `Hello`, and a client that reads another one tells its user which side to update.

pub mod images;
pub mod leaderboard;
pub mod release;
pub mod transcript;
pub mod visuals;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use transcript::{Entry, Event};

/// The wire's version. Raised when a change would make an older host or client misread a
/// newer one (not for a field added with a default). 1 was pi's entries as opaque JSON,
/// from hosts that sent no number.
pub const PROTOCOL: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptMode {
    /// Start a run (queued as follow-up if one is already running).
    Prompt,
    /// Hand it to the model after its current tool calls.
    Steer,
    /// Hand it to the model once it would stop: the run goes on with it.
    FollowUp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    ListProjects {
        id: u64,
    },
    /// Register a directory on this host as a project: an absolute path, or one under `~/`
    /// (this host's home). A path that already is a project answers with that one.
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
    /// be delivered (see `Prompt`) the thread is deleted again and the request fails, so a
    /// client never ends up with a thread that silently never started. Until it is delivered
    /// the thread takes no other prompt.
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
    /// `model` (its id) and `effort` switch the thread before sending, if they differ.
    /// `images` are attached to the message; the host scales them to what a model takes.
    /// `title` (sent with a thread's first prompt) asks the host to name the thread from `text`.
    /// `tools` names the tool switches that are on from now on (`async_bash`, `ask_user`,
    /// `dirigent`, `computer`, `codemode`) and the options `bash_background` and
    /// `computer_cursor`; the host remembers them for the thread. `None` leaves them as they are.
    /// The reply (`Ok`) says the model has the message: its run started, or it is queued
    /// behind the one in progress. An `Error` says it has not, and why (the session didn't
    /// load, the proxy doesn't serve the model): the conversation has nothing of the message,
    /// so the client gives it back to its user.
    Prompt {
        id: u64,
        thread_id: String,
        text: String,
        mode: PromptMode,
        model: Option<String>,
        effort: Option<String>,
        #[serde(default)]
        images: Vec<ImageAttachment>,
        #[serde(default)]
        title: Option<TitleGen>,
        #[serde(default)]
        tools: Option<Vec<String>>,
    },
    /// Rewrite one of the thread's user messages (`entry_id`, a `Body::User` entry) and run
    /// from there: the conversation goes back to just before it and `text` is sent in its
    /// place, as a `Prompt` in mode `prompt` with these fields would be. The old message and
    /// everything after it stay in the thread's history, on a branch no client shows (see
    /// `transcript::Entry`). Refused while the thread runs.
    EditMessage {
        id: u64,
        thread_id: String,
        entry_id: String,
        text: String,
        model: Option<String>,
        effort: Option<String>,
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
    /// form's `Insert into composer`).
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
    /// Drop one steer / follow-up (by text) that is queued but not yet handed to the model.
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
    /// not just its first prompt. `model` (its id) and `reasoning` are the `[title]`
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
    /// Pin one of the thread's messages (by the id of the entry it came from; see
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
    /// The models this host's proxy serves.
    ListModels {
        id: u64,
    },
    /// The answer to prompt `prompt` of the login (`McpLogin`) started by request `login` (see
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
    /// The MCP servers in this host's `agent.toml` (Settings › MCP servers), as configured:
    /// nothing is connected to (`CheckMcpServer` does that).
    ListMcpServers {
        id: u64,
    },
    /// Connect to configured server `name` as a session would (from the host's home), and
    /// say how that went and what tools it offers. Disconnects again.
    CheckMcpServer {
        id: u64,
        name: String,
    },
    /// Add server `name` to `agent.toml`, or replace its entry, with `config` (an entry of
    /// the common `mcpServers` shape). Sessions read the file when they start: the host
    /// unloads its idle ones, so their next prompt sees the change.
    SaveMcpServer {
        id: u64,
        name: String,
        config: Value,
    },
    /// Remove server `name` from `agent.toml`, and the OAuth credentials stored for it. Idle
    /// sessions are unloaded, as for `SaveMcpServer`.
    RemoveMcpServer {
        id: u64,
        name: String,
    },
    /// Sign in to OAuth server `name` on this host. Its steps come to this connection only,
    /// as `LoginStep`s under this request's id; the prompts among them are answered with
    /// `LoginAnswer`, and `LoginCancel` stops it. The reply (`Ok`, or `Error`) ends it; so
    /// does the connection dropping. Sessions waiting for the sign-in pick it up on their
    /// next turn.
    McpLogin {
        id: u64,
        name: String,
    },
    /// Remove the OAuth credentials stored for server `name`.
    McpLogout {
        id: u64,
        name: String,
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
    /// A piece of the picture of the thread's screen (`ThreadSummary::screen`): replies
    /// `Screen`, an empty one when it has none. A picture comes in small pieces, each asked
    /// for once the one before is in, so never more than one piece of it is ahead of
    /// anything else the host has to say, however slow the connection. `offset` 0 asks for
    /// the first piece of the latest picture (`frame` is ignored); the next ones name the
    /// `frame` the first came with and how much of it the client has. The host keeps that
    /// picture for the connection until its last piece is asked for, or another picture is
    /// started: a client reads one at a time, and starts the next (a `screen` event named a
    /// newer one) only then, so a slow connection skips pictures instead of queueing them.
    GetScreen {
        id: u64,
        thread_id: String,
        #[serde(default)]
        frame: u64,
        #[serde(default)]
        offset: u64,
    },
    /// Token / cost usage of every assistant message with `since <= timestamp < until`,
    /// summed per `bucket_ms`-wide slot (counted from `since`) and per provider (who makes
    /// the model) + model.
    ListUsage {
        id: u64,
        since: i64,
        until: i64,
        bucket_ms: i64,
    },
    /// This host's CLIProxyAPI (`Proxy`): its accounts, asked for anew when what the host
    /// holds is more than a few seconds old, and their limits' history with
    /// `since <= time < until`: the highest use per `bucket_ms`-wide slot (counted from
    /// `since`). An empty window (`since == until`) asks for no history.
    ListProxy {
        id: u64,
        since: i64,
        until: i64,
        bucket_ms: i64,
    },
    /// Point this host at its CLIProxyAPI (Settings › Proxy): `url` is the proxy's address,
    /// `key` one of its API keys (what every model request is made with) and
    /// `management_key` its management key (what Usage reads the accounts with; optional).
    /// An empty key is the one the host holds for that address. An empty `url` forgets
    /// everything. The host asks the proxy at once: the reply is a `Proxy` with what it
    /// found (no history).
    SetProxy {
        id: u64,
        url: String,
        key: String,
        #[serde(default)]
        management_key: String,
    },
    /// The newer release this host's service can update itself to: replies `Update`. Only a
    /// service that runs on its own (`dirigent_service`, from a release's install) has one. A
    /// desktop updates the service it embeds by updating itself, and a nix package is nix's.
    CheckUpdate {
        id: u64,
    },
    /// Install the release `CheckUpdate` told of and restart as it. The reply (`Ok`) says the
    /// service restarts now, as that release (on Windows its setup installs it once the
    /// service is gone): every connection drops, and the threads that run are cut off, so a
    /// client asks when the host is idle. An `Error` says why nothing changed.
    InstallUpdate {
        id: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Hello {
        host_name: String,
        version: String,
        /// Random, made once per service (kept in its db): what the leaderboard counts a
        /// service's usage under, whichever desktops upload it.
        #[serde(default)]
        service_id: String,
        /// The `PROTOCOL` it speaks. 1 from a host that doesn't say.
        #[serde(default = "first_protocol")]
        protocol: u32,
    },
    /// Reply to `ListProjects`, or pushed (`reply_to: None`) whenever a project changes.
    Projects {
        reply_to: Option<u64>,
        projects: Vec<Project>,
    },
    /// Reply to `AddProject`: the new project, or the one that path already was.
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
    /// Reply to `GetThread`: every entry of the thread, in `seq` order.
    Thread {
        reply_to: u64,
        thread_id: String,
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
    /// Pushed to all clients whenever an entry is kept for a thread. Images are blobs
    /// (`transcript::Block::Image`, and the `blobs` of a reply's text): fetch them with
    /// `GetBlob`.
    Entry {
        thread_id: String,
        entry: Entry,
    },
    /// What happens in a thread between entries: a reply streaming, a tool running, a
    /// message queued.
    Event {
        thread_id: String,
        event: Event,
    },
    Models {
        reply_to: u64,
        models: Vec<ModelInfo>,
    },
    /// Reply to `ListMcpServers`: by name; `path` is the file they are in (`agent.toml`).
    McpServers {
        reply_to: u64,
        servers: Vec<McpServer>,
        path: String,
    },
    /// Reply to `CheckMcpServer`.
    McpStatus {
        reply_to: u64,
        status: McpServerStatus,
    },
    /// A step of the login started by request `login` (`ClientMessage::McpLogin`), to the
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
    /// Reply to `GetScreen`: a piece of the picture `frame` (see `Event`). The picture is
    /// an image file made small for the wire (a JPEG) as base64 text, `total` characters
    /// long; `data` is the part of that text from `offset` on, a few KB of it. The piece
    /// with which `offset` + its length reaches `total` is the last. A `total` of 0: there
    /// is no picture (the thread has no screen any more, or the host no longer holds the one
    /// asked to go on with). Sent when nothing else waits, as `Blob`s are.
    Screen {
        reply_to: u64,
        thread_id: String,
        frame: u64,
        mime_type: String,
        total: u64,
        offset: u64,
        data: String,
    },
    Usage {
        reply_to: u64,
        buckets: Vec<UsageBucket>,
    },
    /// Reply to `ListProxy` and `SetProxy`.
    Proxy {
        reply_to: u64,
        proxy: Proxy,
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
    /// Reply to `CheckUpdate`: the version `InstallUpdate` would install. `None`: the service
    /// runs its channel's latest release, or isn't one that updates itself.
    Update {
        reply_to: u64,
        version: Option<String>,
    },
}

/// An MCP server in a host's `agent.toml` (see `ClientMessage::ListMcpServers`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServer {
    pub name: String,
    /// Its entry as the file has it, as JSON (the common `mcpServers` shape): what an edit
    /// starts from, so the fields a client doesn't show stay as they are.
    pub config: Value,
    /// Why sessions skip the entry (a field that is wrong, a name clashing with another's).
    #[serde(default)]
    pub error: Option<String>,
    /// OAuth credentials are stored for it: what `McpLogout` removes.
    #[serde(default)]
    pub signed_in: bool,
}

/// How connecting to an MCP server went (`ClientMessage::CheckMcpServer`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerStatus {
    pub state: McpState,
    #[serde(default)]
    pub tools: Vec<McpTool>,
    /// Why it isn't connected (with a failed command's last stderr lines).
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpState {
    Connected,
    /// It wants an OAuth sign-in (`McpLogin`).
    NeedsAuth,
    Failed,
    /// A state a newer host knows.
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpTool {
    pub name: String,
    /// The first line of its description.
    #[serde(default)]
    pub description: Option<String>,
}

/// A step of an MCP server's sign-in (`ServerMessage::LoginStep`), in order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LoginStep {
    /// Sign in at `url` in a browser.
    AuthUrl { url: String },
    /// What the sign-in is doing.
    Progress { message: String },
    /// A question, answered with `ClientMessage::LoginAnswer { prompt: n }` and the text
    /// typed: the address the browser ended up on, pasted. The way in when the browser
    /// can't reach the host (a remote one).
    Prompt {
        n: u32,
        message: String,
        #[serde(default)]
        placeholder: Option<String>,
    },
    /// Prompt `n` needs no answer any more (the browser got to the host).
    PromptClosed { n: u32 },
    /// A step a newer host knows: nothing to show, rather than the whole message dropped.
    #[serde(other)]
    Unknown,
}

/// A host in a `hosts.toml`, as an agent connects to it: its name there, its `ws(s)://`
/// url (an `ssh://` host's local tunnel end) and the bearer token to send, if any.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostInfo {
    pub name: String,
    pub url: String,
    /// Sent as `Authorization: Bearer` on connect (a public host's API key).
    #[serde(default)]
    pub bearer: Option<String>,
    /// Why it can't be reached now (its ssh tunnel failed); `url` is then its address.
    #[serde(default)]
    pub error: Option<String>,
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

fn default_limit() -> u64 {
    20
}

fn first_protocol() -> u32 {
    1
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

/// How to name a thread from its first prompt: the model (its id), its reasoning effort and
/// the system prompt. Comes from the desktop's `config.toml`.
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
    /// Dollars, at the models' list prices.
    pub cost: f64,
    /// Assistant messages summed into this bucket.
    pub messages: u64,
}

/// A host's CLIProxyAPI as its service last saw it (`ServerMessage::Proxy`): whether it is
/// set up, the accounts behind it and how much of each one's limits is used. No key ever
/// comes back.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Proxy {
    /// The proxy's address; empty: the host has none set.
    pub url: String,
    /// The host holds an API key for it: its threads can run.
    #[serde(default)]
    pub key: bool,
    /// The host holds its management key: the accounts and their limits can be read.
    #[serde(default)]
    pub management_key: bool,
    /// How many models it serves the host.
    #[serde(default)]
    pub models: u32,
    /// Why it serves none: it isn't reached, or turned the API key down.
    #[serde(default)]
    pub models_error: Option<String>,
    /// Why the host's last look at the proxy's accounts failed. They are then as of the
    /// last one that worked.
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub accounts: Vec<ProxyAccount>,
    /// The limits' history for the window asked for, oldest slot first.
    #[serde(default)]
    pub samples: Vec<ProxySample>,
    /// The proxy is outdated: releases behind the latest one, enough of them to say so. The
    /// host knows with the management key only, and as of the last time it asked the proxy
    /// for the latest release, a few times a day.
    #[serde(default)]
    pub update: Option<ProxyUpdate>,
}

/// The CLIProxyAPI release a proxy is outdated by.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProxyUpdate {
    /// The release the proxy runs (`8.0.75`).
    pub running: String,
    pub latest: String,
}

/// An account (a credential) behind a CLIProxyAPI.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProxyAccount {
    /// The proxy's id for it (its credential file's name): what `ProxySample::account` names.
    pub id: String,
    /// `claude`, `codex`, ...
    pub provider: String,
    /// Its email, or the credential's label.
    pub name: String,
    /// Switched off in the proxy.
    #[serde(default)]
    pub disabled: bool,
    /// Its sign-in ran out: it has to sign in again on the proxy.
    #[serde(default)]
    pub sign_in: bool,
    /// What the proxy says is wrong with it, while it is failing. Being out of quota is no
    /// failure: the `limits` say that.
    #[serde(default)]
    pub error: Option<String>,
    /// Its limits as the provider last reported them. Empty until the proxy has sent a
    /// request through the account (it keeps them in memory only), and for providers
    /// whose limits the host can't read (it reads Anthropic's and Codex's).
    #[serde(default)]
    pub limits: Vec<ProxyLimit>,
}

/// One of an account's rate limits, as of the last reply the provider gave the proxy.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProxyLimit {
    /// What it is called. Anthropic's own names: `5h`, `7d`, and `7d_oi`, the part of the
    /// week's limit its top model may use. Codex's are named the same way, by how long
    /// their window is: `5h` and `7d`.
    pub name: String,
    /// The share used: 0 to 1, a little over when the provider let the last request through.
    pub used: f64,
    /// When it starts over (unix ms).
    #[serde(default)]
    pub resets_at: Option<i64>,
}

impl ProxyLimit {
    /// The share used as of `now`: nothing once the limit has started over. The proxy only
    /// hears of a limit when the account answers a request, so a `used` from before the
    /// reset means the account has done nothing since.
    pub fn used_at(&self, now: i64) -> f64 {
        if self.resets_at.is_some_and(|at| at <= now) { 0. } else { self.used }
    }
}

/// A limit's use in one time slot (see `ClientMessage::ListProxy`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProxySample {
    /// Unix millis the slot starts at: `since + n * bucket_ms`.
    pub start: i64,
    /// A `ProxyAccount::id`.
    pub account: String,
    /// A `ProxyLimit::name`.
    pub limit: String,
    /// The highest share used the host saw in the slot.
    pub used: f64,
}

/// A thread's first prompt, sent along with `CreateThread`: the fields of a `Prompt`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FirstPrompt {
    pub text: String,
    pub model: Option<String>,
    pub effort: Option<String>,
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

/// A model a host's proxy serves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    /// What a thread names it by: `claude-opus-5-5`.
    pub id: String,
    pub name: String,
    /// Who makes it: `anthropic`, `openai`.
    pub owner: String,
    pub context_window: u64,
    /// The reasoning efforts it takes, lowest first; empty: it doesn't reason.
    pub efforts: Vec<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadSummary {
    pub id: String,
    /// Missing when the project was removed (orphan thread).
    pub project_id: Option<String>,
    pub cwd: String,
    pub name: Option<String>,
    /// The model (its id) and reasoning effort it was last prompted with.
    pub model: Option<String>,
    pub effort: Option<String>,
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
    /// The entry ids of the pinned messages (`ClientMessage::PinMessage`).
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
    /// It came from pi's storage with entries that could not be converted: it can be read,
    /// and forked from before them, but takes no message.
    #[serde(default)]
    pub locked: bool,
    /// The step its host is setting it up with, before it can take a message. Runtime
    /// state, never stored.
    #[serde(default)]
    pub setup: Option<Setup>,
    /// Unix millis its host started compacting the thread's context, while it does. Runtime
    /// state, never stored.
    #[serde(default)]
    pub compacting_since: Option<i64>,
    /// Its agent is using a screen in this run (one of its own, or its host's desktop), and
    /// there is a picture of the window it works in to show (`ClientMessage::GetScreen`;
    /// `screen` events tell of every step).
    /// Runtime state, never stored.
    #[serde(default)]
    pub screen: bool,
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

/// The one name a thread's workspace goes by: its directory, its git branch, its jj
/// workspace. `dirigent-` + the last 8 characters of the thread id (the random part of a
/// v7 uuid), short enough to read in a branch list. The desktop shows it while the host is
/// still making the workspace (`Setup::Workspace`).
pub fn workspace_name(thread_id: &str) -> String {
    let short: String = thread_id.chars().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect();
    format!("dirigent-{short}")
}

/// Tokens and dollars summed over a thread's assistant messages.
#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Spend {
    /// Every prompt token: fresh, cache read and cache write.
    pub input: u64,
    pub output: u64,
    /// The part of `input` read from cache.
    pub cached: u64,
    /// Dollars, at the models' list prices.
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
                effort: Some("low".into()),
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
            effort: None,
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
            effort: Some("low".into()),
            images: vec![ImageAttachment { mime_type: "image/png".into(), data: "AAAA".into(), n: Some(1) }],
            tools: Some(vec![]),
        });
        roundtrip(ClientMessage::ForkThread { id: 27, thread_id: "t".into(), entry_id: "e".into(), fork_id: Some("f".into()) });
        roundtrip(ClientMessage::Abort { id: 5, thread_id: "t".into() });
        roundtrip(ClientMessage::CancelQueued { id: 9, thread_id: "t".into(), text: "x".into() });
        roundtrip(ClientMessage::ArchiveThread { id: 7, thread_id: "t".into(), archived: true });
        roundtrip(ClientMessage::DeleteThread { id: 8, thread_id: "t".into() });
        roundtrip(ClientMessage::ListModels { id: 6 });
        roundtrip(ClientMessage::LoginAnswer { id: 28, login: 27, prompt: 1, value: "sk-x".into() });
        roundtrip(ClientMessage::ListMcpServers { id: 31 });
        roundtrip(ClientMessage::CheckMcpServer { id: 32, name: "docs".into() });
        roundtrip(ClientMessage::SaveMcpServer { id: 33, name: "docs".into(), config: json!({"url": "https://x/mcp"}) });
        roundtrip(ClientMessage::RemoveMcpServer { id: 34, name: "docs".into() });
        roundtrip(ClientMessage::McpLogin { id: 35, name: "docs".into() });
        roundtrip(ClientMessage::McpLogout { id: 36, name: "docs".into() });
        roundtrip(ClientMessage::LoginCancel { id: 29, login: 27 });
        roundtrip(ClientMessage::GetBlob { id: 10, blob_id: "abc".into() });
        roundtrip(ClientMessage::GetScreen { id: 33, thread_id: "t".into(), frame: 3, offset: 16384 });
        roundtrip(ClientMessage::ListUsage { id: 11, since: 0, until: 86_400_000, bucket_ms: 3_600_000 });
        roundtrip(ClientMessage::ListProxy { id: 31, since: 0, until: 86_400_000, bucket_ms: 3_600_000 });
        roundtrip(ClientMessage::SetProxy { id: 32, url: "http://127.0.0.1:8317".into(), key: "k".into(), management_key: "m".into() });
        roundtrip(ClientMessage::CheckUpdate { id: 37 });
        roundtrip(ClientMessage::InstallUpdate { id: 38 });
        assert_eq!(
            serde_json::to_value(ClientMessage::Prompt {
                id: 4,
                thread_id: "t".into(),
                text: "hi".into(),
                mode: PromptMode::Steer,
                model: None,
                effort: None,
                images: vec![],
                title: None,
                tools: None,
            })
            .unwrap(),
            json!({"type":"prompt","id":4,"thread_id":"t","text":"hi","mode":"steer","model":null,"effort":null,"images":[],"title":null,"tools":null})
        );
    }

    #[test]
    fn added_fields_are_optional() {
        let t: ThreadSummary = serde_json::from_value(json!({
            "id":"t","project_id":null,"cwd":"/x","name":null,"model":null,"effort":null,
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
        assert!(!t.locked);
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
            effort: None,
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
            screen: true,
            locked: true,
        };
        assert!(t.asking());
        roundtrip(ServerMessage::Focus { thread_id: "t".into() });
        roundtrip(ServerMessage::Hosts {
            reply_to: 1,
            hosts: vec![HostInfo { name: "h".into(), url: "ws://h:7777/ws".into(), bearer: Some("dk_x".into()), error: None }],
        });
        roundtrip(ServerMessage::Search { reply_to: 1, hits: vec![SearchHit { thread_id: "t".into(), snippet: "…sqlite…".into() }] });
        let p = Project { id: "p".into(), path: "/x".into(), origin: Some("github.com/a/b".into()), created_at: 1 };
        let m = ModelInfo { id: "b".into(), name: "B".into(), owner: "a".into(), context_window: 1, efforts: vec!["low".into()] };
        roundtrip(ServerMessage::Projects { reply_to: None, projects: vec![p.clone()] });
        roundtrip(ServerMessage::ProjectAdded { reply_to: 1, project: p });
        roundtrip(ServerMessage::Dirs { reply_to: 1, dirs: vec![Dir { name: "work".into(), repo: true }] });
        roundtrip(ServerMessage::Models { reply_to: 1, models: vec![m] });
        roundtrip(ServerMessage::McpServers {
            reply_to: 3,
            servers: vec![McpServer { name: "fs".into(), config: json!({"command": "npx"}), error: None, signed_in: false }],
            path: "/home/x/.config/dirigent/agent.toml".into(),
        });
        let status: McpServerStatus =
            serde_json::from_value(json!({"state":"needs_auth","tools":[{"name":"search"}],"error":"401"})).unwrap();
        assert_eq!(status.state, McpState::NeedsAuth);
        assert_eq!(status.tools, [McpTool { name: "search".into(), description: None }]);
        roundtrip(ServerMessage::McpStatus { reply_to: 4, status });
        let newer: McpServerStatus = serde_json::from_value(json!({"state":"sleeping"})).unwrap();
        assert_eq!(newer.state, McpState::Other);
        let step: LoginStep = serde_json::from_value(json!({"type":"prompt","n":2,"message":"Paste it"})).unwrap();
        assert_eq!(step, LoginStep::Prompt { n: 2, message: "Paste it".into(), placeholder: None });
        let newer: LoginStep = serde_json::from_value(json!({"type":"qr_code","data":"x"})).unwrap();
        assert_eq!(newer, LoginStep::Unknown);
        for step in [
            step,
            LoginStep::AuthUrl { url: "https://x".into() },
            LoginStep::Progress { message: "p".into() },
            LoginStep::PromptClosed { n: 2 },
        ] {
            roundtrip(ServerMessage::LoginStep { login: 27, step });
        }
        let e = Entry {
            seq: 0,
            id: "a".into(),
            parent_id: None,
            at: 1,
            body: transcript::Body::User { content: vec![transcript::Block::text("hi")] },
        };
        roundtrip(ServerMessage::Hello { host_name: "h".into(), version: "0".into(), service_id: "s".into(), protocol: PROTOCOL });
        let old: ServerMessage = serde_json::from_value(json!({"type":"hello","host_name":"h","version":"0.0.10"})).unwrap();
        assert!(matches!(old, ServerMessage::Hello { protocol: 1, .. }));
        roundtrip(ServerMessage::Threads { reply_to: None, threads: vec![t.clone()] });
        roundtrip(ServerMessage::Thread { reply_to: 1, thread_id: "t".into(), entries: vec![e.clone()] });
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
        roundtrip(ServerMessage::Update { reply_to: 9, version: Some("0.0.11".into()) });
        roundtrip(ServerMessage::Error { reply_to: Some(4), message: "boom".into() });
        roundtrip(ServerMessage::Entry { thread_id: "t".into(), entry: e });
        roundtrip(ServerMessage::Event { thread_id: "t".into(), event: Event::RunStart });
        roundtrip(ServerMessage::Blob { reply_to: 5, blob_id: "abc".into(), mime_type: "image/png".into(), data: "AAAA".into() });
        roundtrip(ServerMessage::Screen {
            reply_to: 8,
            thread_id: "t".into(),
            frame: 3,
            mime_type: "image/jpeg".into(),
            total: 40000,
            offset: 16384,
            data: "AAAA".into(),
        });
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
        let limit = ProxyLimit { name: "7d_oi".into(), used: 0.42, resets_at: Some(2000) };
        assert_eq!((limit.used_at(1999), limit.used_at(2000)), (0.42, 0.));
        roundtrip(ServerMessage::Proxy {
            reply_to: 7,
            proxy: Proxy {
                url: "http://127.0.0.1:8317".into(),
                key: true,
                management_key: true,
                models: 20,
                models_error: None,
                error: None,
                accounts: vec![ProxyAccount {
                    id: "a.json".into(),
                    provider: "claude".into(),
                    name: "a@example.com".into(),
                    error: Some("token expired".into()),
                    limits: vec![limit],
                    ..ProxyAccount::default()
                }],
                samples: vec![ProxySample { start: 0, account: "a.json".into(), limit: "7d_oi".into(), used: 0.42 }],
                update: Some(ProxyUpdate { running: "8.0.75".into(), latest: "8.0.85".into() }),
            },
        });
        assert_eq!(serde_json::from_value::<Proxy>(json!({"url": ""})).unwrap(), Proxy::default());
    }
}
