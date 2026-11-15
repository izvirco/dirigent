//! Settings › MCP servers: the MCP servers a host's pi gives its sessions (its `mcp.json`,
//! which the terminal pi shares), managed as `dirigent pi mcp …` and pi's `/mcp` do in a
//! terminal: add, edit and remove one, turn it off and on, pick how its tools reach the
//! model, and sign in to one that uses OAuth.
//!
//! The host lists the servers without connecting to them; each enabled one is then checked
//! (the host connects to it as a session would, and lets go) and its row says how that went:
//! its tools, a sign-in it wants, that it failed. Clicking a row unfolds it: the server's
//! description, why it failed, its tools, and what can be done to it. A change is saved on
//! the host at once (a switch or the exposure chip shows it before the host confirms), then
//! the list is asked for anew and the changed server checked again. The host unloads its idle
//! sessions, so their next prompt sees the change.
//!
//! The form adds or edits a server: its name, then a command (split into words as a shell
//! would, without running one) or an `http(s)://` URL, the command's environment or the
//! URL's headers (a line each), and a description. An edit keeps the entry's other fields
//! (`cwd`, `timeout`, `oauth`, `toolExposure`…), which only a hand edit of the file sets.
//!
//! The host is Settings › Providers' (`providers.host`, its chip). A sign-in is a login of
//! that tab's (`LoginKind::McpServer`), shown here in place of the list.

use crate::app::{Dirigent, Menu, Modal};
use crate::input::{self, Submitted, TextInput};
use crate::menu::{self, Side};
use crate::net::HostStatus;
use crate::providers::{Login, LoginKind, action, hint, host_chip, login_panel, too_old};
use crate::settings::{self, field, saved_to, section, switch};
use crate::theme::{self, text, u};
use dirigent_protocol::{ClientMessage, McpServer, McpServerStatus, McpState};
use dirigent_service::hosts;
use gpui::{AnyElement, Context, Div, Entity, Focusable, FontWeight, SharedString, Stateful, div, prelude::*, rgba};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::Path;

/// A server row's height, in design px.
const ROW_H: f32 = 38.;

/// How a server's tools can reach the model (pi's `exposure`), with what that means; the
/// first is pi's default.
const EXPOSURES: [(&str, &str); 4] = [
    ("codemode", "called from codemode scripts"),
    ("deferred", "loaded when tool_search finds them"),
    ("direct", "declared to the model, as built-in tools are"),
    ("hidden", "not reachable"),
];

const RUN_PLACEHOLDER: &str = "npx -y @modelcontextprotocol/server-filesystem ~, or https://…/mcp";
const ENV_PLACEHOLDER: &str = "KEY=value, a line each";
const HEADERS_PLACEHOLDER: &str = "Authorization: Bearer ${TOKEN}, a line each";

/// The tab's state, on `Dirigent`.
pub struct State {
    /// The shown host's (`providers.host`) servers and the file they're in, as last listed,
    /// or why listing failed.
    list: Option<Result<(Vec<McpServer>, String), String>>,
    list_request: Option<u64>,
    /// Each server's check, running or done. A server without one gets checked when the
    /// list comes (`check_servers`).
    checks: HashMap<String, Check>,
    /// Changes in flight (a row's switch or exposure, a remove, a sign-out): by request id,
    /// the server and what a toast says if it doesn't go through.
    changes: HashMap<u64, (String, String)>,
    /// The row unfolded.
    open: Option<String>,
    pub form: Option<Form>,
    name: Entity<TextInput>,
    /// The command line, or the URL.
    run: Entity<TextInput>,
    /// The command's environment, or the URL's headers.
    pairs: Entity<TextInput>,
    description: Entity<TextInput>,
}

/// A server's check.
enum Check {
    /// The request, not answered yet.
    Running(u64),
    /// What it found (`Err`: the request failed).
    Done(Result<McpServerStatus, String>),
}

/// The open form.
pub struct Form {
    /// The server it edits, as listed; `None`: a new one.
    editing: Option<McpServer>,
    /// Why the last save didn't go through, until the next edit.
    error: Option<String>,
    /// The save the host hasn't answered yet, and the server's name.
    saving: Option<(u64, String)>,
    /// The command field holds a URL (`is_url`): the labels say so. Kept here rather than
    /// read from the field as the form renders.
    url: bool,
}

impl State {
    pub fn new(cx: &mut Context<Dirigent>) -> Self {
        let name = cx.new(|cx| TextInput::new(cx, "filesystem", true));
        let run = cx.new(|cx| TextInput::new(cx, RUN_PLACEHOLDER, true));
        let pairs = cx.new(|cx| TextInput::new(cx, ENV_PLACEHOLDER, false));
        let description = cx.new(|cx| TextInput::new(cx, "What it offers, in a sentence (optional)", true));
        for input in [&name, &run, &pairs, &description] {
            // Enter saves (ctrl+Enter in the multiline one).
            cx.subscribe(input, |this, _, _: &Submitted, cx| this.save_mcp_form(cx)).detach();
            cx.subscribe(input, |this, _, _: &input::Changed, cx| {
                if let Some(form) = this.mcp.form.as_mut().filter(|f| f.error.is_some()) {
                    form.error = None;
                    cx.notify();
                }
            })
            .detach();
        }
        // The form's labels and the second field's placeholder follow what's typed: a command
        // or a URL.
        cx.subscribe(&run, |this, run, _: &input::Changed, cx| {
            let url = is_url(run.read(cx).text());
            if let Some(form) = this.mcp.form.as_mut().filter(|f| f.url != url) {
                form.url = url;
                this.mcp.pairs.update(cx, |p, cx| p.set_placeholder(pairs_placeholder(url), cx));
                cx.notify();
            }
        })
        .detach();
        Self {
            list: None,
            list_request: None,
            checks: HashMap::new(),
            changes: HashMap::new(),
            open: None,
            form: None,
            name,
            run,
            pairs,
            description,
        }
    }

