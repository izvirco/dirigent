//! `bash` runs a command and gives its output; `async_bash` runs one in the background and
//! tells the model when it ends, so it can keep working (or `wait`, instead of sleeping).
//! Both run their command as a `Job` (`shell.rs`). With `async_bash` and its option
//! `bash_background` on, a `bash` command still running after `BASH_GRACE` becomes a
//! background job too: the call ends with the job's id, and the command's output comes as
//! the job's notice once it ends.

use super::shell::{Job, Tell};
use super::truncate;
use crate::session::Session;
use crate::tool::{Call, Tool, ToolOutput, ToolSpec};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use dirigent_protocol::Notify;
use dirigent_protocol::transcript::Notice;
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{Read, Seek};
use std::path::Path;
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// An `async_bash` command that ends within this long is answered inline, with no notice.
const START_GRACE: Duration = Duration::from_secs(2);
/// A `bash` command still running after this long moves to the background. (Sooner under
/// test, which has no ten seconds to wait.)
const BASH_GRACE: Duration = Duration::from_millis(if cfg!(test) { 300 } else { 10_000 });
/// How long `wait` blocks when the model gives no `timeout`, in seconds.
const WAIT_S: f64 = 600.;
/// The longest `timeout` a command takes, in seconds.
const MAX_TIMEOUT_S: f64 = 2_147_483.647;
/// How much of a command's output a codemode script gets: more than the model would.
const SCRIPT_OUTPUT_BYTES: u64 = 1024 * 1024;

fn session(weak: &Weak<Session>) -> Result<Arc<Session>> {
    weak.upgrade().ok_or_else(|| anyhow!("the thread was unloaded"))
}

pub struct Bash {
    session: Weak<Session>,
}

impl Bash {
    pub fn new(session: Weak<Session>) -> Self {
        Self { session }
    }
}

#[derive(Deserialize)]
struct BashInput {
    command: String,
    timeout: Option<f64>,
}

