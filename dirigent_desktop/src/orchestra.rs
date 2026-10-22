//! The orchestration panel: split off the pane's right like the work log (same hairline,
//! handle and width; one of the two at a time), it shows what the open thread set in
//! motion. **Agents**: its family tree (`lineage`: the topmost ancestor down through every
//! sub-agent and the workflow runs in between; the working ones bright, the rest dim; a
//! click opens a thread). **Workflows**: every run the thread started (or was spawned by)
//! as the list of steps it was planned to take (`WorkflowRun::plan`, from running the
//! script against placeholders), each overwritten by the real step as it is reported, so
//! the whole path is visible from the start and fills in as the run goes. A real step of
//! another kind than planned marks the point where the run left its plan. Open by default
//! while there is something to show; the panel switcher (`thread::panel_switch`) toggles it.

use crate::app::Dirigent;
use crate::icons::icon;
use crate::theme::{self, Status};
use crate::thread::{duration, elapsed, side_panel};
use dirigent_protocol::{RunStep, ThreadSummary, WorkflowRun};
use gpui::{AnyElement, Context, Div, FontWeight, SharedString, Stateful, div, prelude::*, px, rgba};
use serde_json::Value;
use std::collections::HashSet;

const ROW: f32 = 30.;
const STEP_ROW: f32 = 24.;
/// Indent per tree level.
const INDENT: f32 = 16.;

/// One row of the agent tree: a thread or a workflow run, `depth` levels in.
pub enum LineageNode<'a> {
    Thread { thread: &'a ThreadSummary, depth: usize },
    Run { run: &'a WorkflowRun, depth: usize },
}

/// The family of thread `id` among a host's `threads` and `runs`: from its topmost ancestor
/// (through parents and the runs that spawned it) down, depth-first. A node's children (a
/// thread's sub-agents and the runs it started together, or a run's threads) come in the
/// order they were last given a task: a run when it started, a thread at its last new turn
/// (`prompted_at`). So a new or re-used child lands last and nothing else moves. Empty for
/// a thread with no relations.
pub fn lineage<'a>(threads: &'a [ThreadSummary], runs: &'a [WorkflowRun], id: &str) -> Vec<LineageNode<'a>> {
    let find = |id: &str| threads.iter().find(|t| t.id == id);
    let run_of = |t: &ThreadSummary| runs.iter().find(|r| Some(r.id.as_str()) == t.run_id.as_deref());
    // Up to the root: a thread's parent, or the parent of the run that spawned it.
    let Some(mut root) = find(id) else { return vec![] };
    let mut seen = HashSet::new();
    while seen.insert(&root.id) {
        match root.parent_id.as_deref().or_else(|| run_of(root).and_then(|r| r.parent_id.as_deref())).and_then(find) {
            Some(up) => root = up,
            None => break,
        }
    }
    let mut out = vec![];
    // A root spawned by a run nobody started from a thread: the run is the root.
    let root = match run_of(root) {
        Some(run) => LineageNode::Run { run, depth: 0 },
        None => LineageNode::Thread { thread: root, depth: 0 },
    };
    descend(threads, runs, root, &mut out);
    if out.len() == 1 { vec![] } else { out }
}

fn descend<'a>(threads: &'a [ThreadSummary], runs: &'a [WorkflowRun], node: LineageNode<'a>, out: &mut Vec<LineageNode<'a>>) {
    // Sort key: (last given a task, created, id), so ties come out the same every time.
    let thread = |t: &'a ThreadSummary, depth| {
        ((t.prompted_at.unwrap_or(t.created_at), t.created_at, t.id.as_str()), LineageNode::Thread { thread: t, depth })
    };
    let mut kids: Vec<_> = match &node {
        LineageNode::Thread { thread: t, depth } => threads
            .iter()
            .filter(|k| k.parent_id.as_deref() == Some(&t.id))
            .map(|k| thread(k, depth + 1))
            .chain(
                runs.iter()
                    .filter(|r| r.parent_id.as_deref() == Some(&t.id))
                    .map(|r| ((r.started_at, r.started_at, r.id.as_str()), LineageNode::Run { run: r, depth: depth + 1 })),
            )
            .collect(),
        LineageNode::Run { run, depth } => {
            threads.iter().filter(|k| k.run_id.as_deref() == Some(&run.id)).map(|k| thread(k, depth + 1)).collect()
        }
    };
    kids.sort_by_key(|(key, _)| *key);
    out.push(node);
    for (_, kid) in kids {
        descend(threads, runs, kid, out);
    }
}