    pub fn inputs(&self) -> [Entity<TextInput>; 4] {
        [self.name.clone(), self.run.clone(), self.pairs.clone(), self.description.clone()]
    }

    /// The server whose check is request `id`, still running.
    fn checked_by(&self, id: u64) -> Option<String> {
        self.checks.iter().find(|(_, c)| matches!(c, Check::Running(r) if *r == id)).map(|(name, _)| name.clone())
    }

    /// What the last check of server `name` found, once it is done.
    fn found(&self, name: &str) -> Option<&Result<McpServerStatus, String>> {
        match self.checks.get(name)? {
            Check::Done(found) => Some(found),
            Check::Running(_) => None,
        }
    }

    /// Another host shows: nothing of the last one's stays (answers from it are dropped).
    pub(crate) fn reset(&mut self) {
        self.list = None;
        self.list_request = None;
        self.checks.clear();
        self.changes.clear();
        self.open = None;
        self.form = None;
    }
}

impl Dirigent {
    /// The tab shows (or its host changed): a host since removed gives way to `local`, the
    /// servers are listed and checked anew (what one says may have changed since; checks
    /// still running stay), and the keys go where the tab wants them.
    pub(crate) fn enter_mcp(&mut self, cx: &mut Context<Self>) {
        if self.host(&self.providers.host).is_none() {
            self.show_host(hosts::LOCAL.to_owned());
        }
        self.mcp.checks.retain(|_, c| matches!(c, Check::Running(_)));
        self.request_mcp();
        self.focus_mcp(cx);
    }

    /// Settings › MCP servers is showing.
    pub(crate) fn mcp_open(&self) -> bool {
        self.modal == Some(Modal::Settings) && self.settings.tab == settings::Tab::Mcp
    }

    /// Ask the shown host for its servers.
    fn request_mcp(&mut self) {
        let host = self.providers.host.clone();
        if self.connected(&host) {
            self.mcp.list_request = Some(self.request(&host, |id| ClientMessage::ListMcpServers { id }));
        }
    }

    /// The shown host connected (asked again) or dropped (no answer is coming).
    pub(crate) fn mcp_status(&mut self, status: HostStatus, cx: &mut Context<Self>) {
        let m = &mut self.mcp;
        if status == HostStatus::Disconnected {
            m.list_request = None;
            m.checks.retain(|_, c| matches!(c, Check::Done(_)));
            m.changes.clear();
            if let Some(form) = m.form.as_mut().filter(|f| f.saving.is_some()) {
                form.saving = None;
                form.error = Some("The connection to the host dropped".into());
            }
        }
        if status == HostStatus::Connected && self.mcp_open() {
            self.mcp.checks.clear();
            self.request_mcp();
        }
        cx.notify();
    }

    pub(crate) fn on_mcp_servers(&mut self, host: &str, reply_to: u64, servers: Vec<McpServer>, path: String, cx: &mut Context<Self>) {
        if host != self.providers.host || self.mcp.list_request.take_if(|r| *r == reply_to).is_none() {
            return;
        }
        // A server gone (removed, renamed by hand) takes its check along.
        self.mcp.checks.retain(|name, _| servers.iter().any(|s| s.name == *name));
        self.mcp.list = Some(Ok((servers, path)));
        self.check_servers();
        cx.notify();
    }

    /// Check every enabled server pi takes that has no check yet.
    fn check_servers(&mut self) {
        let Some(Ok((servers, _))) = &self.mcp.list else { return };
        let due: Vec<String> = servers
            .iter()
            .filter(|s| s.error.is_none() && enabled(&s.config) && !self.mcp.checks.contains_key(&s.name))
            .map(|s| s.name.clone())
            .collect();
        let host = self.providers.host.clone();
        for name in due {
            let id = self.request(&host, |id| ClientMessage::CheckMcpServer { id, name: name.clone() });
            self.mcp.checks.insert(name, Check::Running(id));
        }
    }

    /// `Check again`: connect to the server anew.
    pub fn recheck_mcp(&mut self, name: &str, cx: &mut Context<Self>) {
        self.mcp.checks.remove(name);
        self.check_servers();
        cx.notify();
    }

    pub(crate) fn on_mcp_status(&mut self, host: &str, reply_to: u64, status: McpServerStatus) {
        if host != self.providers.host {
            return;
        }
        // An answer to a check since dropped (asked again) is stale.
        if let Some(name) = self.mcp.checked_by(reply_to) {
            self.mcp.checks.insert(name, Check::Done(Ok(status)));
        }
    }

