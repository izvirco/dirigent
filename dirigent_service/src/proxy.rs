use crate::Service;
use crate::now;
use anyhow::Context;
use dirigent_agent::config::Config;
use dirigent_protocol::{Proxy, ProxyAccount, ProxyLimit, ProxySample};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use tracing::{info, warn};

pub const SAMPLE_MS: i64 = 5 * 60_000;
const KEEP_MS: i64 = 35 * 24 * 60 * 60_000;
pub const FRESH_MS: i64 = 10_000;
const TIMEOUT: Duration = Duration::from_secs(10);
/// Anthropic's rate-limit headers: `<prefix><limit>-utilization` and `<prefix><limit>-reset`.
const ANTHROPIC: &str = "anthropic-ratelimit-unified-";
/// Codex's: `<prefix><slot>-used-percent` and so on, for the slots `primary` and `secondary`.
const CODEX: &str = "x-codex-";

fn path() -> PathBuf {
    crate::data_dir().join("proxy.json")
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct File {
    url: String,
    key: String,
}

impl File {
    fn read(path: &Path) -> Option<File> {
        let text = std::fs::read_to_string(path).ok()?;
        static WARNED: std::sync::Once = std::sync::Once::new();
        serde_json::from_str(&text).inspect_err(|e| WARNED.call_once(|| warn!("{}: {e} (no proxy until fixed)", path.display()))).ok()
    }

    fn save(path: &Path, url: &str, key: &str) -> anyhow::Result<()> {
        let url = url.trim().trim_end_matches('/');
        if url.is_empty() {
            match std::fs::remove_file(path) {
                Ok(()) => info!("proxy: management key removed"),
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e).with_context(|| format!("remove {}", path.display())),
                Err(_) => {}
            }
            return Ok(());
        }
        anyhow::ensure!(url.starts_with("http://") || url.starts_with("https://"), "The address starts with http:// or https://");
        let held = File::read(path).filter(|held| held.url == url).map(|held| held.key);
        let key = Some(key.trim().to_owned()).filter(|k| !k.is_empty()).or(held);
        let file = File { url: url.to_owned(), key: key.context("The proxy's management key is needed")? };
        crate::hosts::write_private(path, &serde_json::to_string_pretty(&file)?).with_context(|| format!("write {}", path.display()))?;
        info!(url, "proxy: management key set");
        Ok(())
    }
}

#[derive(Default)]
pub struct Poller {
    state: Mutex<State>,
    busy: tokio::sync::Mutex<()>,
}

#[derive(Default)]
struct State {
    accounts: Vec<ProxyAccount>,
    error: Option<String>,
    asked_at: i64,
    refused: bool,
}

struct Failure {
    message: String,
    refused: bool,
}

impl Service {
    pub(crate) async fn poll_proxy(&self, max_age: i64) {
        let _busy = self.proxy.busy.lock().await;
        {
            let state = self.proxy.state.lock().unwrap();
            if state.refused || now() - state.asked_at < max_age {
                return;
            }
        }
        let Some(File { url, key }) = File::read(&path()) else { return };
        let result = tokio::task::spawn_blocking(move || fetch(&url, &key))
            .await
            .unwrap_or_else(|e| Err(Failure { message: format!("{e}"), refused: false }));
        let at = now();
        if let Ok(accounts) = &result {
            let samples: Vec<(&str, &str, f64)> =
                accounts.iter().flat_map(|a| a.limits.iter().map(move |l| (a.id.as_str(), l.name.as_str(), l.used_at(at)))).collect();
            if let Err(e) = self.storage.lock().unwrap().add_proxy_samples(at - at % SAMPLE_MS, &samples, at - KEEP_MS) {
                warn!("proxy: keeping the limits: {e:#}");
            }
        }
        let mut state = self.proxy.state.lock().unwrap();
        state.asked_at = at;
        match result {
            Ok(accounts) => {
                if state.error.take().is_some() {
                    info!("proxy: answering again");
                }
                state.accounts = accounts;
            }
            Err(f) => {
                if state.error.as_deref() != Some(&f.message) {
                    warn!("proxy: {}", f.message);
                }
                state.refused = f.refused;
                state.error = Some(f.message);
            }
        }
    }

