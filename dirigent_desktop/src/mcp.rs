use crate::app::{Dirigent, Menu, Modal};
use crate::input::{self, Submitted, TextInput};
use crate::login::{Login, action, hint, host_chip, login_panel, too_old};
use crate::menu::{self, Side};
use crate::net::HostStatus;
use crate::settings::{self, field, saved_to, section, switch};
use crate::theme::{self, text, u};
use dirigent_protocol::{ClientMessage, McpServer, McpServerStatus, McpState};
use dirigent_service::hosts;
use gpui::{AnyElement, Context, Div, Entity, Focusable, FontWeight, SharedString, Stateful, div, prelude::*, rgba};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::Path;

const ROW_H: f32 = 38.;

const SUMMARY_CHARS: usize = 300;

// MUST KILL: type: EXPOSURES as an enum with a Default, not a tuple array whose first entry is the default
const EXPOSURES: [(&str, &str); 2] =
    [("codemode", "only scripts call them; declared when codemode is off"), ("direct", "declared to the model, as built-in tools are")];

const RUN_PLACEHOLDER: &str = "npx -y @modelcontextprotocol/server-filesystem ~, or https://…/mcp";
const ENV_PLACEHOLDER: &str = "KEY=value, a line each";
const HEADERS_PLACEHOLDER: &str = "Authorization: Bearer ${TOKEN}, a line each";
const CLIENT_PLACEHOLDER: &str = "Claude Code, if the server refuses unknown clients";

pub struct State {
    // MUST KILL: type: list as an enum (Unlisted / Listed { servers, path } / Failed), not an Option of a Result of a tuple
    list: Option<Result<(Vec<McpServer>, String), String>>,
    list_request: Option<u64>,
    checks: HashMap<String, Check>,
    // MUST KILL: type: changes' (server, failure toast) as a struct, not a tuple
    changes: HashMap<u64, (String, String)>,
    open: Option<String>,
    pub form: Option<Form>,
    name: Entity<TextInput>,
    run: Entity<TextInput>,
    // MUST KILL: rename: pairs -> env_or_headers
    pairs: Entity<TextInput>,
    client: Entity<TextInput>,
    description: Entity<TextInput>,
}

enum Check {
    Running(u64),
    Done(Result<McpServerStatus, String>),
}

pub struct Form {
    editing: Option<McpServer>,
    error: Option<String>,
    // MUST KILL: type: saving's (request id, server name) as a struct, not a tuple
    saving: Option<(u64, String)>,
    // MUST KILL: rename: url -> run_is_url
    url: bool,
}