#[async_trait]
impl Tool for Bash {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            snippet: Some("Execute bash commands (ls, grep, find, etc.)".into()),
            returns: Some(
                "{ output: string; truncated: boolean; full_output_path?: string; exit_code: number; wall_time_seconds: number }".into(),
            ),
            ..ToolSpec::new(
                "bash",
                &format!(
                    "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last \
                     {} lines or {}KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide \
                     a timeout in seconds.",
                    truncate::MAX_LINES,
                    truncate::MAX_BYTES / 1024
                ),
                json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "Shell command to execute" },
                        "timeout": { "type": "number", "description": "Timeout in seconds (optional, no default timeout)" },
                    },
                    "required": ["command"],
                }),
            )
        }
    }

    async fn call(&self, input: Value, call: &Call) -> Result<ToolOutput> {
        let input: BashInput = serde_json::from_value(input)?;
        let session = session(&self.session)?;
        let timeout = match input.timeout {
            Some(t) if !t.is_finite() || t <= 0. => bail!("Invalid timeout: must be a finite number of seconds"),
            Some(t) if t > MAX_TIMEOUT_S => bail!("Invalid timeout: maximum is {MAX_TIMEOUT_S} seconds"),
            t => t,
        };
        if call.cancel.is_cancelled() {
            bail!("Command aborted");
        }
        let job = session.jobs.spawn(&session.env, &input.command, &session.env.cwd)?;
        let deadline = async {
            match timeout {
                Some(t) => tokio::time::sleep(Duration::from_secs_f64(t)).await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(deadline);
        // A script waits for its commands: only the model can take a job's notice later.
        let may_background = !call.script && session.is_on("async_bash") && session.is_on("bash_background");
        let grace = async {
            match may_background {
                true => tokio::time::sleep(BASH_GRACE).await,
                false => std::future::pending().await,
            }
        };
        let ended = tokio::select! {
            // Killed: the run was stopped (`Session::abort`), which this call hears of as well.
            code = job.exited() => if job.killed() { Ended::Aborted } else { Ended::Exit(code) },
            _ = call.cancel.cancelled() => {
                job.kill();
                job.exited().await;
                Ended::Aborted
            }
            _ = &mut deadline => {
                job.stop();
                job.exited().await;
                Ended::TimedOut(timeout.unwrap_or_default())
            }
            _ = grace => {
                job.background(Tell::Asap);
                session.report_jobs();
                let text = format!(
                    "Still running after {}s, so it moved to the background as job {} (log: {}). You will be told when it ends, \
                     with its output; `async_bash wait` if you have nothing else to do.",
                    BASH_GRACE.as_secs(), job.id, job.log.display()
                );
                // Its `timeout` holds in the background too; what it came to is the notice.
                let remaining = timeout.map(|t| Duration::from_secs_f64(t).saturating_sub(BASH_GRACE));
                tokio::spawn(async move {
                    let ended = match remaining {
                        Some(left) => match tokio::time::timeout(left, job.exited()).await {
                            Ok(code) => Ended::Exit(code),
                            Err(_) => {
                                job.stop();
                                job.exited().await;
                                Ended::TimedOut(timeout.unwrap_or_default())
                            }
                        },
                        None => Ended::Exit(job.exited().await),
                    };
                    session.report_jobs();
                    if !job.killed() {
                        // The model has the log's path: the log stays.
                        let output = match finish(&job, ended, false, true) {
                            Ok(output) => output.as_text(),
                            Err(e) => e.to_string(),
                        };
                        session.deliver(job_notice(&job, format!("{}\n{output}", job.describe(false))), Notify::Asap);
                    }
                });
                return Ok(ToolOutput::text(text));
            }
        };
        session.jobs.forget(&job);
        finish(&job, ended, call.script, false)
    }
}

/// How a `bash` command's run ended.
enum Ended {
    Exit(i32),
    Aborted,
    TimedOut(f64),
}

/// A command's result from its log: the end of its output, what happened to it, and where
/// the whole output is if that isn't all of it. A log that is all in the result is removed,
/// unless the model was told where it is (`keep_log`: the command went to the background).
fn finish(job: &Job, ended: Ended, script: bool, keep_log: bool) -> Result<ToolOutput> {
    let output = Output::read(&job.log).with_context(|| format!("read {}", job.log.display()))?;
    let cut = &output.tail;
    let mut text = cut.text.clone();
    if let Some(by) = cut.by {
        let (first, last) = (cut.total_lines - cut.lines + 1, cut.total_lines);
        let log = job.log.display();
        text.push_str(&match (cut.partial, by) {
            (true, _) => format!(
                "\n\n[Showing last {} of line {last} (line is {}). Full output: {log}]",
                truncate::size(cut.text.len()),
                truncate::size(output.last_line_bytes)
            ),
            (_, truncate::By::Lines) => format!("\n\n[Showing lines {first}-{last} of {last}. Full output: {log}]"),
            (_, truncate::By::Bytes) => {
                format!("\n\n[Showing lines {first}-{last} of {last} ({} limit). Full output: {log}]", truncate::size(truncate::MAX_BYTES))
            }
        });
    }
    let status = |text: String, status: String| if text.is_empty() { status } else { format!("{text}\n\n{status}") };
    let value = |code: i32| -> Result<Option<Value>> {
        if !script {
            return Ok(None);
        }
        let (full, truncated) = output.for_script(&job.log)?;
        let mut value = json!({
            "output": full, "truncated": truncated, "exit_code": code,
            "wall_time_seconds": (job.duration_ms() as f64 / 100.).round() / 10.,
        });
        if truncated {
            value["full_output_path"] = job.log.to_string_lossy().into();
        }
        Ok(Some(value))
    };
    let result = match ended {
        Ended::Exit(0) => {
            Ok(ToolOutput { value: value(0)?, ..ToolOutput::text(if text.is_empty() { "(no output)".into() } else { text }) })
        }
        Ended::Exit(code) => {
            let text = if text.is_empty() { "(no output)".into() } else { text };
            Ok(ToolOutput { value: value(code)?, ..ToolOutput::error(status(text, format!("Command exited with code {code}"))) })
        }
        Ended::Aborted => Err(anyhow!(status(text, "Command aborted".into()))),
        Ended::TimedOut(secs) => Err(anyhow!(status(text, format!("Command timed out after {secs} seconds")))),
    };
    // The whole output is in the result: the log has nothing more to say.
    if cut.by.is_none() && !keep_log {
        let _ = std::fs::remove_file(&job.log);
    }
    result
}

/// What a command wrote, as far as the model is shown it.
struct Output {
    /// Its end, cut to the limits; `total_lines` is of the whole output.
    tail: truncate::Truncated,
    bytes: u64,
    last_line_bytes: usize,
}

impl Output {
    /// Read a log without holding more of it than can be shown: its lines are counted, its
    /// end is read.
    fn read(log: &Path) -> std::io::Result<Self> {
        let mut file = std::fs::File::open(log)?;
        let bytes = file.metadata()?.len();
        let (mut newlines, mut last_line_bytes, mut ends_with_newline) = (0usize, 0usize, true);
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            let n = file.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            for piece in chunk[..n].split_inclusive(|b| *b == b'\n') {
                ends_with_newline = piece.ends_with(b"\n");
                if ends_with_newline {
                    newlines += 1;
                    last_line_bytes = 0;
                } else {
                    last_line_bytes += piece.len();
                }
            }
        }
        let total_lines = newlines + usize::from(!ends_with_newline);
        // Twice the byte limit holds whatever the cut can keep.
        let window = (2 * truncate::MAX_BYTES as u64).min(bytes);
        file.seek(std::io::SeekFrom::Start(bytes - window))?;
        let mut end = vec![];
        file.read_to_end(&mut end)?;
        let mut text = String::from_utf8_lossy(&end).into_owned();
        // A window that starts midway starts mid-line: that line goes, unless it is all there is.
        if window < bytes
            && let Some(newline) = text.find('\n')
        {
            text.drain(..=newline);
        }
        let mut tail = truncate::tail(&text);
        let by = if total_lines > truncate::MAX_LINES { Some(truncate::By::Lines) } else { None };
        tail.by = tail.by.or(by).or((bytes > truncate::MAX_BYTES as u64).then_some(truncate::By::Bytes));
        tail.total_lines = total_lines;
        Ok(Self { tail, bytes, last_line_bytes })
    }

    /// The output for a codemode script: all of it up to `SCRIPT_OUTPUT_BYTES`, else its
    /// start and end around a note of how much is left out. And whether that was cut.
    fn for_script(&self, log: &Path) -> Result<(String, bool)> {
        let mut file = std::fs::File::open(log)?;
        if self.bytes <= SCRIPT_OUTPUT_BYTES {
            let mut all = vec![];
            file.read_to_end(&mut all)?;
            return Ok((String::from_utf8_lossy(&all).into_owned(), false));
        }
        let half = SCRIPT_OUTPUT_BYTES / 2;
        let (mut head, mut tail) = (vec![0u8; half as usize], vec![]);
        file.read_exact(&mut head)?;
        file.seek(std::io::SeekFrom::Start(self.bytes - half))?;
        file.read_to_end(&mut tail)?;
        let (head, tail) = (String::from_utf8_lossy(&head), String::from_utf8_lossy(&tail));
        Ok((format!("{head}\n\n[... {} bytes omitted ...]\n\n{tail}", self.bytes - 2 * half), true))
    }
}

