//! Settings › Providers: the model providers a host's pi knows, how each is set up, and
//! signing in and out of them here, as pi's `/login` and `/logout` do in a terminal.
//!
//! A login is pi's own flow, shown as it goes (`LoginStep`s from the host): a page the
//! browser is sent to, a device code to enter, questions answered in `answer` or picked
//! from a list. Credentials are saved on the host, in pi's `auth.json` (which the terminal
//! pi shares), and the host's model list is fetched again once a login or logout is done.
//! Closing the modal cancels a login; so does the host's connection dropping.

use crate::app::{Dirigent, Menu};
use crate::input::TextInput;
use crate::menu::{self, Side};
use crate::net::HostStatus;
use crate::settings::{field, saved_to, section};
use crate::theme::{self, text, u};
use dirigent_protocol::{AuthMethod, ClientMessage, LoginStep, PromptKind, ProviderInfo};
use dirigent_service::hosts;
use gpui::{AnyElement, ClipboardItem, Context, Div, Entity, Focusable, FontWeight, SharedString, Stateful, div, prelude::*, rgba};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// The tab's state, on `Dirigent`.
pub struct State {
    /// The host whose providers show.
    pub host: String,
    /// Its providers and the file logins are saved to, as last listed, or why listing failed.
    pub list: Option<Result<(Vec<ProviderInfo>, String), String>>,
    list_request: Option<u64>,
    search: Entity<TextInput>,
    /// The login in progress, or the one that just failed (until `Back`).
    login: Option<Login>,
    /// The field the login's open question is answered in.
    answer: Entity<TextInput>,
    /// `Logout` requests in flight, with their host and provider.
    logouts: HashMap<u64, (String, ProviderInfo)>,
    /// Logins cancelled here: their end needs no showing (but a login that got through
    /// anyway still changes the list).
    cancelled: HashSet<u64>,
}

struct Login {
    host: String,
    /// The `Login` request, whose id names the login in its steps and answers.
    request: u64,
    name: String,
    method: AuthMethod,
    /// What pi showed so far, oldest first. A prompt leaves once answered or withdrawn.
    steps: Vec<LoginStep>,
    /// Why it failed; it is over then.
    error: Option<String>,
}

impl Login {
    /// The question waiting for an answer (pi asks one at a time): its number and kind.
    fn prompt(&self) -> Option<(u32, PromptKind)> {
        self.steps.iter().rev().find_map(|s| match s {
            LoginStep::Prompt { n, kind, .. } => Some((*n, *kind)),
            _ => None,
        })
    }
}

impl State {
    pub fn new(cx: &mut Context<Dirigent>) -> Self {
        let search = cx.new(|cx| TextInput::new(cx, "Search providers", true));
        let answer = cx.new(|cx| TextInput::new(cx, "", true));
        cx.subscribe(&answer, |this, _, _: &crate::input::Submitted, cx| this.submit_login_answer(cx)).detach();
        Self {
            host: hosts::LOCAL.to_owned(),
            list: None,
            list_request: None,
            search,
            login: None,
            answer,
            logouts: HashMap::new(),
            cancelled: HashSet::new(),
        }
    }
}

impl Dirigent {
    /// Settings on the Providers tab, for `host` if given (the model picker's "sign in" for
    /// the composer's host).
    pub fn open_providers(&mut self, host: Option<String>, cx: &mut Context<Self>) {
        if let Some(host) = host.filter(|h| *h != self.providers.host) {
            self.providers.host = host;
            self.providers.list = None;
        }
        self.settings.tab = crate::settings::Tab::Providers;
        self.open_settings(cx);
    }

    /// Ask the shown host for its providers (on opening the tab, and after every change).
    pub fn request_providers(&mut self) {
        let host = self.providers.host.clone();
        if self.host(&host).is_some_and(|h| h.status == HostStatus::Connected) {
            self.providers.list_request = Some(self.request(&host, |id| ClientMessage::ListProviders { id }));
        }
    }

    pub fn set_providers_host(&mut self, host: String, cx: &mut Context<Self>) {
        self.menu = None;
        if host != self.providers.host {
            self.providers.host = host;
            self.providers.list = None;
            self.request_providers();
        }
        cx.notify();
    }

    pub fn start_login(&mut self, provider: &ProviderInfo, method: AuthMethod, cx: &mut Context<Self>) {
        let host = self.providers.host.clone();
        if self.host(&host).is_none_or(|h| h.status != HostStatus::Connected) {
            return self.toast(format!("{host} is offline"), cx);
        }
        let id = provider.id.clone();
        let request = self.request(&host, |rid| ClientMessage::Login { id: rid, provider: id, method });
        self.providers.login = Some(Login { host, request, name: provider.name.clone(), method, steps: vec![], error: None });
        self.focus_providers(cx);
        cx.notify();
    }

