use super::config::{self, Reach, Server};
use super::oauth::{Login, Store, Tokens};
use crate::config::Dirs;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Default, PartialEq)]
pub struct Imported {
    /// pi's servers as entries of `agent.toml` (`Config::mcp`), for the caller to save.
    pub servers: BTreeMap<String, Server>,
    /// What couldn't be taken as it was, a line each for the user, who reads them one by
    /// one: an entry left out, a setting that is gone, a sign-in to do again.
    pub notes: Vec<String>,
}

/// Read what pi's agent dir (`<config dir>/agent`) has of MCP servers: the entries of its
/// `mcp.json`, and with `sign_ins` their sign-ins, which go from its `mcp-auth.json` into
/// ours (one of ours already there stays). Nothing about it is fatal: what can't be taken
/// is a note.
///
/// `sign_ins` is for the install the pi dir is of, and no other. A test instance that
/// starts from the real install's files would refresh its copy of a sign-in, and an
/// authorization server that gives out a new refresh token each time then takes the old
/// one no more: the real install is signed out.
pub fn import_pi(agent_dir: &Path, dirs: &Dirs, sign_ins: bool) -> Imported {
    let mut imported = Imported::default();
    let Some(file) = read(&agent_dir.join("mcp.json"), &mut imported.notes) else { return imported };
    let Some(entries) = file.get("mcpServers").and_then(Value::as_object) else { return imported };
    let mut servers = BTreeMap::new();
    for (name, entry) in entries {
        if let Some(server) = server_of(name, entry, &mut imported.notes) {
            servers.insert(name.clone(), server);
        }
    }
    for (name, server, reach) in config::checked(&servers) {
        match reach {
            Ok(_) => drop(imported.servers.insert(name.to_owned(), Server { kind: None, ..server.clone() })),
            Err(why) => imported.notes.push(format!("MCP server {name} is left out: {why}")),
        }
    }
    if sign_ins && let Some(Value::Object(states)) = read(&agent_dir.join("mcp-auth.json"), &mut imported.notes) {
        store_sign_ins(&states, dirs, &mut imported);
    }
    imported
}

fn read(path: &Path, notes: &mut Vec<String>) -> Option<Value> {
    let parsed = match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => Err(e.to_string()),
    };
    parsed.map_err(|e| notes.push(format!("{} not read: {e}", path.display()))).ok()
}

fn server_of(name: &str, entry: &Value, notes: &mut Vec<String>) -> Option<Server> {
    let mut fields = entry.as_object().cloned().unwrap_or_default();
    if let Some(provider) = fields.remove("auth") {
        let provider = provider.get("provider").and_then(Value::as_str).unwrap_or_default().to_owned();
        notes.push(format!(
            "MCP server {name} is left out: it sent the credentials of pi's provider \"{provider}\", and there are no providers any more"
        ));
        return None;
    }
    if fields.remove("toolExposure").is_some() {
        notes.push(format!("MCP server {name}: its tools all are reached the way the server is set to now (it had settings per tool)"));
    }
    match fields.get("exposure").and_then(Value::as_str) {
        Some("deferred") => {
            notes.push(format!(
                "MCP server {name}: its tools are called from codemode scripts now (they were found with tool_search, which is gone)"
            ));
            fields.remove("exposure");
        }
        Some("hidden") => {
            notes.push(format!("MCP server {name} is switched off (its tools were hidden from the model and its scripts)"));
            fields.remove("exposure");
            fields.insert("enabled".into(), json!(false));
        }
        // An older name of the default.
        Some("codemode-deferred") => drop(fields.remove("exposure")),
        _ => {}
    }
    if let Some(seconds) = fields.get("timeout").and_then(Value::as_f64) {
        fields.insert("timeout".into(), json!(seconds.ceil().max(0.) as u64));
    }
    if let Some(Value::Object(oauth)) = fields.get_mut("oauth")
        && oauth.remove("clientRegistration").is_some_and(|how| how == "cimd")
    {
        notes.push(format!(
            "MCP server {name}: a sign-in registers the app with the server now (it was told to sign in as pi's client id document)"
        ));
    }
    serde_json::from_value(Value::Object(fields)).map_err(|e| notes.push(format!("MCP server {name} is left out: {}", wrong(&e)))).ok()
}

// MUST KILL: rename: `wrong` → `error_without_value` (a note must never quote an entry's value: it may be a token)
fn wrong(error: &serde_json::Error) -> String {
    let message = error.to_string();
    match message.rsplit_once(", expected ") {
        Some((_, expected)) => format!("expected {expected}"),
        None => message,
    }
}

/// The sign-ins of the imported servers, from pi's `mcp-auth.json` (`states`: by server
/// name and address, or by the address alone if an older pi wrote it).
fn store_sign_ins(states: &serde_json::Map<String, Value>, dirs: &Dirs, imported: &mut Imported) {
    let store = Store::new(dirs);
    for (name, server) in &imported.servers {
        let Ok(Reach::Http(url)) = server.reach(name) else { continue };
        let key = Store::key(name, &url);
        let Some(state) = states.get(&key).or_else(|| states.get(url.as_str())).filter(|_| server.uses_oauth()) else { continue };
        let take = |at: &str| state.pointer(at).cloned();
        let Some(tokens) = take("/tokens").and_then(|tokens| serde_json::from_value::<Tokens>(tokens).ok()) else { continue };
        // Milliseconds there.
        let expires_at = state.get("tokensExpireAt").and_then(Value::as_f64).map(|at| (at / 1000.) as u64);
        let client = take("/clientInformation").and_then(|client| serde_json::from_value(client).ok());
        // Tokens without a client that was registered for them are pi's own client's
        // (its client id document), unless the entry names theirs.
        if client.is_none() && server.oauth.as_ref().is_none_or(|oauth| oauth.client_id.is_none()) {
            imported
                .notes
                .push(format!("MCP server {name}: sign in again (Settings › MCP servers): pi was signed in under its own name there"));
            continue;
        }
        let login = Login {
            client,
            tokens: Some(Tokens { expires_at, ..tokens }),
            auth_server: take("/discovery/authorizationServerMetadata").and_then(|server| serde_json::from_value(server).ok()),
            resource: state.pointer("/discovery/resourceMetadata/resource").and_then(Value::as_str).map(str::to_owned),
        };
        let stored = store.update(&key, |ours| {
            if *ours == Login::default() {
                *ours = login;
            }
        });
        if let Err(e) = stored {
            imported.notes.push(format!("MCP server {name}: sign in again (Settings › MCP servers): {e:#}"));
        }
    }
}
