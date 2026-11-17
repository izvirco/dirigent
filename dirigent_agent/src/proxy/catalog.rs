use super::pricing::Prices;
use super::{Connection, Error, Model, Price, Proxy, now_ms};
use dirigent_protocol::transcript::Api;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const FILE: &str = "models.json";
const REFRESH_AFTER_MS: i64 = 5 * 60 * 1000;
const RETRY_AFTER_MS: i64 = 30 * 1000;

#[derive(Default)]
pub(super) struct Cache {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    catalog: Option<Arc<Catalog>>,
    prices: Option<Arc<Prices>>,
    asked_at: i64,
}

#[derive(Serialize, Deserialize)]
struct Catalog {
    url: String,
    fetched_at: i64,
    models: Vec<Model>,
}

impl Proxy {
    /// The models the proxy serves. What it said last, when that is known (it is then asked
    /// again behind the scenes once that is a few minutes old); else it is asked now.
    pub async fn models(&self) -> anyhow::Result<Vec<Model>> {
        let connection = self.connection()?;
        let Some(catalog) = self.remembered(&connection.url) else { return Ok(self.fetch(&connection).await?) };
        let now = now_ms();
        let due = {
            let mut state = self.inner.catalog.state.lock().unwrap();
            let due = now - catalog.fetched_at > REFRESH_AFTER_MS && now - state.asked_at > RETRY_AFTER_MS;
            if due {
                state.asked_at = now;
            }
            due
        };
        if due {
            let proxy = self.clone();
            tokio::spawn(async move {
                if let Err(e) = proxy.fetch(&connection).await {
                    tracing::debug!("refresh the models: {e}");
                }
            });
        }
        Ok(catalog.models.clone())
    }

    /// Ask the proxy what it serves, now: after its accounts changed, or to try an address
    /// and key that were just entered.
    pub async fn refresh(&self) -> anyhow::Result<Vec<Model>> {
        Ok(self.fetch(&self.connection()?).await?)
    }

    /// The model `id`, or an error that says the proxy doesn't serve it.
    pub async fn model(&self, id: &str) -> anyhow::Result<Model> {
        let find = |models: Vec<Model>| models.into_iter().find(|model| model.id == id);
        if let Some(model) = find(self.models().await?) {
            return Ok(model);
        }
        let connection = self.connection()?;
        let just_asked = self.remembered(&connection.url).is_some_and(|catalog| now_ms() - catalog.fetched_at < 10_000);
        let models = if just_asked { vec![] } else { self.fetch(&connection).await? };
        find(models).ok_or_else(|| Error::other(format!("The proxy does not serve the model {id}.")).into())
    }

    fn remembered(&self, url: &str) -> Option<Arc<Catalog>> {
        let mut state = self.inner.catalog.state.lock().unwrap();
        if state.catalog.as_ref().is_none_or(|catalog| catalog.url != url) {
            let file = std::fs::read(self.inner.dirs.data.join(FILE)).ok();
            let catalog = file.and_then(|bytes| serde_json::from_slice::<Catalog>(&bytes).ok());
            state.catalog = catalog.filter(|catalog| catalog.url == url).map(Arc::new);
        }
        state.catalog.clone()
    }

    async fn fetch(&self, connection: &Connection) -> Result<Vec<Model>, Error> {
        // With `client_version` the proxy answers as the Codex backend does, with what each
        // model can do; without, as OpenAI's API does, with who makes it.
        let (catalog, list, prices) =
            tokio::join!(self.get(connection, "/v1/models?client_version=dirigent"), self.get(connection, "/v1/models"), self.prices());
        self.inner.catalog.state.lock().unwrap().asked_at = now_ms();
        let mut models = models(&catalog?, &list?).ok_or_else(Error::not_a_proxy)?;
        // MUST KILL: extract: fetch's "an empty listing keeps the cached models" rule into a fn of its own
        if models.is_empty() && self.remembered(&connection.url).is_some_and(|catalog| !catalog.models.is_empty()) {
            return Ok(models);
        }
        for model in &mut models {
            model.price = prices.of(&model.id);
        }
        let catalog = Arc::new(Catalog { url: connection.url.clone(), fetched_at: now_ms(), models });
        let dir = &self.inner.dirs.data;
        let written = std::fs::create_dir_all(dir)
            .and_then(|()| std::fs::write(dir.join(FILE), serde_json::to_vec_pretty(&*catalog).unwrap_or_default()));
        if let Err(e) = written {
            tracing::warn!("write {}: {e}", dir.join(FILE).display());
        }
        self.inner.catalog.state.lock().unwrap().catalog = Some(catalog.clone());
        Ok(catalog.models.clone())
    }