    /// Stop the login (Cancel, or the modal closing); a failed one is just put away.
    pub fn cancel_login(&mut self, cx: &mut Context<Self>) {
        let Some(login) = self.providers.login.take() else { return };
        if login.error.is_none() {
            self.providers.cancelled.insert(login.request);
            self.request(&login.host, |id| ClientMessage::LoginCancel { id, login: login.request });
        }
        self.focus_providers(cx);
        cx.notify();
    }

    /// Answer the login's open question: `value`, or what the field holds.
    pub fn answer_login(&mut self, value: Option<String>, cx: &mut Context<Self>) {
        // Empty is an answer too: GitHub's "Enterprise domain, blank for github.com".
        let value = value.unwrap_or_else(|| self.providers.answer.read(cx).text().trim().to_owned());
        let Some(login) = &mut self.providers.login else { return };
        let Some((n, _)) = login.prompt() else { return };
        login.steps.retain(|s| !matches!(s, LoginStep::Prompt { n: m, .. } if *m == n));
        let (host, request) = (login.host.clone(), login.request);
        self.request(&host, |id| ClientMessage::LoginAnswer { id, login: request, prompt: n, value });
        self.set_input(&self.providers.answer, "", cx);
        self.focus_providers(cx);
        cx.notify();
    }

    fn submit_login_answer(&mut self, cx: &mut Context<Self>) {
        self.answer_login(None, cx);
    }

    pub fn logout(&mut self, provider: &ProviderInfo, cx: &mut Context<Self>) {
        let host = self.providers.host.clone();
        if self.host(&host).is_none_or(|h| h.status != HostStatus::Connected) {
            return self.toast(format!("{host} is offline"), cx);
        }
        let id = provider.id.clone();
        let request = self.request(&host, |rid| ClientMessage::Logout { id: rid, provider: id });
        self.providers.logouts.insert(request, (host, provider.clone()));
        cx.notify();
    }

    pub(crate) fn on_providers(&mut self, host: &str, reply_to: u64, providers: Vec<ProviderInfo>, auth_path: String) {
        if host == self.providers.host && self.providers.list_request.take_if(|r| *r == reply_to).is_some() {
            self.providers.list = Some(Ok((providers, auth_path)));
        }
    }

    pub(crate) fn on_login_step(&mut self, host: &str, login: u64, step: LoginStep, cx: &mut Context<Self>) {
        if !self.providers.login.as_ref().is_some_and(|l| l.host == host && l.request == login) {
            return;
        }
        match &step {
            // Sent to the browser at once, as pi does; the step offers it again.
            LoginStep::AuthUrl { url, .. } => cx.open_url(url),
            LoginStep::Prompt { kind, placeholder, .. } => {
                let secret = *kind == PromptKind::Secret;
                let placeholder = placeholder.clone().unwrap_or_default();
                self.providers.answer.update(cx, |a, cx| {
                    a.set_secret(secret, cx);
                    a.set_placeholder(placeholder, cx);
                });
                self.set_input(&self.providers.answer, "", cx);
            }
            _ => {}
        }
        let Some(l) = &mut self.providers.login else { return };
        match step {
            LoginStep::PromptClosed { n } => l.steps.retain(|s| !matches!(s, LoginStep::Prompt { n: m, .. } if *m == n)),
            step => l.steps.push(step),
        }
        self.focus_providers(cx);
    }

