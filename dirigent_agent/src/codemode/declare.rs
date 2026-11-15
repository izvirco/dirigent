//! What a script is told of the tools: their names as identifiers, their TypeScript
//! declarations made from their JSON Schemas (as pi-codemode's `declarations.js` made them), and
//! the `codemode` tool's description built from those.

use super::Namespace;
use crate::tool::ToolSpec;
use serde_json::{Map, Number, Value};
use std::collections::BTreeMap;

/// The longest input type a declaration shows, in characters; a longer one is `unknown`.
const INPUT_MAX_CHARS: usize = 16_000;
/// Local `$ref`s followed per rendered schema, so shared definitions cannot blow up the output.
const MAX_REF_EXPANSIONS: u32 = 32;
/// The longest property tag; longer ones (large defaults, long patterns) are left out.
const MAX_TAG_CHARS: usize = 80;
/// What the tools listed in the description may cost together, in estimated tokens.
const INLINE_BUDGET: usize = 3000;
const CHARS_PER_TOKEN: usize = 4;

const INTRO: &str = r#"Run JavaScript that calls other tools. `code` is raw JavaScript (no code fence), run as the body of an async function in a QuickJS sandbox: top-level `await` and `return` work. No Node, file system, network, or timers.
- `await tools.<name>({ ...args })` resolves to a string, or to what the tool's declaration says, and rejects with an Error on failure. A tool whose declaration returns an object resolves on failure too when it returned that object; check its fields. A tool that gave an image resolves to an image block `{ type: "image", data, mimeType, note }`, which `image()` shows. Calls run side by side (`Promise.all`, `Promise.allSettled`); those still running when the script ends are cancelled.
- Only what the script puts out reaches you: filter and cut large results down in the script.
- Optional first line: `// @options: {"max_output_tokens": 10000, "timeout_ms": 60000}`. Output longer than `max_output_tokens` (default 10000) keeps its start and end, and the full text is written to a file. `timeout_ms` is a deadline for the whole script; there is none by default.

Globals:
- `text(value)`, `image(dataUrlOrImageBlock)`, `console.log(...)`, and top-level `return` add output; `exit()` ends the script. With several text items, each starts with a `==> text N/M <==` line, and `console` lines follow the other output in one `<console_output>` block. `image()` also saves the image to a temp file and the result names its path.
- `store(key, value)` and `load(key)` keep small JSON values across codemode calls, when the script succeeds.
- `ALL_TOOLS` (every tool as `{ name, description }`), `await searchTools(query, { limit?, namespace? })` (the best matches among them), `await describeTool(name)` (a tool's description and declaration), `await describeNamespace(name)` (an MCP server's `{ name, description?, instructions?, tools }`, `tools` being its tools' names): find the tools that are not listed here, such as MCP tools."#;

/// The identifier a script uses for a tool: characters that are not valid in a JavaScript
/// identifier become `_`. `mcp__docs__search` stays as is, `my-tool` becomes `my_tool`.
pub fn identifier(name: &str) -> String {
    let valid = |i: usize, c: char| c == '_' || c == '$' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit());
    let id: String = name.chars().enumerate().map(|(i, c)| if valid(i, c) { c } else { '_' }).collect();
    if id.is_empty() { "_".into() } else { id }
}

/// What a script reads of a tool (`describeTool`, `ALL_TOOLS`): its description, its rules
/// (the system prompt only has those of the tools declared to the model) and its declaration.
pub fn sample(tool: &ToolSpec) -> String {
    let rules: Vec<String> = tool.guidelines.iter().map(|g| g.trim()).filter(|g| !g.is_empty()).map(|g| format!("- {g}")).collect();
    let about = if rules.is_empty() { tool.description.clone() } else { format!("{}\n\n{}", tool.description.trim(), rules.join("\n")) };
    format!("{}\n\ncodemode tool declaration:\n```ts\ndeclare const tools: {{ {} }};\n```", about.trim(), signature(tool, true))
}

