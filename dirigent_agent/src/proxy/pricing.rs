use super::{Price, Tier};
use dirigent_protocol::transcript::{Cost, Usage};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

const SOURCE: &str = "https://models.dev/api.json";
const FILE: &str = "prices.json";
const FRESH_MS: i64 = 24 * 60 * 60 * 1000;

pub(super) fn cost(price: &Price, usage: &Usage, cache_write_1h: u64) -> Cost {
    let (input, output, cache_read, cache_write) = match price.long {
        Some(tier) if usage.prompt() > tier.above => (tier.input, tier.output, tier.cache_read, tier.cache_write),
        _ => (price.input, price.output, price.cache_read, price.cache_write),
    };
    let dollars = |per_million: f64, tokens: u64| per_million * tokens as f64 / 1e6;
    let hour = cache_write_1h.min(usage.cache_write);
    Cost {
        input: dollars(input, usage.input),
        output: dollars(output, usage.output),
        cache_read: dollars(cache_read, usage.cache_read),
        // The listed rate is for five minutes; Anthropic charges twice the input rate for an hour.
        cache_write: dollars(cache_write, usage.cache_write - hour) + dollars(input * 2.0, hour),
    }
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct Prices {
    pub fetched_at: i64,
    providers: BTreeMap<String, BTreeMap<String, Price>>,
}

impl Prices {
    pub async fn load(http: &reqwest::Client, dir: &Path) -> Self {
        let file = dir.join(FILE);
        let stale = match std::fs::read(&file).ok().and_then(|bytes| serde_json::from_slice::<Prices>(&bytes).ok()) {
            Some(prices) if prices.is_fresh() => return prices,
            stale => stale,
        };
        match fetch(http).await {
            Ok(prices) => {
                let written =
                    std::fs::create_dir_all(dir).and_then(|()| std::fs::write(&file, serde_json::to_vec(&prices).unwrap_or_default()));
                if let Err(e) = written {
                    tracing::warn!("write {}: {e}", file.display());
                }
                prices
            }
            Err(e) => {
                tracing::warn!("fetch prices from {SOURCE}: {e:#}");
                stale.unwrap_or_default()
            }
        }
    }

    pub fn is_fresh(&self) -> bool {
        super::now_ms() - self.fetched_at < FRESH_MS
    }

    fn from_models_dev(json: &Value) -> Self {
        let mut providers = BTreeMap::new();
        for (provider, entry) in json.as_object().into_iter().flatten() {
            let models = entry["models"].as_object().into_iter().flatten();
            let prices: BTreeMap<String, Price> =
                models.filter_map(|(id, model)| Some((id.to_lowercase(), price(&model["cost"])?))).collect();
            if !prices.is_empty() {
                providers.insert(provider.clone(), prices);
            }
        }
        Self { fetched_at: super::now_ms(), providers }
    }

    pub fn of(&self, id: &str) -> Price {
        let id = id.trim().to_lowercase();
        let bare = strip_namespace(&id);
        let mut trusted: Vec<&str> = namespace(&id).into_iter().collect();
        let makers = MAKERS.iter().find(|(families, _)| families.iter().any(|family| bare.starts_with(family)));
        trusted.extend(makers.map_or(&[][..], |(_, makers)| makers));

        let mut same_id = vec![];
        let mut same_letters = vec![];
        for (provider, models) in &self.providers {
            for (model, price) in models {
                if *model == id || model == bare || strip_namespace(model) == bare {
                    same_id.push((provider.as_str(), *price));
                }
                if letters(model).eq(letters(bare)) {
                    same_letters.push((provider.as_str(), *price));
                }
            }
        }
        [same_id, same_letters].iter().find_map(|listed| pick(listed, &trusted)).unwrap_or_default()
    }
}

async fn fetch(http: &reqwest::Client) -> anyhow::Result<Prices> {
    let response = http.get(SOURCE).timeout(Duration::from_secs(8)).send().await?.error_for_status()?;
    let bytes = response.bytes().await?;
    let prices =
        tokio::task::spawn_blocking(move || serde_json::from_slice::<Value>(&bytes).map(|json| Prices::from_models_dev(&json))).await??;
    anyhow::ensure!(!prices.providers.is_empty(), "no prices in the answer");
    Ok(prices)
}

fn price(cost: &Value) -> Option<Price> {
    let rate = |of: &Value, key: &str| of[key].as_f64().filter(|rate| rate.is_finite());
    let (input, output) = (rate(cost, "input"), rate(cost, "output"));
    if input.is_none() && output.is_none() {
        return None;
    }
    let base = Price {
        input: input.unwrap_or(0.0),
        output: output.unwrap_or(0.0),
        cache_read: rate(cost, "cache_read").unwrap_or(0.0),
        cache_write: rate(cost, "cache_write").unwrap_or(0.0),
        long: None,
    };
    let tier = |of: &Value, above: u64| Tier {
        above,
        input: rate(of, "input").unwrap_or(base.input),
        output: rate(of, "output").unwrap_or(base.output),
        cache_read: rate(of, "cache_read").unwrap_or(base.cache_read),
        cache_write: rate(of, "cache_write").unwrap_or(base.cache_write),
    };
    // MUST KILL: type: Price.long holds only the first of models.dev's context tiers; make it a list of Tier
    let by_context =
        cost["tiers"].as_array().into_iter().flatten().filter(|t| t["tier"]["type"].as_str().is_none_or(|kind| kind == "context"));
    let first = by_context.filter_map(|t| Some(tier(t, t["tier"]["size"].as_u64().filter(|size| *size > 0)?))).min_by_key(|t| t.above);
    let over_200k = &cost["context_over_200k"];
    Some(Price { long: first.or_else(|| over_200k.is_object().then(|| tier(over_200k, 200_000))), ..base })
}

const NAMESPACES: &[&str] =
    &["openai", "anthropic", "google-vertex", "google", "xai", "deepseek", "mistral", "cohere", "zhipuai", "moonshotai", "minimax", "meta"];

fn namespace(id: &str) -> Option<&'static str> {
    NAMESPACES.iter().copied().find(|name| id.strip_prefix(name).is_some_and(|rest| rest.starts_with(['/', ':', '.'])))
}

