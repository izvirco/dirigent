use crate::app::Dirigent;
use crate::icons::icon;
use crate::input::{Submitted, TextInput};
use crate::login::{host_chip, too_old};
use crate::net::HostStatus;
use crate::settings::{self, field, primary_button, row, section, text_button};
use crate::theme::{self, text};
use dirigent_protocol::{ClientMessage, Proxy, ProxyUpdate};
use dirigent_service::hosts;
use gpui::{Context, Div, Entity, Focusable, SharedString, div, prelude::*, rgba};

const DEFAULT_URL: &str = "http://127.0.0.1:8317";
const KEY_UNSET: &str = "One of the proxy's api-keys";
const MANAGEMENT_UNSET: &str = "Optional: its management secret-key, for Usage";
const KEY_SET: &str = "Saved. Type another to replace it";

pub(crate) fn list_without_samples(id: u64) -> ClientMessage {
    ClientMessage::ListProxy { id, since: 0, until: 0, bucket_ms: 1 }
}

pub fn outdated(update: &ProxyUpdate, host: Option<&str>) -> String {
    let on = host.map(|host| format!(" on {host}")).unwrap_or_default();
    format!("CLIProxyAPI {}{on} is outdated: update it to {}", update.running, update.latest)
}

// MUST KILL: type: State's shown / request / saving → one enum of the tab's phase (loading, saving, shown)
pub struct State {
    url: Entity<TextInput>,
    key: Entity<TextInput>,
    management: Entity<TextInput>,
    shown: Option<Result<Proxy, String>>,
    request: Option<u64>,
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
    pub fn open_proxy_settings(&mut self, host: Option<String>, cx: &mut Context<Self>) {
        if let Some(host) = host {
            self.show_host(host);
        }
        self.settings.tab = settings::Tab::Proxy;
        self.open_settings(cx);
    }

    pub(crate) fn enter_proxy_settings(&mut self, cx: &mut Context<Self>) {
        if self.host(&self.login.host).is_none() {
            self.show_host(hosts::LOCAL.to_owned());
        }
        self.request_proxy_settings(cx);
        self.focus(self.proxy.key.focus_handle(cx), cx);
    }

    pub(crate) fn proxy_open(&self) -> bool {
        let tab = self.modal == Some(crate::app::Modal::Settings) && self.settings.tab == settings::Tab::Proxy;
        tab || self.onboarding.as_ref().is_some_and(|s| s.step == crate::onboarding::Step::Proxy)
    }

    pub(crate) fn request_proxy_settings(&mut self, cx: &mut Context<Self>) {
        let host = self.login.host.clone();
        self.proxy.shown = None;
        self.proxy.saving = false;
        self.proxy.request = None;
        if self.connected(&host) {
            self.proxy.request = Some(self.request(&host, list_without_samples));
        }
        for key in [&self.proxy.key, &self.proxy.management] {
            self.set_input(key, "", cx);
        }
    }

    pub fn save_proxy(&mut self, cx: &mut Context<Self>) {
        let typed = |input: &Entity<TextInput>| input.read(cx).text().trim().to_owned();
        let (url, key, management_key) = (typed(&self.proxy.url), typed(&self.proxy.key), typed(&self.proxy.management));
        self.set_proxy(url, key, management_key, cx);
    }

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

    // MUST KILL: rename: on_proxy_settings → take_proxy_reply, false when the reply is none of the tab's
    pub(crate) fn on_proxy_settings(&mut self, reply_to: u64, outcome: Result<Proxy, String>, cx: &mut Context<Self>) -> bool {
        if self.proxy.request.take_if(|r| *r == reply_to).is_none() {
            return false;
        }
        self.proxy.saving = false;
        if outcome.is_ok() {
            let host = self.login.host.clone();
            self.request(&host, |id| ClientMessage::ListModels { id });
        }
        if let Ok(proxy) = &outcome {
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

pub fn render(this: &Dirigent, cx: &mut Context<Dirigent>) -> Vec<Div> {
    let about = div().text_size(text::sm()).text_color(rgba(theme::t().fg_1)).child(
        "Dirigent reaches every model through a CLIProxyAPI you run. Point each host at its proxy: the models it serves are \
         the ones that host's threads can use.",
    );
    vec![section("CLIProxyAPI", theme::stack().gap_3().child(about).child(body(this, cx)))]
}

pub fn body(this: &Dirigent, cx: &mut Context<Dirigent>) -> Div {
    let t = theme::t();
    let p = &this.proxy;
    let host = &this.login.host;
    let note = |text: SharedString, color: u32| div().min_w_0().text_size(text::sm()).text_color(rgba(color)).child(text);
    let count = |n: usize, what: &str| if n == 1 { format!("1 {what}") } else { format!("{n} {what}s") };
    let marked = |mark: gpui::AnyElement, line: Div| {
        let mark = div().w(theme::u(14.)).flex_shrink_0().flex().justify_center().child(mark);
        div().min_w_0().flex().flex_row().items_center().gap_2().child(mark).child(line)
    };
    let set = matches!(&p.shown, Some(Ok(proxy)) if !proxy.url.is_empty());
    let status: Vec<Div> = match &p.shown {
        _ if p.saving => vec![note("Asking the proxy…".into(), t.fg_2)],
        None if this.host(host).is_none_or(|h| h.status != HostStatus::Connected) => {
            vec![note(format!("{host} is offline").into(), t.fg_2)]
        }
        None => vec![note("Loading…".into(), t.fg_2)],
        Some(Err(e)) => vec![note(e.clone().into(), t.error)],
        Some(Ok(proxy)) if proxy.url.is_empty() => vec![],
        Some(Ok(proxy)) => {
            let models = match &proxy.models_error {
                Some(e) => note(e.clone().into(), t.error),
                None => {
                    let connected = note(format!("Connected, {}", count(proxy.models as usize, "model")).into(), t.fg_1);
                    marked(theme::dot(t.done).into_any_element(), connected)
                }
            };
            let accounts = proxy.management_key.then(|| match &proxy.error {
                Some(e) => note(format!("Its accounts can't be read: {e}").into(), t.error),
                None => note(format!("{} behind it, in Usage", count(proxy.accounts.len(), "account")).into(), t.fg_2),
            });
            let update = proxy
                .update
                .as_ref()
                .map(|update| marked(icon("triangle-alert", t.warn).into_any_element(), note(outdated(update, None).into(), t.fg_1)));
            std::iter::once(models).chain(update).chain(accounts).collect()
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