impl State {
    pub fn new(cx: &mut Context<Dirigent>) -> Self {
        let name = cx.new(|cx| TextInput::new(cx, "filesystem", true));
        let run = cx.new(|cx| TextInput::new(cx, RUN_PLACEHOLDER, true));
        let pairs = cx.new(|cx| TextInput::new(cx, ENV_PLACEHOLDER, false));
        let client = cx.new(|cx| TextInput::new(cx, CLIENT_PLACEHOLDER, true));
        let description = cx.new(|cx| TextInput::new(cx, "What it offers, in a sentence (optional)", true));
        for input in [&name, &run, &pairs, &client, &description] {
            cx.subscribe(input, |this, _, _: &Submitted, cx| this.save_mcp_form(cx)).detach();
            cx.subscribe(input, |this, _, _: &input::Changed, cx| {
                if let Some(form) = this.mcp.form.as_mut().filter(|f| f.error.is_some()) {
                    form.error = None;
                    cx.notify();
                }
            })
            .detach();
        }
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
            client,
            description,
        }
    }

    pub fn inputs(&self) -> [Entity<TextInput>; 5] {
        [self.name.clone(), self.run.clone(), self.pairs.clone(), self.client.clone(), self.description.clone()]
    }

    fn checked_by(&self, id: u64) -> Option<String> {
        self.checks.iter().find(|(_, c)| matches!(c, Check::Running(r) if *r == id)).map(|(name, _)| name.clone())
    }

    fn found(&self, name: &str) -> Option<&Result<McpServerStatus, String>> {
        match self.checks.get(name)? {
            Check::Done(found) => Some(found),
            Check::Running(_) => None,
        }
    }

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
    pub(crate) fn enter_mcp(&mut self, cx: &mut Context<Self>) {
        if self.host(&self.login.host).is_none() {
            self.show_host(hosts::LOCAL.to_owned());
        }
        self.mcp.checks.retain(|_, c| matches!(c, Check::Running(_)));
        self.request_mcp();
        self.focus_mcp(cx);
    }

    pub(crate) fn mcp_open(&self) -> bool {
        self.modal == Some(Modal::Settings) && self.settings.tab == settings::Tab::Mcp
    }

    fn request_mcp(&mut self) {
        let host = self.login.host.clone();
        if self.connected(&host) {
            self.mcp.list_request = Some(self.request(&host, |id| ClientMessage::ListMcpServers { id }));
        }
    }

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
        if host != self.login.host || self.mcp.list_request.take_if(|r| *r == reply_to).is_none() {
            return;
        }
        self.mcp.checks.retain(|name, _| servers.iter().any(|s| s.name == *name));
        self.mcp.list = Some(Ok((servers, path)));
        self.check_servers();
        cx.notify();
    }

    fn check_servers(&mut self) {
        let Some(Ok((servers, _))) = &self.mcp.list else { return };
        let due: Vec<String> = servers
            .iter()
            .filter(|s| s.error.is_none() && enabled(&s.config) && !self.mcp.checks.contains_key(&s.name))
            .map(|s| s.name.clone())
            .collect();
        let host = self.login.host.clone();
        for name in due {
            let id = self.request(&host, |id| ClientMessage::CheckMcpServer { id, name: name.clone() });
            self.mcp.checks.insert(name, Check::Running(id));
        }
    }

    pub fn recheck_mcp(&mut self, name: &str, cx: &mut Context<Self>) {
        self.mcp.checks.remove(name);
        self.check_servers();
        cx.notify();
    }

    pub(crate) fn on_mcp_status(&mut self, host: &str, reply_to: u64, status: McpServerStatus) {
        if host != self.login.host {
            return;
        }
        if let Some(name) = self.mcp.checked_by(reply_to) {
            self.mcp.checks.insert(name, Check::Done(Ok(status)));
        }
    }

    // MUST KILL: rename: on_mcp_reply -> take_mcp_reply, false when the reply is none of the tab's
    pub(crate) fn on_mcp_reply(&mut self, host: &str, reply_to: u64, outcome: Result<(), String>, cx: &mut Context<Self>) -> bool {
        let shown = host == self.login.host;
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
            self.mcp_changed(host, Some(&name), cx);
            return true;
        }
        false
    }

    pub(crate) fn mcp_changed(&mut self, host: &str, name: Option<&str>, cx: &mut Context<Self>) {
        if host != self.login.host {
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

    fn change_mcp(&mut self, name: &str, failure: String, make: impl FnOnce(u64, String) -> ClientMessage, cx: &mut Context<Self>) {
        let host = self.login.host.clone();
        if !self.connected(&host) {
            return self.toast(format!("{host} is offline"), cx);
        }
        let id = self.request(&host, |id| make(id, name.to_owned()));
        self.mcp.changes.insert(id, (name.to_owned(), failure));
        cx.notify();
    }

    fn save_mcp_entry(&mut self, name: &str, config: Value, failure: String, cx: &mut Context<Self>) {
        if !self.connected(&self.login.host) {
            return self.toast(format!("{} is offline", self.login.host), cx);
        }
        if let Some(Ok((servers, _))) = &mut self.mcp.list
            && let Some(s) = servers.iter_mut().find(|s| s.name == name)
        {
            s.config = config.clone();
        }
        self.change_mcp(name, failure, |id, name| ClientMessage::SaveMcpServer { id, name, config }, cx);
    }

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
        let config = with(&server.config, "exposure", (exposure != EXPOSURES[0].0).then(|| json!(exposure)));
        self.save_mcp_entry(&server.name, config, format!("Couldn't change how {}'s tools are reached", server.name), cx);
    }

    pub fn remove_mcp(&mut self, name: &str, cx: &mut Context<Self>) {
        self.change_mcp(name, format!("Couldn't remove {name}"), |id, name| ClientMessage::RemoveMcpServer { id, name }, cx);
    }

    pub fn mcp_logout(&mut self, name: &str, cx: &mut Context<Self>) {
        self.change_mcp(name, format!("Couldn't sign out of {name}"), |id, name| ClientMessage::McpLogout { id, name }, cx);
    }

    pub fn start_mcp_login(&mut self, name: &str, cx: &mut Context<Self>) {
        let host = self.login.host.clone();
        if !self.connected(&host) {
            return self.toast(format!("{host} is offline"), cx);
        }
        self.cancel_login(cx);
        self.drop_mcp_form();
        let request = self.request(&host, |id| ClientMessage::McpLogin { id, name: name.to_owned() });
        self.login.login = Some(Login::new(host, request, name.to_owned()));
        self.focus_mcp(cx);
        cx.notify();
    }

    pub fn toggle_mcp_row(&mut self, name: &str, cx: &mut Context<Self>) {
        self.menu = None;
        self.mcp.open = if self.mcp.open.as_deref() == Some(name) { None } else { Some(name.to_owned()) };
        cx.notify();
    }

    pub fn open_mcp_form(&mut self, editing: Option<McpServer>, cx: &mut Context<Self>) {
        self.menu = None;
        self.drop_mcp_form();
        let (run, pairs, description) = editing.as_ref().map(|s| fields_of(&s.config)).unwrap_or_default();
        let name = editing.as_ref().map(|s| s.name.as_str()).unwrap_or_default();
        let client = editing.as_ref().map(|s| client_name(&s.config)).unwrap_or_default();
        let m = &self.mcp;
        for (input, text) in [(&m.name, name), (&m.run, &run), (&m.pairs, &pairs), (&m.client, client), (&m.description, &description)] {
            self.set_input(input, text, cx);
        }
        let url = is_url(&run);
        self.mcp.pairs.update(cx, |p, cx| p.set_placeholder(pairs_placeholder(url), cx));
        self.mcp.form = Some(Form { editing, error: None, saving: None, url });
        self.focus_mcp(cx);
        cx.notify();
    }

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
            let base = form.editing.as_ref().map(|s| &s.config);
            config_from(base, &text(&m.run), &text(&m.pairs), &text(&m.client), &text(&m.description))
        };
        let host = self.login.host.clone();
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

    pub fn close_mcp_form(&mut self, cx: &mut Context<Self>) {
        if self.mcp.form.is_some() {
            self.drop_mcp_form();
            self.menu = None;
            self.focus_root(cx);
            cx.notify();
        }
    }

    // MUST KILL: rename: drop_mcp_form -> drop_mcp_form_keeping_save, an unanswered save becomes a tracked change
    pub(crate) fn drop_mcp_form(&mut self) {
        if let Some((id, name)) = self.mcp.form.take().and_then(|f| f.saving) {
            self.mcp.changes.insert(id, (name.clone(), format!("Couldn't save {name}")));
        }
    }

    // MUST KILL: rename: mcp_form_escape -> close_mcp_form_on_escape, true when it handled the Esc
    pub fn mcp_form_escape(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.mcp_open() || self.mcp.form.is_none() {
            return false;
        }
        self.close_mcp_form(cx);
        true
    }

    pub(crate) fn focus_mcp(&self, cx: &mut Context<Self>) {
        if !self.mcp_open() {
            return;
        }
        let login = self.login.login.as_ref();
        let m = &self.mcp;
        match (login, &m.form) {
            (Some(l), _) if l.typing() => self.focus(self.login.answer.focus_handle(cx), cx),
            (None, Some(f)) if f.editing.is_some() => self.focus(m.run.focus_handle(cx), cx),
            (None, Some(_)) => self.focus(m.name.focus_handle(cx), cx),
            _ => self.focus_root(cx),
        }
    }
}

