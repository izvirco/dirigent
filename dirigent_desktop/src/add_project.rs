//! The Add project modal, behind the Projects page's top-bar button. Two pages:
//!
//! - Browse: `<host ▾> <path>`, the path starting at `~/`, over the subdirectories of the
//!   path's directory part (everything up to its last `/`, listed by the host:
//!   `ListDirs`) whose names start with the part after it. Up / Down move the selection,
//!   Enter (or a click) goes into the selected directory, ctrl+Enter or `Add` adds the
//!   path as typed.
//! - Link, only when the added project joined no other one (no shared git origin) and a
//!   project of the same directory name (not renamed) lives on another host: link them, or don't.
//!
//! The state is here; the methods that send requests and change pages are on `Dirigent`.

use crate::app::{Dirigent, Menu};
use crate::icons::icon;
use crate::menu::{self, Side};
use crate::net::HostStatus;
use crate::projects::Group;
use crate::theme;
use dirigent_protocol::Dir;
use gpui::{
    AnyElement, Context, Div, FontWeight, MouseButton, ScrollStrategy, SharedString, UniformListScrollHandle, div, prelude::*, px, rgba,
    uniform_list,
};
use std::ops::Range;

const WIDTH: f32 = 560.;
const ROW_H: f32 = 28.;
/// The directory list's fixed height (8 rows), so the card doesn't jump as it filters.
const LIST_H: f32 = ROW_H * 8. + 8.; // + its padding

#[derive(Default)]
pub struct State {
    /// The listing under the field: of `dir` (a directory part, ending in `/`) on `host`.
    pub host: String,
    pub dir: String,
    /// `None` while the host hasn't answered.
    pub dirs: Option<Result<Vec<Dir>, String>>,
    pub list_request: Option<u64>,
    /// The `AddProject` in flight, and its failure.
    pub add_request: Option<u64>,
    pub error: Option<String>,
    /// Index into `visible`.
    pub selected: usize,
    pub scroll: UniformListScrollHandle,
    /// The link page, once the project is added and has a same-named group to join.
    pub link: Option<Offer>,
}

/// The project just added and the groups it could be linked into.
pub struct Offer {
    /// Its group key as added (it's alone in it).
    pub key: String,
    pub name: String,
    /// (group key, its members as `host path`).
    pub candidates: Vec<(String, String)>,
}

/// `~/work/di` → (`~/work/`, `di`); no `/` at all → (``, the text).
pub fn split(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(i) => path.split_at(i + 1),
        None => ("", path),
    }
}

/// The directories whose name starts with `prefix` (ignoring case); dot-directories only
/// once the prefix starts with a dot.
pub fn matches<'a>(dirs: &'a [Dir], prefix: &str) -> Vec<&'a Dir> {
    let prefix = prefix.to_lowercase();
    dirs.iter().filter(|d| (prefix.starts_with('.') || !d.name.starts_with('.')) && d.name.to_lowercase().starts_with(&prefix)).collect()
}

/// Where a project just added on `host` could be linked: nowhere if it already shares a
/// group (a git origin match), else the unrenamed groups of the same directory name with
/// no member on `host`.
pub fn offer(groups: &[Group], host: &str, project_id: &str) -> Option<Offer> {
    let own = groups.iter().find(|g| g.has(host, project_id))?;
    if own.members.len() > 1 {
        return None;
    }
    let others: Vec<(String, String)> = groups
        .iter()
        // By directory name: a rename is how the user says two same-named dirs are unrelated.
        .filter(|g| g.key != own.key && g.dir_name == own.dir_name && g.name == g.dir_name && g.members.iter().all(|m| m.host != host))
        .map(|g| {
            // Paths too: same-named candidates on one host would look alike otherwise.
            let members: Vec<String> = g.members.iter().map(|m| format!("{} {}", m.host, m.project.path)).collect();
            (g.key.clone(), members.join(", "))
        })
        .collect();
    (!others.is_empty()).then(|| Offer { key: own.key.clone(), name: own.name.clone(), candidates: others })
}

impl State {
    /// The listing filtered by the field's text, if it is the listing of the field's directory.
    pub fn visible(&self, text: &str) -> Vec<&Dir> {
        let (dir, prefix) = split(text);
        match &self.dirs {
            Some(Ok(dirs)) if dir == self.dir => matches(dirs, prefix),
            _ => vec![],
        }
    }
}