    /// The reply to one of the tab's requests (`Err`: an `Error` reply). False when the
    /// request is none of them.
    pub(crate) fn on_mcp_reply(&mut self, host: &str, reply_to: u64, outcome: Result<(), String>, cx: &mut Context<Self>) -> bool {
        let shown = host == self.providers.host;
        let m = &mut self.mcp;
        if shown && m.list_request.take_if(|r| *r == reply_to).is_some() {
            if let Err(e) = outcome {
                m.list = Some(Err(too_old(e)));
            }
            return true;
        }
        if let Some(name) = m.checked_by(reply_to).filter(|_| shown) {
            if let Err(e) = outcome {
                m.checks.insert(name, Check::Done(Err(too_old(e))));
            }
            return true;
        }
        if let Some(form) = m.form.as_mut().filter(|f| f.saving.as_ref().is_some_and(|(id, _)| *id == reply_to)) {
            let (_, name) = form.saving.take().unwrap();
            match outcome {
                Ok(()) => {
                    self.close_mcp_form(cx);
                    self.mcp_changed(host, Some(&name), cx);
                }
                Err(e) => form.error = Some(too_old(e)),
            }
            cx.notify();
            return true;
        }
        if let Some((name, failure)) = m.changes.remove(&reply_to) {
            if let Err(e) = outcome {
                self.toast(format!("{failure}: {}", too_old(e)), cx);
            }
            // Also after a failure: a switch flipped ahead of the host goes back.
            self.mcp_changed(host, Some(&name), cx);
            return true;
        }
        false
    }

    /// Server `name` changed on `host` (saved, removed, signed in or out): the list anew,
    /// and the server checked again. `None`: one this side doesn't know, so all of them.
    pub(crate) fn mcp_changed(&mut self, host: &str, name: Option<&str>, cx: &mut Context<Self>) {
        if host != self.providers.host {
            return;
        }
        match name {
            Some(name) => {
                self.mcp.checks.remove(name);
            }
            None => self.mcp.checks.clear(),
        }
        self.request_mcp();
        cx.notify();
    }

    /// Send the shown host a change to server `name` (`make` builds it around an id and the
    /// name); `failure` is what a toast calls it if it doesn't go through.
    fn change_mcp(&mut self, name: &str, failure: String, make: impl FnOnce(u64, String) -> ClientMessage, cx: &mut Context<Self>) {
        let host = self.providers.host.clone();
        if !self.connected(&host) {
            return self.toast(format!("{host} is offline"), cx);
        }
        let id = self.request(&host, |id| make(id, name.to_owned()));
        self.mcp.changes.insert(id, (name.to_owned(), failure));
        cx.notify();
    }

    /// A row's change of the server's entry: shown at once, the host's list confirms it.
    fn save_mcp_entry(&mut self, name: &str, config: Value, failure: String, cx: &mut Context<Self>) {
        if !self.connected(&self.providers.host) {
            return self.toast(format!("{} is offline", self.providers.host), cx);
        }
        if let Some(Ok((servers, _))) = &mut self.mcp.list
            && let Some(s) = servers.iter_mut().find(|s| s.name == name)
        {
            s.config = config.clone();
        }
        self.change_mcp(name, failure, |id, name| ClientMessage::SaveMcpServer { id, name, config }, cx);
    }

    /// The row's switch: pi connects to the server, or leaves it be.
    pub fn toggle_mcp(&mut self, server: &McpServer, cx: &mut Context<Self>) {
        let on = !enabled(&server.config);
        let config = with(&server.config, "enabled", (!on).then_some(Value::Bool(false)));
        let failure = format!("Couldn't turn {} {}", server.name, if on { "on" } else { "off" });
        self.save_mcp_entry(&server.name, config, failure, cx);
    }

    pub fn set_mcp_exposure(&mut self, server: &McpServer, exposure: &str, cx: &mut Context<Self>) {
        self.menu = None;
        if exposure == self::exposure(&server.config) {
            return cx.notify();
        }
        // pi's default goes unsaid, as pi writes it.
        let config = with(&server.config, "exposure", (exposure != EXPOSURES[0].0).then(|| json!(exposure)));
        self.save_mcp_entry(&server.name, config, format!("Couldn't change how {}'s tools are reached", server.name), cx);
    }

    pub fn remove_mcp(&mut self, name: &str, cx: &mut Context<Self>) {
        self.change_mcp(name, format!("Couldn't remove {name}"), |id, name| ClientMessage::RemoveMcpServer { id, name }, cx);
    }

    pub fn mcp_logout(&mut self, name: &str, cx: &mut Context<Self>) {
        self.change_mcp(name, format!("Couldn't sign out of {name}"), |id, name| ClientMessage::McpLogout { id, name }, cx);
    }