/// A tool as a member of `tools`: `name(args: T): Promise<R>;`. `comments`: with what its
/// schema says of each argument.
fn signature(tool: &ToolSpec, comments: bool) -> String {
    let returns = tool.returns.as_deref().unwrap_or("string");
    format!("{}(args: {}): Promise<{returns}>;", identifier(&tool.name), schema_type(&tool.input, comments, Some(INPUT_MAX_CHARS)))
}

/// The `codemode` tool's description: what a script is, its globals, and the declarations of
/// the tools the model has (`deferred` ones are never listed: the text stays the same while
/// MCP servers connect), grouped by namespace, as many as fit `INLINE_BUDGET`.
///
/// The listed tools are declared to the model as well, with their descriptions and schemas,
/// so a tool is listed by its signature alone: the name a script calls it by, its arguments
/// in short, and what a call resolves to.
pub fn description(tools: &[ToolSpec], namespace: impl Fn(&str) -> Option<Namespace>) -> String {
    // Tools without a namespace first, then the namespaces by name.
    let mut groups: BTreeMap<Option<&str>, Vec<(usize, String)>> = BTreeMap::new();
    for tool in tools.iter().filter(|tool| !tool.deferred) {
        let id = identifier(&tool.name);
        let alias = if id == tool.name { String::new() } else { format!(" // {}", comment_line(&tool.name)) };
        let line = format!("  {}{alias}", signature(tool, false));
        groups.entry(tool.namespace.as_deref()).or_default().push((line.len().div_ceil(CHARS_PER_TOKEN), line));
    }
    if groups.is_empty() {
        return INTRO.into();
    }
    let costs: Vec<Vec<usize>> = groups.values().map(|entries| entries.iter().map(|(cost, _)| *cost).collect()).collect();
    let shown = select(&costs, INLINE_BUDGET);

    let mut sections = vec![INTRO.to_string(), "Tools (declared to you as well: a script passes the same arguments):".to_string()];
    for ((name, entries), shown) in groups.iter().zip(shown) {
        let lines: Vec<&str> = entries.iter().zip(&shown).filter(|(_, shown)| **shown).map(|((_, line), _)| line.as_str()).collect();
        if let Some(name) = name {
            let listing = match lines.len() {
                n if n == entries.len() => "",
                0 => " (tools not listed)",
                _ => " (some tools not listed)",
            };
            let about = namespace(name).and_then(|n| n.description).map(|d| d.trim().to_string()).filter(|d| !d.is_empty());
            sections.push(format!("## {name}{listing}{}", about.map(|d| format!("\n{d}")).unwrap_or_default()));
        }
        if !lines.is_empty() {
            sections.push(format!("```ts\ndeclare const tools: {{\n{}\n}};\n```", lines.join("\n")));
        }
    }
    sections.join("\n\n")
}

/// Which tools fit the budget, by group: in each round every group places its cheapest
/// remaining tool; a group whose next tool does not fit drops out while the others go on. So
/// every namespace is represented before any is complete.
fn select(groups: &[Vec<usize>], budget: usize) -> Vec<Vec<bool>> {
    let mut shown: Vec<Vec<bool>> = groups.iter().map(|costs| vec![false; costs.len()]).collect();
    // Each group's tools, cheapest first (ties in their own order).
    let mut queues: Vec<std::collections::VecDeque<usize>> = groups
        .iter()
        .map(|costs| {
            let mut order: Vec<usize> = (0..costs.len()).collect();
            order.sort_by_key(|&i| costs[i]);
            order.into()
        })
        .collect();
    let mut remaining = budget;
    let mut active: Vec<usize> = (0..groups.len()).collect();
    while !active.is_empty() {
        active.retain(|&group| {
            let Some(&next) = queues[group].front() else { return false };
            if groups[group][next] > remaining {
                return false;
            }
            remaining -= groups[group][next];
            shown[group][next] = true;
            queues[group].pop_front();
            true
        });
    }
    shown
}