/// The modal over a dimmed window; a press on the dim closes it (the card occludes it,
/// so presses inside the card never reach it).
pub fn render(this: &Dirigent, state: &State, cx: &mut Context<Dirigent>) -> AnyElement {
    let card = div()
        .id("add-project")
        .occlude()
        .w(px(WIDTH))
        .flex()
        .flex_col()
        .rounded_lg()
        .bg(rgba(theme::t().popup))
        .border_1()
        .border_color(rgba(theme::t().line))
        .shadow_lg()
        .child(match &state.link {
            Some(offer) => link_page(offer, cx),
            None => browse_page(this, state, cx),
        });
    div()
        .id("add-project-dim")
        .occlude()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(rgba(0x00000080))
        .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| this.dim_pressed(cx)))
        .child(card)
        .into_any()
}

/// `<host ▾> <path> [Add]`, a hairline, then the directory list (or a note in its place:
/// loading, an error, nothing matches).
fn browse_page(this: &Dirigent, state: &State, cx: &mut Context<Dirigent>) -> Div {
    let host = this.new_project_host().unwrap_or_default();
    let host_open = this.menu == Some(Menu::NewProjectHost);
    let items = this
        .hosts
        .iter()
        .map(|h| {
            let name = h.name.clone();
            let status = format!("{:?}", h.status).to_lowercase();
            menu::item(
                format!("nph/{name}"),
                name.clone(),
                status,
                name == host,
                move |this, cx| this.pick_new_project_host(name.clone(), cx),
                cx,
            )
        })
        .collect();
    let host_chip = theme::chip("new-project-host", host.clone(), false, host_open)
        .flex_shrink_0()
        .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(Menu::NewProjectHost, cx)))
        .children(host_open.then(|| menu::popup(Side::Below(24.), items, cx)));
    let add = theme::button("add-project-confirm", "plus", "Add").on_click(cx.listener(|this, _, _, cx| this.confirm_add_project(cx)));
    let top = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .px_2()
        .py_2()
        .child(host_chip)
        .child(div().flex_1().min_w_0().child(this.path_input.clone()))
        .child(add);

    let text = this.path_input.read(cx).text().to_owned();
    let (dir, _) = split(&text);
    let visible: Vec<(SharedString, bool)> = state.visible(&text).into_iter().map(|d| (d.name.clone().into(), d.repo)).collect();
    let connected = this.hosts.iter().any(|h| h.name == host && h.status == HostStatus::Connected);
    // (text, is an error)
    let note = match &state.dirs {
        _ if state.error.is_some() => state.error.clone().map(|e| (e, true)),
        _ if !connected => Some((format!("{host} is not connected"), true)),
        _ if dir.is_empty() => Some(("Start with / or ~/".to_owned(), false)),
        Some(Err(e)) if dir == state.dir => Some((e.clone(), true)),
        Some(Ok(_)) if dir == state.dir && visible.is_empty() => Some(("No matching directories".to_owned(), false)),
        Some(Ok(_)) if dir == state.dir => None,
        _ => Some(("Loading…".to_owned(), false)),
    };
    let list = div().h(px(LIST_H)).flex().flex_col().p_1().child(match note {
        Some((note, error)) => div()
            .p_2()
            .text_size(px(12.))
            .text_color(rgba(if error { theme::t().error } else { theme::t().fg_2 }))
            .child(note)
            .into_any_element(),
        None => dir_list(visible, state.selected, dir.to_owned(), &state.scroll, cx).into_any_element(),
    });
    div().flex().flex_col().child(top).child(div().h(px(1.)).bg(rgba(theme::t().line))).child(list)
}

/// The (filtered) subdirectories; a click goes into one. Repos get a branch icon.
fn dir_list(
    rows: Vec<(SharedString, bool)>,
    selected: usize,
    dir: String,
    scroll: &UniformListScrollHandle,
    cx: &mut Context<Dirigent>,
) -> impl IntoElement {
    uniform_list(
        "dirs",
        rows.len(),
        cx.processor(move |_, range: Range<usize>, _, cx| {
            range
                .map(|i| {
                    let (name, repo) = rows[i].clone();
                    let path = format!("{dir}{name}/");
                    div()
                        .id(i)
                        .w_full()
                        .h(px(ROW_H))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .rounded_md()
                        .cursor_pointer()
                        .when(i == selected, |d| d.bg(rgba(theme::t().accent_active)))
                        .hover(|d| d.bg(rgba(theme::t().line)))
                        .on_click(cx.listener(move |this, _, _, cx| this.set_project_path(path.clone(), cx)))
                        .child(
                            icon(if repo { "git-branch" } else { "folder" }, if repo { theme::t().accent_ink } else { theme::t().fg_2 })
                                .size(px(14.)),
                        )
                        .child(div().min_w_0().truncate().child(name))
                        .into_any_element()
                })
                .collect::<Vec<_>>()
        }),
    )
    .track_scroll(scroll)
    .flex_1()
}

