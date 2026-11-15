//! Settings › Proxy, and the setup's proxy step: where a host's CLIProxyAPI is and the keys
//! to it. Every model a host's threads run on is reached through its proxy, so this is
//! what makes a host usable: the address and one of the proxy's API keys. Its management
//! key is optional; with it the host also reads the accounts behind the proxy and their
//! limits, which the Usage modal's Proxy tab shows (`usage.rs`). The host keeps all three
//! (`dirigent_service`'s `proxy.rs`) and no key ever comes back. The host is the one the
//! tabs about one host share (`login.host`, with its chip).
//!
//! Nothing is sent as it is typed: `Save` hands the host the address and keys, and the
//! host's look at the proxy with them is the answer shown under the fields. A wrong
//! management key counts against the host at the proxy, which bans it after five.

use crate::app::Dirigent;
use crate::input::{Submitted, TextInput};
use crate::login::{host_chip, too_old};
use crate::net::HostStatus;
use crate::settings::{self, field, primary_button, row, section, text_button};
use crate::theme::{self, text};
use dirigent_protocol::{ClientMessage, Proxy};
use dirigent_service::hosts;
use gpui::{Context, Div, Entity, Focusable, SharedString, div, prelude::*, rgba};

/// Where a CLIProxyAPI listens unless told otherwise: what the address field starts with.
const DEFAULT_URL: &str = "http://127.0.0.1:8317";
/// The key fields' placeholders: before the host has the key, and once it does.
const KEY_UNSET: &str = "One of the proxy's api-keys";
const MANAGEMENT_UNSET: &str = "Optional: its management secret-key, for Usage";
const KEY_SET: &str = "Saved. Type another to replace it";

/// The tab's state, on `Dirigent`.
pub struct State {
    url: Entity<TextInput>,
    key: Entity<TextInput>,
    management: Entity<TextInput>,
    /// The shown host's (`login.host`) proxy as it last answered here, or why it couldn't
    /// take what was saved. `None` until it answers.
    shown: Option<Result<Proxy, String>>,
    /// The request `shown` waits for.
    request: Option<u64>,
    /// That request is a save (or a remove): the host is asking the proxy.
    saving: bool,
}

impl State {
    pub fn new(cx: &mut Context<Dirigent>) -> Self {
        let url = cx.new(|cx| TextInput::new(cx, DEFAULT_URL, true));
        let secret = |placeholder: &'static str, cx: &mut Context<Dirigent>| {
            cx.new(|cx| {
                let mut key = TextInput::new(cx, placeholder, true);
                key.set_secret(true, cx);
                key
            })
        };
        let (key, management) = (secret(KEY_UNSET, cx), secret(MANAGEMENT_UNSET, cx));
        for input in [&url, &key, &management] {
            cx.subscribe(input, |this, _, _: &Submitted, cx| this.save_proxy(cx)).detach();
        }
        Self { url, key, management, shown: None, request: None, saving: false }
    }
}

impl Dirigent {
    /// Settings on the Proxy tab, for `host` if given (the model picker's way there for a
    /// host without models; the Usage modal's `Set up in Settings`).
    pub fn open_proxy_settings(&mut self, host: Option<String>, cx: &mut Context<Self>) {
        if let Some(host) = host {
            self.show_host(host);
        }
        self.settings.tab = settings::Tab::Proxy;
        self.open_settings(cx);
    }

    /// The tab (or the setup's step) shows: a host since removed gives way to `local`, the
    /// host is asked for its proxy and the API key's field takes the keys.
    pub(crate) fn enter_proxy_settings(&mut self, cx: &mut Context<Self>) {
        if self.host(&self.login.host).is_none() {
            self.show_host(hosts::LOCAL.to_owned());
        }
        self.request_proxy_settings(cx);
        self.focus(self.proxy.key.focus_handle(cx), cx);
    }

    /// Settings › Proxy or the setup's proxy step is showing.
    pub(crate) fn proxy_open(&self) -> bool {
        let tab = self.modal == Some(crate::app::Modal::Settings) && self.settings.tab == settings::Tab::Proxy;
        tab || self.onboarding.as_ref().is_some_and(|s| s.step == crate::onboarding::Step::Proxy)
    }

    /// Ask the shown host for its proxy (no history): on showing the tab, and when the
    /// host changes or reconnects.
    pub(crate) fn request_proxy_settings(&mut self, cx: &mut Context<Self>) {
        let host = self.login.host.clone();
        self.proxy.shown = None;
        self.proxy.saving = false;
        self.proxy.request = None;
        if self.connected(&host) {
            self.proxy.request = Some(self.request(&host, |id| ClientMessage::ListProxy { id, since: 0, until: 0, bucket_ms: 1 }));
        }
        for key in [&self.proxy.key, &self.proxy.management] {
            self.set_input(key, "", cx);
        }
    }

    /// `Save`, or Enter in a field: the host takes the address and keys and asks the
    /// proxy. An empty key keeps the one the host has.
    pub fn save_proxy(&mut self, cx: &mut Context<Self>) {
        let typed = |input: &Entity<TextInput>| input.read(cx).text().trim().to_owned();
        let (url, key, management_key) = (typed(&self.proxy.url), typed(&self.proxy.key), typed(&self.proxy.management));
        self.set_proxy(url, key, management_key, cx);
    }

