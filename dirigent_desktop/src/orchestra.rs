use crate::app::{ArchiveAt, Dirigent};
use crate::icons::icon;
use crate::menu::Side;
use crate::theme::{self, Status, text, u};
use crate::thread::{self, duration, elapsed};
use dirigent_protocol::{Spend, ThreadSummary};
use gpui::{AnyElement, Context, Div, FontWeight, SharedString, Stateful, div, prelude::*, rgba};
use std::collections::HashSet;

const ROW: f32 = 30.;
const FOLD_ROW: f32 = 20.;
const INDENT: f32 = 16.;

pub struct LineageNode<'a> {
    pub line: Line<'a>,
    pub depth: usize,
}

pub enum Line<'a> {
    Thread(&'a ThreadSummary),
    Archived { parent: &'a str, count: usize, open: bool },
}

// MUST KILL: type: lineage's empty Vec for a thread with no relations → an Option
pub fn lineage<'a>(threads: &'a [ThreadSummary], id: &str, unfolded: &HashSet<String>) -> Vec<LineageNode<'a>> {
    let find = |id: &str| threads.iter().find(|t| t.id == id);
    let Some(mut root) = find(id) else { return vec![] };
    let mut path = HashSet::new();
    while path.insert(root.id.as_str()) {
        match root.parent_id.as_deref().and_then(find) {
            Some(up) => root = up,
            None => break,
        }
    }
    let mut out = vec![];
    Family { threads, path, unfolded }.descend(root, 0, true, &mut out);
    if out.len() == 1 { vec![] } else { out }
}

struct Family<'a, 'b> {
    threads: &'a [ThreadSummary],
    path: HashSet<&'a str>,
    unfolded: &'b HashSet<String>,
}

impl<'a> Family<'a, '_> {
    fn kids(&self, id: &str) -> Vec<&'a ThreadSummary> {
        self.threads.iter().filter(|k| k.parent_id.as_deref() == Some(id)).collect()
    }

    // MUST KILL: rename: archived → archived_with_all_below (it checks the whole branch)
    fn archived(&self, t: &ThreadSummary) -> bool {
        t.archived && self.kids(&t.id).into_iter().all(|k| self.archived(k))
    }

    // MUST KILL: type: descend's `fold` bool → an enum telling a closed tree from an open fold's archived-only branch
    fn descend(&self, node: &'a ThreadSummary, depth: usize, fold: bool, out: &mut Vec<LineageNode<'a>>) {
        out.push(LineageNode { line: Line::Thread(node), depth });
        let mut kids = self.kids(&node.id);
        kids.sort_by_key(|k| (k.prompted_at.unwrap_or(k.created_at), k.created_at, k.id.as_str()));
        let open = self.unfolded.contains(&node.id);
        let (folded, live): (Vec<_>, Vec<_>) =
            kids.into_iter().partition(|k| fold && self.archived(k) && (open || !self.path.contains(k.id.as_str())));
        for kid in live {
            self.descend(kid, depth + 1, fold, out);
        }
        if !folded.is_empty() {
            out.push(LineageNode { line: Line::Archived { parent: &node.id, count: folded.len(), open }, depth: depth + 1 });
            for kid in folded.into_iter().filter(|_| open) {
                self.descend(kid, depth + 1, false, out);
            }
        }
    }
}

pub fn tree(this: &Dirigent, cx: &mut Context<Dirigent>) -> Option<Div> {
    let open = this.open()?;
    let (host, id) = (open.host.clone(), open.id.clone());
    let (host, id, now) = (host.as_str(), id.as_str(), now_ms());
    let nodes = this.lineage(host, id);
    let Some(Line::Thread(root)) = nodes.first().map(|n| &n.line) else { return None };
    let total = this.host(host).map(|h| spend(&h.threads, root)).unwrap_or_default();
    let rows: Vec<AnyElement> = nodes
        .iter()
        .map(|n| match n.line {
            Line::Thread(t) => thread_row(host, t, n.depth, t.id == id, now, this, cx),
            Line::Archived { parent, count, open } => fold_row(parent, count, open, n.depth, cx),
        })
        .collect();
    Some(div().flex().flex_col().gap(u(2.)).pt_2().children(rows).when(total != Spend::default(), |d| d.child(spend_line(total))))
}

