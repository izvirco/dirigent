//! Settings › Proxy: where a host's CLIProxyAPI is, and the management key to it. The host
//! keeps both and asks the proxy for its accounts and their limits (`dirigent_service`'s
//! `proxy.rs`); the Usage modal's Proxy tab shows them (`usage.rs`). The host is the one
//! Settings › Providers shows (`providers.host`, with its chip): the proxy is a provider
//! of that host's models.
//!
//! Nothing is sent as it is typed: `Save` hands the host the address and key, and the
//! host's look at the proxy with them is the answer shown under the fields. A wrong key
//! counts against the host at the proxy, which bans it after five.

use crate::app::Dirigent;
use crate::input::{Submitted, TextInput};
use crate::net::HostStatus;
use crate::providers::host_chip;
use crate::settings::{self, field, primary_button, row, section, text_button};
use crate::theme::{self, text};
use dirigent_protocol::{ClientMessage, Proxy};
use dirigent_service::hosts;
use gpui::{Context, Div, Entity, Focusable, SharedString, div, prelude::*, rgba};

/// Where a CLIProxyAPI listens unless told otherwise: what the address field starts with.
const DEFAULT_URL: &str = "http://127.0.0.1:8317";
/// The key field's placeholder: before the host has a key, and once it does.
const KEY_UNSET: &str = "The proxy's management secret-key";
const KEY_SET: &str = "Saved. Type another to replace it";

/// The tab's state, on `Dirigent`.
pub struct State {
    url: Entity<TextInput>,
    key: Entity<TextInput>,
    /// The shown host's (`providers.host`) proxy as it last answered here, or why it
    /// couldn't take what was saved. `None` until it answers.
    shown: Option<Result<Proxy, String>>,
    /// The request `shown` waits for.
    request: Option<u64>,
    /// That request is a save (or a remove): the host is asking the proxy.
    saving: bool,
}

impl State {
    pub fn new(cx: &mut Context<Dirigent>) -> Self {
        let url = cx.new(|cx| TextInput::new(cx, DEFAULT_URL, true));
        let key = cx.new(|cx| {
            let mut key = TextInput::new(cx, KEY_UNSET, true);
            key.set_secret(true, cx);
            key
        });
        for input in [&url, &key] {
            cx.subscribe(input, |this, _, _: &Submitted, cx| this.save_proxy(cx)).detach();
        }
        Self { url, key, shown: None, request: None, saving: false }
    }
}

impl Dirigent {
    /// Settings on the Proxy tab (the Usage modal's `Set up in Settings`).
    pub fn open_proxy_settings(&mut self, cx: &mut Context<Self>) {
        self.settings.tab = settings::Tab::Proxy;
        self.open_settings(cx);
    }

    /// The tab shows: a host since removed gives way to `local`, the host is asked for its
    /// proxy and the key field takes the keys.
    pub(crate) fn enter_proxy_settings(&mut self, cx: &mut Context<Self>) {
        if self.host(&self.providers.host).is_none() {
            self.providers.host = hosts::LOCAL.to_owned();
            self.providers.list = None;
        }
        self.request_proxy_settings(cx);
        self.focus(self.proxy.key.focus_handle(cx), cx);
    }

    /// Ask the shown host for its proxy (no history): on showing the tab, and when the
    /// host changes or reconnects.
    pub(crate) fn request_proxy_settings(&mut self, cx: &mut Context<Self>) {
        let host = self.providers.host.clone();
        self.proxy.shown = None;
        self.proxy.saving = false;
        self.proxy.request = None;
        if self.connected(&host) {
            self.proxy.request = Some(self.request(&host, |id| ClientMessage::ListProxy { id, since: 0, until: 0, bucket_ms: 1 }));
        }
        self.set_input(&self.proxy.key, "", cx);
    }

    /// `Save`, or Enter in a field: the host takes the address and key and asks the proxy.
    /// An empty key keeps the one the host has.
    pub fn save_proxy(&mut self, cx: &mut Context<Self>) {
        let (url, key) = (self.proxy.url.read(cx).text().trim().to_owned(), self.proxy.key.read(cx).text().trim().to_owned());
        self.set_proxy(url, key, cx);
    }

