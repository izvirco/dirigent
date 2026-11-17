use crate::app::{Dirigent, Menu};
use crate::icons::icon;
use crate::menu::{self, Side};
use crate::modal;
use crate::net::HostStatus;
use crate::projects::Group;
use crate::theme::{self, text, u};
use dirigent_protocol::Dir;
use gpui::{
    AnyElement, Context, Div, FontWeight, ScrollStrategy, SharedString, UniformListScrollHandle, div, prelude::*, px, rgba, uniform_list,
};
use std::ops::Range;

const WIDTH: f32 = 560.;
const ROW_H: f32 = 28.;
// MUST KILL: extract: the list's p_1 padding (the + 8.) as a named const
const LIST_H: f32 = ROW_H * 8. + 8.;

#[derive(Default)]
pub struct State {
    // MUST KILL: extract: host, dir, dirs and list_request into a Listing struct
    pub host: String,
    pub dir: String,
    pub dirs: Option<Result<Vec<Dir>, String>>,
    pub list_request: Option<u64>,
    pub add_request: Option<u64>,
    pub error: Option<String>,
    // MUST KILL: rename: selected -> selected_visible, an index into visible()
    pub selected: usize,
    pub scroll: UniformListScrollHandle,
    pub link: Option<Offer>,
}

pub struct Offer {
    pub key: String,
    pub name: String,
    // MUST KILL: type: candidates as a struct with group key and members fields, not a tuple
    pub candidates: Vec<(String, String)>,
}

/// `~/work/di` → (`~/work/`, `di`); no `/` at all → (``, the text). `\` splits too: a
/// Windows host takes either (`C:\work\di` → (`C:\work\`, `di`)).
pub fn split(path: &str) -> (&str, &str) {
    match path.rfind(['/', '\\']) {
        Some(i) => path.split_at(i + 1),
        None => ("", path),
    }
}

pub fn matches<'a>(dirs: &'a [Dir], prefix: &str) -> Vec<&'a Dir> {
    let prefix = prefix.to_lowercase();
    dirs.iter().filter(|d| (prefix.starts_with('.') || !d.name.starts_with('.')) && d.name.to_lowercase().starts_with(&prefix)).collect()
}

pub fn offer(groups: &[Group], host: &str, project_id: &str) -> Option<Offer> {
    let own = groups.iter().find(|g| g.has(host, project_id))?;
    if own.members.len() > 1 {
        return None;
    }
    let others: Vec<(String, String)> = groups
        .iter()
        // MUST KILL: extract: the unrenamed same-dir-name test in offer() as a named predicate
        .filter(|g| g.key != own.key && g.dir_name == own.dir_name && g.name == g.dir_name && g.members.iter().all(|m| m.host != host))
        .map(|g| {
            let members: Vec<String> = g.members.iter().map(|m| format!("{} {}", m.host, m.project.path)).collect();
            (g.key.clone(), members.join(", "))
        })
        .collect();
    (!others.is_empty()).then(|| Offer { key: own.key.clone(), name: own.name.clone(), candidates: others })
}

impl State {
    pub fn visible(&self, text: &str) -> Vec<&Dir> {
        let (dir, prefix) = split(text);
        match &self.dirs {
            Some(Ok(dirs)) if dir == self.dir => matches(dirs, prefix),
            _ => vec![],
        }
    }
}

pub fn render(this: &Dirigent, state: &State, cx: &mut Context<Dirigent>) -> AnyElement {
    modal::frame(div().w(u(WIDTH)).child(page(this, state, cx)), cx)
}

pub fn page(this: &Dirigent, state: &State, cx: &mut Context<Dirigent>) -> Div {
    match &state.link {
        Some(offer) => link_page(offer, cx),
        None => browse_page(this, state, cx),
    }
}

fn browse_page(this: &Dirigent, state: &State, cx: &mut Context<Dirigent>) -> Div {
    let host = this.new_project_host().unwrap_or_default();
    let host_open = this.menu == Some(Menu::NewProjectHost);
    let host_menu = host_open.then(|| {
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
        menu::popup(Side::Below(24.), items, cx)
    });
    let host_chip = theme::chip("new-project-host", host.clone(), false, host_open)
        .flex_shrink_0()
        .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(Menu::NewProjectHost, cx)))
        .children(host_menu);
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
    // MUST KILL: type: note as an enum (Error / Info) instead of a (String, bool)
    let note = match &state.dirs {
        _ if state.error.is_some() => state.error.clone().map(|e| (e, true)),
        _ if !connected => Some((format!("{host} is not connected"), true)),
        _ if dir.is_empty() => Some(("Start with ~/ or an absolute path".to_owned(), false)),
        Some(Err(e)) if dir == state.dir => Some((e.clone(), true)),
        Some(Ok(_)) if dir == state.dir && visible.is_empty() => Some(("No matching directories".to_owned(), false)),
        Some(Ok(_)) if dir == state.dir => None,
        _ => Some(("Loading…".to_owned(), false)),
    };
    let list = div().h(u(LIST_H)).flex().flex_col().p_1().child(match note {
        Some((note, error)) => div()
            .p_2()
            .text_size(text::sm())
            .text_color(rgba(if error { theme::t().error } else { theme::t().fg_2 }))
            .child(note)
            .into_any_element(),
        None => dir_list(visible, state.selected, dir.to_owned(), &state.scroll, cx).into_any_element(),
    });
    div().flex().flex_col().child(top).child(div().h(px(1.)).bg(rgba(theme::t().line))).child(list)
}

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
                        .h(u(ROW_H))
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
                                .size(u(14.)),
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

pub fn reveal(state: &State) {
    state.scroll.scroll_to_item(state.selected, ScrollStrategy::Nearest);
}

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
            .h(u(36.))
            .px_2()
            .rounded_md()
            .bg(rgba(theme::t().raised))
            .child(div().font_weight(FontWeight::MEDIUM).child(offer.name.clone()))
            .child(div().flex_1().min_w_0().truncate().text_size(text::sm()).text_color(rgba(theme::t().fg_2)).child(members.clone()))
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
        assert_eq!(split(r"C:\work\di"), (r"C:\work\", "di"));
        assert_eq!(split("C:/"), ("C:/", ""));
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
                linked: false,
            })
            .collect();
        Group { key: key.into(), name: name.into(), dir_name: name.into(), members, threads: vec![] }
    }

    #[test]
    fn offers_same_named_groups_on_other_hosts() {
        let groups = [
            group("local/new", "forge", &[("local", "new")]),
            group("aurora/a", "forge", &[("aurora", "a")]),
            group("local/b", "forge", &[("local", "b")]),
            group("aurora/c", "other", &[("aurora", "c")]),
            Group { name: "forge app".into(), ..group("aurora/e", "forge", &[("aurora", "e")]) },
        ];
        let o = offer(&groups, "local", "new").unwrap();
        assert_eq!((o.key.as_str(), o.name.as_str()), ("local/new", "forge"));
        assert_eq!(o.candidates, [("aurora/a".to_owned(), "aurora /p/forge".to_owned())]);
        let joined =
            [group("github.com/x/forge", "forge", &[("aurora", "a"), ("local", "new")]), group("aurora/d", "forge", &[("aurora", "d")])];
        assert!(offer(&joined, "local", "new").is_none());
    }
}
