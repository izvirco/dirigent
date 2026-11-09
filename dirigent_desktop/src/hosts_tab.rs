//! Settings › Hosts: every host (`local` and what `hosts.toml` lists), one line each, and
//! the form that adds or edits one.
//!
//! A row is the host's name, address, auth and state; a failing host says `error` and
//! shows why on hover, so every row keeps one line. Clicking a row (or its pencil) opens
//! the form in its place, filled in; `Add host` asks for the auth first (its menu), then
//! opens the same form empty under the rows. The form has the fields its auth needs and
//! its auth chip changes that; Enter saves, Esc cancels, and what's wrong (a bad address,
//! a name taken) shows on its bottom line. Saving rewrites `hosts.toml`, a rename
//! replacing the old entry and taking what's cached for it along (`Dirigent::rename_host`),
//! and `sync_hosts` reconnects whatever changed.

use crate::app::{Dirigent, Menu, Modal};
use crate::icons::icon;
use crate::input::{self, Submitted, TextInput};
use crate::menu::{self, Side};
use crate::net::HostStatus;
use crate::projects::HostState;
use crate::settings::{self, field, saved_to, section};
use crate::theme::{self, text, u};
use dirigent_service::hosts::{self, Host};
use gpui::{AnyElement, Context, Div, Entity, Focusable, FontWeight, SharedString, Stateful, div, prelude::*, rgba};
use std::collections::BTreeMap;

/// A host row's height, in design px.
const ROW_H: f32 = 38.;

/// How a host is authenticated to, as the form picks it (`Host::key` comes from its field).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HostAuth {
    /// Tailscale, ssh or a private network: nothing to send.
    None,
    /// An API key of the host's public listener.
    Key,
}

impl HostAuth {
    const ALL: [HostAuth; 2] = [HostAuth::None, HostAuth::Key];

    fn of(host: &Host) -> Self {
        if host.key.is_some() { HostAuth::Key } else { HostAuth::None }
    }

    fn label(self) -> &'static str {
        match self {
            HostAuth::None => "none",
            HostAuth::Key => "API key",
        }
    }

    /// What it's for, in the menus.
    fn hint(self) -> &'static str {
        match self {
            HostAuth::None => "tailscale, ssh or plain ws",
            HostAuth::Key => "a public host's key",
        }
    }

    /// The address field's placeholder: the forms an address of this kind takes.
    fn placeholder(self) -> &'static str {
        match self {
            HostAuth::None => "host, ssh://user@host or ws://host:7777/ws",
            HostAuth::Key => "wss://host/ws",
        }
    }
}

/// The open form.
pub struct Form {
    /// The host it edits (its name in `hosts.toml`); `None`: a new one.
    pub editing: Option<String>,
    pub auth: HostAuth,
    /// Why the last save didn't go through, until the next edit.
    pub error: Option<String>,
}

/// The tab's state, on `Dirigent`: the form's fields and the form, if open.
pub struct State {
    pub name: Entity<TextInput>,
    pub url: Entity<TextInput>,
    pub key: Entity<TextInput>,
    pub form: Option<Form>,
}

impl State {
    pub fn new(cx: &mut Context<Dirigent>) -> Self {
        let name = cx.new(|cx| TextInput::new(cx, "hostname", true));
        let url = cx.new(|cx| TextInput::new(cx, HostAuth::None.placeholder(), true));
        let key = cx.new(|cx| {
            let mut key = TextInput::new(cx, "dk_…", true);
            key.set_secret(true, cx);
            key
        });
        for input in [&name, &url, &key] {
            cx.subscribe(input, |this, _, _: &Submitted, cx| this.save_host_form(cx)).detach();
            cx.subscribe(input, |this, _, _: &input::Changed, cx| {
                if let Some(form) = this.hosts_tab.form.as_mut().filter(|f| f.error.is_some()) {
                    form.error = None;
                    cx.notify();
                }
            })
            .detach();
        }
        Self { name, url, key, form: None }
    }

    pub fn inputs(&self) -> [Entity<TextInput>; 3] {
        [self.name.clone(), self.url.clone(), self.key.clone()]
    }
}