    /// The reply to one of this tab's requests (`Err`: an `Error` reply). False when the
    /// request is none of them.
    pub(crate) fn on_providers_reply(&mut self, host: &str, reply_to: u64, outcome: Result<(), String>, cx: &mut Context<Self>) -> bool {
        let p = &mut self.providers;
        // Cancelled too late (it was fetching the model list): it is set up all the same.
        if p.cancelled.remove(&reply_to) {
            if outcome.is_ok() {
                self.refresh_after_auth(host);
            }
            return true;
        }
        if host == p.host && p.list_request.take_if(|r| *r == reply_to).is_some() {
            if let Err(e) = outcome {
                p.list = Some(Err(too_old(e)));
            }
            return true;
        }
        if let Some((_, provider)) = p.logouts.remove(&reply_to) {
            let name = provider.name;
            match outcome {
                Ok(()) => {
                    let message = match provider.stored {
                        Some(AuthMethod::Oauth) => format!("Signed out of {name}"),
                        _ => format!("Removed the key for {name}"),
                    };
                    self.toast(message, cx);
                    self.refresh_after_auth(host);
                }
                Err(e) => {
                    self.toast(format!("Couldn't sign out of {name}: {e}"), cx);
                    // pi may have removed it and failed after (updating its model list).
                    self.refresh_after_auth(host);
                }
            }
            return true;
        }
        let Some(login) = p.login.as_mut().filter(|l| l.host == host && l.request == reply_to) else { return false };
        match outcome {
            Ok(()) => {
                let message = match login.method {
                    AuthMethod::Oauth => format!("Signed in to {}", login.name),
                    AuthMethod::ApiKey => format!("{} is set up", login.name),
                };
                p.login = None;
                self.toast(message, cx);
                self.refresh_after_auth(host);
            }
            Err(e) => {
                login.error = Some(too_old(e));
                // pi may have saved it and failed after (updating its model list).
                self.refresh_after_auth(host);
            }
        }
        self.focus_providers(cx);
        true
    }

    /// A login or logout changed what the host offers: its providers and models anew.
    fn refresh_after_auth(&mut self, host: &str) {
        self.request(host, |id| ClientMessage::ListModels { id });
        if host == self.providers.host {
            self.request_providers();
        }
    }

    /// A host connected or dropped. A login dies with its connection (the host cancels it).
    pub(crate) fn providers_status(&mut self, host: &str, status: HostStatus) {
        if status == HostStatus::Disconnected {
            if let Some(l) = self.providers.login.as_mut().filter(|l| l.host == host && l.error.is_none()) {
                l.error = Some("The connection to the host dropped".into());
            }
            if host == self.providers.host {
                self.providers.list_request = None;
            }
            self.providers.logouts.retain(|_, (h, _)| h != host);
        }
        if status == HostStatus::Connected && host == self.providers.host && self.providers_open() {
            self.request_providers();
        }
    }

    /// Keys go where the tab wants them: the field of an open question, else the search
    /// (no login), else the modal itself, so Esc closes it.
    pub(crate) fn focus_providers(&self, cx: &mut Context<Self>) {
        if !self.providers_open() {
            return;
        }
        let p = &self.providers;
        let typing = p.login.as_ref().and_then(Login::prompt).is_some_and(|(_, kind)| kind != PromptKind::Select);
        match (&p.login, typing) {
            (_, true) => self.focus(p.answer.focus_handle(cx), cx),
            (None, _) => self.focus(p.search.focus_handle(cx), cx),
            _ => self.focus_root(cx),
        }
    }

    fn providers_open(&self) -> bool {
        self.modal == Some(crate::app::Modal::Settings) && self.settings.tab == crate::settings::Tab::Providers
    }
}

/// An `Error` reply from a service that doesn't know the request yet (see the generic
/// case in `Dirigent::on_server_message`).
fn too_old(message: String) -> String {
    if message.starts_with("bad request: unknown variant") {
        "This host's service is too old to manage providers; update it".into()
    } else {
        message
    }
}

/// The tab: the host's providers (set up first), or the login in progress.
pub fn render(this: &Dirigent, cx: &mut Context<Dirigent>) -> Vec<Div> {
    let p = &this.providers;
    let up = this.host(&p.host).is_some_and(|h| h.status == HostStatus::Connected);
    let body = match (&p.login, &p.list) {
        (Some(login), _) => login_panel(login, this, cx),
        (None, None) if !up => hint(format!("{} is offline", p.host)),
        (None, None) => hint("Loading…"),
        (None, Some(Err(e))) => div().text_size(text::sm()).text_color(rgba(theme::t().error)).child(e.clone()),
        (None, Some(Ok((providers, _)))) => list(providers, this, cx),
    };
    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(
            div().text_size(text::sm()).text_color(rgba(theme::t().fg_1)).child("Sign in to the providers whose models you want to use."),
        )
        // Not during a login: it is the host's it started on.
        .children((this.hosts.len() > 1 && p.login.is_none()).then(|| host_chip(this, cx)));
    let mut out = vec![section("Providers", div().flex().flex_col().gap_3().child(header).child(body))];
    if let Some(Ok((_, path))) = &p.list {
        out.push(saved_to(Path::new(path)));
    }
    out
}