    pub(crate) async fn set_proxy(&self, url: &str, key: &str, management_key: &str) -> anyhow::Result<()> {
        let url = url.trim().trim_end_matches('/');
        anyhow::ensure!(
            url.is_empty() || url.starts_with("http://") || url.starts_with("https://"),
            "The address starts with http:// or https://"
        );
        Config::update(&self.agent.dirs, |config| {
            let held = (config.proxy.url == url).then(|| std::mem::take(&mut config.proxy.key));
            let key = Some(key.trim().to_owned()).filter(|k| !k.is_empty()).or(held).filter(|_| !url.is_empty());
            anyhow::ensure!(url.is_empty() || key.is_some(), "The proxy's API key is needed");
            config.proxy = dirigent_agent::config::Proxy { url: url.to_owned(), key: key.unwrap_or_default() };
            Ok(())
        })?;
        info!(url, "proxy set");
        let held = File::read(&path()).is_some_and(|held| held.url == url);
        File::save(&path(), if management_key.trim().is_empty() && !held { "" } else { url }, management_key)?;
        *self.proxy.state.lock().unwrap() = State::default();
        self.poll_proxy(0).await;
        Ok(())
    }

    pub(crate) async fn proxy(&self, samples: Vec<ProxySample>, ask: bool) -> Proxy {
        let config = match Config::load(&self.agent.dirs) {
            Ok(config) => config.proxy,
            Err(e) => return Proxy { models_error: Some(format!("{e:#}")), ..Proxy::default() },
        };
        let served = match (config.url.is_empty(), ask) {
            (true, _) => Ok(vec![]),
            (false, true) => self.models.refresh().await,
            (false, false) => self.models.models().await,
        };
        let (models, models_error) = match served {
            Ok(models) => (models.len() as u32, None),
            Err(e) => (0, Some(format!("{e:#}"))),
        };
        let management_key = File::read(&path()).is_some_and(|file| file.url == config.url);
        let state = self.proxy.state.lock().unwrap();
        Proxy {
            url: config.url,
            key: !config.key.is_empty(),
            management_key,
            models,
            models_error,
            error: state.error.clone(),
            accounts: state.accounts.clone(),
            samples,
        }
    }
}

fn fetch(url: &str, key: &str) -> Result<Vec<ProxyAccount>, Failure> {
    let fail = |message: String| Failure { message, refused: false };
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .user_agent(format!("Dirigent/{}", dirigent_protocol::release::VERSION))
            .timeout_global(Some(TIMEOUT))
            // The proxy says why in the body.
            .http_status_as_error(false)
            .build(),
    );
    let mut res = agent
        .get(format!("{url}/v8/management/credentials"))
        .header("Authorization", format!("Bearer {key}"))
        .call()
        .map_err(|e| fail(format!("Can't reach the proxy at {url}: {e}")))?;
    let status = res.status().as_u16();
    let body: Value = res.body_mut().read_to_string().ok().and_then(|b| serde_json::from_str(&b).ok()).unwrap_or_default();
    let said = body.get("error").and_then(Value::as_str).unwrap_or("no reason given");
    match status {
        200 => accounts(&body).ok_or_else(|| fail(format!("{url} doesn't answer like a CLIProxyAPI"))),
        401 => Err(Failure { message: format!("The proxy turned the management key down: {said}"), refused: true }),
        // No management routes at all: a proxy without a management key has none.
        404 => Err(fail(format!("No management API at {url}: the proxy needs a `management.secret-key` in its config"))),
        _ => Err(fail(format!("The proxy answered {status}: {said}"))),
    }
}

fn accounts(body: &Value) -> Option<Vec<ProxyAccount>> {
    Some(body.get("files")?.as_array()?.iter().filter_map(account).collect())
}

fn account(file: &Value) -> Option<ProxyAccount> {
    let text = |key: &str| file.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let flag = |key: &str| file.get(key).and_then(Value::as_bool).unwrap_or(false);
    // The latest reply's limits, then each model's latest: a limit only some models' replies
    // carry (`7d_oi`) stays, as of the newest reply that had it.
    let snapshots =
        file.get("quota").into_iter().chain(file.get("model_quotas").and_then(Value::as_object).into_iter().flat_map(|m| m.values()));
    let mut seen: Vec<(i64, ProxyLimit)> = vec![];
    for snapshot in snapshots {
        let at = snapshot.get("observed_at").and_then(time).unwrap_or(0);
        for limit in limits(snapshot.get("signals"), at) {
            match seen.iter_mut().find(|(_, l)| l.name == limit.name) {
                Some(old) if old.0 >= at => {}
                Some(old) => *old = (at, limit),
                None => seen.push((at, limit)),
            }
        }
    }
    let mut limits: Vec<ProxyLimit> = seen.into_iter().map(|(_, l)| l).collect();
    limits.sort_by(|a, b| a.name.cmp(&b.name));
    Some(ProxyAccount {
        id: text("id")?.to_owned(),
        provider: text("provider").unwrap_or_default().to_owned(),
        name: text("email").or(text("label")).or(text("id"))?.to_owned(),
        disabled: flag("disabled"),
        sign_in: flag("requires_sign_in"),
        // The proxy calls an account that ran out of quota an error too: that is its limits' to show.
        error: (text("status") == Some("error") && !flag("quota_blocked"))
            .then(|| text("status_message").map(reason).unwrap_or_else(|| "It is failing".into())),
        limits,
    })
}