impl Dirigent {
    /// `Add host`'s menu: the form for a new host authenticated with `auth`.
    pub fn add_host_form(&mut self, auth: HostAuth, cx: &mut Context<Self>) {
        self.open_host_form(None, auth, ["", "", ""], cx);
    }

    /// A row's click: the form for host `name`, filled in.
    pub fn edit_host_form(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some(host) = self.host(name).map(|h| h.config.clone()) else { return };
        let key = host.key.clone().unwrap_or_default();
        self.open_host_form(Some(name.to_owned()), HostAuth::of(&host), [name, &host.url, &key], cx);
    }

    fn open_host_form(&mut self, editing: Option<String>, auth: HostAuth, [name, url, key]: [&str; 3], cx: &mut Context<Self>) {
        self.menu = None;
        let tab = &self.hosts_tab;
        for (input, text) in [(&tab.name, name), (&tab.url, url), (&tab.key, key)] {
            self.set_input(input, text, cx);
        }
        self.hosts_tab.url.update(cx, |i, cx| i.set_placeholder(auth.placeholder(), cx));
        self.hosts_tab.form = Some(Form { editing, auth, error: None });
        self.focus(self.hosts_tab.name.focus_handle(cx), cx);
        cx.notify();
    }

    /// The form's auth chip: the fields follow (an API key's takes the focus).
    pub fn set_host_auth(&mut self, auth: HostAuth, cx: &mut Context<Self>) {
        self.menu = None;
        let Some(form) = &mut self.hosts_tab.form else { return };
        let changed = form.auth != auth;
        form.auth = auth;
        form.error = None;
        self.hosts_tab.url.update(cx, |i, cx| i.set_placeholder(auth.placeholder(), cx));
        if changed && auth == HostAuth::Key {
            self.focus(self.hosts_tab.key.focus_handle(cx), cx);
        }
        cx.notify();
    }

    /// Enter or Save: write the form's host to `hosts.toml` and close it, or say what's wrong.
    pub fn save_host_form(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &self.hosts_tab.form else { return };
        let text = |input: &Entity<TextInput>| input.read(cx).text().to_owned();
        let (name, url, key) = (text(&self.hosts_tab.name), text(&self.hosts_tab.url), text(&self.hosts_tab.key));
        let editing = form.editing.clone();
        let saved = host_from(&name, &url, form.auth, &key).and_then(|(name, host)| {
            let mut list = hosts::load().map_err(|e| format!("{e:#}"))?;
            put(&mut list, editing.as_deref(), name.clone(), host)?;
            hosts::save(&list).map_err(|e| format!("{e:#}"))?;
            Ok(name)
        });
        match saved {
            Ok(name) => {
                match editing.filter(|old| *old != name) {
                    Some(old) => self.rename_host(&old, &name, cx),
                    None => self.sync_hosts(cx),
                }
                // After: a thread reopened under the new name takes the focus, which goes back
                // to the modal here.
                self.close_host_form(cx);
            }
            Err(e) => {
                if let Some(form) = &mut self.hosts_tab.form {
                    form.error = Some(e);
                }
                cx.notify();
            }
        }
    }

    /// Cancel, Esc or a save: the form goes, nothing else changes.
    pub fn close_host_form(&mut self, cx: &mut Context<Self>) {
        if self.hosts_tab.form.take().is_some() {
            self.menu = None;
            self.focus_root(cx);
            cx.notify();
        }
    }

    /// Esc: closes the form if the tab shows one (and says so; else the modal closes).
    pub fn host_form_escape(&mut self, cx: &mut Context<Self>) -> bool {
        let on_tab = self.modal == Some(Modal::Settings) && self.settings.tab == settings::Tab::Hosts;
        if !on_tab || self.hosts_tab.form.is_none() {
            return false;
        }
        self.close_host_form(cx);
        true
    }