/// `text` as the rest of a `//` comment: nothing in it ends the line.
fn comment_line(text: &str) -> String {
    text.chars().map(|c| if is_line_terminator(c) { ' ' } else { c }).collect()
}

fn is_line_terminator(c: char) -> bool {
    matches!(c, '\r' | '\n' | '\u{2028}' | '\u{2029}')
}

/// A JSON Schema as a TypeScript type: objects on one line (`{ a: string; b?: number; }`) with
/// the properties sorted by name, or, with `comments`, one property per line under `//`
/// comments when one has a description or constraints the type cannot say
/// (`// @integer @minimum 1 @default 20`); `Array<T>` for arrays. Local references
/// (`#/$defs/...`) are followed; recursive and remote ones are `unknown`. A type longer than
/// `max_chars` is rendered again without the constraints, and is `unknown` if still too long.
pub fn schema_type(schema: &Value, comments: bool, max_chars: Option<usize>) -> String {
    let render = |tags: bool| Render { root: schema, resolving: vec![], expansions: 0, comments, tags }.ty(schema);
    let fits = |ty: &str| max_chars.is_none_or(|max| ty.chars().count() <= max);
    let ty = render(comments);
    if fits(&ty) {
        return ty;
    }
    let untagged = render(false);
    if fits(&untagged) { untagged } else { "unknown".into() }
}

struct Render<'a> {
    root: &'a Value,
    /// The references being followed: one met again is recursive.
    resolving: Vec<&'a str>,
    expansions: u32,
    comments: bool,
    tags: bool,
}

impl<'a> Render<'a> {
    fn ty(&mut self, schema: &'a Value) -> String {
        let schema = match schema {
            Value::Bool(true) => return "unknown".into(),
            Value::Bool(false) => return "never".into(),
            Value::Object(schema) => schema,
            _ => return "unknown".into(),
        };
        if let Some(Value::String(reference)) = schema.get("$ref") {
            if self.resolving.contains(&reference.as_str()) || self.expansions >= MAX_REF_EXPANSIONS {
                return "unknown".into();
            }
            let Some(target) = resolve_ref(reference, self.root) else { return "unknown".into() };
            self.expansions += 1;
            self.resolving.push(reference);
            let ty = self.ty(target);
            self.resolving.pop();
            return ty;
        }
        if let Some(value) = schema.get("const") {
            return json(value);
        }
        if let Some(Value::Array(values)) = schema.get("enum") {
            return union(values.iter().map(json));
        }
        let variants = match (schema.get("anyOf"), schema.get("oneOf")) {
            (Some(Value::Array(variants)), _) | (_, Some(Value::Array(variants))) => Some(variants),
            _ => None,
        };
        if let Some(variants) = variants {
            return union(variants.iter().map(|variant| self.ty(variant)));
        }
        if let Some(Value::Array(parts)) = schema.get("allOf") {
            let parts: Vec<String> = parts.iter().map(|part| self.ty(part)).filter(|part| part != "unknown").collect();
            let parts: Vec<String> = parts.into_iter().map(|part| if part.contains(" | ") { format!("({part})") } else { part }).collect();
            return if parts.is_empty() { "unknown".into() } else { parts.join(" & ") };
        }
        match schema.get("type") {
            Some(Value::Array(types)) => union(types.iter().map(|ty| self.of_type(schema, Some(ty)))),
            ty => self.of_type(schema, ty),
        }
    }

    /// `schema` read as being of JSON Schema type `ty`.
    fn of_type(&mut self, schema: &'a Map<String, Value>, ty: Option<&Value>) -> String {
        let has = |keys: &[&str]| keys.iter().any(|key| schema.contains_key(*key));
        match ty {
            Some(Value::String(ty)) => match ty.as_str() {
                "string" => "string".into(),
                "number" | "integer" => "number".into(),
                "boolean" => "boolean".into(),
                "null" => "null".into(),
                "array" => self.array(schema),
                "object" => self.object(schema),
                _ => "unknown".into(),
            },
            None if has(&["properties", "additionalProperties", "required"]) => self.object(schema),
            None if has(&["items", "prefixItems"]) => self.array(schema),
            _ => "unknown".into(),
        }
    }