/// `Host  local ▾`: whose providers show.
fn host_chip(this: &Dirigent, cx: &mut Context<Dirigent>) -> Stateful<Div> {
    let open = this.menu == Some(Menu::ProvidersHost);
    let menu = open.then(|| {
        let items = this
            .hosts
            .iter()
            .map(|h| {
                let name = h.name.clone();
                let detail = if h.status == HostStatus::Connected { "" } else { "offline" };
                let selected = name == this.providers.host;
                menu::item(
                    format!("ph/{name}"),
                    name.clone(),
                    detail,
                    selected,
                    move |this, cx| this.set_providers_host(name.clone(), cx),
                    cx,
                )
            })
            .collect();
        menu::popup(Side::BelowRight(26.), items, cx)
    });
    theme::labeled_chip("providers-host", "Host", this.providers.host.clone(), false, open)
        .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(Menu::ProvidersHost, cx)))
        .children(menu)
}

/// The search field, then the providers matching it: those set up, then the rest.
fn list(providers: &[ProviderInfo], this: &Dirigent, cx: &mut Context<Dirigent>) -> Div {
    let query = this.providers.search.read(cx).text().to_lowercase();
    let matches = |p: &&ProviderInfo| query.split_whitespace().all(|w| p.name.to_lowercase().contains(w) || p.id.contains(w));
    let (set_up, rest): (Vec<&ProviderInfo>, Vec<&ProviderInfo>) =
        providers.iter().filter(matches).partition(|p| p.status.is_some() || p.stored.is_some());
    let shown = &this.providers.host;
    let busy: HashSet<&str> = this.providers.logouts.values().filter(|(h, _)| h == shown).map(|(_, p)| p.id.as_str()).collect();
    let group = |title: &'static str, rows: Vec<&ProviderInfo>, cx: &mut Context<Dirigent>| {
        (!rows.is_empty()).then(|| {
            div()
                .flex()
                .flex_col()
                .child(div().h(u(26.)).flex().items_end().pb_1().text_size(text::xs()).text_color(rgba(theme::t().fg_2)).child(title))
                .children(rows.into_iter().map(|p| provider_row(p, busy.contains(p.id.as_str()), cx)))
        })
    };
    let empty = (set_up.is_empty() && rest.is_empty()).then(|| hint("No provider matches"));
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(field(this.providers.search.clone()))
        .children(group("Set up", set_up, cx))
        .children(group("Not set up", rest, cx))
        .children(empty)
}

/// `● Name   status            [Sign in] [API key]` (or `[Sign out]` for what a login stored).
fn provider_row(p: &ProviderInfo, signing_out: bool, cx: &mut Context<Dirigent>) -> Stateful<Div> {
    let t = theme::t();
    let mut actions: Vec<AnyElement> = vec![];
    if let Some(stored) = p.stored {
        let label = if signing_out {
            "Signing out…"
        } else if stored == AuthMethod::Oauth {
            "Sign out"
        } else {
            "Remove key"
        };
        let provider = p.clone();
        actions.push(
            action(format!("logout/{}", p.id), label, None)
                .on_click(cx.listener(move |this, _, _, cx| this.logout(&provider, cx)))
                .into_any(),
        );
    } else {
        for (method, label, what) in [(AuthMethod::Oauth, "Sign in", p.oauth.clone()), (AuthMethod::ApiKey, "API key", p.api_key.clone())] {
            let Some(what) = what else { continue };
            let provider = p.clone();
            actions.push(
                action(format!("login/{}/{label}", p.id), label, Some(what))
                    .on_click(cx.listener(move |this, _, _, cx| this.start_login(&provider, method, cx)))
                    .into_any(),
            );
        }
    }
    div()
        .id(SharedString::from(format!("provider/{}", p.id)))
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .h(u(38.))
        .px_2()
        .child(theme::dot(if p.status.is_some() { t.done } else { theme::alpha(t.fg_2, 0x00) }))
        .child(div().w(u(200.)).flex_shrink_0().truncate().font_weight(FontWeight::MEDIUM).child(p.name.clone()))
        .child(div().flex_1().min_w_0().truncate().text_size(text::sm()).text_color(rgba(t.fg_1)).children(p.status.clone()))
        .child(div().flex().flex_row().gap_1p5().flex_shrink_0().children(actions))
}

