use super::Namespace;
use super::declare::resolve_ref;
use crate::tool::ToolSpec;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

const NAME_WEIGHT: f64 = 3.0;
const NAMESPACE_WEIGHT: f64 = 0.5;
const K1: f64 = 1.2;
const B: f64 = 0.75;

const STOP_WORDS: [&str; 21] = [
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "in", "is", "it", "of", "on", "or", "that", "the", "this", "to", "with",
];

pub struct Document<'a> {
    pub name: &'a str,
    text: String,
    namespace: String,
}

impl<'a> Document<'a> {
    pub fn new(tool: &'a ToolSpec, namespace: Option<&Namespace>) -> Self {
        let mut parts = vec![tool.description.as_str()];
        schema_text(&tool.input, &tool.input, &mut parts, &mut vec![&tool.input]);
        let text = parts.into_iter().filter(|part| !part.trim().is_empty()).collect::<Vec<_>>().join(" ");
        let namespace =
            namespace.map(|n| [n.name.as_str(), n.description.as_deref().unwrap_or(""), n.instructions.as_deref().unwrap_or("")].join(" "));
        Self { name: &tool.name, text, namespace: namespace.unwrap_or_default() }
    }
}

fn schema_text<'a>(schema: &'a Value, root: &'a Value, parts: &mut Vec<&'a str>, seen: &mut Vec<&'a Value>) {
    let Some(fields) = schema.as_object() else { return };
    if let Some(target) = fields.get("$ref").and_then(Value::as_str).and_then(|reference| resolve_ref(reference, root))
        && target.is_object()
        && !seen.iter().any(|seen| std::ptr::eq(*seen, target))
    {
        seen.push(target);
        schema_text(target, root, parts, seen);
    }
    parts.extend(fields.get("description").and_then(Value::as_str));
    if let Some(Value::Object(properties)) = fields.get("properties") {
        for (name, property) in properties {
            parts.push(name);
            schema_text(property, root, parts, seen);
        }
    }
    if let Some(items) = fields.get("items") {
        schema_text(items, root, parts, seen);
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        for variant in fields.get(key).and_then(Value::as_array).into_iter().flatten() {
            schema_text(variant, root, parts, seen);
        }
    }
}

fn tokenize(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut spaced = String::with_capacity(text.len());
    for (i, &c) in chars.iter().enumerate() {
        let previous = i.checked_sub(1).map(|i| chars[i]);
        let next = chars.get(i + 1);
        let after_word = previous.is_some_and(|p| p.is_ascii_lowercase() || p.is_ascii_digit());
        let ends_acronym = previous.is_some_and(|p| p.is_ascii_uppercase()) && next.is_some_and(|n| n.is_ascii_lowercase());
        if c.is_ascii_uppercase() && (after_word || ends_acronym) {
            spaced.push(' ');
        }
        spaced.push(c);
    }
    let words = spaced.to_lowercase();
    let words = words.split(|c: char| !c.is_alphanumeric()).filter(|word| !word.is_empty() && !STOP_WORDS.contains(word));
    words.map(|word| if word.is_ascii() { stem(word) } else { word.to_string() }).collect()
}

fn stem(term: &str) -> String {
    let cut = |n: usize| &term[..term.len() - n];
    if term.len() > 4 && term.ends_with("ies") {
        return format!("{}y", cut(3));
    }
    if term.len() > 4 && ["ches", "shes", "sses", "xes", "zes"].iter().any(|end| term.ends_with(end)) {
        return cut(2).into();
    }
    if term.len() > 3 && term.ends_with('s') && !term.ends_with("ss") {
        return cut(1).into();
    }
    term.into()
}

fn normalize_name(name: &str) -> String {
    let valid = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '$';
    name.trim().to_lowercase().chars().map(|c| if valid(c) { c } else { '_' }).collect()
}

struct Indexed<'a> {
    name: &'a str,
    name_terms: HashSet<String>,
    own_name_terms: Vec<String>,
    text_counts: HashMap<String, usize>,
    text_length: usize,
    namespace_terms: HashSet<String>,
}

fn index<'a>(document: &Document<'a>) -> Indexed<'a> {
    let mut text_counts: HashMap<String, usize> = HashMap::new();
    for term in tokenize(&document.text) {
        *text_counts.entry(term).or_default() += 1;
    }
    Indexed {
        name: document.name,
        name_terms: tokenize(document.name).into_iter().collect(),
        own_name_terms: tokenize(document.name.rsplit("__").next().unwrap_or_default()),
        text_length: text_counts.values().sum(),
        text_counts,
        namespace_terms: tokenize(&document.namespace).into_iter().collect(),
    }
}

struct Match<'a> {
    exact: bool,
    covered: usize,
    score: f64,
    name: &'a str,
}

