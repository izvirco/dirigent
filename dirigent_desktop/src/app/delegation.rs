//! The bundled extension calls these operations over the existing Pi bridge.
//! Dirigent remains the sole owner of child sessions, assignments, and workspaces.

use super::*;
use crate::delegation::{AgentJob, AgentRun, WorkStatus, bounded_text};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SpawnAgent {
    name: String,
    model: String,
    thinking: String,
    workspace: String,
    prompt: String,
    #[serde(default)]
    allow_dirty_base: bool,
}

impl Dirigent {
    fn delegation_reply(
        &mut self,
        parent: Id,
        generation: u64,
        request: &str,
        result: Result<Value, String>,
    ) {
        let Some(index) = self
            .harnesses
            .iter()
            .position(|h| h.id == parent && h.process_generation == generation)
        else {
            return;
        };
        let response = match result {
            Ok(value) => json!({"requestId": request, "value": value}),
            Err(error) => json!({"requestId": request, "error": error}),
        };
        // Extension commands execute during tools and must not wait for the parent to idle.
        self.send_value(
            index,
            json!({
                "id": "dirigent-agent-response", "type": "prompt",
                "message": format!("/dirigent-agents-response {response}")
            }),
        );
    }

    fn delegation_job_running(&self, parent: Id, job: &str) -> bool {
        self.harnesses
            .iter()
            .find(|h| h.id == parent)
            .is_some_and(|h| {
                h.delegation
                    .jobs
                    .iter()
                    .any(|j| j.id == job && j.status == WorkStatus::Running)
            })
    }

