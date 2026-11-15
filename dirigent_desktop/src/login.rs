//! What Settings' tabs about one host share: which host they show (`State::host`, its
//! chip), and an MCP server's sign-in, shown as it goes (`LoginStep`s from the host): a
//! page the browser is sent to, and a field for the address it ended up on, for when it
//! can't reach the host. Closing the modal cancels a sign-in; so does the host's
//! connection dropping.
//!
//! Settings › Proxy (`proxy.rs`) and MCP servers (`mcp.rs`) are the tabs; the setup's
//! proxy step shows the same host.

use crate::app::{Dirigent, Menu};
use crate::input::TextInput;
use crate::menu::{self, Side};
use crate::net::HostStatus;
use crate::settings::field;
use crate::theme::{self, text, u};
use dirigent_protocol::{ClientMessage, LoginStep};
use dirigent_service::hosts;
use gpui::{AnyElement, ClipboardItem, Context, Div, Entity, FontWeight, SharedString, Stateful, div, prelude::*, rgba};
use std::collections::HashSet;

/// On `Dirigent`.
pub struct State {
    /// The host whose proxy and MCP servers show.
    pub host: String,
    /// The sign-in in progress, or the one that just failed (until `Back`).
    pub(crate) login: Option<Login>,
    /// The field the sign-in's open question is answered in.
    pub(crate) answer: Entity<TextInput>,
    /// Sign-ins cancelled here: their end needs no showing (but one that got through
    /// anyway still changes the server's state).
    cancelled: HashSet<u64>,
}

pub(crate) struct Login {
    pub host: String,
    /// The `McpLogin` request, whose id names the sign-in in its steps and answers.
    request: u64,
    /// The server's.
    pub name: String,
    /// What the host showed so far, oldest first. A question leaves once answered or withdrawn.
    steps: Vec<LoginStep>,
    /// Why it failed; it is over then.
    error: Option<String>,
}

impl Login {
    pub fn new(host: String, request: u64, name: String) -> Self {
        Self { host, request, name, steps: vec![], error: None }
    }

    /// The question waiting for an answer, by its number.
    fn prompt(&self) -> Option<u32> {
        self.steps.iter().rev().find_map(|s| match s {
            LoginStep::Prompt { n, .. } => Some(*n),
            _ => None,
        })
    }

    /// It has a question open, answered by typing (into `State::answer`).
    pub fn typing(&self) -> bool {
        self.prompt().is_some()
    }
}

impl State {
    pub fn new(cx: &mut Context<Dirigent>) -> Self {
        let answer = cx.new(|cx| TextInput::new(cx, "", true));
        cx.subscribe(&answer, |this, _, _: &crate::input::Submitted, cx| this.answer_login(cx)).detach();
        Self { host: hosts::LOCAL.to_owned(), login: None, answer, cancelled: HashSet::new() }
    }
}

impl Dirigent {
    /// Show `host` on the tabs about one host (Proxy, MCP servers): what they hold of the
    /// last one goes. The tab showing asks for the rest.
    pub(crate) fn show_host(&mut self, host: String) {
        if host != self.login.host {
            self.login.host = host;
            self.mcp.reset();
        }
    }

    /// The host chip's pick.
    pub fn set_shown_host(&mut self, host: String, cx: &mut Context<Self>) {
        self.menu = None;
        if host != self.login.host {
            self.show_host(host);
            self.request_proxy_settings(cx);
            if self.mcp_open() {
                self.enter_mcp(cx);
            }
        }
        cx.notify();
    }

    /// Stop the sign-in (Cancel, or the modal closing); a failed one is just put away.
    pub fn cancel_login(&mut self, cx: &mut Context<Self>) {
        let Some(login) = self.login.login.take() else { return };
        if login.error.is_none() {
            self.login.cancelled.insert(login.request);
            self.request(&login.host, |id| ClientMessage::LoginCancel { id, login: login.request });
        }
        self.focus_mcp(cx);
        cx.notify();
    }