    /// `Remove`: the host forgets its proxy. The history it sampled stays.
    pub fn remove_proxy(&mut self, cx: &mut Context<Self>) {
        self.set_proxy(String::new(), String::new(), String::new(), cx);
    }

    fn set_proxy(&mut self, url: String, key: String, management_key: String, cx: &mut Context<Self>) {
        let host = self.login.host.clone();
        if !self.connected(&host) {
            return self.toast(format!("{host} is offline"), cx);
        }
        self.proxy.request = Some(self.request(&host, |id| ClientMessage::SetProxy { id, url, key, management_key }));
        self.proxy.saving = true;
        cx.notify();
    }

    /// A reply to one of the tab's requests (`Err`: an `Error` reply). False when the
    /// request is none of them.
    pub(crate) fn on_proxy_settings(&mut self, reply_to: u64, outcome: Result<Proxy, String>, cx: &mut Context<Self>) -> bool {
        if self.proxy.request.take_if(|r| *r == reply_to).is_none() {
            return false;
        }
        // What the host serves may differ from what the pickers hold (a save just now, or
        // its `agent.toml` edited by hand): its models are asked for anew.
        self.proxy.saving = false;
        if outcome.is_ok() {
            let host = self.login.host.clone();
            self.request(&host, |id| ClientMessage::ListModels { id });
        }
        if let Ok(proxy) = &outcome {
            // What the host holds now: its address in the field, the keys never shown.
            self.set_input(&self.proxy.url, if proxy.url.is_empty() { DEFAULT_URL } else { &proxy.url }, cx);
            let keys = [(&self.proxy.key, proxy.key, KEY_UNSET), (&self.proxy.management, proxy.management_key, MANAGEMENT_UNSET)];
            for (input, held, unset) in keys {
                self.set_input(input, "", cx);
                input.update(cx, |key, cx| key.set_placeholder(if held { KEY_SET } else { unset }, cx));
            }
        }
        self.proxy.shown = Some(outcome.map_err(too_old));
        true
    }
}

/// The tab: what the proxy is for, then its fields.
pub fn render(this: &Dirigent, cx: &mut Context<Dirigent>) -> Vec<Div> {
    let about = div().text_size(text::sm()).text_color(rgba(theme::t().fg_1)).child(
        "Dirigent reaches every model through a CLIProxyAPI you run. Point each host at its proxy: the models it serves are \
         the ones that host's threads can use.",
    );
    vec![section("CLIProxyAPI", theme::stack().gap_3().child(about).child(body(this, cx)))]
}

/// The fields, also the setup's proxy step: the host's chip (with several hosts), the
/// address and the keys, `Save` (and `Remove` once the host has a proxy), and what the
/// host found there.
pub fn body(this: &Dirigent, cx: &mut Context<Dirigent>) -> Div {
    let t = theme::t();
    let p = &this.proxy;
    let host = &this.login.host;
    let note = |text: SharedString, color: u32| div().min_w_0().text_size(text::sm()).text_color(rgba(color)).child(text);
    let count = |n: usize, what: &str| if n == 1 { format!("1 {what}") } else { format!("{n} {what}s") };
    let set = matches!(&p.shown, Some(Ok(proxy)) if !proxy.url.is_empty());
    let status: Vec<Div> = match &p.shown {
        _ if p.saving => vec![note("Asking the proxy…".into(), t.fg_2)],
        None if this.host(host).is_none_or(|h| h.status != HostStatus::Connected) => {
            vec![note(format!("{host} is offline").into(), t.fg_2)]
        }
        None => vec![note("Loading…".into(), t.fg_2)],
        Some(Err(e)) => vec![note(e.clone().into(), t.error)],
        // None set: the fields say it all.
        Some(Ok(proxy)) if proxy.url.is_empty() => vec![],
        Some(Ok(proxy)) => {
            let models = match &proxy.models_error {
                Some(e) => note(e.clone().into(), t.error),
                None => {
                    let connected = note(format!("Connected, {}", count(proxy.models as usize, "model")).into(), t.fg_1);
                    div().min_w_0().flex().flex_row().items_center().gap_2().child(theme::dot(t.done)).child(connected)
                }
            };
            // Its accounts, which only the management key opens.
            let accounts = proxy.management_key.then(|| match &proxy.error {
                Some(e) => note(format!("Its accounts can't be read: {e}").into(), t.error),
                None => note(format!("{} behind it, in Usage", count(proxy.accounts.len(), "account")).into(), t.fg_2),
            });
            std::iter::once(models).chain(accounts).collect()
        }
    };
    let actions = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .pt_2()
        .child(primary_button("proxy-save", "Save").on_click(cx.listener(|this, _, _, cx| this.save_proxy(cx))))
        .when(set, |d| d.child(text_button("proxy-remove", "Remove").on_click(cx.listener(|this, _, _, cx| this.remove_proxy(cx)))));
    theme::stack()
        .children((this.hosts.len() > 1).then(|| row("Host", host_chip(this, cx))))
        .child(row("Address", field(p.url.clone())))
        .child(row("API key", field(p.key.clone())))
        .child(row("Management key", field(p.management.clone())))
        .child(actions)
        .child(theme::stack().gap_1().pt_2().children(status))
}