    pub(super) fn handle_delegation_request(
        &mut self,
        index: usize,
        value: &Value,
        cx: &mut Context<Self>,
    ) {
        let parent = self.harnesses[index].id;
        let generation = self.harnesses[index].process_generation;
        let request = value
            .get("requestId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let job = value
            .get("jobId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let args = value.get("args").cloned().unwrap_or(Value::Null);
        if request.is_empty() || request.len() > 128 || job.len() > 128 {
            return;
        }
        if method == "spawn" && self.delegation_job_running(parent, &job) {
            let parsed = serde_json::from_value::<SpawnAgent>(args).map_err(|e| e.to_string());
            let parsed = parsed.and_then(|args| {
                validate_spawn(&args)?;
                Ok(args)
            });
            let args = match parsed {
                Ok(args) => args,
                Err(error) => {
                    self.delegation_reply(parent, generation, &request, Err(error));
                    return;
                }
            };
            let cwd = match self.working_directory_for_harness(parent) {
                Ok(cwd) => cwd,
                Err(error) => {
                    self.delegation_reply(parent, generation, &request, Err(error));
                    return;
                }
            };
            // VCS probing can invoke Git/JJ. Never block the UI or use a stale project snapshot.
            let task = cx.background_spawn(async move {
                let snapshot = if args.workspace == "new" {
                    let snapshot = crate::vcs::probe_repository(&cwd)?
                        .ok_or_else(|| "The current workspace is not a Git/JJ repository.".to_string())?;
                    if snapshot.backend == crate::model::WorkspaceBackend::Git && snapshot.dirty && !args.allow_dirty_base {
                        return Err("The Git checkout is dirty. A new worktree starts at HEAD, not at uncommitted changes. Commit first, use current, or explicitly set allowDirtyBase: true to exclude those changes.".to_string());
                    }
                    Some(snapshot)
                } else { None };
                Ok((args, snapshot))
            });
            cx.spawn(async move |this, cx| {
                let result = task.await;
                let _ = this.update(cx, |this, cx| {
                    if !this
                        .harnesses
                        .iter()
                        .any(|h| h.id == parent && h.process_generation == generation)
                    {
                        return;
                    }
                    let result = result.and_then(|(args, snapshot)| {
                        if !this.delegation_job_running(parent, &job) {
                            return Err("Workflow no longer running.".into());
                        }
                        this.spawn_delegated_agent(parent, &job, &request, args, snapshot, cx)
                    });
                    this.delegation_reply(parent, generation, &request, result);
                    cx.notify();
                });
            })
            .detach();
            return;
        }
        let result = self.delegation_operation(index, &job, &request, method, args);
        self.delegation_reply(parent, generation, &request, result);
        cx.notify();
    }

    fn delegation_operation(
        &mut self,
        parent_index: usize,
        job: &str,
        request: &str,
        method: &str,
        args: Value,
    ) -> Result<Value, String> {
        let parent = self.harnesses[parent_index].id;
        match method {
            "job_start" => {
                if job.is_empty() {
                    return Err("Missing workflow ID.".into());
                }
                if self.harnesses[parent_index]
                    .delegation
                    .jobs
                    .iter()
                    .any(|j| j.id == job)
                {
                    return Err(
                        "Workflow ID already exists; inspect it rather than replaying.".into(),
                    );
                }
                self.harnesses[parent_index].delegation.jobs.push(AgentJob {
                    id: job.into(),
                    handoff: args["handoff"].as_bool().unwrap_or(false),
                    tool_call_id: string_arg(&args, "toolCallId")?,
                    title: bounded_text(&string_arg(&args, "title")?, 160),
                    status: WorkStatus::Running,
                    result: String::new(),
                });
                self.persist();
                return Ok(json!({"jobId": job}));
            }
            "jobs" => return Ok(json!(self.harnesses[parent_index].delegation.jobs)),
            "cancel_job" => {
                let status = if args["status"].as_str() == Some("failed") {
                    WorkStatus::Failed
                } else {
                    WorkStatus::Cancelled
                };
                self.cancel_delegation_job(parent, job, status);
                return Ok(Value::Null);
            }
            "job_finish" => {
                let status: WorkStatus =
                    serde_json::from_value(args["status"].clone()).map_err(|e| e.to_string())?;
                if status == WorkStatus::Running {
                    return Err("Expected a terminal workflow status.".into());
                }
                let entry = self.harnesses[parent_index]
                    .delegation
                    .jobs
                    .iter_mut()
                    .find(|j| j.id == job)
                    .ok_or_else(|| "Unknown workflow.".to_string())?;
                if entry.status == WorkStatus::Running || entry.status == status {
                    entry.status = status;
                    entry.result =
                        bounded_text(args["result"].as_str().unwrap_or_default(), 24_000);
                    self.persist();
                }
                return Ok(Value::Null);
            }
            _ => {}
        }
        if !self.delegation_job_running(parent, job) {
            return Err("Workflow is cancelled or no longer running.".into());
        }
        match method {
            "ping_parent" => {
                let message = string_arg(&args, "message")?;
                if message.len() > 8000 {
                    return Err("Parent messages must be at most 8000 bytes.".into());
                }
                let manager = self.harnesses[parent_index]
                    .delegation
                    .parent
                    .ok_or("Only child agents can ping a parent.")?;
                let run = self.harnesses[parent_index]
                    .delegation
                    .runs
                    .last()
                    .filter(|run| run.status == WorkStatus::Running)
                    .ok_or("No active assignment to report on.")?;
                if !self.handoff_job(manager, &run.job_id) {
                    return Err("This assignment was not launched with mode: handoff.".into());
                }
                let run = self.harnesses[parent_index]
                    .delegation
                    .active_run_mut()
                    .unwrap();
                // Delivery waits for settlement so the manager can immediately send feedback.
                run.parent_message = Some(message);
                self.persist();
                Ok(json!({"queued": true, "delivery": "when this assignment settles"}))
            }
            "list" => Ok(Value::Array(
                self.harnesses
                    .iter()
                    .filter(|h| h.delegation.parent == Some(parent))
                    .map(|h| self.delegated_agent_info(h.id, false))
                    .collect(),
            )),
            "inspect" => {
                let id = id_arg(&args, "agentId")?;
                self.child_index(parent, id)?;
                Ok(self.delegated_agent_info(id, args["transcript"].as_bool() == Some(true)))
            }
            "runs" => {
                let ids: Vec<String> =
                    serde_json::from_value(args["runIds"].clone()).map_err(|e| e.to_string())?;
                if ids.is_empty() || ids.len() > 64 {
                    return Err("Wait requires 1–64 run IDs.".into());
                }
                ids.iter()
                    .map(|id| {
                        self.harnesses
                            .iter()
                            .filter(|h| h.delegation.parent == Some(parent))
                            .find_map(|h| {
                                h.delegation
                                    .runs
                                    .iter()
                                    .find(|r| &r.id == id)
                                    .map(|run| json!({"agentId": h.id.to_string(), "run": run}))
                            })
                            .ok_or_else(|| format!("Unknown or deleted run {id}."))
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map(Value::Array)
            }
            "send" => {
                let id = id_arg(&args, "agentId")?;
                let index = self.child_index(parent, id)?;
                if self.harnesses[index].cancellation_pending
                    || (self.harnesses[index].startup_settings_pending
                        && self.harnesses[index].process.is_some())
                    || matches!(
                        self.harnesses[index].status,
                        HarnessStatus::Working | HarnessStatus::Starting
                    )
                    || self.harnesses[index].delegation.active_run_mut().is_some()
                {
                    return Err(
                        "Child is busy. Wait for its assignment or stop it before sending another."
                            .into(),
                    );
                }
                self.working_directory_for_harness(id)?;
                let prompt = assignment_prompt(
                    parent,
                    request,
                    &string_arg(&args, "prompt")?,
                    self.handoff_job(parent, job),
                );
                self.harnesses[index].archived = false;
                self.harnesses[index]
                    .delegation
                    .runs
                    .push(new_run(request, job));
                self.harnesses[index]
                    .messages
                    .push(Message::new(MessageRole::User, prompt.clone()));
                self.persist();
                self.start_harness(id, Some((prompt, Vec::new())));
                self.sync_conversation_list(index, None);
                if let Some(tool_call_id) = self.harnesses[parent_index]
                    .delegation
                    .jobs
                    .iter()
                    .find(|candidate| candidate.id == job)
                    .map(|job| job.tool_call_id.clone())
                {
                    self.remeasure_work_group_for_tool_call(&tool_call_id);
                }
                Ok(json!({"agentId": id.to_string(), "runId": request}))
            }
            "stop" => {
                let id = id_arg(&args, "agentId")?;
                self.child_index(parent, id)?;
                self.abort_harness(id);
                Ok(Value::Null)
            }
            _ => Err(format!("Unknown delegation operation: {method}")),
        }
    }

    fn child_index(&self, parent: Id, id: Id) -> Result<usize, String> {
        self.harnesses
            .iter()
            .position(|h| h.id == id && h.delegation.parent == Some(parent))
            .ok_or_else(|| {
                "Unknown child agent (only this manager's children can be controlled).".into()
            })
    }

    fn spawn_delegated_agent(
        &mut self,
        parent: Id,
        job: &str,
        request: &str,
        args: SpawnAgent,
        snapshot: Option<RepositorySnapshot>,
        cx: &mut Context<Self>,
    ) -> Result<Value, String> {
        let parent_index = self
            .harnesses
            .iter()
            .position(|h| h.id == parent)
            .ok_or("Manager was deleted.")?;
        if self
            .harnesses
            .iter()
            .filter(|h| {
                h.delegation.parent == Some(parent)
                    && matches!(h.status, HarnessStatus::Starting | HarnessStatus::Working)
            })
            .count()
            >= 16
        {
            return Err("At most 16 active children per manager.".into());
        }
        let mut ancestor = Some(parent);
        let mut depth = 0;
        while let Some(id) = ancestor {
            depth += 1;
            if depth > 4 {
                return Err("Delegation is limited to four levels of children.".into());
            }
            ancestor = self
                .harnesses
                .iter()
                .find(|h| h.id == id)
                .and_then(|h| h.delegation.parent);
        }
        let id = self.allocate_id();
        let prompt =
            assignment_prompt(parent, request, &args.prompt, self.handoff_job(parent, job));
        let mut harness = Harness::new(
            id,
            self.harnesses[parent_index].project_id,
            args.name,
            self.allocate_sidebar_order(),
        );
        harness.set_derived_title();
        harness.delegation.parent = Some(parent);
        harness.delegation.runs.push(new_run(request, job));
        harness.nix_enabled = self.harnesses[parent_index].nix_enabled;
        harness.model = Some(args.model);
        harness.thinking_level = Some(args.thinking);
        if snapshot.is_none() {
            harness.workspace_id = self.harnesses[parent_index].workspace_id.clone();
        }
        harness
            .messages
            .push(Message::new(MessageRole::User, prompt.clone()));
        harness.pending_initial_prompt = Some((prompt, Vec::new()));
        self.harnesses.push(harness);
        self.add_composer_input(id, cx);
        if let Some(tool_call_id) = self.harnesses[parent_index]
            .delegation
            .jobs
            .iter()
            .find(|candidate| candidate.id == job)
            .map(|job| job.tool_call_id.clone())
        {
            self.remeasure_work_group_for_tool_call(&tool_call_id);
        }
        self.persist();
        if let Some(snapshot) = snapshot {
            self.provision_harness_workspace(id, snapshot);
        } else {
            self.start_harness(id, None);
        }
        Ok(json!({"agentId": id.to_string(), "runId": request}))
    }

    fn delegated_agent_info(&self, id: Id, transcript: bool) -> Value {
        let h = self
            .harnesses
            .iter()
            .find(|h| h.id == id)
            .expect("known child");
        let workspace = self.workspace_for_harness(id);
        let mut info = json!({
            "agentId": id.to_string(), "name": h.title, "model": h.model, "thinking": h.thinking_level,
            "workspace": workspace, "cwd": self.working_directory_for_harness(id).ok(),
            "sessionFile": h.session_file, "runs": h.delegation.runs,
            "needsInput": h.attention_required,
            "cancelling": h.cancellation_pending || (h.startup_settings_pending && h.process.is_some()),
        });
        if transcript {
            let text = h
                .messages
                .iter()
                .rev()
                .take(40)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .map(|m| format!("{:?}: {}", m.role, m.copy_text))
                .collect::<Vec<_>>()
                .join("\n\n");
            info["transcript"] = json!(bounded_text(&text, 30_000));
            info["transcriptLoaded"] = json!(h.loaded_messages);
        }
        info
    }

    pub(super) fn record_delegated_message(&mut self, index: usize, value: &Value) {
        let message = &value["message"];
        if let Some(run) = self.harnesses[index].delegation.active_run_mut() {
            if message["role"].as_str() == Some("assistant") {
                run.handed_off = false;
                run.stop_reason = message["stopReason"].as_str().map(str::to_string);
                run.result = bounded_text(&content_text(&message["content"]), 12_000);
                run.error = message["errorMessage"]
                    .as_str()
                    .map(|s| bounded_text(s, 2_000));
            } else if message["role"].as_str() == Some("toolResult")
                && message["toolName"].as_str() == Some("dirigent_agents")
                && message["isError"].as_bool() == Some(false)
                && message["details"]["handoff"].as_bool() == Some(true)
            {
                run.handed_off = true;
                run.result = bounded_text(&content_text(&message["content"]), 12_000);
            }
        }
    }

    pub(super) fn finish_delegated_run(&mut self, index: usize, status: WorkStatus) {
        let error = self.harnesses[index].error.clone();
        let Some(run) = self.harnesses[index].delegation.active_run_mut() else {
            return;
        };
        run.finish(status, error);
        let run = run.clone();
        self.persist();
        // Explicit stop must not immediately wake the manager back up.
        if run.status == WorkStatus::Cancelled {
            return;
        }
        let child = &self.harnesses[index];
        let Some(parent_index) = child
            .delegation
            .parent
            .and_then(|parent| self.harnesses.iter().position(|h| h.id == parent))
        else {
            return;
        };
        let parent = &self.harnesses[parent_index];
        if !self.handoff_job(parent.id, &run.job_id)
            || parent.process.is_none()
            || parent.cancellation_pending
        {
            return;
        }
        let notice = json!({
            "jobId": run.job_id, "agentId": child.id.to_string(), "name": child.title, "run": run,
        });
        // The receiving extension checks the launch branch and queues behind any human turn.
        self.send_value(
            parent_index,
            json!({
                "id": "dirigent-agent-response", "type": "prompt",
                "message": format!("/dirigent-agents-notify {notice}"),
            }),
        );
    }

    fn handoff_job(&self, parent: Id, job: &str) -> bool {
        self.harnesses
            .iter()
            .find(|h| h.id == parent)
            .is_some_and(|h| {
                h.delegation.jobs.iter().any(|j| {
                    j.id == job
                        && j.handoff
                        && matches!(j.status, WorkStatus::Running | WorkStatus::Completed)
                })
            })
    }

    fn cancel_delegation_job(&mut self, parent: Id, job: &str, status: WorkStatus) {
        if let Some(h) = self.harnesses.iter_mut().find(|h| h.id == parent)
            && let Some(j) = h.delegation.jobs.iter_mut().find(|j| j.id == job)
            && j.status == WorkStatus::Running
        {
            j.status = status;
        }
        let children = self
            .harnesses
            .iter()
            .filter(|h| {
                h.delegation.parent == Some(parent)
                    && h.delegation
                        .runs
                        .last()
                        .is_some_and(|r| r.job_id == job && r.status == WorkStatus::Running)
            })
            .map(|h| h.id)
            .collect::<Vec<_>>();
        for id in children {
            self.abort_harness(id);
        }
        self.persist();
    }

    /// Stops scripts and delegated work without deleting sessions or workspaces.
    pub(crate) fn stop_delegation(&mut self, parent: Id) {
        if let Some(index) = self.harnesses.iter().position(|h| h.id == parent) {
            let jobs = self.harnesses[index]
                .delegation
                .jobs
                .iter()
                .filter(|j| j.status == WorkStatus::Running)
                .map(|j| j.id.clone())
                .collect::<Vec<_>>();
            for job in jobs {
                self.cancel_delegation_job(parent, &job, WorkStatus::Cancelled);
            }
            if let Some(process) = self.harnesses[index].process.as_ref() {
                let _ = process.send(json!({"id": "dirigent-agent-response", "type": "prompt", "message": "/dirigent-agents-stop"}));
            }
        }
        // Also cover children explicitly left running after their launching script returned.
        let children = self
            .harnesses
            .iter()
            .filter(|h| {
                h.delegation.parent == Some(parent)
                    && (h
                        .delegation
                        .runs
                        .last()
                        .is_some_and(|r| r.status == WorkStatus::Running)
                        || h.delegation
                            .jobs
                            .iter()
                            .any(|j| j.status == WorkStatus::Running))
            })
            .map(|h| h.id)
            .collect::<Vec<_>>();
        for id in children {
            self.abort_harness(id);
        }
    }
}

fn assignment_prompt(parent: Id, run: &str, prompt: &str, handoff: bool) -> String {
    let instructions = if handoff {
        "\n\nYour parent has handed off this assignment and may be idle or chatting with the human. \
         When blocked, needing a decision, or ready for review, use dirigent_agents with \
         title: \"Report to parent\", mode: \"handoff\", and code: \
         `return await agents.pingParent(\"Your concise question or review summary\");`. \
         Make that your only tool call in the batch; it ends your turn without waiting. \
         The message is delivered once you settle, and the parent can continue this same session \
         with feedback. Read the API with {} first. Pinging is authorized communication, not \
         permission to delegate further. Include changed files, checks, and remaining concerns \
         when reporting results. Normal completion or failure also notifies the parent automatically."
    } else {
        ""
    };
    format!("[Dirigent assignment; manager #{parent}; run {run}]{instructions}\n\n{prompt}")
}

fn new_run(id: &str, job: &str) -> AgentRun {
    AgentRun {
        id: id.into(),
        job_id: job.into(),
        status: WorkStatus::Running,
        result: String::new(),
        error: None,
        stop_reason: None,
        handed_off: false,
        parent_message: None,
    }
}

fn string_arg(args: &Value, name: &str) -> Result<String, String> {
    args[name]
        .as_str()
        .filter(|s| !s.trim().is_empty() && s.len() <= 64_000)
        .map(str::to_string)
        .ok_or_else(|| format!("Expected nonempty {name} (up to 64 KB)."))
}

fn id_arg(args: &Value, name: &str) -> Result<Id, String> {
    string_arg(args, name)?
        .parse()
        .map_err(|_| format!("Invalid {name}."))
}

fn validate_spawn(args: &SpawnAgent) -> Result<(), String> {
    if args.name.trim().is_empty()
        || args.name.len() > 160
        || args.prompt.trim().is_empty()
        || args.prompt.len() > 64_000
    {
        return Err(
            "Provide a name (1–160 bytes) and a self-contained prompt (1–64000 bytes).".into(),
        );
    }
    if !args
        .model
        .split_once('/')
        .is_some_and(|(p, m)| !p.is_empty() && !m.is_empty())
    {
        return Err("Use an exact provider/model ID from agents.models().".into());
    }
    if !matches!(
        args.thinking.as_str(),
        "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
    ) {
        return Err("Invalid thinking level.".into());
    }
    if !matches!(args.workspace.as_str(), "current" | "new") {
        return Err("Workspace must be current or new.".into());
    }
    Ok(())
}