/// The notice of a background job's end; `text` says how it went.
fn job_notice(job: &Job, text: String) -> Notice {
    Notice::Job {
        text,
        job_id: job.id.clone(),
        command: job.command.clone(),
        exit_code: job.exit().unwrap_or(-1),
        duration_ms: job.duration_ms(),
        log: job.log.to_string_lossy().into_owned(),
    }
}

pub struct AsyncBash {
    session: Weak<Session>,
}

impl AsyncBash {
    pub fn new(session: Weak<Session>) -> Self {
        Self { session }
    }
}

#[derive(Deserialize)]
struct AsyncBashInput {
    action: Option<String>,
    command: Option<String>,
    job_id: Option<String>,
    notify: Option<String>,
    timeout: Option<f64>,
}

#[async_trait]
impl Tool for AsyncBash {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            snippet: Some("Run commands in the background while you keep working, or leave servers and watchers running".into()),
            guidelines: vec![
                // True with `bash_background` off too: then bash never returns a job id.
                "If a bash call returns a job id instead of output, the command moved to the background: its output comes in a message when it ends.".into(),
                "To wait for async_bash jobs or sub-agents, call async_bash `wait` instead of `sleep` / polling: it returns the moment one ends.".into(),
            ],
            ..ToolSpec::new(
                "async_bash",
                "Run a command in the background and keep working; a message tells you when it ends. Start a server or watcher you \
                 leave running with `notify: \"never\"`. Use `bash` when you'd only wait for the output. When you have nothing left \
                 to do but wait for jobs or sub-agents, use `wait`; never `sleep` or poll `status`.",
                json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["start", "status", "kill", "wait"],
                            "description": "\"start\" (default) runs `command`; \"status\" reports on `job_id` (or all jobs); \"kill\" stops \
                                            `job_id`; \"wait\" blocks until one of your jobs or sub-agents ends (whatever its `notify`, bar \
                                            \"never\") or a message comes in, then says what woke it (in full in the next message).",
                        },
                        "command": { "type": "string", "description": "Bash command to run in the background." },
                        "job_id": { "type": "string", "description": "The job to report on or kill." },
                        "notify": {
                            "type": "string",
                            "enum": ["asap", "when_idle", "never"],
                            "description": "start: when its end reaches you: \"asap\" (default) after your current tool calls, \"when_idle\" \
                                            once your turn has ended, \"never\" for a server or watcher you leave running (nothing tells \
                                            you it ended, and your thread doesn't wait on it).",
                        },
                        "timeout": { "type": "number", "description": format!("wait: give up after this many seconds (default {WAIT_S}).") },
                    },
                }),
            )
        }
    }

    async fn call(&self, input: Value, call: &Call) -> Result<ToolOutput> {
        let input: AsyncBashInput = serde_json::from_value(input)?;
        let session = session(&self.session)?;
        let job = |id: &Option<String>| -> Result<Arc<Job>> {
            let id = id.as_deref().ok_or_else(|| anyhow!("`job_id` is required"))?;
            session.jobs.get(id).ok_or_else(|| anyhow!("no job {id} in this thread"))
        };
        let text = match input.action.as_deref().unwrap_or("start") {
            "start" => {
                let command = input
                    .command
                    .as_deref()
                    .filter(|c| !c.trim().is_empty())
                    .ok_or_else(|| anyhow!("`command` is required to start a job"))?;
                let tell = match input.notify.as_deref().unwrap_or("asap") {
                    "asap" => Tell::Asap,
                    "when_idle" => Tell::WhenIdle,
                    "never" => Tell::Never,
                    other => bail!("`notify` is \"asap\", \"when_idle\" or \"never\", not \"{other}\""),
                };
                start(&session, command, tell, &call.cancel).await?
            }
            "wait" => session.wait(Duration::from_secs_f64(input.timeout.unwrap_or(WAIT_S).clamp(0., MAX_TIMEOUT_S)), &call.cancel).await,
            "kill" => {
                let job = job(&input.job_id)?;
                match job.exit() {
                    Some(code) => format!("job {} already ended with exit {code}", job.id),
                    None => {
                        job.kill();
                        session.report_jobs();
                        format!("job {} killed", job.id)
                    }
                }
            }
            "status" if input.job_id.is_some() => job(&input.job_id)?.describe(true),
            "status" => match session.jobs.all() {
                jobs if jobs.is_empty() => "no jobs".into(),
                jobs => jobs.iter().map(|j| j.describe(false)).collect::<Vec<_>>().join("\n"),
            },
            other => bail!("`action` is \"start\", \"status\", \"kill\" or \"wait\", not \"{other}\""),
        };
        Ok(ToolOutput::text(text))
    }
}