    /// Answer the sign-in's open question with what the field holds.
    pub fn answer_login(&mut self, cx: &mut Context<Self>) {
        let value = self.login.answer.read(cx).text().trim().to_owned();
        let Some(login) = &mut self.login.login else { return };
        let Some(n) = login.prompt() else { return };
        login.steps.retain(|s| !matches!(s, LoginStep::Prompt { n: m, .. } if *m == n));
        let (host, request) = (login.host.clone(), login.request);
        self.request(&host, |id| ClientMessage::LoginAnswer { id, login: request, prompt: n, value });
        self.set_input(&self.login.answer, "", cx);
        self.focus_mcp(cx);
        cx.notify();
    }

    pub(crate) fn on_login_step(&mut self, host: &str, login: u64, step: LoginStep, cx: &mut Context<Self>) {
        if !self.login.login.as_ref().is_some_and(|l| l.host == host && l.request == login) {
            return;
        }
        match &step {
            // Sent to the browser at once; the step offers it again.
            LoginStep::AuthUrl { url } => cx.open_url(url),
            LoginStep::Prompt { placeholder, .. } => {
                let placeholder = placeholder.clone().unwrap_or_default();
                self.login.answer.update(cx, |a, cx| a.set_placeholder(placeholder, cx));
                self.set_input(&self.login.answer, "", cx);
            }
            _ => {}
        }
        let Some(l) = &mut self.login.login else { return };
        match step {
            LoginStep::PromptClosed { n } => l.steps.retain(|s| !matches!(s, LoginStep::Prompt { n: m, .. } if *m == n)),
            step => l.steps.push(step),
        }
        self.focus_mcp(cx);
    }

    /// The reply that ends a sign-in (`Err`: an `Error` reply). False when the request is
    /// no sign-in's.
    pub(crate) fn on_login_reply(&mut self, host: &str, reply_to: u64, outcome: Result<(), String>, cx: &mut Context<Self>) -> bool {
        // Cancelled too late (it was connecting): the server is signed in all the same.
        if self.login.cancelled.remove(&reply_to) {
            if outcome.is_ok() {
                self.mcp_changed(host, None, cx);
            }
            return true;
        }
        let Some(login) = self.login.login.as_mut().filter(|l| l.host == host && l.request == reply_to) else { return false };
        let name = login.name.clone();
        match outcome {
            Ok(()) => {
                self.login.login = None;
                self.toast(format!("Signed in to {name}"), cx);
            }
            Err(e) => login.error = Some(too_old(e)),
        }
        // Also after a failure: the host may have stored the sign-in and failed to connect after.
        self.mcp_changed(host, Some(&name), cx);
        self.focus_mcp(cx);
        true
    }

    /// A host connected or dropped. A sign-in dies with its connection (the host cancels
    /// it), and the tabs showing the host ask it again, or stop waiting.
    pub(crate) fn shown_host_status(&mut self, host: &str, status: HostStatus, cx: &mut Context<Self>) {
        if status == HostStatus::Disconnected
            && let Some(l) = self.login.login.as_mut().filter(|l| l.host == host && l.error.is_none())
        {
            l.error = Some("The connection to the host dropped".into());
        }
        if host != self.login.host {
            return;
        }
        if self.proxy_open() {
            self.request_proxy_settings(cx);
        }
        self.mcp_status(status, cx);
    }
}

/// An `Error` reply from a service that doesn't know the request yet (see the generic
/// case in `Dirigent::on_server_message`).
pub(crate) fn too_old(message: String) -> String {
    if message.starts_with("bad request: unknown variant") { "This host's service is too old for this; update it".into() } else { message }
}

/// `Host  local ▾`: whose proxy and MCP servers show, in their tabs.
pub(crate) fn host_chip(this: &Dirigent, cx: &mut Context<Dirigent>) -> Stateful<Div> {
    let open = this.menu == Some(Menu::SettingsHost);
    let menu = open.then(|| {
        let items = this
            .hosts
            .iter()
            .map(|h| {
                let name = h.name.clone();
                let detail = if h.status == HostStatus::Connected { "" } else { "offline" };
                let selected = name == this.login.host;
                menu::item(format!("sh/{name}"), name.clone(), detail, selected, move |this, cx| this.set_shown_host(name.clone(), cx), cx)
            })
            .collect();
        menu::popup(Side::BelowRight(26.), items, cx)
    });
    theme::labeled_chip("settings-host", "Host", this.login.host.clone(), false, open)
        .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(Menu::SettingsHost, cx)))
        .children(menu)
}