    /// `Sign in`: the host runs the server's OAuth sign-in, a login shown in the list's place.
    pub fn start_mcp_login(&mut self, name: &str, cx: &mut Context<Self>) {
        let host = self.providers.host.clone();
        if !self.connected(&host) {
            return self.toast(format!("{host} is offline"), cx);
        }
        // One login at a time: one left running on another tab goes. The form would hide
        // under the sign-in (and take an Esc meant for it).
        self.cancel_login(cx);
        self.drop_mcp_form();
        let request = self.request(&host, |id| ClientMessage::McpLogin { id, name: name.to_owned() });
        self.providers.login = Some(Login::new(host, request, name.to_owned(), LoginKind::McpServer));
        self.focus_mcp(cx);
        cx.notify();
    }

    /// A row's click: unfold it, or fold it again.
    pub fn toggle_mcp_row(&mut self, name: &str, cx: &mut Context<Self>) {
        self.menu = None;
        self.mcp.open = if self.mcp.open.as_deref() == Some(name) { None } else { Some(name.to_owned()) };
        cx.notify();
    }

    /// `Add server`, or an unfolded row's `Edit`: the form, empty or filled in.
    pub fn open_mcp_form(&mut self, editing: Option<McpServer>, cx: &mut Context<Self>) {
        self.menu = None;
        self.drop_mcp_form();
        let (run, pairs, description) = editing.as_ref().map(|s| fields_of(&s.config)).unwrap_or_default();
        let name = editing.as_ref().map(|s| s.name.as_str()).unwrap_or_default();
        let m = &self.mcp;
        for (input, text) in [(&m.name, name), (&m.run, &run), (&m.pairs, &pairs), (&m.description, &description)] {
            self.set_input(input, text, cx);
        }
        let url = is_url(&run);
        self.mcp.pairs.update(cx, |p, cx| p.set_placeholder(pairs_placeholder(url), cx));
        self.mcp.form = Some(Form { editing, error: None, saving: None, url });
        self.focus_mcp(cx);
        cx.notify();
    }

    /// Enter or Save: the host saves the form's server, or what's wrong shows.
    pub fn save_mcp_form(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &self.mcp.form else { return };
        if form.saving.is_some() {
            return;
        }
        let text = |input: &Entity<TextInput>| input.read(cx).text().to_owned();
        let m = &self.mcp;
        let name = match &form.editing {
            Some(s) => s.name.clone(),
            None => text(&m.name).trim().to_owned(),
        };
        let listed = match &m.list {
            Some(Ok((servers, _))) => servers.iter().any(|s| s.name == name),
            _ => false,
        };
        let made = if name.is_empty() {
            Err("Give the server a name".to_owned())
        } else if form.editing.is_none() && listed {
            Err(format!("There's already a server named {name}"))
        } else {
            config_from(form.editing.as_ref().map(|s| &s.config), &text(&m.run), &text(&m.pairs), &text(&m.description))
        };
        let host = self.providers.host.clone();
        let made = made.and_then(|config| if self.connected(&host) { Ok(config) } else { Err(format!("{host} is offline")) });
        match made {
            Ok(config) => {
                let id = self.request(&host, |id| ClientMessage::SaveMcpServer { id, name: name.clone(), config });
                if let Some(form) = &mut self.mcp.form {
                    form.saving = Some((id, name));
                }
            }
            Err(e) => {
                if let Some(form) = &mut self.mcp.form {
                    form.error = Some(e);
                }
            }
        }
        cx.notify();
    }

    /// Cancel, Esc or a save: the form goes.
    pub fn close_mcp_form(&mut self, cx: &mut Context<Self>) {
        if self.mcp.form.is_some() {
            self.drop_mcp_form();
            self.menu = None;
            self.focus_root(cx);
            cx.notify();
        }
    }

    /// The form goes; a save of it the host hasn't answered becomes a change like a row's,
    /// so its answer still refreshes the list (or says why it failed).
    pub(crate) fn drop_mcp_form(&mut self) {
        if let Some((id, name)) = self.mcp.form.take().and_then(|f| f.saving) {
            self.mcp.changes.insert(id, (name.clone(), format!("Couldn't save {name}")));
        }
    }

    /// Esc: closes the form if the tab shows one (and says so; else the modal closes).
    pub fn mcp_form_escape(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.mcp_open() || self.mcp.form.is_none() {
            return false;
        }
        self.close_mcp_form(cx);
        true
    }

    /// Keys go where the tab wants them: a sign-in's question, else the form's first field
    /// (the name, or the command of a server being edited), else the modal, so Esc closes it.
    pub(crate) fn focus_mcp(&self, cx: &mut Context<Self>) {
        if !self.mcp_open() {
            return;
        }
        let login = self.providers.login.as_ref().filter(|l| l.kind == LoginKind::McpServer);
        let m = &self.mcp;
        match (login, &m.form) {
            (Some(l), _) if l.typing() => self.focus(self.providers.answer.focus_handle(cx), cx),
            (None, Some(f)) if f.editing.is_some() => self.focus(m.run.focus_handle(cx), cx),
            (None, Some(_)) => self.focus(m.name.focus_handle(cx), cx),
            _ => self.focus_root(cx),
        }
    }
}

/// Pi connects to it: `enabled` isn't false.
fn enabled(config: &Value) -> bool {
    config.get("enabled") != Some(&Value::Bool(false))
}

