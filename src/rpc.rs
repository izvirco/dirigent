use std::{
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Child, ChildStdin, Stdio},
    sync::{Arc, Mutex},
    thread,
};

use async_channel::Sender;
use serde_json::Value;

use crate::{model::Id, platform};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RuntimeTarget {
    Harness(Id, u64),
    Project(Id),
}

pub(crate) enum RuntimeEvent {
    Json {
        target: RuntimeTarget,
        value: Value,
    },
    Error {
        target: RuntimeTarget,
        message: String,
    },
    Exited {
        target: RuntimeTarget,
    },
}

pub(crate) struct PiProcess {
    stdin: Arc<Mutex<ChildStdin>>,
    child: Arc<Mutex<Child>>,
}

impl PiProcess {
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
                            .send_blocking(RuntimeEvent::Json { target, value })
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(error) => {
                        let line = String::from_utf8_lossy(&buffer);
                        let _ = events.send_blocking(RuntimeEvent::Error {
                            target,
                            message: format!("invalid JSON from pi: {error} ({line})"),
                        });
                    }
                }
            }
            Err(error) => {
                let _ = events.send_blocking(RuntimeEvent::Error {
                    target,
                    message: format!("could not read pi output: {error}"),
                });
                break;
            }
        }
    }
    let _ = events.send_blocking(RuntimeEvent::Exited { target });
}

fn read_stderr(target: RuntimeTarget, stderr: impl Read, events: Sender<RuntimeEvent>) {
    for line in BufReader::new(stderr).lines() {
        match line {
            Ok(line) if !line.trim().is_empty() => {
                if events
                    .send_blocking(RuntimeEvent::Error {
                        target,
                        message: line,
                    })
                    .is_err()
                {
                    break;
                }
            }
            Ok(_) => {}
            Err(error) => {
                let _ = events.send_blocking(RuntimeEvent::Error {
                    target,
                    message: format!("could not read pi diagnostics: {error}"),
                });
                break;
            }
        }
    }
}

#[cfg(test)]
fn rpc_command_path(session_file: Option<&Path>) -> (String, Vec<String>) {
    let mut args = vec!["--mode".into(), "rpc".into()];
    if let Some(path) = session_file {
        args.push("--session".into());
        args.push(path.to_string_lossy().into_owned());
    }
    ("pi".into(), args)
}

#[cfg(test)]
mod tests {
    use super::rpc_command_path;
    use std::path::Path;

    #[test]
    fn resumed_process_uses_native_pi_session() {
        let (program, args) = rpc_command_path(Some(Path::new("/tmp/session.jsonl")));
        assert_eq!(program, "pi");
        assert_eq!(args, ["--mode", "rpc", "--session", "/tmp/session.jsonl"]);
    }
}