/// Anthropic names its limits: `…-5h-utilization` is a share, `…-5h-reset` unix seconds.
/// Codex has two slots, `primary` and `secondary`, each saying how long its window is
/// (`…-window-minutes`): the 5 hours and the week on Plus, the week alone on the bigger
/// plans. A slot's use is a percent, its reset a time (unix seconds) or seconds after the
/// reply. Its limits are named by their length here (`5h`, `7d`), as Anthropic's are. The
/// limits Codex keeps per model on the side (`x-codex-<model>-…`) are left out.
fn limits(signals: Option<&Value>, at: i64) -> Vec<ProxyLimit> {
    let headers: Vec<(String, &str)> =
        signals.and_then(Value::as_object).into_iter().flatten().filter_map(|(k, v)| Some((k.to_lowercase(), v.as_str()?))).collect();
    let number = |name: String| headers.iter().find(|(k, _)| *k == name)?.1.parse::<f64>().ok().filter(|n| n.is_finite());
    let anthropic = headers.iter().filter_map(|(key, _)| {
        let name = key.strip_prefix(ANTHROPIC)?.strip_suffix("-utilization")?;
        let resets_at = number(format!("{ANTHROPIC}{name}-reset")).map(|s| s as i64 * 1000);
        Some(ProxyLimit { name: name.to_owned(), used: number(key.clone())?, resets_at })
    });
    let codex = ["primary", "secondary"].into_iter().filter_map(|slot| {
        let field = |name: &str| number(format!("{CODEX}{slot}-{name}"));
        let minutes = field("window-minutes")? as i64;
        let after = field("reset-after-seconds").filter(|_| at > 0).map(|s| at + s as i64 * 1000);
        let resets_at = field("reset-at").map(|s| s as i64 * 1000).or(after);
        Some(ProxyLimit { name: span(minutes), used: field("used-percent")? / 100., resets_at })
    });
    anthropic.chain(codex).collect()
}

fn span(minutes: i64) -> String {
    match minutes {
        m if m % 1440 == 0 => format!("{}d", m / 1440),
        m if m % 60 == 0 => format!("{}h", m / 60),
        m => format!("{m}m"),
    }
}

fn time(v: &Value) -> Option<i64> {
    Some(chrono::DateTime::parse_from_rfc3339(v.as_str()?).ok()?.timestamp_millis())
}