/// How its tools reach the model: `exposure`, pi's default when unsaid.
fn exposure(config: &Value) -> &str {
    config.get("exposure").and_then(Value::as_str).unwrap_or(EXPOSURES[0].0)
}

/// The command line or URL it runs as.
fn transport(config: &Value) -> String {
    fields_of(config).0
}

/// What's typed in the command field is a URL: the server is an HTTP one.
fn is_url(run: &str) -> bool {
    let run = run.trim_start();
    run.starts_with("https://") || run.starts_with("http://")
}

fn pairs_placeholder(url: bool) -> &'static str {
    if url { HEADERS_PLACEHOLDER } else { ENV_PLACEHOLDER }
}

/// `config` with `key` set to `value`, or gone (`None`).
fn with(config: &Value, key: &str, value: Option<Value>) -> Value {
    let mut config = config.clone();
    if let Some(obj) = config.as_object_mut() {
        match value {
            Some(v) => obj.insert(key.to_owned(), v),
            None => obj.remove(key),
        };
    }
    config
}

/// The form's fields for an entry: its command line (or URL), its environment (or headers),
/// its description. What isn't of the right type shows as nothing.
fn fields_of(config: &Value) -> (String, String, String) {
    let str_of = |key: &str| config.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
    let pairs = |key: &str, sep: &str| {
        let map = config.get(key).and_then(Value::as_object);
        map.into_iter().flatten().filter_map(|(k, v)| Some(format!("{k}{sep}{}", v.as_str()?))).collect::<Vec<_>>().join("\n")
    };
    let description = str_of("description");
    if config.get("url").is_some() {
        return (str_of("url"), pairs("headers", ": "), description);
    }
    let args = config.get("args").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned);
    let words: Vec<String> = std::iter::once(str_of("command")).chain(args).collect();
    (if words[0].is_empty() { String::new() } else { join_words(&words) }, pairs("env", "="), description)
}

/// The entry the form's fields make, from `base` (an edit's: its other fields stay), or
/// what's wrong with them. pi checks the rest when the host saves it.
fn config_from(base: Option<&Value>, run: &str, pairs: &str, description: &str) -> Result<Value, String> {
    let mut config = base.and_then(Value::as_object).cloned().unwrap_or_default();
    // Implied by `command` or `url`; one left from the other kind would contradict it.
    config.remove("type");
    let mut set = |key: &str, value: Option<Value>| match value {
        Some(v) => config.insert(key.to_owned(), v),
        None => config.remove(key),
    };
    let nonempty = |map: Map<String, Value>| (!map.is_empty()).then(|| Value::Object(map));
    let run = run.trim();
    if run.is_empty() {
        return Err("Enter the command that runs the server, or its URL".into());
    }
    if is_url(run) {
        for key in ["command", "args", "env", "cwd"] {
            set(key, None);
        }
        set("url", Some(json!(run)));
        set("headers", nonempty(parse_pairs(pairs, ':', "Header", "Name: value")?));
    } else {
        for key in ["url", "headers", "oauth", "auth"] {
            set(key, None);
        }
        let mut words = split_words(run)?.into_iter();
        set("command", words.next().map(Value::String));
        let args: Vec<Value> = words.map(Value::String).collect();
        set("args", (!args.is_empty()).then_some(Value::Array(args)));
        set("env", nonempty(parse_pairs(pairs, '=', "Environment", "KEY=value")?));
    }
    let description = description.trim();
    set("description", (!description.is_empty()).then(|| json!(description)));
    Ok(Value::Object(config))
}

/// `KEY=value` / `Name: value` lines (split at the first `sep`) as an object; blank lines
/// are skipped. `what` and `shape` name them in an error.
fn parse_pairs(text: &str, sep: char, what: &str, shape: &str) -> Result<Map<String, Value>, String> {
    let mut map = Map::new();
    for (i, line) in text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
        match line.split_once(sep) {
            Some((k, v)) if !k.trim().is_empty() => map.insert(k.trim().to_owned(), json!(v.trim())),
            _ => return Err(format!("{what} line {} isn't {shape}", i + 1)),
        };
    }
    Ok(map)
}

/// A command line's words, as a shell splits them (nothing is run or expanded): whitespace
/// parts them, quotes ('…' or "…") keep what they hold as it is and join the text around
/// them. Backslashes are plain characters, so Windows paths need no quoting.
fn split_words(line: &str) -> Result<Vec<String>, String> {
    let mut words = vec![];
    let mut word: Option<String> = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' => {
                let word = word.get_or_insert_default();
                loop {
                    match chars.next() {
                        Some(q) if q == c => break,
                        Some(ch) => word.push(ch),
                        None => return Err(format!("A {c} isn't closed")),
                    }
                }
            }
            c if c.is_whitespace() => words.extend(word.take()),
            c => word.get_or_insert_default().push(c),
        }
    }
    words.extend(word);
    Ok(words)
}

