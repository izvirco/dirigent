//! Runs Pi in RPC mode and transports newline-delimited events.

use std::{
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Child, ChildStdin, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::Instant,
};

use async_channel::Sender;
use serde_json::Value;

use crate::{model::Id, platform};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RuntimeTarget {
    Harness(Id, u64),
    Project(Id),
}

pub(crate) struct RuntimeEvent {
    pub(crate) queued_at: Instant,
    pub(crate) kind: RuntimeEventKind,
}

pub(crate) enum RuntimeEventKind {
    Json {
        target: RuntimeTarget,
        value: Value,
    },
    Error {
        target: RuntimeTarget,
        message: String,
    },
    Diagnostic {
        target: RuntimeTarget,
        message: String,
    },
    Exited {
        target: RuntimeTarget,
    },
}

impl RuntimeEvent {
    fn new(kind: RuntimeEventKind) -> Self {
        Self {
            queued_at: Instant::now(),
            kind,
        }
    }

    pub(crate) fn target(&self) -> RuntimeTarget {
        match &self.kind {
            RuntimeEventKind::Json { target, .. }
            | RuntimeEventKind::Error { target, .. }
            | RuntimeEventKind::Diagnostic { target, .. }
            | RuntimeEventKind::Exited { target } => *target,
        }
    }

    pub(crate) fn diagnostic_kind(&self) -> &str {
        let RuntimeEventKind::Json { value, .. } = &self.kind else {
            return match &self.kind {
                RuntimeEventKind::Error { .. } => "error",
                RuntimeEventKind::Diagnostic { .. } => "diagnostic",
                RuntimeEventKind::Exited { .. } => "exited",
                RuntimeEventKind::Json { .. } => unreachable!(),
            };
        };
        match value.get("type").and_then(Value::as_str) {
            Some("message_update") => value
                .pointer("/assistantMessageEvent/type")
                .and_then(Value::as_str)
                .unwrap_or("message_update"),
            Some(kind) => kind,
            None => "unknown",
        }
    }
}

pub(crate) struct PiProcess {
    stdin: Arc<Mutex<ChildStdin>>,
    child: Arc<Mutex<Child>>,
}