    fn array(&mut self, schema: &'a Map<String, Value>) -> String {
        let tuple = match (schema.get("items"), schema.get("prefixItems")) {
            (Some(items), _) if !items.is_array() => return format!("Array<{}>", self.ty(items)),
            (_, Some(Value::Array(tuple))) | (Some(Value::Array(tuple)), _) => tuple.as_slice(),
            _ => &[],
        };
        if tuple.is_empty() {
            return "unknown[]".into();
        }
        format!("[{}]", tuple.iter().map(|item| self.ty(item)).collect::<Vec<_>>().join(", "))
    }

    fn object(&mut self, schema: &'a Map<String, Value>) -> String {
        let mut properties: Vec<(&'a String, &'a Value)> = match schema.get("properties") {
            Some(Value::Object(properties)) => properties.iter().collect(),
            _ => vec![],
        };
        properties.sort_by_key(|(name, _)| *name);
        let required: Vec<&str> = match schema.get("required") {
            Some(Value::Array(required)) => required.iter().filter_map(Value::as_str).collect(),
            _ => vec![],
        };
        let mut members: Vec<String> = properties
            .iter()
            .map(|(name, property)| {
                let key = if identifier(name) == **name { (*name).clone() } else { json(&Value::String((*name).clone())) };
                let optional = if required.contains(&name.as_str()) { "" } else { "?" };
                format!("{key}{optional}: {};", self.ty(property))
            })
            .collect();
        match schema.get("additionalProperties") {
            None if properties.is_empty() => members.push("[key: string]: unknown;".into()),
            None | Some(Value::Bool(false)) => {}
            Some(Value::Bool(true)) => members.push("[key: string]: unknown;".into()),
            Some(additional) => members.push(format!("[key: string]: {};", self.ty(additional))),
        }
        if members.is_empty() {
            return "{}".into();
        }
        let comments: Vec<Vec<String>> =
            if self.comments { properties.iter().map(|(_, property)| self.comment(property)).collect() } else { vec![] };
        if comments.iter().all(Vec::is_empty) {
            return format!("{{ {} }}", members.join(" "));
        }
        let mut lines = vec!["{".to_string()];
        for (index, member) in members.iter().enumerate() {
            for line in comments.get(index).into_iter().flatten() {
                lines.push(format!("  // {line}"));
            }
            lines.push(format!("  {}", member.replace('\n', "\n  ")));
        }
        lines.push("}".into());
        lines.join("\n")
    }

    /// The `//` lines over a property: its description, then its constraints.
    fn comment(&self, property: &Value) -> Vec<String> {
        let description = property.get("description").and_then(Value::as_str).unwrap_or_default();
        let mut lines: Vec<String> =
            description.split(is_line_terminator).map(str::trim).filter(|line| !line.is_empty()).map(String::from).collect();
        let tags = if self.tags { constraint_tags(property) } else { String::new() };
        if !tags.is_empty() {
            lines.push(tags);
        }
        lines
    }
}

/// The subschema a local `$ref` (`#`, `#/$defs/Item`) names in `root`; `None` for remote
/// references and ones that name nothing.
pub fn resolve_ref<'a>(reference: &str, root: &'a Value) -> Option<&'a Value> {
    if reference != "#" && !reference.starts_with("#/") {
        return None;
    }
    let mut current = root;
    for segment in reference.get(2..).unwrap_or_default().split('/').filter(|segment| !segment.is_empty()) {
        let key = percent_decode(segment)?.replace("~1", "/").replace("~0", "~");
        current = current.as_object()?.get(&key)?;
    }
    (current.is_boolean() || current.is_object()).then_some(current)
}