pub fn panel(this: &Dirigent, cx: &mut Context<Dirigent>) -> Option<Div> {
    if !this.orchestra_open() {
        return None;
    }
    let open = this.open()?;
    let (host, id) = (open.host.clone(), open.id.clone());
    let now = now_ms();
    let tree = agents(this, &host, &id, now, cx);
    let runs = workflows(this, &host, &id, now, cx);
    let body = if tree.is_none() && runs.is_none() {
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .px_6()
            .text_size(px(12.))
            .text_color(rgba(theme::t().fg_2))
            .child("Sub-agents this thread spawns and workflows it runs show up here.")
            .into_any_element()
    } else {
        div().id("orchestra").flex_1().overflow_y_scroll().flex().flex_col().px_2().pb_4().children(tree).children(runs).into_any_element()
    };
    Some(side_panel("Agents & workflows", body, this, cx))
}

/// The family tree, when the thread has any relations.
fn agents(this: &Dirigent, host: &str, id: &str, now: i64, cx: &mut Context<Dirigent>) -> Option<Div> {
    let nodes = this.lineage(host, id);
    if nodes.is_empty() {
        return None;
    }
    let rows: Vec<AnyElement> = nodes
        .iter()
        .map(|n| match n {
            LineageNode::Thread { thread, depth } => {
                let (h, tid) = (host.to_owned(), thread.id.clone());
                let status = this.status(host, thread);
                let selected = thread.id == id;
                let active = status.busy() || status == Status::Asking;
                let color = if active || selected { this.title_color(host, thread) } else { theme::t().fg_1 };
                let model = thread.model.as_deref().map(theme::short_model).unwrap_or("").to_owned();
                let time = match (thread.running_since, thread.last_run_ms) {
                    (Some(since), _) => elapsed(now - since),
                    (None, Some(ms)) => duration(ms),
                    (None, None) => String::new(),
                };
                tree_row(format!("agent/{h}/{tid}"), *depth, selected, cx, move |this, cx| this.select_thread(&h, &tid, cx))
                    .child(status.glyph())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(rgba(color))
                            .when(selected, |d| d.font_weight(FontWeight::MEDIUM))
                            .child(thread.name.clone().unwrap_or_else(|| "Untitled".into())),
                    )
                    .child(small(model).max_w(px(120.)).truncate())
                    .child(small(time).w(px(40.)).text_right())
                    .into_any_element()
            }
            LineageNode::Run { run, depth } => {
                let rid = run.id.clone();
                tree_row(format!("agent-run/{host}/{rid}"), *depth, false, cx, move |this, cx| this.toggle_run(&rid, cx))
                    .child(run_glyph(run))
                    .child(div().flex_1().min_w_0().truncate().text_color(rgba(theme::t().fg_1)).child(run.name.clone()))
                    .child(small("workflow"))
                    .child(small(run_state(run, now)).w(px(40.)).text_right())
                    .into_any_element()
            }
        })
        .collect();
    Some(div().flex().flex_col().gap(px(2.)).pt_2().child(label("Agents")).children(rows))
}