pub fn rank<'a>(query: &str, documents: &[Document<'a>], limit: usize) -> Vec<&'a str> {
    let mut terms = tokenize(query);
    let mut seen = HashSet::new();
    terms.retain(|term| seen.insert(term.clone()));
    if terms.is_empty() || documents.is_empty() {
        return vec![];
    }
    let indexed: Vec<Indexed> = documents.iter().map(index).collect();
    let count = indexed.len() as f64;
    let average_length = match indexed.iter().map(|d| d.text_length).sum::<usize>() as f64 / count {
        0.0 => 1.0,
        average => average,
    };
    let has = |d: &Indexed, term: &str| d.name_terms.contains(term) || d.text_counts.contains_key(term) || d.namespace_terms.contains(term);
    let idf: Vec<f64> = terms
        .iter()
        .map(|term| {
            let frequency = indexed.iter().filter(|d| has(d, term)).count() as f64;
            (1.0 + (count - frequency + 0.5) / (frequency + 0.5)).ln()
        })
        .collect();
    let full_name_bonus: f64 = idf.iter().sum();
    let exact_query = normalize_name(query);

    let mut matches: Vec<Match> = vec![];
    for document in &indexed {
        let norm = K1 * (1.0 - B + B * document.text_length as f64 / average_length);
        let (mut score, mut covered) = (0.0, 0);
        for (term, idf) in terms.iter().zip(&idf) {
            let mentions = document.text_counts.get(term).copied().unwrap_or(0) as f64;
            let in_name = if document.name_terms.contains(term) { NAME_WEIGHT } else { 0.0 };
            let in_namespace = if document.namespace_terms.contains(term) { NAMESPACE_WEIGHT } else { 0.0 };
            let weight = in_name + in_namespace + mentions * (K1 + 1.0) / (mentions + norm);
            if weight > 0.0 {
                covered += 1;
            }
            score += idf * weight;
        }
        if score == 0.0 {
            continue;
        }
        if !document.own_name_terms.is_empty() && document.own_name_terms.iter().all(|term| terms.contains(term)) {
            score += full_name_bonus;
        }
        let name = normalize_name(document.name);
        let exact = name == exact_query || name.ends_with(&format!("__{exact_query}"));
        matches.push(Match { exact, covered, score, name: document.name });
    }
    matches.sort_by(|a, b| b.exact.cmp(&a.exact).then(b.covered.cmp(&a.covered)).then(b.score.total_cmp(&a.score)));
    matches.into_iter().take(limit).map(|found| found.name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str, description: &str) -> ToolSpec {
        ToolSpec::new(name, description, json!({ "type": "object", "properties": {} }))
    }

    fn ranked(query: &str, tools: &[ToolSpec], limit: usize) -> Vec<String> {
        let documents: Vec<Document> = tools.iter().map(|tool| Document::new(tool, None)).collect();
        rank(query, &documents, limit).into_iter().map(String::from).collect()
    }

    #[test]
    fn words_are_split_lowered_and_made_singular() {
        assert_eq!(tokenize("searchIssues in the HTTPServer queries"), ["search", "issue", "http", "server", "query"]);
        assert_eq!(
            tokenize("mcp__dev-radius__thread_read: Reads threads, boxes & classes"),
            ["mcp", "dev", "radiu", "thread", "read", "read", "thread", "box", "class"]
        );
        assert_eq!(tokenize("Grüße, 世界!"), ["grüße", "世界"]);
    }

    #[test]
    fn tools_are_ranked_for_a_query() {
        let tools = [
            tool("mcp__github__create_issue", "Open a new issue in a repository."),
            tool("mcp__github__search_issues", "Search issues and pull requests."),
            tool("mcp__peekaboo__window", "Move, resize and focus a window."),
            tool("mcp__peekaboo__see", "Take a screenshot of a window or the screen and describe it."),
            tool("mcp__peekaboo__app", "Launch or quit an app."),
            tool("bash", "Run a shell command."),
        ];
        assert_eq!(ranked("search issues", &tools, 8), ["mcp__github__search_issues", "mcp__github__create_issue"]);
        assert_eq!(ranked("screenshot window", &tools, 1), ["mcp__peekaboo__see"]);
        assert_eq!(ranked("peekaboo app", &tools, 1), ["mcp__peekaboo__app"]);
        assert_eq!(ranked("create_issue", &tools, 1), ["mcp__github__create_issue"]);
        assert_eq!(ranked("the of", &tools, 8), Vec::<String>::new());
        assert_eq!(ranked("quantum", &tools, 8), Vec::<String>::new());
    }

    #[test]
    fn the_schema_and_the_namespace_are_searched_too() {
        let mut send = ToolSpec::new(
            "mcp__mail__send",
            "Send it.",
            json!({ "type": "object", "properties": { "recipient": { "$ref": "#/$defs/Person" } }, "$defs": { "Person": { "type": "object", "description": "Who gets the letter.", "properties": { "boss": { "$ref": "#/$defs/Person" } } } } }),
        );
        send.namespace = Some("mcp__mail".into());
        let other = tool("other", "Nothing of note.");
        let namespace = Namespace { name: "mcp__mail".into(), description: Some("Post office".into()), instructions: None };
        let documents = [Document::new(&send, Some(&namespace)), Document::new(&other, None)];
        for query in ["recipient", "letter", "boss", "office"] {
            assert_eq!(rank(query, &documents, 8), ["mcp__mail__send"], "{query}");
        }
    }
}
