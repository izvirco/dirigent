//! The lineage tree at the top of the right panel (`thread::side_panel`, over the work
//! log): the open thread's family (`lineage`: the topmost ancestor down through every
//! sub-agent; the working ones bright, the rest dim; a click opens a thread). A quiet
//! line under the tree says what the whole family consumed.

use crate::app::Dirigent;
use crate::theme::{self, Status, text, u};
use crate::thread::{duration, elapsed};
use dirigent_protocol::{Spend, ThreadSummary};
use gpui::{AnyElement, Context, Div, FontWeight, SharedString, Stateful, div, prelude::*, rgba};
use std::collections::HashSet;

const ROW: f32 = 30.;
/// Indent per tree level.
const INDENT: f32 = 16.;

/// One row of the agent tree: a thread, `depth` levels in.
pub struct LineageNode<'a> {
    pub thread: &'a ThreadSummary,
    pub depth: usize,
}

/// The family of thread `id` among a host's `threads`: from its topmost ancestor down,
/// depth-first. A thread's sub-agents come in the order they were last given a task (their
/// last new turn, `prompted_at`), so a new or re-used child lands last and nothing else
/// moves. Empty for a thread with no relations.
pub fn lineage<'a>(threads: &'a [ThreadSummary], id: &str) -> Vec<LineageNode<'a>> {
    let find = |id: &str| threads.iter().find(|t| t.id == id);
    let Some(mut root) = find(id) else { return vec![] };
    let mut seen = HashSet::new();
    while seen.insert(&root.id) {
        match root.parent_id.as_deref().and_then(find) {
            Some(up) => root = up,
            None => break,
        }
    }
    let mut out = vec![];
    descend(threads, LineageNode { thread: root, depth: 0 }, &mut out);
    if out.len() == 1 { vec![] } else { out }
}

fn descend<'a>(threads: &'a [ThreadSummary], node: LineageNode<'a>, out: &mut Vec<LineageNode<'a>>) {
    let mut kids: Vec<_> = threads.iter().filter(|k| k.parent_id.as_deref() == Some(&node.thread.id)).collect();
    // (last given a task, created, id), so ties come out the same every time.
    kids.sort_by_key(|k| (k.prompted_at.unwrap_or(k.created_at), k.created_at, k.id.as_str()));
    let depth = node.depth + 1;
    out.push(node);
    for kid in kids {
        descend(threads, LineageNode { thread: kid, depth }, out);
    }
}

/// The open thread's family tree, when it has any relations.
pub fn tree(this: &Dirigent, cx: &mut Context<Dirigent>) -> Option<Div> {
    let open = this.open()?;
    let (host, id) = (open.host.clone(), open.id.clone());
    let (host, id, now) = (host.as_str(), id.as_str(), now_ms());
    let nodes = this.lineage(host, id);
    if nodes.is_empty() {
        return None;
    }
    let mut total = Spend::default();
    let rows: Vec<AnyElement> = nodes
        .iter()
        .map(|&LineageNode { thread, depth }| {
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
            total += thread.spend;
            tree_row(format!("agent/{h}/{tid}"), depth, selected, cx, move |this, cx| this.select_thread(&h, &tid, cx))
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
                .child(small(model).max_w(u(120.)).truncate())
                // Fits `12m 01s` at 11px; longer values widen the column rather than wrap.
                .child(small(time).min_w(u(48.)).whitespace_nowrap().text_right())
                .into_any_element()
        })
        .collect();
    Some(div().flex().flex_col().gap(u(2.)).pt_2().children(rows).when(total != Spend::default(), |d| d.child(spend_line(total))))
}

/// The family's totals, right-aligned under the tree (once anything was spent), worded
/// like the context chip's card.
fn spend_line(s: Spend) -> Div {
    let cached = if s.input == 0 { 0 } else { (s.cached as f64 / s.input as f64 * 100.).round() as u64 };
    let (input, output, cost) = (theme::kilo(s.input), theme::kilo(s.output), theme::dollars(s.cost));
    let text = format!("{input} in ({cached}% cached) / {output} out · {cost}");
    div().flex().justify_end().pr_2().pt_1().child(small(text))
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
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .h(u(ROW))
        .pl(u(8. + depth as f32 * INDENT))
        .pr_2()
        .rounded_md()
        .text_size(text::sm())
        .cursor_pointer()
        .when(selected, |d| d.bg(rgba(theme::t().hover)))
        .hover(|d| d.bg(rgba(theme::t().hover)))
        .on_click(cx.listener(move |this, _, _, cx| on_click(this, cx)))
}

fn small(text: impl Into<SharedString>) -> Div {
    div().flex_shrink_0().text_size(text::xs()).text_color(rgba(theme::t().fg_2)).child(text.into())
}

fn now_ms() -> i64 {
    std::time::UNIX_EPOCH.elapsed().map(|d| d.as_millis() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(id: &str, parent: Option<&str>, created_at: i64, prompted_at: i64) -> ThreadSummary {
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
            snoozed: false,
            pinned: false,
            seen_at: None,
            pinned_entries: vec![],
            prompted_at: Some(prompted_at),
            jobs: 0,
            spend: Default::default(),
        }
    }

    /// Children come in the order they were last given a task; re-used ones (a later
    /// `prompted_at`) move last.
    #[test]
    fn lineage_follows_the_last_task() {
        let threads = [
            thread("p", None, 0, 0),
            thread("c1", Some("p"), 1, 7),
            thread("c2", Some("p"), 2, 2),
            thread("g1", Some("c2"), 4, 8),
            thread("g2", Some("c2"), 5, 5),
            thread("c3", Some("p"), 6, 6),
        ];
        let rows = |id| lineage(&threads, id).iter().map(|n| format!("{}{}", " ".repeat(n.depth), n.thread.id)).collect::<Vec<_>>();
        let tree = ["p", " c2", "  g2", "  g1", " c3", " c1"];
        assert_eq!(rows("p"), tree);
        // The same tree from anywhere in it.
        assert_eq!(rows("g2"), tree);
    }
}