fn thread_row(
    host: &str,
    t: &ThreadSummary,
    depth: usize,
    selected: bool,
    now: i64,
    this: &Dirigent,
    cx: &mut Context<Dirigent>,
) -> AnyElement {
    let (h, tid, archived) = (host.to_owned(), t.id.clone(), t.archived);
    let status = this.status(host, t);
    let active = status.busy() || status == Status::Asking;
    let color = if active || selected { this.title_color(host, t) } else { theme::t().fg_1 };
    let model = t.model.as_deref().map(theme::short_model).unwrap_or("").to_owned();
    let time = match (t.running_since, t.last_run_ms) {
        (Some(since), _) => elapsed(now - since),
        (None, Some(ms)) => duration(ms),
        (None, None) => String::new(),
    };
    let archive_confirm = thread::archive_confirm(host, &t.id, ArchiveAt::Tree, Side::BelowRight(20.), this, cx);
    let asking = archive_confirm.is_some();
    let action = (archived || this.at_rest(host, t)).then(|| {
        let (h, tid) = (h.clone(), tid.clone());
        div()
            .id(SharedString::from(format!("agent-archive/{h}/{tid}")))
            .absolute()
            .top(u((ROW - 20.) / 2.))
            .right(u(-4.))
            .size(u(20.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_sm()
            .opacity(if asking { 1. } else { 0. })
            .group_hover("agent", |d| d.opacity(1.))
            .when(asking, |d| d.bg(rgba(theme::t().line)))
            .hover(|d| d.bg(rgba(theme::t().line)))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.set_archived(&h, &tid, !archived, ArchiveAt::Tree, cx)
            }))
            .child(icon(if archived { "archive-restore" } else { "archive" }, theme::t().fg_1))
            .children(archive_confirm)
    });
    let time = div()
        .relative()
        .h_full()
        .flex_shrink_0()
        .min_w(u(48.))
        .flex()
        .items_center()
        .justify_end()
        .child(
            small(time)
                .whitespace_nowrap()
                .when(asking, |d| d.opacity(0.))
                .when(action.is_some(), |d| d.group_hover("agent", |d| d.opacity(0.))),
        )
        .children(action);
    tree_row(format!("agent/{h}/{tid}"), depth, ROW, selected, cx, move |this, cx| this.select_thread(&h, &tid, cx))
        .group("agent")
        .child(status.glyph())
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(rgba(color))
                .when(selected, |d| d.font_weight(FontWeight::MEDIUM))
                .child(t.name.clone().unwrap_or_else(|| "Untitled".into())),
        )
        .child(small(model).max_w(u(120.)).truncate())
        .child(time)
        .into_any_element()
}

fn fold_row(parent: &str, count: usize, open: bool, depth: usize, cx: &mut Context<Dirigent>) -> AnyElement {
    let p = parent.to_owned();
    let chevron = icon(if open { "chevron-down" } else { "chevron-right" }, theme::t().fg_2).size(u(12.));
    tree_row(format!("archived/{parent}"), depth, FOLD_ROW, false, cx, move |this, cx| this.toggle_unfolded(&p, cx))
        .text_size(text::xs())
        .text_color(rgba(theme::t().fg_2))
        .child(div().w(u(14.)).flex_shrink_0().flex().justify_center().child(chevron))
        .child(format!("{count} archived"))
        .into_any_element()
}

fn spend(threads: &[ThreadSummary], t: &ThreadSummary) -> Spend {
    let mut total = t.spend;
    for kid in threads.iter().filter(|k| k.parent_id.as_deref() == Some(&t.id)) {
        total += spend(threads, kid);
    }
    total
}

fn spend_line(s: Spend) -> Div {
    let cached = if s.input == 0 { 0 } else { (s.cached as f64 / s.input as f64 * 100.).round() as u64 };
    let (input, output, cost) = (theme::kilo(s.input), theme::kilo(s.output), theme::dollars(s.cost));
    let text = format!("{input} in ({cached}% cached) / {output} out · {cost}");
    div().flex().justify_end().pr_2().pt_1().child(small(text))
}

fn tree_row(
    id: String,
    depth: usize,
    height: f32,
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
        .h(u(height))
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
            effort: None,
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
            free_jobs: 0,
            spend: Default::default(),
            forked_from: None,
            setup: None,
            compacting_since: None,
            screen: false,
            locked: false,
        }
    }

    fn rows(threads: &[ThreadSummary], id: &str, unfolded: &[&str]) -> Vec<String> {
        let unfolded = unfolded.iter().map(|s| s.to_string()).collect();
        lineage(threads, id, &unfolded)
            .iter()
            .map(|n| {
                let line = match n.line {
                    Line::Thread(t) => t.id.clone(),
                    Line::Archived { count, open, .. } => format!("{}{count}", if open { '-' } else { '+' }),
                };
                format!("{}{line}", " ".repeat(n.depth))
            })
            .collect()
    }

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
        let tree = ["p", " c2", "  g2", "  g1", " c3", " c1"];
        assert_eq!(rows(&threads, "p", &[]), tree);
        assert_eq!(rows(&threads, "g2", &[]), tree);
    }

    #[test]
    fn archived_sub_agents_fold() {
        let archived = |t: ThreadSummary| ThreadSummary { archived: true, ..t };
        let threads = [
            thread("p", None, 0, 0),
            archived(thread("a1", Some("p"), 1, 1)),
            thread("c1", Some("p"), 2, 2),
            archived(thread("a2", Some("p"), 3, 3)),
            archived(thread("g1", Some("a2"), 4, 4)),
            archived(thread("a3", Some("p"), 5, 5)),
            thread("g2", Some("a3"), 6, 6),
        ];
        assert_eq!(rows(&threads, "p", &[]), ["p", " c1", " a3", "  g2", " +2"]);
        assert_eq!(rows(&threads, "p", &["p"]), ["p", " c1", " a3", "  g2", " -2", " a1", " a2", "  g1"]);
        assert_eq!(rows(&threads, "g1", &[]), ["p", " c1", " a2", "  g1", " a3", "  g2", " +1"]);
        assert_eq!(rows(&threads, "g1", &["p"]), ["p", " c1", " a3", "  g2", " -2", " a1", " a2", "  g1"]);
    }
}