impl PiProcess {
    /// Starts Pi and dedicates reader threads to its structured stdout and diagnostic stderr.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn(
        target: RuntimeTarget,
        cwd: &Path,
        session_file: Option<&Path>,
        session_name: &str,
        bridge_extension: Option<&Path>,
        nix_enabled: bool,
        ephemeral: bool,
        events: Sender<RuntimeEvent>,
    ) -> Result<Self, String> {
        let mut command = platform::pi_command(nix_enabled)?;
        // npm installs Pi as a .cmd shim on Windows, where batch arguments cannot
        // contain line breaks. Initial titles may come from multiline prompts.
        let session_name = single_line_session_name(session_name);
        command
            .arg("--mode")
            .arg("rpc")
            .arg("--name")
            .arg(session_name)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some(extension) = bridge_extension {
            command.arg("--extension").arg(extension);
        }
        if ephemeral {
            command.arg("--no-session");
        } else if let Some(session_file) = session_file {
            command.arg("--session").arg(session_file);
        }

        let mut child = command
            .spawn()
            .map_err(|error| format!("could not start pi RPC process: {error}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "pi did not provide an RPC stdin pipe".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "pi did not provide an RPC stdout pipe".to_string())?;
        let stderr = child.stderr.take();

        let process = Self {
            stdin: Arc::new(Mutex::new(stdin)),
            child: Arc::new(Mutex::new(child)),
        };

        let process_label = match target {
            RuntimeTarget::Harness(id, generation) => format!("harness-{id}-{generation}"),
            RuntimeTarget::Project(id) => format!("project-{id}"),
        };
        let stdout_events = events.clone();
        thread::Builder::new()
            .name(format!("pi-rpc-{process_label}"))
            .spawn(move || read_stdout(target, stdout, stdout_events))
            .map_err(|error| format!("could not start pi output reader: {error}"))?;

        if let Some(stderr) = stderr {
            thread::Builder::new()
                .name(format!("pi-stderr-{process_label}"))
                .spawn(move || read_stderr(target, stderr, events))
                .map_err(|error| format!("could not start pi error reader: {error}"))?;
        }

        Ok(process)
    }

    /// Writes one complete newline-delimited JSON command while holding the shared stdin lock.
    pub(crate) fn send(&self, command: Value) -> Result<(), String> {
        let mut stdin = self
            .stdin
            .lock()
            .map_err(|_| "pi RPC input lock was poisoned".to_string())?;
        serde_json::to_writer(&mut *stdin, &command)
            .map_err(|error| format!("could not encode pi RPC command: {error}"))?;
        stdin
            .write_all(b"\n")
            .and_then(|_| stdin.flush())
            .map_err(|error| format!("could not send command to pi: {error}"))
    }

    pub(crate) fn stop(&self) {
        if let Ok(mut child) = self.child.lock() {
            platform::stop_child(&mut child);
        }
    }
}

impl Drop for PiProcess {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Buffers assistant text on the reader thread, before it can wake the UI or reparse Markdown.
/// Each reader belongs to one process generation, so partial lines cannot leak between agents.
#[derive(Default)]
struct LineBufferedEvents {
    pending: Option<Value>,
}

impl LineBufferedEvents {
    fn flush(&mut self) -> Option<Value> {
        self.pending.take().filter(|value| {
            value["assistantMessageEvent"]["delta"]
                .as_str()
                .is_some_and(|delta| !delta.is_empty())
        })
    }

    fn push(&mut self, mut value: Value) -> Vec<Value> {
        let mut ready = Vec::new();
        let event_type = value["type"].as_str();
        let kind = value["assistantMessageEvent"]["type"].as_str();
        if event_type == Some("message_update")
            && matches!(kind, Some("text_delta" | "thinking_delta"))
        {
            if self.pending.as_ref().is_some_and(|pending| {
                pending["assistantMessageEvent"]["type"] != value["assistantMessageEvent"]["type"]
                    || pending["assistantMessageEvent"]["contentIndex"]
                        != value["assistantMessageEvent"]["contentIndex"]
            }) {
                ready.extend(self.flush());
            }
            let delta = value["assistantMessageEvent"]["delta"].take();
            let delta = delta.as_str().unwrap_or_default();
            let pending = self.pending.get_or_insert_with(|| {
                value["assistantMessageEvent"]["delta"] = Value::String(String::new());
                value
            });
            let Value::String(text) = &mut pending["assistantMessageEvent"]["delta"] else {
                unreachable!("buffered delta is a string");
            };
            // The buffered remainder contains no newline; scan only the new chunk.
            let newline = delta.rfind('\n').map(|offset| text.len() + offset);
            text.push_str(delta);
            if let Some(newline) = newline {
                let remainder = text.split_off(newline + 1);
                ready.push(pending.clone());
                pending["assistantMessageEvent"]["delta"] = Value::String(remainder);
            }
            return ready;
        }

        // Unrelated RPC replies and tool snapshots must not reveal an unfinished line.
        // Block boundaries also flush when a provider omits the corresponding text_end.
        let boundary = match event_type {
            Some("message_update") => matches!(
                kind,
                Some(
                    "text_start"
                        | "thinking_start"
                        | "toolcall_start"
                        | "text_end"
                        | "thinking_end"
                        | "done"
                        | "error"
                )
            ),
            Some(
                "message_end"
                | "agent_end"
                | "agent_settled"
                | "tool_execution_start"
                | "compaction_start",
            ) => true,
            _ => false,
        };
        if boundary {
            ready.extend(self.flush());
        }
        // The conversation uses tool_execution_* events, not streamed tool arguments.
        if event_type == Some("message_update")
            && matches!(
                kind,
                Some(
                    "text_start"
                        | "thinking_start"
                        | "toolcall_start"
                        | "toolcall_delta"
                        | "toolcall_end"
                )
            )
        {
            return ready;
        }
        ready.push(value);
        ready
    }
}

/// Decodes Pi's newline-delimited stdout without terminating the stream on one malformed record.
fn read_stdout(target: RuntimeTarget, stdout: impl Read, events: Sender<RuntimeEvent>) {
    let mut reader = BufReader::new(stdout);
    let mut buffer = Vec::new();
    let mut lines = LineBufferedEvents::default();
    loop {
        buffer.clear();
        match reader.read_until(b'\n', &mut buffer) {
            Ok(0) => break,
            Ok(_) => {
                if buffer.last() == Some(&b'\n') {
                    buffer.pop();
                }
                if buffer.last() == Some(&b'\r') {
                    buffer.pop();
                }
                if buffer.is_empty() {
                    continue;
                }
                match serde_json::from_slice(&buffer) {
                    Ok(value) => {
                        for value in lines.push(value) {
                            if events
                                .send_blocking(RuntimeEvent::new(RuntimeEventKind::Json {
                                    target,
                                    value,
                                }))
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                    Err(error) => {
                        let line = String::from_utf8_lossy(&buffer);
                        let _ = events.send_blocking(RuntimeEvent::new(RuntimeEventKind::Error {
                            target,
                            message: format!("invalid JSON from pi: {error} ({line})"),
                        }));
                    }
                }
            }
            Err(error) => {
                if let Some(value) = lines.flush() {
                    let _ = events
                        .send_blocking(RuntimeEvent::new(RuntimeEventKind::Json { target, value }));
                }
                let _ = events.send_blocking(RuntimeEvent::new(RuntimeEventKind::Error {
                    target,
                    message: format!("could not read pi output: {error}"),
                }));
                break;
            }
        }
    }
    if let Some(value) = lines.flush() {
        let _ = events.send_blocking(RuntimeEvent::new(RuntimeEventKind::Json { target, value }));
    }
    let _ = events.send_blocking(RuntimeEvent::new(RuntimeEventKind::Exited { target }));
}

fn single_line_session_name(name: &str) -> String {
    name.split(['\r', '\n'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn read_stderr(target: RuntimeTarget, stderr: impl Read, events: Sender<RuntimeEvent>) {
    for line in BufReader::new(stderr).lines() {
        match line {
            Ok(line) if !line.trim().is_empty() => {
                // RPC protocol failures arrive on stdout as structured events. Stderr is
                // unstructured diagnostic output from Pi and extensions, so it must not
                // fail a harness or add an error message to the conversation.
                if events
                    .send_blocking(RuntimeEvent::new(RuntimeEventKind::Diagnostic {
                        target,
                        message: line,
                    }))
                    .is_err()
                {
                    break;
                }
            }
            Ok(_) => {}
            Err(error) => {
                let _ = events.send_blocking(RuntimeEvent::new(RuntimeEventKind::Error {
                    target,
                    message: format!("could not read pi diagnostics: {error}"),
                }));
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn delta(kind: &str, index: usize, text: &str) -> Value {
        json!({"type": "message_update", "assistantMessageEvent": {
            "type": kind, "contentIndex": index, "delta": text
        }})
    }

    fn text(value: &Value) -> &str {
        value["assistantMessageEvent"]["delta"].as_str().unwrap()
    }

    #[test]
    fn buffers_actual_lines_and_flushes_the_remainder_before_response_end() {
        let mut lines = LineBufferedEvents::default();
        assert!(lines.push(delta("text_delta", 0, "Hello 🌍")).is_empty());
        let ready = lines.push(delta("text_delta", 0, "!\nSecond\npart"));
        assert_eq!(ready.len(), 1);
        assert_eq!(text(&ready[0]), "Hello 🌍!\nSecond\n");
        assert!(lines.push(delta("text_delta", 0, "ial")).is_empty());

        let reply = json!({"type": "response", "command": "get_state"});
        assert_eq!(lines.push(reply.clone()), vec![reply]);
        let end = json!({"type": "message_update", "assistantMessageEvent": {"type": "text_end"}});
        let ready = lines.push(end.clone());
        assert_eq!(ready.len(), 2);
        assert_eq!(text(&ready[0]), "partial");
        assert_eq!(ready[1], end);
        assert!(lines.flush().is_none());
    }

    #[test]
    fn separates_blocks_and_thinking_and_ignores_tool_argument_tokens() {
        let mut lines = LineBufferedEvents::default();
        assert!(lines.push(delta("thinking_delta", 0, "Think")).is_empty());
        let ready = lines.push(delta("text_delta", 1, "First"));
        assert_eq!(text(&ready[0]), "Think");
        let ready = lines.push(delta("text_delta", 2, "Second\n"));
        assert_eq!(ready.len(), 2);
        assert_eq!(text(&ready[0]), "First");
        assert_eq!(text(&ready[1]), "Second\n");
        assert!(
            lines
                .push(delta("toolcall_delta", 3, "{\"path\""))
                .is_empty()
        );
        assert!(lines.flush().is_none());
    }

    #[test]
    fn reader_flushes_unterminated_responses_before_exit() {
        let target = RuntimeTarget::Harness(7, 2);
        let (sender, receiver) = async_channel::unbounded();
        let input = format!(
            "{}\n{}\n",
            delta("text_delta", 0, "No "),
            delta("text_delta", 0, "newline")
        );
        read_stdout(target, input.as_bytes(), sender);
        let events = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].target(), target);
        let RuntimeEventKind::Json { value, .. } = &events[0].kind else {
            panic!("expected text")
        };
        assert_eq!(text(value), "No newline");
        assert!(matches!(events[1].kind, RuntimeEventKind::Exited { .. }));
    }
}