/// The thread's runs, newest first.
fn workflows(this: &Dirigent, host: &str, id: &str, now: i64, cx: &mut Context<Dirigent>) -> Option<Div> {
    let runs = this.thread_runs(host, id);
    if runs.is_empty() {
        return None;
    }
    let mut body = div().flex().flex_col().gap(px(2.)).pt_3().child(label("Workflows"));
    for (i, run) in runs.iter().enumerate() {
        let expanded = this.run_expanded(run, i == 0);
        body = body.child(run_header(host, run, expanded, now, cx));
        if expanded {
            body = body.children(run_input(run, this)).children(steps(host, run, this.steps_of(host, &run.id), this, cx));
        }
    }
    Some(body)
}

/// A run's row: glyph, name, its state on the right (swapped for a stop button on hover
/// while it runs), a chevron. Click folds / unfolds its steps.
fn run_header(host: &str, run: &WorkflowRun, expanded: bool, now: i64, cx: &mut Context<Dirigent>) -> Stateful<Div> {
    let t = theme::t();
    let (h, rid) = (host.to_owned(), run.id.clone());
    let rid2 = rid.clone();
    let abort = run.running().then(|| {
        div()
            .id(SharedString::from(format!("abort-run/{}", run.id)))
            .absolute()
            .top(px(4.))
            .right(px(24.))
            .size(px(22.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_sm()
            .opacity(0.)
            .group_hover("row", |d| d.opacity(1.))
            .hover(|d| d.bg(rgba(t.line)))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.abort_run(&h, &rid2, cx)
            }))
            .child(icon("square", t.error).size(px(12.)))
    });
    tree_row(format!("run/{host}/{}", run.id), 0, false, cx, move |this, cx| this.toggle_run(&rid, cx))
        .relative()
        .child(run_glyph(run))
        .child(div().flex_1().min_w_0().truncate().font_weight(FontWeight::MEDIUM).child(run.name.clone()))
        .child(small(run_state(run, now)).when(run.running(), |d| d.group_hover("row", |d| d.opacity(0.))))
        .children(abort)
        .child(icon(if expanded { "chevron-down" } else { "chevron-right" }, t.fg_2).size(px(12.)))
}

/// `word = saudade` under the header, when the run took input.
fn run_input(run: &WorkflowRun, this: &Dirigent) -> Option<Div> {
    let pairs: Vec<String> = run
        .input
        .as_object()?
        .iter()
        .map(|(k, v)| match v {
            Value::String(s) => format!("{k} = {s}"),
            other => format!("{k} = {other}"),
        })
        .collect();
    (!pairs.is_empty()).then(|| {
        div()
            .pl(px(8. + INDENT + 18.))
            .pr_2()
            .pb_1()
            .truncate()
            .font_family(this.fonts.mono.clone())
            .text_size(px(11.))
            .text_color(rgba(theme::t().fg_2))
            .child(pairs.join("   "))
    })
}