/// A small outlined text button; `tip` explains it on hover.
fn action(id: String, label: &'static str, tip: Option<String>) -> Stateful<Div> {
    let t = theme::t();
    div()
        .id(SharedString::from(id))
        .h(u(24.))
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

/// The login in progress: its title and Cancel, then pi's steps in order.
fn login_panel(login: &Login, this: &Dirigent, cx: &mut Context<Dirigent>) -> Div {
    let t = theme::t();
    let title = match login.method {
        AuthMethod::Oauth => format!("Sign in to {}", login.name),
        AuthMethod::ApiKey => format!("Set up {}", login.name),
    };
    let close = if login.error.is_some() { "Back" } else { "Cancel" };
    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(div().font_weight(FontWeight::MEDIUM).child(title))
        .child(action("login-close".into(), close, None).on_click(cx.listener(|this, _, _, cx| this.cancel_login(cx))));
    // A failed login shows why; its steps are of no use any more.
    let body: Vec<AnyElement> = match &login.error {
        Some(e) => vec![div().text_size(text::sm()).text_color(rgba(t.error)).child(e.clone()).into_any()],
        None if login.steps.is_empty() => vec![hint("Starting…").into_any()],
        None => {
            let open = login.prompt().map(|(n, _)| n);
            login.steps.iter().map(|s| step(s, matches!(s, LoginStep::Prompt { n, .. } if Some(*n) == open), this, cx)).collect()
        }
    };
    div().flex().flex_col().gap_3().p_4().rounded_lg().border_1().border_color(rgba(t.line)).bg(rgba(t.raised)).child(header).children(body)
}

/// One of pi's steps. `open`: the question being asked (it gets the answer field).
fn step(s: &LoginStep, open: bool, this: &Dirigent, cx: &mut Context<Dirigent>) -> AnyElement {
    let t = theme::t();
    let mono = this.fonts.mono.clone();
    match s {
        LoginStep::AuthUrl { url, instructions } => {
            let (open_url, copy_url) = (url.clone(), url.clone());
            div()
                .flex()
                .flex_col()
                .gap_1p5()
                .child(instructions.clone().unwrap_or_else(|| "Finish signing in with your browser.".into()))
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
        LoginStep::DeviceCode { user_code, verification_uri } => {
            let (code, page) = (user_code.clone(), verification_uri.clone());
            div()
                .flex()
                .flex_col()
                .gap_1p5()
                .child(format!("Enter this code at {verification_uri}:"))
                .child(div().font_family(mono).text_size(text::lg()).font_weight(FontWeight::BOLD).child(user_code.clone()))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_1p5()
                        .child(
                            action("device-copy".into(), "Copy code", None)
                                .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(code.clone()))),
                        )
                        .child(action("device-open".into(), "Open page", None).on_click(move |_, _, cx| cx.open_url(&page))),
                )
                .child(hint("Waiting for you to finish there…"))
                .into_any()
        }
        LoginStep::Info { message, links } => div()
            .flex()
            .flex_col()
            .gap_1()
            .child(message.clone())
            .children(links.iter().enumerate().map(|(i, l)| {
                let url = l.url.clone();
                div()
                    .id(SharedString::from(format!("auth-link/{i}")))
                    .cursor_pointer()
                    .text_size(text::sm())
                    .text_color(rgba(t.accent_ink))
                    .hover(|d| d.underline())
                    .on_click(move |_, _, cx| cx.open_url(&url))
                    .child(l.label.clone().unwrap_or_else(|| l.url.clone()))
            }))
            .into_any(),
        LoginStep::Progress { message } => hint(message.clone()).into_any(),
        LoginStep::Prompt { message, kind, options, .. } => {
            let question = div().child(message.clone());
            if !open {
                return question.into_any();
            }
            let answer = if *kind == PromptKind::Select {
                div()
                    .flex()
                    .flex_col()
                    .children(options.iter().map(|o| {
                        let id = o.id.clone();
                        menu::row(format!("auth-option/{}", o.id), false)
                            .on_click(cx.listener(move |this, _, _, cx| this.answer_login(Some(id.clone()), cx)))
                            .child(div().flex_shrink_0().child(o.label.clone()))
                            .children(
                                o.description
                                    .clone()
                                    .map(|d| div().min_w_0().truncate().text_size(text::sm()).text_color(rgba(t.fg_1)).child(d)),
                            )
                    }))
                    .into_any()
            } else {
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .items_center()
                    .child(field(this.providers.answer.clone()).flex_1())
                    .child(
                        action("auth-answer".into(), "Continue", None).on_click(cx.listener(|this, _, _, cx| this.answer_login(None, cx))),
                    )
                    .into_any()
            };
            div().flex().flex_col().gap_1p5().child(question).child(answer).into_any()
        }
        // Never kept (it removes its prompt), or from a newer host.
        LoginStep::PromptClosed { .. } | LoginStep::Unknown => div().into_any(),
    }
}

/// Small grey text: waiting, progress, an empty list.
fn hint(text: impl Into<SharedString>) -> Div {
    div().text_size(text::sm()).text_color(rgba(theme::t().fg_2)).child(text.into())
}