/// A credential's status message for people: the provider's own words when the proxy kept
/// its whole error reply (`{"error": {"message": ...}}`).
fn reason(message: &str) -> String {
    let inner = serde_json::from_str::<Value>(message).ok().and_then(|v| Some(v.pointer("/error/message")?.as_str()?.to_owned()));
    inner.unwrap_or_else(|| message.chars().take(300).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_key_is_kept_private_and_for_its_address() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.json");
        let file = |url: &str, key: &str| Some(File { url: url.into(), key: key.into() });
        assert_eq!(File::read(&path), None);
        assert!(File::save(&path, "http://p:1", "").is_err());
        assert!(File::save(&path, "p:1", "k").is_err());
        File::save(&path, " http://p:1/ ", " k1 ").unwrap();
        assert_eq!(File::read(&path), file("http://p:1", "k1"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        File::save(&path, "http://p:1", "").unwrap();
        assert!(File::save(&path, "http://q:1", "").is_err());
        assert_eq!(File::read(&path), file("http://p:1", "k1"));
        File::save(&path, "http://q:1", "k2").unwrap();
        assert_eq!(File::read(&path), file("http://q:1", "k2"));
        File::save(&path, "", "").unwrap();
        File::save(&path, "", "").unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn reads_the_proxys_accounts() {
        let rate_limited = "{\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"Try again later.\"}}";
        let limited = json!({
            "id": "a@example.com.json", "provider": "claude", "email": "a@example.com", "label": "a@example.com",
            "status": "error", "disabled": false, "requires_sign_in": false, "unavailable": true, "quota_blocked": true,
            "status_message": rate_limited,
            "quota": {"observed_at": "2026-11-10T05:36:59.153986554Z", "signals": {
                "Anthropic-Ratelimit-Unified-5h-Reset": "1794292800",
                "Anthropic-Ratelimit-Unified-5h-Status": "rejected",
                "Anthropic-Ratelimit-Unified-5h-Utilization": "1.0",
                "Anthropic-Ratelimit-Unified-7d-Reset": "1794304800",
                "Anthropic-Ratelimit-Unified-7d-Utilization": "0.31",
                "Anthropic-Ratelimit-Unified-Fallback-Percentage": "0.5",
                "Retry-After": "3780",
            }},
            "model_quotas": {"claude-fable-5-2": {"observed_at": "2026-11-10T05:00:00Z", "signals": {
                "Anthropic-Ratelimit-Unified-7d-Reset": "1794304800",
                "Anthropic-Ratelimit-Unified-7d-Utilization": "0.25",
                "Anthropic-Ratelimit-Unified-7d_oi-Reset": "1794304800",
                "Anthropic-Ratelimit-Unified-7d_oi-Utilization": "0.42",
            }}},
        });
        let idle =
            json!({"id": "b.json", "provider": "claude", "label": "b", "status": "active", "cooldowns": [], "quota": {"signals": {}}});
        let failing =
            json!({"id": "c.json", "provider": "codex", "email": "c@example.com", "status": "error", "status_message": "token expired"});
        let plus = json!({"id": "d.json", "provider": "codex", "email": "d@example.com", "status": "active",
        "quota": {"observed_at": "2026-11-10T05:00:00Z", "signals": {
            "X-Codex-Plan-Type": "plus",
            "X-Codex-Primary-Used-Percent": "3",
            "X-Codex-Primary-Window-Minutes": "300",
            "X-Codex-Primary-Reset-After-Seconds": "10148",
            "X-Codex-Primary-Reset-At": "1794300000",
            "X-Codex-Secondary-Used-Percent": "63",
            "X-Codex-Secondary-Window-Minutes": "10080",
            "X-Codex-Secondary-Reset-After-Seconds": "60",
            "X-Codex-Bengalfox-Limit-Name": "GPT-5.3-Codex-Spark",
            "X-Codex-Bengalfox-Primary-Used-Percent": "35",
            "X-Codex-Bengalfox-Primary-Window-Minutes": "300",
        }}});
        let pro = json!({"id": "e.json", "provider": "codex", "email": "e@example.com", "status": "active",
        "quota": {"observed_at": "2026-11-10T05:00:00Z", "signals": {
            "X-Codex-Plan-Type": "pro",
            "X-Codex-Primary-Used-Percent": "51",
            "X-Codex-Primary-Window-Minutes": "10080",
            "X-Codex-Primary-Reset-At": "1794800000",
        }}});
        let got = accounts(&json!({"files": [limited, idle, failing, plus, pro, {"provider": "claude"}]})).unwrap();
        let limit = |name: &str, used: f64, resets_at: i64| ProxyLimit { name: name.into(), used, resets_at: Some(resets_at) };
        assert_eq!(
            got,
            [
                ProxyAccount {
                    id: "a@example.com.json".into(),
                    provider: "claude".into(),
                    name: "a@example.com".into(),
                    limits: vec![
                        limit("5h", 1., 1_794_292_800_000),
                        limit("7d", 0.31, 1_794_304_800_000),
                        limit("7d_oi", 0.42, 1_794_304_800_000)
                    ],
                    ..ProxyAccount::default()
                },
                ProxyAccount { id: "b.json".into(), provider: "claude".into(), name: "b".into(), ..ProxyAccount::default() },
                ProxyAccount {
                    id: "c.json".into(),
                    provider: "codex".into(),
                    name: "c@example.com".into(),
                    error: Some("token expired".into()),
                    ..ProxyAccount::default()
                },
                ProxyAccount {
                    id: "d.json".into(),
                    provider: "codex".into(),
                    name: "d@example.com".into(),
                    limits: vec![limit("5h", 0.03, 1_794_300_000_000), limit("7d", 0.63, 1_794_286_860_000)],
                    ..ProxyAccount::default()
                },
                ProxyAccount {
                    id: "e.json".into(),
                    provider: "codex".into(),
                    name: "e@example.com".into(),
                    limits: vec![limit("7d", 0.51, 1_794_800_000_000)],
                    ..ProxyAccount::default()
                },
            ]
        );
        assert_eq!([span(300), span(10080), span(90)], ["5h", "7d", "90m"]);
        assert_eq!(accounts(&json!({"endpoints": []})), None);
        assert_eq!(reason(rate_limited), "Try again later.");
    }
}