/// `async_bash start`: a job, answered inline if it ends within `START_GRACE`. Until then
/// it is its call's, and ends with it when the run is stopped, whatever its `tell`: the
/// model was not told of it yet.
async fn start(session: &Arc<Session>, command: &str, tell: Tell, cancel: &CancellationToken) -> Result<String> {
    if cancel.is_cancelled() {
        bail!("Command aborted");
    }
    let job = session.jobs.spawn(&session.env, command, &session.env.cwd)?;
    tokio::select! {
        _ = job.exited() => {}
        _ = cancel.cancelled() => {
            job.kill();
            job.exited().await;
        }
        _ = tokio::time::sleep(START_GRACE) => {}
    }
    if job.exit().is_some() {
        session.jobs.forget(&job);
        return Ok(job.describe(true));
    }
    job.background(tell);
    session.report_jobs();
    let then = match tell {
        Tell::Never => "It runs on until killed; you won't be told when it ends.",
        _ => "You will be told when it ends; `wait` if you have nothing else to do.",
    };
    let text = format!("job {} started: `{command}`\nlog: {}\n{then}", job.id, job.log.display());
    let session = session.clone();
    tokio::spawn(async move {
        job.exited().await;
        session.report_jobs();
        let when = match tell {
            Tell::Asap => Notify::Asap,
            Tell::WhenIdle => Notify::WhenIdle,
            Tell::Never => return,
        };
        if !job.killed() {
            session.deliver(job_notice(&job, job.describe(true)), when);
        }
    });
    Ok(text)
}