    async fn prices(&self) -> Arc<Prices> {
        let remembered = self.inner.catalog.state.lock().unwrap().prices.clone();
        if let Some(prices) = remembered.filter(|prices| prices.is_fresh()) {
            return prices;
        }
        let prices = Arc::new(Prices::load(&self.inner.http, &self.inner.dirs.data).await);
        self.inner.catalog.state.lock().unwrap().prices = Some(prices.clone());
        prices
    }
}

fn models(catalog: &Value, list: &Value) -> Option<Vec<Model>> {
    let entries = |json: &Value| json["models"].as_array().or(json["data"].as_array()).cloned();
    let (catalog, list) = (entries(catalog)?, entries(list)?);
    let owners: HashMap<&str, &str> = list.iter().filter_map(|entry| Some((entry["id"].as_str()?, entry["owned_by"].as_str()?))).collect();
    let model = |entry: &Value| {
        let text = |keys: &[&str]| keys.iter().find_map(|key| entry[key].as_str()).map(str::trim).filter(|text| !text.is_empty());
        let number = |keys: &[&str]| keys.iter().find_map(|key| entry[key].as_u64()).filter(|number| *number > 0);
        let id = text(&["slug", "id"])?;
        if entry["visibility"].as_str().is_some_and(|visibility| visibility.eq_ignore_ascii_case("hide")) {
            return None;
        }
        let owner = owners.get(id).copied().or(entry["owned_by"].as_str()).unwrap_or_default();
        let mut efforts: Vec<String> = vec![];
        for level in entry["supported_reasoning_levels"].as_array().into_iter().flatten() {
            let effort = level["effort"].as_str().or(level.as_str()).unwrap_or_default().trim().to_lowercase();
            if !effort.is_empty() && !efforts.contains(&effort) {
                efforts.push(effort);
            }
        }
        if efforts == ["none"] {
            efforts.clear();
        }
        let modalities = entry["input_modalities"].as_array();
        Some(Model {
            id: id.to_owned(),
            name: text(&["display_name", "name"]).unwrap_or(id).to_owned(),
            owner: owner.to_owned(),
            api: if owner.eq_ignore_ascii_case("anthropic") { Api::Anthropic } else { Api::Codex },
            context_window: number(&["context_window", "max_context_window"]).unwrap_or(128_000),
            max_output: number(&["max_tokens", "max_output_tokens", "max_completion_tokens"]).unwrap_or(16_384),
            efforts,
            images: modalities.is_some_and(|modalities| {
                modalities.iter().any(|modality| modality.as_str().is_some_and(|m| m.eq_ignore_ascii_case("image")))
            }),
            price: Price::default(),
        })
    };
    Some(catalog.iter().filter_map(model).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::Failure;
    use crate::proxy::tests::{Answer, dirs, point_at, serve};
    use serde_json::json;

    fn answers() -> (Value, Value) {
        let level = |effort: &str| json!({"description": "...", "effort": effort});
        let catalog = json!({"models": [
            {"slug": "claude-haiku-5-5", "display_name": "Claude Haiku 5.5", "context_window": 1000000, "max_context_window": 1000000, "max_tokens": 128000,
             "input_modalities": ["text", "image"], "visibility": "list", "default_reasoning_level": "medium",
             "supported_reasoning_levels": [level("low"), level("medium"), level("high"), level("xhigh"), level("max")]},
            {"slug": "claude-haiku-4-5-20251001", "display_name": "Claude 4.5 Haiku", "context_window": 200000, "max_tokens": 64000,
             "input_modalities": ["text", "image"], "visibility": "list", "supported_reasoning_levels": []},
            {"slug": "gpt-5.5", "display_name": "GPT-5.5", "max_context_window": 272000, "input_modalities": ["text"], "visibility": "list",
             "supported_reasoning_levels": ["None", "low", "low"]},
            {"slug": "codex-auto-review", "display_name": "Auto review", "visibility": "hide"},
            {"display_name": "No id"},
        ]});
        let list = json!({"object": "list", "data": [
            {"created": 1784038800, "id": "claude-haiku-5-5", "object": "model", "owned_by": "anthropic"},
            {"created": 1759276800, "id": "claude-haiku-4-5-20251001", "object": "model", "owned_by": "anthropic"},
            {"created": 1784038800, "id": "gpt-5.5", "object": "model", "owned_by": "openai"},
        ]});
        (catalog, list)
    }

    #[test]
    fn the_proxys_answers_become_models() {
        let (catalog, list) = answers();
        let models = models(&catalog, &list).unwrap();
        let efforts = |efforts: &[&str]| efforts.iter().map(|effort| effort.to_string()).collect::<Vec<_>>();
        let haiku = Model {
            id: "claude-haiku-5-5".into(),
            name: "Claude Haiku 5.5".into(),
            owner: "anthropic".into(),
            api: Api::Anthropic,
            context_window: 1_000_000,
            max_output: 128_000,
            efforts: efforts(&["low", "medium", "high", "xhigh", "max"]),
            images: true,
            price: Price::default(),
        };
        let gpt = Model {
            id: "gpt-5.5".into(),
            name: "GPT-5.5".into(),
            owner: "openai".into(),
            api: Api::Codex,
            context_window: 272_000,
            max_output: 16_384,
            efforts: efforts(&["none", "low"]),
            images: false,
            price: Price::default(),
        };
        assert_eq!(models.len(), 3, "the hidden model and the one without an id are left out");
        assert_eq!((&models[0], &models[2]), (&haiku, &gpt));
        assert_eq!((models[1].api, models[1].efforts.len(), models[1].max_output), (Api::Anthropic, 0, 64_000));
        let plain = super::models(&list, &list).unwrap();
        assert_eq!(
            (plain.len(), plain[0].id.as_str(), plain[0].api, plain[0].context_window),
            (3, "claude-haiku-5-5", Api::Anthropic, 128_000)
        );
        assert_eq!(super::models(&json!({"models": []}), &json!({"object": "list", "data": []})), Some(vec![]));
        for other in [json!({}), json!([]), json!({"error": "not found"})] {
            assert_eq!(super::models(&other, &list), None, "{other}");
            assert_eq!(super::models(&catalog, &other), None, "{other}");
        }
    }

    #[tokio::test]
    async fn models_outlive_the_proxy() {
        let says = Arc::new(Mutex::new(answers()));
        let said = says.clone();
        let url = serve(move |path, _| {
            let (catalog, list) = &*said.lock().unwrap();
            match path {
                "/v1/models?client_version=dirigent" => Answer::json(200, catalog),
                "/v1/models" => Answer::json(200, list),
                _ => Answer::json(404, &json!({"error": "not found"})),
            }
        })
        .await;
        let (_tmp, dirs) = dirs();
        std::fs::create_dir_all(&dirs.data).unwrap();
        let prices = json!({"fetched_at": now_ms(), "providers": {"anthropic": {"claude-haiku-5-5": {"input": 0.1, "output": 0.5, "cache_read": 0.01, "cache_write": 0.125}}}});
        std::fs::write(dirs.data.join("prices.json"), prices.to_string()).unwrap();

        point_at(&dirs, &url, "key");
        let proxy = Proxy::new(dirs.clone());
        let models = proxy.models().await.unwrap();
        assert_eq!(
            models.iter().map(|model| model.id.as_str()).collect::<Vec<_>>(),
            ["claude-haiku-5-5", "claude-haiku-4-5-20251001", "gpt-5.5"]
        );
        assert_eq!((models[0].price.input, models[0].price.output, models[2].price), (0.1, 0.5, Price::default()));
        assert_eq!(proxy.model("gpt-5.5").await.unwrap().api, Api::Codex);
        let missing = proxy.model("gpt-9").await.unwrap_err();
        assert_eq!(missing.to_string(), "The proxy does not serve the model gpt-9.");

        let file = std::fs::read(dirs.data.join(FILE)).unwrap();
        *says.lock().unwrap() = (json!({"models": []}), json!({"object": "list", "data": []}));
        assert_eq!(proxy.refresh().await.unwrap(), []);
        *says.lock().unwrap() = (json!({}), json!({}));
        assert_eq!(proxy.refresh().await.unwrap_err().downcast_ref::<Error>(), Some(&Error::not_a_proxy()));
        assert_eq!(proxy.models().await.unwrap(), models);
        assert_eq!(std::fs::read(dirs.data.join(FILE)).unwrap(), file);

        let gone = "http://127.0.0.1:9";
        std::fs::write(dirs.data.join(FILE), std::fs::read_to_string(dirs.data.join(FILE)).unwrap().replace(&url, gone)).unwrap();
        point_at(&dirs, gone, "key");
        let proxy = Proxy::new(dirs.clone());
        assert_eq!(proxy.models().await.unwrap(), models);
        assert_eq!(proxy.model("claude-haiku-5-5").await.unwrap(), models[0]);
        let unreachable = proxy.refresh().await.unwrap_err();
        assert!(matches!(unreachable.downcast_ref::<Error>(), Some(Error { failure: Failure::Transient { .. }, .. })), "{unreachable}");
        point_at(&dirs, "http://127.0.0.1:10", "key");
        assert!(proxy.models().await.is_err());
    }
}