/// Words as a command line `split_words` gives them back from, quoted where they need it.
fn join_words(words: &[String]) -> String {
    let quote = |w: &String| {
        if !w.is_empty() && !w.chars().any(|c| c.is_whitespace() || c == '\'' || c == '"') {
            w.clone()
        } else if !w.contains('\'') {
            format!("'{w}'")
        } else if !w.contains('"') {
            format!("\"{w}\"")
        } else {
            // Both quotes: single-quoted runs, each ' between them double-quoted.
            format!("'{}'", w.replace('\'', "'\"'\"'"))
        }
    };
    words.iter().map(quote).collect::<Vec<_>>().join(" ")
}

/// The tab: what it's for and the host chip, then the servers, or a sign-in in progress.
pub fn render(this: &Dirigent, cx: &mut Context<Dirigent>) -> Vec<Div> {
    let mut out = vec![section("MCP servers", body(this, cx))];
    if let Some(Ok((_, path))) = &this.mcp.list {
        out.push(saved_to(Path::new(path)));
    }
    out
}

fn body(this: &Dirigent, cx: &mut Context<Dirigent>) -> Div {
    let host = &this.providers.host;
    let login = this.providers.login.as_ref().filter(|l| l.kind == LoginKind::McpServer);
    let content = match (login, &this.mcp.list) {
        (Some(l), _) => login_panel(l, this, cx),
        (None, None) if !this.connected(host) => hint(format!("{host} is offline")),
        (None, None) => hint("Loading…"),
        (None, Some(Err(e))) => div().text_size(text::sm()).text_color(rgba(theme::t().error)).child(e.clone()),
        (None, Some(Ok((servers, _)))) => list(servers, this, cx),
    };
    // Not during a sign-in: it is the host's it started on.
    let chip = this.hosts.len() > 1 && login.is_none();
    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_3()
        .child(div().text_size(text::sm()).text_color(rgba(theme::t().fg_1)).child("Servers whose tools the agents on this host can use."))
        .children(chip.then(|| host_chip(this, cx)));
    div().flex().flex_col().gap_3().child(header).child(content)
}

/// The rows (the unfolded one with its details under it, the edited one as the form), then
/// `Add server`, or the form adding one.
fn list(servers: &[McpServer], this: &Dirigent, cx: &mut Context<Dirigent>) -> Div {
    let m = &this.mcp;
    let form = m.form.as_ref();
    let editing = form.and_then(|f| f.editing.as_ref()).map(|s| s.name.as_str());
    let mut rows: Vec<AnyElement> = vec![];
    for s in servers {
        match form {
            Some(f) if editing == Some(s.name.as_str()) => rows.push(form_card(this, f, cx).into_any_element()),
            _ => {
                rows.push(row(this, s, cx).into_any_element());
                if m.open.as_ref() == Some(&s.name) {
                    rows.push(details(this, s, cx).into_any_element());
                }
            }
        }
    }
    // A new server's form, or one whose server left the file meanwhile, goes under the rows.
    let in_rows = editing.is_some_and(|e| servers.iter().any(|s| s.name == e));
    let below =
        match form {
            Some(f) if !in_rows => Some(form_card(this, f, cx).mt_2()),
            Some(_) => None,
            None => Some(div().flex().flex_row().pt_3().child(
                theme::button("mcp-add", "plus", "Add server").on_click(cx.listener(|this, _, _, cx| this.open_mcp_form(None, cx))),
            )),
        };
    let empty = (servers.is_empty() && form.is_none()).then(|| hint("No MCP servers yet."));
    div().flex().flex_col().children(empty).children(rows).children(below)
}

/// `● name   command or URL   state   [Sign in]   (switch)`: one line, whatever its state.
fn row(this: &Dirigent, s: &McpServer, cx: &mut Context<Dirigent>) -> Stateful<Div> {
    let t = theme::t();
    let (dot, word, color) = state(&this.mcp, s);
    let open = this.mcp.open.as_ref() == Some(&s.name);
    let needs_auth = matches!(this.mcp.found(&s.name), Some(Ok(c)) if c.state == McpState::NeedsAuth);
    let (name, sign_in, server) = (s.name.clone(), s.name.clone(), s.clone());
    div()
        .id(SharedString::from(format!("mcp/{name}")))
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .h(u(ROW_H))
        .px_2()
        .rounded_md()
        .cursor_pointer()
        .when(open, |d| d.bg(rgba(t.hover)))
        .hover(|d| d.bg(rgba(t.hover)))
        .on_click(cx.listener(move |this, _, _, cx| this.toggle_mcp_row(&name, cx)))
        .child(theme::dot(dot))
        .child(div().w(u(120.)).flex_shrink_0().truncate().font_weight(FontWeight::MEDIUM).child(s.name.clone()))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_family(this.fonts.mono.clone())
                .text_size(text::sm())
                .text_color(rgba(t.fg_1))
                .child(transport(&s.config)),
        )
        .child(div().flex_shrink_0().text_size(text::sm()).text_color(rgba(color)).child(word))
        .children(needs_auth.then(|| {
            action(format!("mcp-sign-in/{}", s.name), "Sign in", None).on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.start_mcp_login(&sign_in, cx)
            }))
        }))
        .child(switch(SharedString::from(format!("mcp-on/{}", s.name)), enabled(&s.config), cx, move |this, cx| {
            cx.stop_propagation();
            this.toggle_mcp(&server, cx)
        }))
}