/// `ask_user` puts questions in front of the user as a form; their answers come later, as
/// a message (`Session::answer`).
pub struct AskUser {
    session: Weak<Session>,
}

impl AskUser {
    pub fn new(session: Weak<Session>) -> Self {
        Self { session }
    }
}

#[derive(Deserialize)]
struct AskInput {
    questions: Vec<dirigent_protocol::Question>,
}

#[async_trait]
impl Tool for AskUser {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            snippet: Some(
                "Ask the user questions in a form, when you're blocked on their decisions (several questions, or a choice between options)"
                    .into(),
            ),
            ..ToolSpec::new(
                "ask_user",
                "Ask the user questions. Use it when you're blocked, or have any open questions on a plan or design (anything you'd \
                 otherwise list as questions in your reply). Tool runs async, so use it before your final reply, don't repeat \
                 questions in prose. Not for offers to do more (Want me to fix it?), ask those in your reply.",
                json!({
                    "type": "object",
                    "properties": {
                        "questions": {
                            "type": "array",
                            "minItems": 1,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "text": { "type": "string", "description": "The question." },
                                    "options": {
                                        "type": "array",
                                        "items": { "type": "string" },
                                        "description": "Choices to pick from. Omit for a free-text answer.",
                                    },
                                    "allow_custom": { "type": "boolean", "description": "Also allow a typed answer next to the options. Default false." },
                                    "multi": { "type": "boolean", "description": "Several options may be picked. Default false." },
                                },
                                "required": ["text"],
                            },
                        },
                    },
                    "required": ["questions"],
                }),
            )
        }
    }

    async fn call(&self, input: Value, _call: &Call) -> Result<ToolOutput> {
        let input: AskInput = serde_json::from_value(input)?;
        if input.questions.is_empty() {
            bail!("`questions` needs at least one question");
        }
        let session = session(&self.session)?;
        let id = crate::short_id(6);
        session.emit(crate::Out::Ask(dirigent_protocol::Ask { id: id.clone(), questions: input.questions }));
        Ok(ToolOutput::text(format!(
            "Asked the user (ask {id}). Their answers arrive as a message; continue with other work or end your turn to wait."
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Script, Step, call, prompt, session, text};

    async fn bash(session: &Arc<Session>, input: Value, cancel: CancellationToken) -> Result<ToolOutput> {
        Bash::new(Arc::downgrade(session)).call(input, &Call { id: "c".into(), cancel, script: false }).await
    }

    #[tokio::test]
    async fn a_command_gives_the_end_of_its_output_and_keeps_the_rest_in_its_log() {
        let dir = tempfile::tempdir().unwrap();
        let (_, session) = session(dir.path(), Script::new(vec![]));
        let logs = || std::fs::read_dir(dir.path().join("data/jobs/thread")).map_or(0, |d| d.count());
        let short = bash(&session, json!({"command": "echo out; echo err >&2"}), CancellationToken::new()).await.unwrap();
        assert_eq!((short.as_text().as_str(), short.is_error, logs()), ("out\nerr\n", false, 0));
        let long = bash(&session, json!({"command": "seq 1 3000"}), CancellationToken::new()).await.unwrap().as_text();
        let (shown, note) = long.split_once("\n\n[").unwrap();
        assert!(shown.starts_with("1001\n") && shown.ends_with("\n3000"));
        assert!(note.starts_with("Showing lines 1001-3000 of 3000. Full output: ") && logs() == 1, "{note}");
    }

    #[tokio::test]
    async fn a_command_ends_at_its_timeout_or_when_the_run_is_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let (_, session) = session(dir.path(), Script::new(vec![]));
        let started = std::time::Instant::now();
        let timed_out = bash(&session, json!({"command": "echo first; sleep 30", "timeout": 0.2}), CancellationToken::new()).await;
        assert_eq!(timed_out.unwrap_err().to_string(), "first\n\n\nCommand timed out after 0.2 seconds");
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            stop.cancel();
        });
        assert_eq!(bash(&session, json!({"command": "sleep 30"}), cancel).await.unwrap_err().to_string(), "Command aborted");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(bash(&session, json!({"command": "true", "timeout": -1}), CancellationToken::new()).await.is_err());
    }

    /// A job is its session's from its start, not only once the call that started it has
    /// answered: a run stopped in its first seconds takes it along, and the call says so.
    #[tokio::test]
    async fn a_job_just_started_ends_with_the_run_that_is_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let script = Script::new(vec![Step::Say(vec![call("c1", "async_bash", json!({"command": "sleep 30", "notify": "never"}))])]);
        let (host, session) = session(dir.path(), script);
        session.prompt(prompt("go")).await.unwrap();
        host.until(|outs| outs.iter().any(|o| matches!(o, crate::Out::Event(dirigent_protocol::transcript::Event::ToolStart { .. }))))
            .await;
        session.abort();
        host.settled(1).await;
        let result = &host.transcript()[2];
        assert!(result.starts_with("result: job ") && result.contains(" was killed after "), "{result}");
        assert!(session.jobs.all().is_empty() && !session.busy());
    }

    /// A `bash` command that moved to the background keeps its log, which the model was
    /// told of, though all it wrote is in the notice of its end.
    #[tokio::test]
    async fn a_command_that_moved_to_the_background_keeps_its_log() {
        let dir = tempfile::tempdir().unwrap();
        let script = Script::new(vec![
            Step::Say(vec![call("c1", "bash", json!({"command": "sleep 1; echo late"}))]),
            Step::Say(vec![text("waiting")]),
            Step::Say(vec![text("done")]),
        ]);
        let (host, session) = session(dir.path(), script);
        let tools = Some(vec!["async_bash".into(), "bash_background".into()]);
        session.prompt(crate::session::Prompt { tools, ..prompt("go") }).await.unwrap();
        host.settled(2).await;
        let transcript = host.transcript();
        let log = transcript[2].split_once("(log: ").and_then(|(_, rest)| rest.split_once(')')).expect("no log named").0;
        assert!(transcript[4].starts_with("notice: job ") && transcript[4].ends_with("\nlate\n"), "{transcript:#?}");
        assert_eq!(std::fs::read_to_string(log).unwrap(), "late\n");
    }

    /// A job that outlives its call ends as a notice, which a waiting model wakes on.
    #[tokio::test]
    async fn a_background_job_tells_of_its_end() {
        let dir = tempfile::tempdir().unwrap();
        let script = Script::new(vec![
            Step::Say(vec![call("c1", "async_bash", json!({"command": "sleep 2.3; echo late"}))]),
            Step::Say(vec![call("c2", "async_bash", json!({"action": "wait"}))]),
            Step::Say(vec![text("done")]),
        ]);
        let (host, session) = session(dir.path(), script);
        session.prompt(prompt("go")).await.unwrap();
        host.settled(1).await;
        let transcript = host.transcript();
        assert_eq!(transcript.len(), 7, "{transcript:#?}");
        assert!(transcript[2].starts_with("result: job ") && transcript[2].contains(" started: `sleep 2.3; echo late`\nlog: "));
        assert!(transcript[4].starts_with("result: Woke after ") && transcript[4].contains("in full in the next message):\njob "));
        assert!(
            transcript[5].starts_with("notice: job ")
                && transcript[5].contains(" succeeded after ")
                && transcript[5].ends_with("last lines:\nlate")
        );
        // The host heard of the job while it ran, and of its end.
        let jobs: Vec<u32> = host
            .outs
            .lock()
            .unwrap()
            .iter()
            .filter_map(|o| if let crate::Out::Jobs { running, .. } = o { Some(*running) } else { None })
            .collect();
        assert_eq!(jobs, [1, 0]);
    }
}
