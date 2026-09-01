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

/// Decodes Pi's newline-delimited stdout without terminating the stream on one malformed record.
fn read_stdout(target: RuntimeTarget, stdout: impl Read, events: Sender<RuntimeEvent>) {
    let mut reader = BufReader::new(stdout);
    let mut buffer = Vec::new();
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
                let _ = events.send_blocking(RuntimeEvent::new(RuntimeEventKind::Error {
                    target,
                    message: format!("could not read pi output: {error}"),
                }));
                break;
            }
        }
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