/// `%41` → `A`; `None` for a malformed escape.
fn percent_decode(segment: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(segment.len());
    let mut rest = segment.as_bytes();
    while let [first, tail @ ..] = rest {
        if *first == b'%' {
            let hex = std::str::from_utf8(tail.get(..2)?).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            rest = &tail[2..];
        } else {
            bytes.push(*first);
            rest = tail;
        }
    }
    String::from_utf8(bytes).ok()
}

/// The types as a union, each once; `unknown` if one of them is.
fn union(types: impl Iterator<Item = String>) -> String {
    let mut unique: Vec<String> = vec![];
    for ty in types {
        if !unique.contains(&ty) {
            unique.push(ty);
        }
    }
    if unique.iter().any(|ty| ty == "unknown") {
        return "unknown".into();
    }
    if unique.is_empty() { "never".into() } else { unique.join(" | ") }
}

/// Constraints of a property that its TypeScript type cannot say, as JSDoc-style tags:
/// `@integer @minimum 1 @maximum 100 @default 20`. Empty when there are none.
fn constraint_tags(property: &Value) -> String {
    let Some(property) = property.as_object() else { return String::new() };
    // An optional field such as Pydantic's `int | None` keeps its constraints in the branch
    // that is not null: they hold together with the property's own, so both are read.
    let variants = match (property.get("anyOf"), property.get("oneOf")) {
        (Some(Value::Array(variants)), _) | (_, Some(Value::Array(variants))) => variants.as_slice(),
        _ => &[],
    };
    let mut non_null = variants.iter().filter(|variant| variant.get("type").and_then(Value::as_str) != Some("null"));
    let branch = match (non_null.next(), non_null.next()) {
        (Some(Value::Object(branch)), None) => Some(branch),
        _ => None,
    };
    let schemas: Vec<&Map<String, Value>> = std::iter::once(property).chain(branch).collect();

    let is = |schema: &Map<String, Value>, key: &str| schema.get(key) == Some(&Value::Bool(true));
    // Draft 4 and OpenAPI 3.0 make `minimum` / `maximum` exclusive with a boolean beside them.
    let keyword = |schema: &'_ Map<String, Value>, keyword: &str| -> Option<Value> {
        let (bound, exclusive) = match keyword {
            "minimum" | "exclusiveMinimum" => ("minimum", "exclusiveMinimum"),
            "maximum" | "exclusiveMaximum" => ("maximum", "exclusiveMaximum"),
            _ => return schema.get(keyword).cloned(),
        };
        match (is(schema, exclusive), keyword == bound) {
            (true, true) => None,
            (true, false) => schema.get(bound).cloned(),
            (false, _) => schema.get(keyword).cloned(),
        }
    };
    let is_integer = |schema: &&Map<String, Value>| match schema.get("type") {
        Some(Value::String(ty)) => ty == "integer",
        Some(Value::Array(types)) => types.iter().any(|ty| ty == "integer"),
        _ => false,
    };

    let mut tags: Vec<String> = vec![];
    if schemas.iter().any(is_integer) {
        tags.push("@integer".into());
    }
    for (name, bound) in [
        ("minimum", Some(true)),
        ("exclusiveMinimum", Some(true)),
        ("maximum", Some(false)),
        ("exclusiveMaximum", Some(false)),
        ("multipleOf", None),
        ("minLength", Some(true)),
        ("maxLength", Some(false)),
        ("pattern", None),
        ("format", None),
        ("minItems", Some(true)),
        ("maxItems", Some(false)),
    ] {
        let values: Vec<Value> = schemas.iter().filter_map(|schema| keyword(schema, name)).collect();
        match bound {
            // Bounds all hold at once: the tightest one matters.
            Some(lower) => {
                let numbers = values.iter().filter_map(Value::as_number);
                let tightest = numbers.reduce(|a, b| if (a.as_f64() > b.as_f64()) == lower { a } else { b });
                tags.extend(tightest.map(|number| format!("@{name} {}", js_number(number))));
            }
            None => {
                let mut seen: Vec<&Value> = vec![];
                for value in &values {
                    let text = match value {
                        Value::Number(number) => js_number(number),
                        Value::String(text) => text.clone(),
                        _ => continue,
                    };
                    if !seen.contains(&value) {
                        seen.push(value);
                        tags.push(format!("@{name} {text}"));
                    }
                }
            }
        }
    }
    if schemas.iter().any(|schema| is(schema, "uniqueItems")) {
        tags.push("@uniqueItems".into());
    }
    // Annotations, unlike constraints, are the property's own.
    if let Some(default) = property.get("default").filter(|default| !default.is_null()) {
        tags.push(format!("@default {}", json(default)));
    }
    if is(property, "deprecated") {
        tags.push("@deprecated".into());
    }
    let escape = |tag: String| -> String {
        tag.chars().map(|c| if is_line_terminator(c) { format!("\\u{:04x}", c as u32) } else { c.to_string() }).collect()
    };
    tags.into_iter().filter(|tag| tag.chars().count() <= MAX_TAG_CHARS).map(escape).collect::<Vec<_>>().join(" ")
}