fn strip_namespace(id: &str) -> &str {
    namespace(id).map_or(id, |name| &id[name.len() + 1..])
}

fn letters(id: &str) -> impl Iterator<Item = char> {
    strip_namespace(id).chars().filter(char::is_ascii_alphanumeric)
}

const MAKERS: &[(&[&str], &[&str])] = &[
    (&["gpt-", "chatgpt-", "codex-", "o1", "o3", "o4"], &["openai", "openai-codex", "opencode"]),
    (&["claude-"], &["anthropic"]),
    (&["gemini-", "gemma-"], &["google", "google-vertex"]),
    (&["grok-"], &["xai"]),
    (&["deepseek-"], &["deepseek"]),
    (&["mistral-"], &["mistral"]),
    (&["command-"], &["cohere"]),
    (&["glm-"], &["zhipuai"]),
    (&["kimi-", "moonshot-"], &["moonshotai"]),
    (&["minimax-"], &["minimax"]),
    (&["llama-"], &["meta"]),
];

fn pick(listed: &[(&str, Price)], trusted: &[&str]) -> Option<Price> {
    let by_trusted = trusted.iter().find_map(|provider| listed.iter().find(|(listed, _)| listed == provider));
    if let Some((_, price)) = by_trusted {
        return Some(*price);
    }
    let (_, first) = listed.first()?;
    listed.iter().all(|(_, price)| price == first).then_some(*first)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn flat(input: f64, output: f64, cache_read: f64, cache_write: f64) -> Price {
        Price { input, output, cache_read, cache_write, long: None }
    }

    fn list() -> Prices {
        Prices::from_models_dev(&json!({
            "anthropic": {"id": "anthropic", "models": {
                "claude-opus-5-5": {"cost": {"input": 4, "output": 20, "cache_read": 0.2, "cache_write": 5}},
                "claude-haiku-5-5": {"cost": {"input": 0.1, "output": 0.5, "cache_read": 0.01, "cache_write": 0.125,
                    "tiers": [{"input": 0.5, "output": 2.5, "cache_read": 0.05, "cache_write": 0.625, "tier": {"type": "context", "size": 100000}}]}},
                "claude-opus-4.5": {"cost": {"input": 5, "output": 25, "cache_read": 0.5, "cache_write": 6.25}},
            }},
            "openai": {"models": {
                "gpt-5.4": {"cost": {"input": 2.5, "output": 15, "cache_read": 0.25, "context_over_200k": {"input": 5, "output": 22.5, "cache_read": 0.5}}},
                "gpt-5-pro": {"cost": {"input": 15, "output": 120}},
            }},
            "teapot-router": {"models": {
                "claude-opus-5-5": {"cost": {"input": 1.4, "output": 7}},
                "gpt-5.4": {"cost": {"input": 1, "output": 2}},
                "oddball-1": {"cost": {"input": 1, "output": 2}},
                "contested-1": {"cost": {"input": 1, "output": 2}},
                "free-1": {"cost": {}},
            }},
            "merge-gateway": {"models": {
                "anthropic/claude-opus-5-5": {"cost": {"input": 9, "output": 9}},
                "oddball-1": {"cost": {"input": 1, "output": 2}},
                "contested-1": {"cost": {"input": 3, "output": 4}},
                "contested@1": {"cost": {"input": 3, "output": 4}},
            }},
        }))
    }

    #[test]
    fn a_model_finds_its_makers_price() {
        let prices = list();
        assert_eq!(prices.of("claude-opus-5-5"), flat(4.0, 20.0, 0.2, 5.0));
        assert_eq!(prices.of("anthropic/claude-opus-5-5"), flat(4.0, 20.0, 0.2, 5.0));
        assert_eq!(prices.of("claude-opus-4-5"), flat(5.0, 25.0, 0.5, 6.25));
        assert_eq!(prices.of("gpt-5-pro"), flat(15.0, 120.0, 0.0, 0.0));
        assert_eq!(prices.of("oddball-1"), flat(1.0, 2.0, 0.0, 0.0));
        assert_eq!(prices.of("contested-1"), Price::default());
        assert_eq!(prices.of("free-1"), Price::default());
        assert_eq!(prices.of("nobody-knows"), Price::default());
        assert_eq!(
            prices.of("claude-haiku-5-5").long,
            Some(Tier { above: 100_000, input: 0.5, output: 2.5, cache_read: 0.05, cache_write: 0.625 })
        );
        assert_eq!(prices.of("gpt-5.4").long, Some(Tier { above: 200_000, input: 5.0, output: 22.5, cache_read: 0.5, cache_write: 0.0 }));
        assert_eq!(serde_json::from_slice::<Prices>(&serde_json::to_vec(&prices).unwrap()).unwrap(), prices);
    }

    #[test]
    fn usage_is_priced_per_million_tokens() {
        let price = list().of("claude-haiku-5-5");
        let usage = Usage { input: 1_000, output: 2_000, cache_read: 40_000, cache_write: 10_000, ..Usage::default() };
        let close = |a: f64, b: f64| (a - b).abs() < 1e-12;
        let short = cost(&price, &usage, 0);
        assert!(
            close(short.input, 0.0001)
                && close(short.output, 0.001)
                && close(short.cache_read, 0.0004)
                && close(short.cache_write, 0.00125)
        );
        assert!(close(cost(&price, &usage, 4_000).cache_write, 0.00075 + 0.0008));
        let long = cost(&price, &Usage { cache_read: 120_000, ..usage }, 0);
        assert!(
            close(long.input, 0.0005) && close(long.output, 0.005) && close(long.cache_read, 0.006) && close(long.cache_write, 0.00625)
        );
        assert_eq!(cost(&Price::default(), &usage, 4_000), Cost::default());
    }
}