    /// `Remove`: the host forgets its proxy. The history it sampled stays.
    pub fn remove_proxy(&mut self, cx: &mut Context<Self>) {
        self.set_proxy(String::new(), String::new(), cx);
    }

    fn set_proxy(&mut self, url: String, key: String, cx: &mut Context<Self>) {
        let host = self.providers.host.clone();
        if !self.connected(&host) {
            return self.toast(format!("{host} is offline"), cx);
        }
        self.proxy.request = Some(self.request(&host, |id| ClientMessage::SetProxy { id, url, key }));
        self.proxy.saving = true;
        cx.notify();
    }

    /// A reply to one of the tab's requests (`Err`: an `Error` reply). False when the
    /// request is none of them.
    pub(crate) fn on_proxy_settings(&mut self, reply_to: u64, outcome: Result<Proxy, String>, cx: &mut Context<Self>) -> bool {
        if self.proxy.request.take_if(|r| *r == reply_to).is_none() {
            return false;
        }
        self.proxy.saving = false;
        if let Ok(proxy) = &outcome {
            // What the host holds now: its address in the field, the key never shown.
            self.set_input(&self.proxy.url, if proxy.url.is_empty() { DEFAULT_URL } else { &proxy.url }, cx);
            self.set_input(&self.proxy.key, "", cx);
            let placeholder = if proxy.url.is_empty() { KEY_UNSET } else { KEY_SET };
            self.proxy.key.update(cx, |key, cx| key.set_placeholder(placeholder, cx));
        }
        self.proxy.shown = Some(outcome.map_err(|e| {
            // A service from before `ListProxy`: serde's list of what it does know says little.
            if e.starts_with("bad request: unknown variant") { "This host's service is too old for this; update it".into() } else { e }
        }));
        true
    }
}

/// The tab: the host's chip (with several hosts), the address and key fields, `Save` (and
/// `Remove` once the host has a proxy), and what the host found there.
pub fn render(this: &Dirigent, cx: &mut Context<Dirigent>) -> Vec<Div> {
    let t = theme::t();
    let p = &this.proxy;
    let host = &this.providers.host;
    let note = |text: SharedString, color: u32| div().min_w_0().text_size(text::sm()).text_color(rgba(color)).child(text);
    let set = matches!(&p.shown, Some(Ok(proxy)) if !proxy.url.is_empty());
    let status = match &p.shown {
        _ if p.saving => Some(note("Asking the proxy…".into(), t.fg_2)),
        None if this.host(host).is_none_or(|h| h.status != HostStatus::Connected) => {
            Some(note(format!("{host} is offline").into(), t.fg_2))
        }
        None => Some(note("Loading…".into(), t.fg_2)),
        Some(Err(e)) => Some(note(e.clone().into(), t.error)),
        // None set: the fields say it all.
        Some(Ok(proxy)) if proxy.url.is_empty() => None,
        Some(Ok(Proxy { error: Some(e), .. })) => Some(note(e.clone().into(), t.error)),
        Some(Ok(proxy)) => {
            let accounts = match proxy.accounts.len() {
                1 => "1 account".to_owned(),
                n => format!("{n} accounts"),
            };
            let connected = note(format!("Connected, {accounts}").into(), t.fg_1);
            Some(div().min_w_0().flex().flex_row().items_center().gap_2().child(theme::dot(t.done)).child(connected))
        }
    };
    let actions = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .pt_2()
        .child(primary_button("proxy-save", "Save").on_click(cx.listener(|this, _, _, cx| this.save_proxy(cx))))
        .when(set, |d| d.child(text_button("proxy-remove", "Remove").on_click(cx.listener(|this, _, _, cx| this.remove_proxy(cx)))))
        .children(status.map(|s| s.ml_1()));
    let fields = div()
        .flex()
        .flex_col()
        .children((this.hosts.len() > 1).then(|| row("Host", host_chip(this, cx))))
        .child(row("Address", field(p.url.clone())))
        .child(row("Management key", field(p.key.clone())))
        .child(actions);
    vec![section("CLIProxyAPI", fields)]
}