    pub fn remove_host(&mut self, name: &str, cx: &mut Context<Self>) {
        if self.hosts_tab.form.as_ref().is_some_and(|f| f.editing.as_deref() == Some(name)) {
            self.close_host_form(cx);
        }
        let saved = hosts::load().and_then(|mut list| {
            list.remove(name);
            hosts::save(&list)
        });
        match saved {
            Ok(()) => self.sync_hosts(cx),
            Err(e) => self.toast(format!("{e:#}"), cx),
        }
    }
}

/// The form's fields as the host to save under its name, or what's wrong with them.
fn host_from(name: &str, address: &str, auth: HostAuth, key: &str) -> Result<(String, Host), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Give the host a name".into());
    }
    if name == hosts::LOCAL {
        return Err("`local` is this machine: pick another name".into());
    }
    if address.trim().is_empty() {
        return Err("Enter the host's address".into());
    }
    let url = hosts::normalize(address).map_err(|e| format!("Address: {e}"))?;
    let key = match (auth, key.trim()) {
        (HostAuth::None, _) => None,
        (HostAuth::Key, "") => return Err("Paste the host's API key".into()),
        (HostAuth::Key, key) => Some(key.to_owned()),
    };
    Ok((name.to_owned(), Host { url, key }))
}

/// Put `host` in `list` as `name`, in place of `editing` (the name it had: a rename drops
/// that entry). A name another host has is refused.
fn put(list: &mut BTreeMap<String, Host>, editing: Option<&str>, name: String, host: Host) -> Result<(), String> {
    if editing != Some(name.as_str()) && list.contains_key(&name) {
        return Err(format!("There's already a host named {name}"));
    }
    if let Some(old) = editing {
        list.remove(old);
    }
    list.insert(name, host);
    Ok(())
}

/// The tab: the rows (one of them the form while it's edited), then `Add host`, or the
/// form adding one. One form at a time: `Add host` waits while one is open.
pub fn render(this: &Dirigent, cx: &mut Context<Dirigent>) -> Vec<Div> {
    let form = this.hosts_tab.form.as_ref();
    let editing = form.and_then(|f| f.editing.as_deref());
    let rows: Vec<AnyElement> = this
        .hosts
        .iter()
        .map(|h| match form {
            Some(f) if editing == Some(h.name.as_str()) => form_card(this, f, cx).into_any_element(),
            _ => row(this, h, cx).into_any_element(),
        })
        .collect();
    // A new host's form, or one whose host left `hosts.toml` meanwhile (a hand edit), goes under the rows.
    let in_rows = editing.is_some_and(|e| this.hosts.iter().any(|h| h.name == e));
    let below = match form {
        Some(f) if !in_rows => Some(form_card(this, f, cx).mt_2()),
        Some(_) => None,
        None => Some(div().flex().flex_row().pt_3().child(add_button(this, cx))),
    };
    vec![section("Hosts", div().flex().flex_col().children(rows).children(below)), saved_to(&hosts::path())]
}

/// `● name   address   auth   state   ✎ ×`: one line, whatever its state.
fn row(this: &Dirigent, h: &HostState, cx: &mut Context<Dirigent>) -> Stateful<Div> {
    let t = theme::t();
    let name = h.name.clone();
    let local = name == hosts::LOCAL;
    let (dot, status, why) = state(h);
    let auth = (HostAuth::of(&h.config) != HostAuth::None).then(|| HostAuth::of(&h.config).label());
    let edit = name.clone();
    div()
        .id(SharedString::from(format!("host/{name}")))
        .group("host-row")
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .h(u(ROW_H))
        .px_2()
        .rounded_md()
        .when(!local, |d| {
            d.cursor_pointer().hover(|d| d.bg(rgba(t.hover))).on_click(cx.listener(move |this, _, _, cx| this.edit_host_form(&edit, cx)))
        })
        .child(theme::dot(dot))
        .child(div().w(u(110.)).flex_shrink_0().truncate().font_weight(FontWeight::MEDIUM).child(name.clone()))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_family(this.fonts.mono.clone())
                .text_size(text::sm())
                .text_color(rgba(t.fg_1))
                .child(h.config.url.clone()),
        )
        .child(div().w(u(64.)).flex_shrink_0().text_size(text::sm()).text_color(rgba(t.fg_2)).children(auth))
        .child(
            div()
                .id(SharedString::from(format!("host-status/{name}")))
                .min_w(u(104.))
                .flex_shrink_0()
                .flex()
                .flex_row()
                .child(status)
                .when_some(why, |d, why| d.tooltip(theme::tooltip(why))),
        )
        .child(actions(&name, local, cx))
}