/// Scroll the selected row into view after the selection moved.
pub fn reveal(state: &State) {
    state.scroll.scroll_to_item(state.selected, ScrollStrategy::Nearest);
}

/// `<name> is also a project on another host`: a row per candidate with `Link`, and
/// `Don't link`.
fn link_page(offer: &Offer, cx: &mut Context<Dirigent>) -> Div {
    let text = div().text_color(rgba(theme::t().fg_1)).child(format!(
        "{} is also a project on another host. Link them to show them as one project, with the threads of both.",
        offer.name
    ));
    let rows = offer.candidates.iter().map(|(key, members)| {
        let target = key.clone();
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .h(px(36.))
            .px_2()
            .rounded_md()
            .bg(rgba(theme::t().raised))
            .child(div().font_weight(FontWeight::MEDIUM).child(offer.name.clone()))
            .child(div().flex_1().min_w_0().truncate().text_size(px(12.)).text_color(rgba(theme::t().fg_2)).child(members.clone()))
            .child(
                theme::button(SharedString::from(format!("link/{key}")), "git-fork", "Link")
                    .on_click(cx.listener(move |this, _, _, cx| this.finish_add_project(Some(target.clone()), cx))),
            )
    });
    let skip = theme::button("dont-link", "x", "Don't link").on_click(cx.listener(|this, _, _, cx| this.finish_add_project(None, cx)));
    div()
        .flex()
        .flex_col()
        .gap_3()
        .p_4()
        .child(text)
        .child(div().flex().flex_col().gap_1().children(rows))
        .child(div().flex().flex_row().justify_end().child(skip))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projects::Member;
    use dirigent_protocol::Project;

    fn dir(name: &str) -> Dir {
        Dir { name: name.into(), repo: false }
    }

    #[test]
    fn splits_at_the_last_slash() {
        assert_eq!(split("~/work/di"), ("~/work/", "di"));
        assert_eq!(split("~/"), ("~/", ""));
        assert_eq!(split("/"), ("/", ""));
        assert_eq!(split("~"), ("", "~"));
    }

    #[test]
    fn filters_by_prefix_hiding_dot_dirs() {
        let dirs = [dir(".config"), dir("Work"), dir("web"), dir("x")];
        let names = |p: &str| matches(&dirs, p).iter().map(|d| d.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(""), ["Work", "web", "x"]);
        assert_eq!(names("w"), ["Work", "web"]);
        assert_eq!(names("."), [".config"]);
    }

    fn group(key: &str, name: &str, members: &[(&str, &str)]) -> Group {
        let members = members
            .iter()
            .map(|(h, id)| Member {
                host: (*h).into(),
                project: Project { id: (*id).into(), path: format!("/p/{name}"), origin: None, created_at: 0 },
            })
            .collect();
        Group { key: key.into(), name: name.into(), dir_name: name.into(), members, threads: vec![], manual: false }
    }

    #[test]
    fn offers_same_named_groups_on_other_hosts() {
        let groups = [
            group("local/new", "forge", &[("local", "new")]),
            group("aurora/a", "forge", &[("aurora", "a")]),
            group("local/b", "forge", &[("local", "b")]),
            group("aurora/c", "other", &[("aurora", "c")]),
            // A `forge` dir the user renamed: unrelated, not offered.
            Group { name: "forge app".into(), ..group("aurora/e", "forge", &[("aurora", "e")]) },
        ];
        let o = offer(&groups, "local", "new").unwrap();
        assert_eq!((o.key.as_str(), o.name.as_str()), ("local/new", "forge"));
        assert_eq!(o.candidates, [("aurora/a".to_owned(), "aurora /p/forge".to_owned())]);
        // Already grouped with another (a git origin match): nothing to offer.
        let joined =
            [group("github.com/x/forge", "forge", &[("aurora", "a"), ("local", "new")]), group("aurora/d", "forge", &[("aurora", "d")])];
        assert!(offer(&joined, "local", "new").is_none());
    }
}