/// `action`'s height, in design px.
const ACTION_H: f32 = 24.;

/// A small outlined text button; `tip` explains it on hover.
pub(crate) fn action(id: String, label: &'static str, tip: Option<String>) -> Stateful<Div> {
    let t = theme::t();
    div()
        .id(SharedString::from(id))
        .h(u(ACTION_H))
        .px_2()
        .flex()
        .items_center()
        .rounded_md()
        .cursor_pointer()
        .border_1()
        .border_color(rgba(t.line))
        .bg(rgba(t.raised))
        .text_size(text::sm())
        .text_color(rgba(t.fg))
        .hover(move |d| d.bg(rgba(t.hover)))
        .when_some(tip, |d, tip| d.tooltip(theme::tooltip(tip)))
        .child(label)
}

/// The sign-in in progress: its title and Cancel, then its steps in order.
pub(crate) fn login_panel(login: &Login, this: &Dirigent, cx: &mut Context<Dirigent>) -> Div {
    let t = theme::t();
    let close = if login.error.is_some() { "Back" } else { "Cancel" };
    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(div().font_weight(FontWeight::MEDIUM).child(format!("Sign in to {}", login.name)))
        .child(action("login-close".into(), close, None).on_click(cx.listener(|this, _, _, cx| this.cancel_login(cx))));
    // A failed sign-in shows why; its steps are of no use any more.
    let body: Vec<AnyElement> = match &login.error {
        Some(e) => vec![div().text_size(text::sm()).text_color(rgba(t.error)).child(e.clone()).into_any()],
        None if login.steps.is_empty() => vec![hint("Starting…").into_any()],
        None => {
            let open = login.prompt();
            login.steps.iter().map(|s| step(s, matches!(s, LoginStep::Prompt { n, .. } if Some(*n) == open), this, cx)).collect()
        }
    };
    theme::stack().gap_3().p_4().rounded_lg().border_1().border_color(rgba(t.line)).bg(rgba(t.raised)).child(header).children(body)
}

/// One of the sign-in's steps. `open`: the question being asked (it gets the answer field).
fn step(s: &LoginStep, open: bool, this: &Dirigent, cx: &mut Context<Dirigent>) -> AnyElement {
    match s {
        LoginStep::AuthUrl { url } => {
            let (open_url, copy_url) = (url.clone(), url.clone());
            theme::stack()
                .gap_1p5()
                .child("Finish signing in with your browser.")
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_1p5()
                        .child(action("auth-open".into(), "Open again", None).on_click(move |_, _, cx| cx.open_url(&open_url)))
                        .child(
                            action("auth-copy".into(), "Copy link", None)
                                .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(copy_url.clone()))),
                        ),
                )
                .into_any()
        }
        LoginStep::Progress { message } => hint(message.clone()).into_any(),
        LoginStep::Prompt { message, .. } => {
            let question = div().child(message.clone());
            if !open {
                return question.into_any();
            }
            // The field slimmed to Continue's height, to sit level with it.
            let answer = div()
                .flex()
                .flex_row()
                .gap_2()
                .items_center()
                .child(field(this.login.answer.clone()).flex_1().h(u(ACTION_H)).py_0().flex().items_center())
                .child(action("auth-answer".into(), "Continue", None).on_click(cx.listener(|this, _, _, cx| this.answer_login(cx))));
            theme::stack().gap_1p5().child(question).child(answer).into_any()
        }
        // Never kept (it removes its question), or from a newer host.
        LoginStep::PromptClosed { .. } | LoginStep::Unknown => div().into_any(),
    }
}

/// Small grey text: waiting, progress, an empty list.
pub(crate) fn hint(text: impl Into<SharedString>) -> Div {
    div().text_size(text::sm()).text_color(rgba(theme::t().fg_2)).child(text.into())
}