/// The row's dot colour, its state word, and the word's colour.
fn state(m: &State, s: &McpServer) -> (u32, String, u32) {
    let t = theme::t();
    if s.error.is_some() {
        return (t.error, "invalid".into(), t.error);
    }
    if !enabled(&s.config) {
        return (t.line, "off".into(), t.fg_2);
    }
    if matches!(m.checks.get(&s.name), Some(Check::Running(_))) {
        return (t.warn, "checking…".into(), t.fg_2);
    }
    match m.found(&s.name) {
        Some(Ok(c)) => match c.state {
            McpState::Connected => {
                let tools = match c.tools.len() {
                    1 => "1 tool".to_owned(),
                    n => format!("{n} tools"),
                };
                (t.done, tools, t.fg_2)
            }
            McpState::NeedsAuth => (t.warn, "needs sign-in".into(), t.warn),
            McpState::Failed | McpState::Other => (t.error, "failed".into(), t.error),
        },
        Some(Err(_)) => (t.error, "check failed".into(), t.error),
        None => (t.line, String::new(), t.fg_2),
    }
}

/// An unfolded row: the server's description, why it doesn't work, its tools, then how they
/// reach the model and what can be done to it.
fn details(this: &Dirigent, s: &McpServer, cx: &mut Context<Dirigent>) -> Div {
    let t = theme::t();
    let mono = this.fonts.mono.clone();
    let check = this.mcp.found(&s.name);
    let description = s.config.get("description").and_then(Value::as_str).map(str::to_owned);
    let error = s.error.clone().or_else(|| match check? {
        Ok(c) => c.error.clone(),
        Err(e) => Some(e.clone()),
    });
    let tools = match check {
        Some(Ok(c)) if c.state == McpState::Connected => c.tools.as_slice(),
        _ => &[],
    };
    let tool_list = (!tools.is_empty()).then(|| {
        div().flex().flex_col().gap_0p5().children(tools.iter().map(|tool| {
            div()
                .flex()
                .flex_row()
                .gap_2()
                .text_size(text::sm())
                .child(div().flex_shrink_0().font_family(mono.clone()).text_color(rgba(t.fg)).child(tool.name.clone()))
                .children(tool.description.clone().map(|d| div().min_w_0().truncate().text_color(rgba(t.fg_2)).child(d)))
        }))
    });
    let name = s.name.clone();
    let can_check = s.error.is_none() && enabled(&s.config);
    let mut actions: Vec<AnyElement> = vec![exposure_chip(this, s, cx).into_any_element()];
    let (edit, recheck, sign_out, remove) = (s.clone(), name.clone(), name.clone(), name.clone());
    actions.push(
        action(format!("mcp-edit/{name}"), "Edit", None)
            .on_click(cx.listener(move |this, _, _, cx| this.open_mcp_form(Some(edit.clone()), cx)))
            .into_any_element(),
    );
    if can_check {
        actions.push(
            action(format!("mcp-check/{name}"), "Check again", None)
                .on_click(cx.listener(move |this, _, _, cx| this.recheck_mcp(&recheck, cx)))
                .into_any_element(),
        );
    }
    if s.signed_in {
        actions.push(
            action(format!("mcp-sign-out/{name}"), "Sign out", Some("Delete the credentials the sign-in stored".into()))
                .on_click(cx.listener(move |this, _, _, cx| this.mcp_logout(&sign_out, cx)))
                .into_any_element(),
        );
    }
    actions.push(
        action(format!("mcp-remove/{name}"), "Remove", None)
            .on_click(cx.listener(move |this, _, _, cx| this.remove_mcp(&remove, cx)))
            .into_any_element(),
    );
    div()
        .flex()
        .flex_col()
        .gap_3()
        .mt_1()
        .mb_2()
        .p_3()
        .rounded_md()
        .border_1()
        .border_color(rgba(t.line))
        .bg(rgba(t.raised))
        .children(description.map(|d| div().text_size(text::sm()).text_color(rgba(t.fg_1)).child(d)))
        .children(error.map(|e| div().font_family(mono.clone()).text_size(text::sm()).text_color(rgba(t.error)).child(e)))
        .children(tool_list)
        .child(div().flex().flex_row().flex_wrap().items_center().gap_1p5().children(actions))
}

/// `Tools  codemode ▾`: how the server's tools reach the model, changeable.
fn exposure_chip(this: &Dirigent, s: &McpServer, cx: &mut Context<Dirigent>) -> Stateful<Div> {
    let open = this.menu == Some(Menu::McpExposure);
    let current = exposure(&s.config).to_owned();
    let menu = open.then(|| {
        let items = EXPOSURES
            .into_iter()
            .map(|(e, what)| {
                let server = s.clone();
                menu::item(format!("mcp-exposure/{e}"), e, what, e == current, move |this, cx| this.set_mcp_exposure(&server, e, cx), cx)
            })
            .collect();
        menu::popup(Side::Below(26.), items, cx)
    });
    theme::labeled_chip(format!("mcp-exposure/{}", s.name), "Tools", current.clone(), false, open)
        .tooltip(theme::tooltip("How the server's tools reach the model"))
        .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(Menu::McpExposure, cx)))
        .children(menu)
}