fn enabled(config: &Value) -> bool {
    config.get("enabled") != Some(&Value::Bool(false))
}

fn exposure(config: &Value) -> &str {
    config.get("exposure").and_then(Value::as_str).unwrap_or(EXPOSURES[0].0)
}

fn transport(config: &Value) -> String {
    fields_of(config).0
}

fn is_url(run: &str) -> bool {
    let run = run.trim_start();
    run.starts_with("https://") || run.starts_with("http://")
}

fn pairs_placeholder(url: bool) -> &'static str {
    if url { HEADERS_PLACEHOLDER } else { ENV_PLACEHOLDER }
}

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

fn client_name(config: &Value) -> &str {
    config.pointer("/oauth/clientName").and_then(Value::as_str).unwrap_or_default()
}

// MUST KILL: type: fields_of returns a struct (run, pairs, description), not a tuple
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

fn config_from(base: Option<&Value>, run: &str, pairs: &str, client: &str, description: &str) -> Result<Value, String> {
    let mut config = base.and_then(Value::as_object).cloned().unwrap_or_default();
    // MUST KILL: extract: the "type" removal in config_from into a named step that clears the other transport's keys
    config.remove("type");
    let mut oauth = config.get("oauth").and_then(Value::as_object).cloned().unwrap_or_default();
    match client.trim() {
        "" => oauth.remove("clientName"),
        name => oauth.insert("clientName".to_owned(), json!(name)),
    };
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
        set("oauth", nonempty(oauth));
    } else {
        for key in ["url", "headers", "oauth"] {
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

// MUST KILL: rename: split_words -> split_command_line, backslashes stay literal unlike a shell
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

fn join_words(words: &[String]) -> String {
    let quote = |w: &String| {
        if !w.is_empty() && !w.chars().any(|c| c.is_whitespace() || c == '\'' || c == '"') {
            w.clone()
        } else if !w.contains('\'') {
            format!("'{w}'")
        } else if !w.contains('"') {
            format!("\"{w}\"")
        } else {
            format!("'{}'", w.replace('\'', "'\"'\"'"))
        }
    };
    words.iter().map(quote).collect::<Vec<_>>().join(" ")
}

pub fn render(this: &Dirigent, cx: &mut Context<Dirigent>) -> Vec<Div> {
    let mut out = vec![section("MCP servers", body(this, cx))];
    if let Some(Ok((_, path))) = &this.mcp.list {
        out.push(saved_to(Path::new(path)));
    }
    out
}

fn body(this: &Dirigent, cx: &mut Context<Dirigent>) -> Div {
    let host = &this.login.host;
    let login = this.login.login.as_ref();
    let content = match (login, &this.mcp.list) {
        (Some(l), _) => login_panel(l, this, cx),
        (None, None) if !this.connected(host) => hint(format!("{host} is offline")),
        (None, None) => hint("Loading…"),
        (None, Some(Err(e))) => div().text_size(text::sm()).text_color(rgba(theme::t().error)).child(e.clone()),
        (None, Some(Ok((servers, _)))) => list(servers, this, cx),
    };
    let chip = this.hosts.len() > 1 && login.is_none();
    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_3()
        .child(div().text_size(text::sm()).text_color(rgba(theme::t().fg_1)).child("Servers whose tools the agents on this host can use."))
        .children(chip.then(|| host_chip(this, cx)));
    theme::stack().gap_3().child(header).child(content)
}

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
    theme::stack().children(empty).children(rows).children(below)
}

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

// MUST KILL: type: state returns a struct (dot, word, word colour), not a tuple
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
        theme::stack().gap_0p5().children(tools.iter().map(|tool| {
            div()
                .flex()
                .flex_row()
                .gap_2()
                .text_size(text::sm())
                .child(div().flex_shrink_0().font_family(mono.clone()).text_color(rgba(t.fg)).child(tool.name.clone()))
                .children(tool.description.as_deref().map(|d| div().min_w_0().truncate().text_color(rgba(t.fg_2)).child(summary(d))))
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
    theme::stack()
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

/// A tool's description as its row shows it: its first paragraph on one line, cut to more
/// than a row holds at its widest. The row truncates it to its width, but gpui shapes all of
/// the text it is given first, and a description can be pages long.
fn summary(description: &str) -> String {
    let blank = |line: &&str| line.trim().is_empty();
    let words = description.lines().skip_while(blank).take_while(|line| !blank(line)).flat_map(str::split_whitespace);
    words.collect::<Vec<_>>().join(" ").chars().take(SUMMARY_CHARS).collect()
}

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
    theme::stack()
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
        .when(url, |d| d.child(line("Client name", &m.client)))
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
        for w in [vec!["npx", "-y", "x"], vec!["a b", "it's", r#"say "hi""#, r#"it's "x""#, "", r"C:\a b\x"]] {
            let w: Vec<String> = w.into_iter().map(String::from).collect();
            assert_eq!(words(&join_words(&w)), w, "{}", join_words(&w));
        }
        assert_eq!(join_words(&words("npx -y @scope/server")), "npx -y @scope/server");
    }

    #[test]
    fn form_fields_make_an_entry() {
        let c = config_from(None, "npx -y srv ~", "A=1\n\n B = two=2 ", "", "  Files ").unwrap();
        assert_eq!(c, json!({"command": "npx", "args": ["-y", "srv", "~"], "env": {"A": "1", "B": "two=2"}, "description": "Files"}));
        let c = config_from(None, "https://x.dev/mcp", "Authorization: Bearer ${T}", "", "").unwrap();
        assert_eq!(c, json!({"url": "https://x.dev/mcp", "headers": {"Authorization": "Bearer ${T}"}}));
        assert!(config_from(None, " ", "", "", "").is_err());
        assert!(config_from(None, "srv", "no separator", "", "").is_err());
        let base =
            json!({"command": "old", "args": ["x"], "env": {"A": "1"}, "cwd": "/w", "timeout": 5, "exposure": "direct", "type": "stdio"});
        let c = config_from(Some(&base), "new", "", "", "").unwrap();
        assert_eq!(c, json!({"command": "new", "cwd": "/w", "timeout": 5, "exposure": "direct"}));
        let c = config_from(Some(&base), "https://x.dev/mcp", "", "", "").unwrap();
        assert_eq!(c, json!({"url": "https://x.dev/mcp", "timeout": 5, "exposure": "direct"}));
    }

    #[test]
    fn the_client_name_is_the_sign_ins() {
        let url = "https://x.dev/mcp";
        let c = config_from(None, url, "", " Claude Code ", "").unwrap();
        assert_eq!(c, json!({"url": url, "oauth": {"clientName": "Claude Code"}}));
        assert_eq!(client_name(&c), "Claude Code");
        let base = json!({"url": url, "oauth": {"clientName": "Old", "scope": "read"}});
        let c = config_from(Some(&base), url, "", "New", "").unwrap();
        assert_eq!(c, json!({"url": url, "oauth": {"clientName": "New", "scope": "read"}}));
        let c = config_from(Some(&base), url, "", "", "").unwrap();
        assert_eq!(c, json!({"url": url, "oauth": {"scope": "read"}}));
        assert_eq!(config_from(Some(&c), url, "", "", "").unwrap(), c);
        let c = config_from(Some(&json!({"url": url, "oauth": {"clientName": "Old"}})), url, "", "", "").unwrap();
        assert_eq!(c, json!({"url": url}));
        assert_eq!(config_from(Some(&base), "srv", "", "Claude Code", "").unwrap(), json!({"command": "srv"}));
    }

    #[test]
    fn a_tools_row_gets_the_start_of_its_description() {
        assert_eq!(summary("\n  Lists files\r\n  in a folder.\n\nIMPORTANT: pages of rules"), "Lists files in a folder.");
        assert_eq!(summary(&"é".repeat(2 * SUMMARY_CHARS)), "é".repeat(SUMMARY_CHARS));
        assert_eq!(summary(" \n"), "");
    }

    #[test]
    fn entries_fill_the_form_back() {
        let c = json!({"command": "uvx", "args": ["my tool"], "env": {"A": "1"}, "description": "d"});
        assert_eq!(fields_of(&c), ("uvx 'my tool'".into(), "A=1".into(), "d".into()));
        let c = json!({"url": "https://x.dev/mcp", "headers": {"X-Key": "k"}});
        assert_eq!(fields_of(&c), ("https://x.dev/mcp".into(), "X-Key: k".into(), String::new()));
        let (run, pairs, description) = fields_of(&c);
        assert_eq!(config_from(Some(&c), &run, &pairs, client_name(&c), &description).unwrap(), c);
        assert_eq!(fields_of(&json!({})), Default::default());
    }
}