/// The row's dot colour, its state word and what its cell shows on hover: `error` (red)
/// while it can't connect, kept through its retries so the row doesn't flicker, with why.
fn state(h: &HostState) -> (u32, AnyElement, Option<String>) {
    let t = theme::t();
    let word = |text: &'static str, color: u32| div().text_size(text::sm()).text_color(rgba(color)).child(text).into_any_element();
    match (&h.error, h.status) {
        (_, HostStatus::Connected) => (t.done, word("connected", t.fg_2), None),
        (Some(e), _) => (t.error, word("error", t.error), Some(e.clone())),
        (None, HostStatus::Connecting) => (t.warn, word("connecting", t.fg_2), None),
        (None, HostStatus::Disconnected) => (t.error, word("disconnected", t.fg_2), None),
    }
}

/// Edit and remove, shown while the row is hovered; `local` has neither.
fn actions(name: &str, local: bool, cx: &mut Context<Dirigent>) -> Div {
    let t = theme::t();
    let button = |id: String, icon_name: &'static str| {
        div()
            .id(SharedString::from(id))
            .size(u(24.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .cursor_pointer()
            .hover(|d| d.bg(rgba(t.line)))
            .child(icon(icon_name, t.fg_1))
    };
    let (edit, remove) = (name.to_owned(), name.to_owned());
    div().w(u(48.)).flex_shrink_0().flex().flex_row().opacity(0.).group_hover("host-row", |d| d.opacity(1.)).when(!local, |d| {
        d.child(button(format!("edit-host/{name}"), "pencil").tooltip(theme::tooltip("Edit")).on_click(cx.listener(
            move |this, _, _, cx| {
                cx.stop_propagation();
                this.edit_host_form(&edit, cx)
            },
        )))
        .child(button(format!("rm-host/{name}"), "x").tooltip(theme::tooltip("Remove")).on_click(cx.listener(
            move |this, _, _, cx| {
                cx.stop_propagation();
                this.remove_host(&remove, cx)
            },
        )))
    })
}

/// `+ Add host ▾`: its menu picks the new host's auth, which opens the form.
fn add_button(this: &Dirigent, cx: &mut Context<Dirigent>) -> Stateful<Div> {
    let open = this.menu == Some(Menu::AddHost);
    let menu = open.then(|| {
        let mut items = vec![menu::header("Auth")];
        items.extend(HostAuth::ALL.into_iter().map(|a| {
            menu::item(format!("add-host/{}", a.label()), a.label(), a.hint(), false, move |this, cx| this.add_host_form(a, cx), cx)
        }));
        menu::popup(Side::Below(26.), items, cx)
    });
    theme::button("add-host", "plus", "Add host")
        .when(open, |d| d.bg(rgba(theme::t().hover)))
        .child(icon("chevron-down", theme::t().fg_2).size(u(12.)))
        .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(Menu::AddHost, cx)))
        .children(menu)
}