/// The form: its title, the fields (labelled for a command or a URL, as typed), then what's
/// wrong (if anything) beside Cancel and Save.
fn form_card(this: &Dirigent, f: &Form, cx: &mut Context<Dirigent>) -> Div {
    let t = theme::t();
    let m = &this.mcp;
    let url = f.url;
    let title = match &f.editing {
        Some(s) => format!("Edit {}", s.name),
        None => "New MCP server".to_owned(),
    };
    let line = |label: &'static str, input: &Entity<TextInput>| {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .child(div().w(u(100.)).flex_shrink_0().text_color(rgba(t.fg_1)).child(label))
            .child(field(input.clone()).flex_1())
    };
    let note = match (&f.error, &f.saving) {
        (Some(e), _) => Some(div().text_color(rgba(t.error)).child(e.clone())),
        (None, Some(_)) => Some(div().text_color(rgba(t.fg_2)).child("Saving…")),
        _ => None,
    };
    let save = if f.editing.is_some() { "Save" } else { "Add server" };
    let footer = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .pt_1()
        .child(div().flex_1().min_w_0().text_size(text::sm()).children(note))
        .child(settings::text_button("mcp-form-cancel", "Cancel").on_click(cx.listener(|this, _, _, cx| this.close_mcp_form(cx))))
        .child(settings::primary_button("mcp-form-save", save).on_click(cx.listener(|this, _, _, cx| this.save_mcp_form(cx))));
    div()
        .flex()
        .flex_col()
        .gap_2p5()
        .p_4()
        .rounded_lg()
        .border_1()
        .border_color(rgba(t.line))
        .bg(rgba(t.raised))
        .child(div().font_weight(FontWeight::MEDIUM).child(title))
        .when(f.editing.is_none(), |d| d.child(line("Name", &m.name)))
        .child(line(if url { "URL" } else { "Command" }, &m.run))
        .child(line(if url { "Headers" } else { "Environment" }, &m.pairs))
        .child(line("Description", &m.description))
        .child(footer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<String> {
        split_words(line).unwrap()
    }

    #[test]
    fn command_lines_split_and_join() {
        assert_eq!(words("  npx -y  @scope/server ~/notes "), ["npx", "-y", "@scope/server", "~/notes"]);
        assert_eq!(words(r#"uvx 'my tool' --name="a b" C:\bin\x.exe"#), ["uvx", "my tool", "--name=a b", r"C:\bin\x.exe"]);
        assert_eq!(words("a '' b"), ["a", "", "b"]);
        assert!(split_words("say 'hi").is_err());
        // What joining quotes comes back as it was.
        for w in [vec!["npx", "-y", "x"], vec!["a b", "it's", r#"say "hi""#, r#"it's "x""#, "", r"C:\a b\x"]] {
            let w: Vec<String> = w.into_iter().map(String::from).collect();
            assert_eq!(words(&join_words(&w)), w, "{}", join_words(&w));
        }
        assert_eq!(join_words(&words("npx -y @scope/server")), "npx -y @scope/server");
    }

    #[test]
    fn form_fields_make_an_entry() {
        let c = config_from(None, "npx -y srv ~", "A=1\n\n B = two=2 ", "  Files ").unwrap();
        assert_eq!(c, json!({"command": "npx", "args": ["-y", "srv", "~"], "env": {"A": "1", "B": "two=2"}, "description": "Files"}));
        let c = config_from(None, "https://x.dev/mcp", "Authorization: Bearer ${T}", "").unwrap();
        assert_eq!(c, json!({"url": "https://x.dev/mcp", "headers": {"Authorization": "Bearer ${T}"}}));
        assert!(config_from(None, " ", "", "").is_err());
        assert!(config_from(None, "srv", "no separator", "").is_err());
        // An edit keeps what the form doesn't show, but not the other kind's fields.
        let base =
            json!({"command": "old", "args": ["x"], "env": {"A": "1"}, "cwd": "/w", "timeout": 5, "exposure": "direct", "type": "stdio"});
        let c = config_from(Some(&base), "new", "", "").unwrap();
        assert_eq!(c, json!({"command": "new", "cwd": "/w", "timeout": 5, "exposure": "direct"}));
        let c = config_from(Some(&base), "https://x.dev/mcp", "", "").unwrap();
        assert_eq!(c, json!({"url": "https://x.dev/mcp", "timeout": 5, "exposure": "direct"}));
    }

    #[test]
    fn entries_fill_the_form_back() {
        let c = json!({"command": "uvx", "args": ["my tool"], "env": {"A": "1"}, "description": "d"});
        assert_eq!(fields_of(&c), ("uvx 'my tool'".into(), "A=1".into(), "d".into()));
        let c = json!({"url": "https://x.dev/mcp", "headers": {"X-Key": "k"}});
        assert_eq!(fields_of(&c), ("https://x.dev/mcp".into(), "X-Key: k".into(), String::new()));
        let (run, pairs, description) = fields_of(&c);
        assert_eq!(config_from(Some(&c), &run, &pairs, &description).unwrap(), c);
        assert_eq!(fields_of(&json!({})), Default::default());
    }
}