/// The run's steps: the real ones so far, then the planned rest. The step after the last
/// real one is the one in progress while the run lives; a real step that isn't what was
/// planned there marks a divergence, and the plan's remainder is shown dim from then on.
fn steps(host: &str, run: &WorkflowRun, actual: &[RunStep], this: &Dirigent, cx: &mut Context<Dirigent>) -> Vec<AnyElement> {
    let t = theme::t();
    let kind_of = |v: &Value| v.get("kind").and_then(Value::as_str).unwrap_or("step").to_owned();
    let n = run.plan.len().max(actual.len());
    let mut diverged = false;
    let mut rows = Vec::with_capacity(n);
    for i in 0..n {
        let (json, state, at) = match (actual.get(i), run.plan.get(i)) {
            (Some(a), p) => {
                let mismatch = p.is_some_and(|p| kind_of(p) != kind_of(&a.json));
                diverged |= mismatch;
                let state = if kind_of(&a.json) == "error" {
                    StepState::Failed
                } else if mismatch {
                    StepState::Diverged
                } else {
                    StepState::Done
                };
                (&a.json, state, Some(a.at))
            }
            (None, Some(p)) => {
                let state = if run.running() && i == actual.len() && !diverged { StepState::Running } else { StepState::Pending };
                (p, state, None)
            }
            (None, None) => unreachable!(),
        };
        let kind = kind_of(json);
        // A real thread the step touched: the row opens it and shows its name.
        let thread = json
            .get("thread_id")
            .and_then(Value::as_str)
            .or_else(|| json.get("thread_ids").and_then(Value::as_array).and_then(|a| a.first()).and_then(Value::as_str))
            .and_then(|tid| this.summary(host, tid));
        let text = match (thread, kind.as_str()) {
            (Some(th), "spawn" | "wait" | "done" | "read" | "send" | "archive" | "snooze" | "abort" | "open") => {
                th.name.clone().unwrap_or_else(|| "Untitled".into())
            }
            (_, "spawn") => json.get("name").or_else(|| json.get("task")).and_then(Value::as_str).unwrap_or("").to_owned(),
            (_, "send") => json.get("text").and_then(Value::as_str).unwrap_or("").to_owned(),
            (_, "log") => json.get("text").and_then(Value::as_str).unwrap_or("").to_owned(),
            (_, "error") => json.get("message").and_then(Value::as_str).unwrap_or("").to_owned(),
            (_, "workflow") => json.get("name").and_then(Value::as_str).unwrap_or("").to_owned(),
            (_, "search") => json.get("query").and_then(Value::as_str).unwrap_or("").to_owned(),
            (_, "finish") => json.get("result").filter(|r| !r.is_null()).map(|r| short(&r.to_string())).unwrap_or_default(),
            (_, "wait" | "done") => {
                let ids = json.get("thread_ids").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
                if ids > 1 { format!("{ids} agents") } else { String::new() }
            }
            _ => String::new(),
        };
        let (glyph, color) = match state {
            StepState::Done => (icon("circle-check", t.done), t.fg_1),
            StepState::Failed => (icon("circle-x", t.error), t.error),
            StepState::Diverged => (icon("circle-check", t.warn), t.fg_1),
            StepState::Running => (icon("circle", t.working), t.fg),
            StepState::Pending => (icon("circle", t.fg_2), t.fg_2),
        };
        let when = at.map(|at| format!("+{}", duration(at - run.started_at))).unwrap_or_default();
        let link = thread.map(|th| (host.to_owned(), th.id.clone()));
        let row = div()
            .id(SharedString::from(format!("step/{}/{i}", run.id)))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .h(px(STEP_ROW))
            .pl(px(8. + INDENT))
            .pr_2()
            .rounded_md()
            .text_size(px(12.))
            .text_color(rgba(color))
            .when(link.is_some(), |d| d.cursor_pointer().hover(|d| d.bg(rgba(t.hover))))
            .when_some(link, |d, (h, tid)| d.on_click(cx.listener(move |this, _, _, cx| this.select_thread(&h, &tid, cx))))
            .child(glyph.size(px(13.)))
            .child(div().w(px(52.)).flex_shrink_0().when(state == StepState::Running, |d| d.font_weight(FontWeight::MEDIUM)).child(kind))
            .child(div().flex_1().min_w_0().truncate().text_color(rgba(t.fg_2)).child(text))
            .when(state == StepState::Diverged, |d| d.child(small("off plan").text_color(rgba(t.warn))))
            .child(small(when).font_family(this.fonts.mono.clone()).w(px(44.)).text_right());
        rows.push(row.into_any_element());
    }
    rows
}

#[derive(Clone, Copy, PartialEq)]
enum StepState {
    Done,
    Failed,
    /// Reported, but not what the plan had here.
    Diverged,
    Running,
    Pending,
}

fn tree_row(
    id: String,
    depth: usize,
    selected: bool,
    cx: &mut Context<Dirigent>,
    on_click: impl Fn(&mut Dirigent, &mut Context<Dirigent>) + 'static,
) -> Stateful<Div> {
    div()
        .id(SharedString::from(id))
        .group("row")
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .h(px(ROW))
        .pl(px(8. + depth as f32 * INDENT))
        .pr_2()
        .rounded_md()
        .text_size(px(12.5))
        .cursor_pointer()
        .when(selected, |d| d.bg(rgba(theme::t().hover)))
        .hover(|d| d.bg(rgba(theme::t().hover)))
        .on_click(cx.listener(move |this, _, _, cx| on_click(this, cx)))
}