/// The form: what it does and its auth chip, the fields that auth needs, then what's wrong
/// (if anything) beside Cancel and Save.
fn form_card(this: &Dirigent, f: &Form, cx: &mut Context<Dirigent>) -> Div {
    let t = theme::t();
    let tab = &this.hosts_tab;
    let title = match &f.editing {
        Some(name) => format!("Edit {name}"),
        None => "New host".to_owned(),
    };
    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(div().font_weight(FontWeight::MEDIUM).child(title))
        .child(auth_chip(this, f, cx));
    let line = |label: &'static str, input: &Entity<TextInput>| {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .child(div().w(u(72.)).flex_shrink_0().text_color(rgba(t.fg_1)).child(label))
            .child(field(input.clone()).flex_1())
    };
    let note = f.error.clone().map(|e| div().truncate().text_color(rgba(t.error)).child(e).into_any_element());
    let save = if f.editing.is_some() { "Save" } else { "Add host" };
    let footer = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .pt_1()
        .child(div().flex_1().min_w_0().text_size(text::sm()).children(note))
        .child(settings::text_button("host-form-cancel", "Cancel").on_click(cx.listener(|this, _, _, cx| this.close_host_form(cx))))
        .child(settings::primary_button("host-form-save", save).on_click(cx.listener(|this, _, _, cx| this.save_host_form(cx))));
    div()
        .flex()
        .flex_col()
        .gap_2p5()
        .p_4()
        .rounded_lg()
        .border_1()
        .border_color(rgba(t.line))
        .bg(rgba(t.raised))
        .child(header)
        .child(line("Name", &tab.name))
        .child(line("Address", &tab.url))
        .when(f.auth == HostAuth::Key, |d| d.child(line("API key", &tab.key)))
        .child(footer)
}

/// `Auth  API key ▾`: the form's auth, changeable.
fn auth_chip(this: &Dirigent, f: &Form, cx: &mut Context<Dirigent>) -> Stateful<Div> {
    let open = this.menu == Some(Menu::HostAuth);
    let menu = open.then(|| {
        let items = HostAuth::ALL
            .into_iter()
            .map(|a| {
                menu::item(
                    format!("host-auth/{}", a.label()),
                    a.label(),
                    a.hint(),
                    a == f.auth,
                    move |this, cx| this.set_host_auth(a, cx),
                    cx,
                )
            })
            .collect();
        menu::popup(Side::BelowRight(26.), items, cx)
    });
    theme::labeled_chip("host-auth", "Auth", f.auth.label(), f.auth == HostAuth::None, open)
        .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(Menu::HostAuth, cx)))
        .children(menu)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(entries: &[(&str, &str)]) -> BTreeMap<String, Host> {
        entries.iter().map(|(n, u)| (n.to_string(), Host::new(u.to_string()))).collect()
    }

    #[test]
    fn form_fields_make_a_host() {
        let (name, host) = host_from(" box ", "ssh://seb@box/7800", HostAuth::None, "dk_ignored").unwrap();
        assert_eq!((name.as_str(), host), ("box", Host::new("ssh://seb@box/7800".into())));
        let (_, host) = host_from("prod", "wss://d.example.com", HostAuth::Key, " dk_abc ").unwrap();
        assert_eq!(host, Host { url: "wss://d.example.com/ws".into(), key: Some("dk_abc".into()) });
        for (name, address, auth, key) in [
            ("", "h", HostAuth::None, ""),
            ("local", "h", HostAuth::None, ""),
            ("x", " ", HostAuth::None, ""),
            ("x", "http://h", HostAuth::None, ""),
            ("x", "h", HostAuth::Key, " "),
        ] {
            assert!(host_from(name, address, auth, key).is_err(), "{name:?} {address:?}");
        }
    }

    #[test]
    fn put_adds_edits_and_renames() {
        let mut hosts = list(&[("a", "ws://a:1/ws"), ("b", "ws://b:1/ws")]);
        // A new host may not take a name in use; an edit keeping its name replaces it.
        assert!(put(&mut hosts, None, "a".into(), Host::new("ws://x:1/ws".into())).is_err());
        put(&mut hosts, Some("a"), "a".into(), Host::new("ws://a:2/ws".into())).unwrap();
        assert_eq!(hosts, list(&[("a", "ws://a:2/ws"), ("b", "ws://b:1/ws")]));
        // A rename leaves no entry under the old name, and can't land on another host's.
        assert!(put(&mut hosts, Some("a"), "b".into(), Host::new("ws://a:2/ws".into())).is_err());
        put(&mut hosts, Some("a"), "c".into(), Host::new("ws://a:2/ws".into())).unwrap();
        assert_eq!(hosts, list(&[("b", "ws://b:1/ws"), ("c", "ws://a:2/ws")]));
        put(&mut hosts, None, "d".into(), Host::new("ws://d:1/ws".into())).unwrap();
        assert_eq!(hosts.len(), 3);
    }
}