/// A JSON value as JavaScript's `JSON.stringify` writes it: `1.0` is `1`.
fn json(value: &Value) -> String {
    match value {
        Value::Number(number) => js_number(number),
        Value::Array(items) => format!("[{}]", items.iter().map(json).collect::<Vec<_>>().join(",")),
        Value::Object(fields) => {
            let fields = fields.iter().map(|(key, value)| format!("{}:{}", Value::String(key.clone()), json(value)));
            format!("{{{}}}", fields.collect::<Vec<_>>().join(","))
        }
        other => other.to_string(),
    }
}

fn js_number(number: &Number) -> String {
    match number.as_f64() {
        Some(float) if number.is_f64() && float.fract() == 0.0 && float.abs() < 1e21 => format!("{float:.0}"),
        _ => number.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str, input: Value) -> ToolSpec {
        ToolSpec::new(name, &format!("About {name}."), input)
    }

    #[test]
    fn names_become_identifiers() {
        for (name, id) in [
            ("mcp__docs__search", "mcp__docs__search"),
            ("my-tool", "my_tool"),
            ("9lives", "_lives"),
            ("a.b c", "a_b_c"),
            ("", "_"),
            ("$x1", "$x1"),
        ] {
            assert_eq!(identifier(name), id);
        }
    }

    /// The expected types are what pi-codemode 1.5.2's `schemaToType` gives for the same schemas.
    #[test]
    fn schemas_become_types_as_pi_made_them() {
        let cases = [
            // Optional fields, sorted by name; no comments: one line.
            (
                json!({ "type": "object", "properties": { "path": { "type": "string" }, "limit": { "type": "number" }, "offset": { "type": ["number", "null"] } }, "required": ["path"] }),
                "{ limit?: number; offset?: number | null; path: string; }",
            ),
            // Enums, a const, a key that is no identifier.
            (
                json!({ "type": "object", "properties": { "mode": { "enum": ["steer", "follow_up", 3, null] }, "kind": { "const": "a\"b" }, "content-type": { "type": "boolean" } }, "required": ["mode", "kind"] }),
                r#"{ "content-type"?: boolean; kind: "a\"b"; mode: "steer" | "follow_up" | 3 | null; }"#,
            ),
            // Nested objects and arrays; a property's description makes the object multi-line.
            (
                json!({ "type": "object", "required": ["edits"], "properties": {
                    "edits": { "type": "array", "description": "One or more replacements.\nIn order.", "items": { "type": "object", "properties": { "oldText": { "type": "string", "description": "Exact text." }, "newText": { "type": "string" } }, "required": ["oldText", "newText"], "additionalProperties": false } },
                    "tags": { "type": "array", "items": { "type": "string" } },
                    "pair": { "type": "array", "prefixItems": [{ "type": "string" }, { "type": "integer" }] },
                    "any": { "type": "array" },
                } }),
                "{\n  any?: unknown[];\n  // One or more replacements.\n  // In order.\n  edits: Array<{\n    newText: string;\n    // Exact text.\n    oldText: string;\n  }>;\n  pair?: [string, number];\n  tags?: Array<string>;\n}",
            ),
            // Unions, intersections, open objects.
            (
                json!({ "type": "object", "properties": {
                    "a": { "anyOf": [{ "type": "string" }, { "type": "null" }] },
                    "b": { "oneOf": [{ "type": "string" }, {}] },
                    "c": { "allOf": [{ "type": "object", "properties": { "x": { "type": "number" } } }, { "anyOf": [{ "type": "string" }, { "type": "number" }] }] },
                    "d": { "type": "object", "additionalProperties": { "type": "string" } },
                    "e": { "type": "object" },
                    "f": { "type": "object", "properties": {}, "additionalProperties": false },
                    "g": { "properties": { "y": { "type": "string" } }, "additionalProperties": true },
                    "h": true,
                    "i": false,
                } }),
                "{ a?: string | null; b?: unknown; c?: { x?: number; } & (string | number); d?: { [key: string]: string; }; e?: { [key: string]: unknown; }; f?: {}; g?: { y?: string; [key: string]: unknown; }; h?: unknown; i?: never; }",
            ),
            // Constraints the type cannot say, as tags.
            (
                json!({ "type": "object", "properties": {
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100.0, "default": 20, "description": "How many." },
                    "name": { "type": "string", "minLength": 1, "pattern": "^[a-z]+$", "format": "hostname", "deprecated": true },
                    "count": { "anyOf": [{ "type": "integer", "minimum": 0, "exclusiveMinimum": true }, { "type": "null" }], "default": null },
                    "ids": { "type": "array", "items": { "type": "string" }, "minItems": 1, "uniqueItems": true, "default": ["a"] },
                    "long": { "type": "string", "default": "x".repeat(100) },
                } }),
                "{\n  // @integer @exclusiveMinimum 0\n  count?: number | null;\n  // @minItems 1 @uniqueItems @default [\"a\"]\n  ids?: Array<string>;\n  // How many.\n  // @integer @minimum 1 @maximum 100 @default 20\n  limit?: number;\n  long?: string;\n  // @minLength 1 @pattern ^[a-z]+$ @format hostname @deprecated\n  name?: string;\n}",
            ),
            // References: local ones are followed, recursive and remote ones are unknown.
            (
                json!({ "type": "object", "properties": { "item": { "$ref": "#/$defs/Item" }, "node": { "$ref": "#/$defs/Node" }, "far": { "$ref": "https://example.com/x.json" }, "odd": { "$ref": "#/$defs/a~1b%20c" } },
                    "$defs": { "Item": { "type": "object", "properties": { "id": { "type": "string" } }, "required": ["id"] }, "Node": { "type": "object", "properties": { "next": { "$ref": "#/$defs/Node" } } }, "a/b c": { "type": "boolean" } } }),
                "{ far?: unknown; item?: { id: string; }; node?: { next?: unknown; }; odd?: boolean; }",
            ),
        ];
        for (schema, expected) in cases {
            assert_eq!(schema_type(&schema, true, None), expected, "{schema}");
        }
        assert_eq!(schema_type(&json!({ "type": "string" }), true, None), "string");
        assert_eq!(schema_type(&json!({}), true, None), "unknown");
        assert_eq!(schema_type(&json!({ "enum": [] }), true, None), "never");
    }

    #[test]
    fn a_type_too_long_loses_its_tags_then_itself() {
        let schema = json!({ "type": "object", "properties": { "n": { "type": "integer", "minimum": 1 } } });
        assert_eq!(schema_type(&schema, true, Some(44)), "{\n  // @integer @minimum 1\n  n?: number;\n}");
        assert_eq!(schema_type(&schema, true, Some(30)), "{ n?: number; }");
        assert_eq!(schema_type(&schema, true, Some(10)), "unknown");
    }

    #[test]
    fn a_sample_has_the_description_the_rules_and_the_declaration() {
        let mut spec = tool(
            "my-tool",
            json!({ "type": "object", "properties": { "q": { "type": "string", "description": "What to find." } }, "required": ["q"] }),
        );
        spec.guidelines = vec!["Use it sparingly.".into(), " ".into()];
        spec.returns = Some("{ hits: string[] }".into());
        assert_eq!(
            sample(&spec),
            "About my-tool.\n\n- Use it sparingly.\n\ncodemode tool declaration:\n```ts\ndeclare const tools: { my_tool(args: {\n  // What to find.\n  q: string;\n}): Promise<{ hits: string[] }>; };\n```"
        );
        // Without `returns` a call resolves to the tool's text.
        let plain = tool("plain", json!({ "type": "object", "properties": {}, "additionalProperties": false }));
        assert!(sample(&plain).ends_with("plain(args: {}): Promise<string>; };\n```"));
    }

    #[test]
    fn the_description_lists_the_declared_tools_by_namespace() {
        let mut bash = tool(
            "bash",
            json!({ "type": "object", "properties": { "command": { "type": "string", "description": "Shell command." } }, "required": ["command"] }),
        );
        bash.returns = Some("{ output: string; exit_code: number }".into());
        let mut search = tool("mcp__docs__search-all", json!({ "type": "object", "properties": { "q": { "type": "string" } } }));
        search.namespace = Some("mcp__docs".into());
        let mut deferred = tool("mcp__later__thing", json!({ "type": "object" }));
        deferred.namespace = Some("mcp__later".into());
        deferred.deferred = true;
        let namespace = |name: &str| Some(Namespace { name: name.into(), description: Some("The docs.".into()), instructions: None });

        let tools = [search.clone(), bash.clone(), deferred.clone()];
        let text = description(&tools, namespace);
        let listed = text.strip_prefix(INTRO).unwrap();
        assert_eq!(
            listed,
            "\n\nTools (declared to you as well: a script passes the same arguments):\n\n\
             ```ts\ndeclare const tools: {\n  bash(args: { command: string; }): Promise<{ output: string; exit_code: number }>;\n};\n```\n\n\
             ## mcp__docs\nThe docs.\n\n\
             ```ts\ndeclare const tools: {\n  mcp__docs__search_all(args: { q?: string; }): Promise<string>; // mcp__docs__search-all\n};\n```"
        );
        // The same tools give the same text, and deferred ones change nothing.
        assert_eq!(description(&tools, namespace), text);
        assert_eq!(description(&[search, bash], namespace), text);
        assert_eq!(description(&[deferred], namespace), INTRO);
    }

    #[test]
    fn the_listing_keeps_to_its_budget() {
        // 80 characters a line: 20 tokens. The budget is spent round by round, a tool of each
        // group at a time, so the small namespace is whole and the big group is cut.
        let named = |i: usize, namespace: Option<&str>| {
            let name = format!("tool_{i:03}_{}", "x".repeat(41));
            let mut spec = tool(&name, json!({ "type": "object", "properties": {}, "additionalProperties": false }));
            spec.namespace = namespace.map(String::from);
            spec
        };
        let mut tools: Vec<ToolSpec> = (0..200).map(|i| named(i, None)).collect();
        tools.extend((0..3).map(|i| named(i, Some("mcp__small"))));
        let text = description(&tools, |_| None);
        let lines = text.lines().filter(|line| line.starts_with("  tool_")).count();
        assert_eq!(lines, INLINE_BUDGET / 20);
        assert!(text.contains("## mcp__small\n\n```ts"), "{text}");
        assert!(text.contains("tool_146_") && !text.contains("tool_147_"));

        let costs = [vec![5, 1, 3], vec![2, 2], vec![9]];
        assert_eq!(select(&costs, 9), [vec![false, true, true], vec![true, true], vec![false]]);
    }
}