fn label(text: &'static str) -> Div {
    div().h(px(26.)).pl_2().flex().items_center().text_size(px(12.)).text_color(rgba(theme::t().fg_2)).child(text)
}

fn small(text: impl Into<SharedString>) -> Div {
    div().flex_shrink_0().text_size(px(11.)).text_color(rgba(theme::t().fg_2)).child(text.into())
}

/// The glyph for a run: the working dot, a check, or a red cross.
fn run_glyph(run: &WorkflowRun) -> AnyElement {
    let t = theme::t();
    if run.running() {
        Status::Working.glyph()
    } else if run.exit_code == Some(0) {
        icon("circle-check", t.done).into_any_element()
    } else {
        icon("circle-x", t.error).into_any_element()
    }
}

/// `12s` running, `12s` done, `failed · 12s`.
fn run_state(run: &WorkflowRun, now: i64) -> String {
    match (run.ended_at, run.exit_code) {
        (None, _) => elapsed(now - run.started_at),
        (Some(end), Some(0)) => duration(end - run.started_at),
        (Some(end), _) => format!("failed · {}", duration(end - run.started_at)),
    }
}

fn short(s: &str) -> String {
    if s.chars().count() <= 160 { s.to_owned() } else { format!("{}…", s.chars().take(160).collect::<String>()) }
}

fn now_ms() -> i64 {
    std::time::UNIX_EPOCH.elapsed().map(|d| d.as_millis() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(id: &str, parent: Option<&str>, run: Option<&str>, created_at: i64, prompted_at: i64) -> ThreadSummary {
        ThreadSummary {
            id: id.into(),
            project_id: None,
            cwd: "/x".into(),
            name: None,
            model: None,
            thinking_level: None,
            created_at,
            updated_at: created_at,
            entry_count: 0,
            running_since: None,
            last_run_ms: None,
            archived: false,
            asks: vec![],
            workspace: None,
            workspace_missing: false,
            parent_id: parent.map(str::to_owned),
            run_id: run.map(str::to_owned),
            snoozed: false,
            prompted_at: Some(prompted_at),
        }
    }

    /// Children come in the order they were last given a task, threads and runs alike;
    /// re-used ones (a later `prompted_at`) move last, a run's threads stay under it.
    #[test]
    fn lineage_follows_the_last_task() {
        let threads = [
            thread("p", None, None, 0, 0),
            thread("c1", Some("p"), None, 1, 7),
            thread("c2", Some("p"), None, 2, 2),
            thread("r1", None, Some("r"), 4, 8),
            thread("r2", None, Some("r"), 5, 5),
            thread("c3", Some("p"), None, 6, 6),
        ];
        let runs = [WorkflowRun {
            id: "r".into(),
            project_id: None,
            name: "slow".into(),
            input: Value::Null,
            parent_id: Some("p".into()),
            started_at: 3,
            ended_at: None,
            exit_code: None,
            steps: 0,
            plan: vec![],
        }];
        let rows = |id| {
            lineage(&threads, &runs, id)
                .iter()
                .map(|n| match n {
                    LineageNode::Thread { thread, depth } => format!("{}{}", " ".repeat(*depth), thread.id),
                    LineageNode::Run { run, depth } => format!("{}{}", " ".repeat(*depth), run.id),
                })
                .collect::<Vec<_>>()
        };
        let tree = ["p", " c2", " r", "  r2", "  r1", " c3", " c1"];
        assert_eq!(rows("p"), tree);
        // The same tree from anywhere in it, up through the run.
        assert_eq!(rows("r2"), tree);
    }
}
