//! Durable delegation metadata. Sessions survive scripts; assignments have immutable run IDs.

use serde::{Deserialize, Serialize};

use crate::model::Id;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkStatus {
    #[default]
    Running,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentRun {
    pub(crate) id: String,
    pub(crate) job_id: String,
    pub(crate) status: WorkStatus,
    pub(crate) result: String,
    pub(crate) error: Option<String>,
    #[serde(default)]
    pub(crate) stop_reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentJob {
    pub(crate) id: String,
    pub(crate) tool_call_id: String,
    pub(crate) title: String,
    pub(crate) status: WorkStatus,
    pub(crate) result: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct Delegation {
    pub(crate) parent: Option<Id>,
    pub(crate) runs: Vec<AgentRun>,
    pub(crate) jobs: Vec<AgentJob>,
}

impl Delegation {
    pub(crate) fn active_run_mut(&mut self) -> Option<&mut AgentRun> {
        self.runs
            .last_mut()
            .filter(|run| run.status == WorkStatus::Running)
    }

    /// Never replay side effects after a restart. Users/managers can inspect and continue sessions.
    pub(crate) fn interrupt(&mut self) {
        for run in &mut self.runs {
            if run.status == WorkStatus::Running {
                run.status = WorkStatus::Interrupted;
            }
        }
        for job in &mut self.jobs {
            if job.status == WorkStatus::Running {
                job.status = WorkStatus::Interrupted;
            }
        }
    }
}

pub(crate) fn bounded_text(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[Truncated; inspect the child session for full output.]",
        &text[..end]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_interrupts_only_active_work() {
        let mut delegation = Delegation::default();
        for status in [WorkStatus::Completed, WorkStatus::Running] {
            delegation.runs.push(AgentRun {
                id: format!("{status:?}"),
                job_id: "job".into(),
                status,
                result: "saved result".into(),
                error: None,
                stop_reason: None,
            });
        }
        let mut restored: Delegation =
            serde_json::from_str(&serde_json::to_string(&delegation).unwrap()).unwrap();
        restored.interrupt();
        assert_eq!(restored.runs[0].status, WorkStatus::Completed);
        assert_eq!(restored.runs[1].status, WorkStatus::Interrupted);
        assert_eq!(restored.runs[0].result, "saved result");
        assert!(restored.active_run_mut().is_none());
    }

    #[test]
    fn truncation_preserves_utf8() {
        assert!(bounded_text("a🦀b", 3).starts_with("a\n[Truncated"));
    }
}
