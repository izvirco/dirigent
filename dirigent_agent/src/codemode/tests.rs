//! The tool end to end: real scripts in real VMs, with a fake session behind them.

use super::*;
use std::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// As much of a PNG as `image()` looks at: its signature.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n and the rest";

#[derive(Default)]
struct FakeHost {
    stored: Mutex<BTreeMap<String, Value>>,
    /// What the tools were up to.
    log: Mutex<Vec<String>>,
}

impl FakeHost {
    fn log(&self, line: String) {
        self.log.lock().unwrap().push(line);
    }
}

fn spec(name: &str, description: &str, properties: Value) -> ToolSpec {
    ToolSpec::new(name, description, json!({ "type": "object", "properties": properties }))
}

#[async_trait]
impl ScriptHost for FakeHost {
    fn tools(&self) -> Vec<ToolSpec> {
        let mut bash = spec("bash", "Run a shell command.", json!({ "command": { "type": "string" } }));
        bash.returns = Some("{ output: string; exit_code: number }".into());
        // An MCP server's tools: only scripts call them.
        let mut search = spec(
            "mcp__docs__search-pages",
            "Search the documentation.",
            json!({ "query": { "type": "string", "description": "What to look for." } }),
        );
        search.guidelines = vec!["Quote exact phrases.".into()];
        let mut fetch = spec("mcp__docs__fetch", "Fetch one page of the documentation.", json!({ "id": { "type": "string" } }));
        for tool in [&mut search, &mut fetch] {
            (tool.namespace, tool.deferred) = (Some("mcp__docs".into()), true);
        }
        let plain = ["echo", "sleep", "fail", "hang", "read"].map(|name| spec(name, &format!("The {name} tool."), json!({})));
        plain.into_iter().chain([bash, search, fetch]).collect()
    }

    async fn call(&self, name: &str, input: Value, call: &Call) -> ToolOutput {
        assert!(call.script);
        self.log(format!("{name} called"));
        match name {
            "echo" | "mcp__docs__search-pages" => ToolOutput::text(input.to_string()),
            "sleep" => {
                tokio::time::sleep(Duration::from_millis(input["ms"].as_u64().unwrap())).await;
                ToolOutput::text("slept")
            }
            "fail" => ToolOutput::error("boom"),
            "hang" => {
                call.cancel.cancelled().await;
                self.log("hang cancelled".into());
                ToolOutput::error("cancelled")
            }
            "read" => ToolOutput {
                content: vec![Content::Text("a picture".into()), Content::Image { mime: "image/png".into(), data: PNG.to_vec() }],
                ..ToolOutput::default()
            },
            // A value of its own, also when the command failed.
            "bash" => ToolOutput { value: Some(json!({ "output": "nope\n", "exit_code": 1 })), ..ToolOutput::error("nope") },
            _ => ToolOutput::error(format!("no tool {name}")),
        }
    }

    fn namespace(&self, name: &str) -> Option<Namespace> {
        let known = Namespace {
            name: name.into(),
            description: Some("The documentation.".into()),
            instructions: Some("Search before you fetch.".into()),
        };
        (name == "mcp__docs").then_some(known)
    }

    fn stored(&self) -> BTreeMap<String, Value> {
        self.stored.lock().unwrap().clone()
    }

    fn store(&self, values: BTreeMap<String, Value>) {
        let mut stored = self.stored.lock().unwrap();
        for (key, value) in values {
            if value.is_null() {
                stored.remove(&key)
            } else {
                stored.insert(key, value)
            };
        }
    }
}

struct Fixture {
    host: Arc<FakeHost>,
    tool: Codemode,
    /// The user's stop.
    cancel: CancellationToken,
    _files: tempfile::TempDir,
}

impl Fixture {
    /// With one VM at a time: a script only starts once the one before has given its place
    /// back, so the tests also see that a VM's thread ends, whichever way its script did.
    fn new() -> Self {
        let (host, files) = (Arc::new(FakeHost::default()), tempfile::tempdir().unwrap());
        let tool = Codemode { host: host.clone(), files: files.path().into(), slots: Box::leak(Box::new(Semaphore::new(1))) };
        Self { host, tool, cancel: CancellationToken::new(), _files: files }
    }

    async fn run(&self, code: &str) -> ToolOutput {
        let call = Call { id: "call-1".into(), cancel: self.cancel.clone(), script: false };
        self.tool.call(json!({ "code": code }), &call).await.unwrap()
    }

    /// What a script that completes puts out.
    async fn output(&self, code: &str) -> String {
        let out = self.run(code).await;
        assert!(!out.is_error, "{}", out.as_text());
        body(&out)
    }

    /// What a script that fails leaves: its output, then its error.
    async fn failure(&self, code: &str) -> String {
        let out = self.run(code).await;
        assert!(out.is_error, "{}", out.as_text());
        body(&out)
    }

    fn log(&self) -> Vec<String> {
        self.host.log.lock().unwrap().clone()
    }

    /// Wait until the script got as far as calling `tool`.
    async fn called(&self, tool: &str) {
        while !self.log().contains(&format!("{tool} called")) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

/// What follows a result's header.
fn body(out: &ToolOutput) -> String {
    let text = out.as_text();
    let mut lines = text.splitn(4, '\n');
    assert_eq!(lines.next(), Some(if out.is_error { "Script failed" } else { "Script completed" }));
    let wall = lines.next().unwrap();
    assert!(wall.starts_with("Wall time ") && wall.ends_with(" seconds"), "{wall}");
    assert_eq!(lines.next(), Some("Output:"));
    lines.next().unwrap_or_default().to_string()
}

#[tokio::test]
async fn a_script_puts_out_text_its_return_and_console_lines() {
    let fixture = Fixture::new();
    assert_eq!(fixture.output("return 1 + 1").await, "2");
    assert_eq!(fixture.output("const answer = await Promise.resolve('fine');\nreturn answer").await, "fine");
    assert_eq!(fixture.output("const nothing = 1").await, "");

    let code = r#"text("a"); console.log("c", { d: 2 }, [1]); text({ b: null }); console.warn("w", 1.5, undefined); return [1, "2"];"#;
    let laid_out = "==> text 1/3 <==\na\n==> text 2/3 <==\n{\"b\":null}\n==> text 3/3 <==\n[1,\"2\"]\n<console_output>\nc {\"d\":2} [1]\nw 1.5 undefined\n</console_output>";
    assert_eq!(fixture.output(code).await, laid_out);

    assert_eq!(fixture.output("text('kept'); exit(); text('never')").await, "kept");
    // Text leaves the VM as UTF-8.
    assert_eq!(fixture.output(r#"return "a\ud800b""#).await, "a\u{fffd}b");
    // No Node, no timers, no network.
    assert_eq!(
        fixture.output("return [typeof setTimeout, typeof require, typeof process, typeof fetch].join()").await,
        "undefined,undefined,undefined,undefined"
    );
}

#[tokio::test]
async fn the_built_ins_cannot_be_patched_but_classes_work() {
    let fixture = Fixture::new();
    let code = r#"
        Array.prototype.join = () => "patched";
        JSON.stringify = () => "patched";
        class Mine extends Error { constructor() { super("mine"); this.name = "Mine"; } }
        text([1, 2].join("-"));
        throw new Mine();
    "#;
    assert!(fixture.failure(code).await.starts_with("1-2\nScript error:\nMine: mine\n    at "));
}

#[tokio::test]
async fn tool_calls_run_side_by_side() {
    let fixture = Fixture::new();
    let started = Instant::now();
    let code =
        "const all = await Promise.all(Array.from({ length: 30 }, () => tools.sleep({ ms: 200 })));\nreturn all.length + ' ' + all[29]";
    assert_eq!(fixture.output(code).await, "30 slept");
    // One after the other they take 6 seconds.
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
}

#[tokio::test]
async fn a_call_resolves_to_the_tools_value_its_image_or_its_text() {
    let fixture = Fixture::new();
    let code = r#"
        const picture = await tools.read({ path: "p.png" });
        image(picture);
        const { data, ...block } = picture;
        return {
            text: await tools.echo({ a: [1, "two"] }),
            bare: await tools.echo(),
            value: await tools.bash({ command: "false" }),
            block,
            dashed: await tools.mcp__docs__search_pages({ q: 1 }),
            same: tools["mcp__docs__search-pages"] === tools.mcp__docs__search_pages,
        };
    "#;
    let out = fixture.run(code).await;
    let [Content::Text(saved), Content::Image { mime, data }, Content::Text(returned)] = out.content.as_slice() else {
        panic!("{:?}", out.content)
    };
    assert_eq!((mime.as_str(), data.as_slice()), ("image/png", PNG));
    let path = saved.rsplit_once("\n[Image saved to ").unwrap().1.strip_suffix(" (image/png, 21B)]").unwrap();
    assert_eq!(std::fs::read(path).unwrap(), PNG);
    let expected = json!({
        "text": "{\"a\":[1,\"two\"]}",
        "bare": "{}",
        "value": { "output": "nope\n", "exit_code": 1 },
        "block": { "type": "image", "mimeType": "image/png", "note": "a picture" },
        "dashed": "{\"q\":1}",
        "same": true,
    });
    assert_eq!(serde_json::from_str::<Value>(returned).unwrap(), expected);
}

#[tokio::test]
async fn a_failed_call_rejects_and_a_failed_script_keeps_its_output() {
    let fixture = Fixture::new();
    let code = r#"
        text("before");
        const settled = await Promise.allSettled([tools.fail({}), tools.echo({})]);
        text(settled.map((call) => call.status + ":" + (call.reason?.message ?? call.value)).join(" "));
        await tools.fail({});
        text("never");
    "#;
    assert_eq!(
        fixture.failure(code).await,
        "==> text 1/2 <==\nbefore\n==> text 2/2 <==\nrejected:boom fulfilled:{}\nScript error:\nError: boom\n\n\
         Tool calls made before the failure (they are not undone): 1 ok, 2 failed (fail ×2)"
    );
    let unknown = fixture.failure("await tools.ech({})").await;
    assert!(unknown.starts_with("Script error:\nTypeError: tools.ech does not exist. Did you mean tools.echo?"), "{unknown}");
    assert!(unknown.ends_with("\n\nNo tool calls were made."), "{unknown}");
}

#[tokio::test]
async fn a_script_that_cannot_go_on_fails() {
    let fixture = Fixture::new();
    let stalled = fixture.failure("text('partial');\nawait new Promise(() => {})").await;
    assert!(stalled.starts_with("partial\nScript error:\nError: The script is waiting on a promise that can never settle"), "{stalled}");
    let syntax = fixture.failure("const x = 1;\nconst = 2;").await;
    assert!(syntax.starts_with("Script error:\nSyntaxError: ") && syntax.contains("codemode.js:2"), "{syntax}");
    // The options line is not cut off: the script's lines keep their numbers.
    let thrown = fixture.failure("// @options: {}\n\nthrow new TypeError('bad')").await;
    assert!(thrown.starts_with("Script error:\nTypeError: bad\n    at ") && thrown.contains("codemode.js:3"), "{thrown}");
    assert!(fixture.failure("throw { code: 7 }").await.starts_with("Script error:\nError: {\"code\":7}\n\n"));
}

#[tokio::test]
async fn what_a_script_stores_is_kept_only_when_it_succeeds() {
    let fixture = Fixture::new();
    let stored = || fixture.host.stored();
    assert_eq!(fixture.output(r#"store("a", { n: 1 }); store("b", [2]); return load("a").n + load("b")[0]"#).await, "3");
    assert_eq!(stored(), BTreeMap::from([("a".into(), json!({ "n": 1 })), ("b".into(), json!([2]))]));

    fixture.failure(r#"store("a", "lost"); throw new Error("no")"#).await;
    assert_eq!(stored()["a"], json!({ "n": 1 }));

    // A later script reads what was kept, and forgets a key with undefined or null.
    let code = r#"store("b", undefined); store("c", null); return [load("a"), load("b") === undefined, load("c") === undefined]"#;
    assert_eq!(fixture.output(code).await, r#"[{"n":1},true,true]"#);
    assert_eq!(stored(), BTreeMap::from([("a".into(), json!({ "n": 1 }))]));

    let too_big = fixture.failure(r#"store("big", "x".repeat(262144))"#).await;
    assert!(
        too_big.contains("RangeError: store(\"big\") value has 262146 characters of JSON, more than the limit of 262144."),
        "{too_big}"
    );
}

/// JavaScript's JSON is not all serde_json reads.
#[tokio::test]
async fn what_crosses_to_the_host_is_json_it_reads() {
    let fixture = Fixture::new();
    // Half an emoji, as a string cut in the middle of one ends, in keys and in values. The
    // letters of its escape are text like any other.
    let code = r#"
        const cut = "cut \ud83d";
        store(cut, [cut]);
        return [await tools.echo({ [cut]: cut, text: "\\ud83d" }), load(cut)];
    "#;
    let cut = "cut \u{fffd}";
    let returned: Value = serde_json::from_str(&fixture.output(code).await).unwrap();
    assert_eq!(returned, json!([json!({ cut: cut, "text": "\\ud83d" }).to_string(), [cut]]));
    assert_eq!(fixture.host.stored(), BTreeMap::from([(cut.into(), json!([cut]))]));

    // A value nested deeper than serde_json reads costs its own key, not the script or its
    // other keys; as a call's arguments, the call.
    let code = r#"
        let deep = [];
        for (let i = 0; i < 200; i++) deep = [deep];
        store("deep", deep);
        store("fine", 1);
        return (await Promise.allSettled([tools.echo({ deep })]))[0].reason.message;
    "#;
    let out = fixture.output(code).await;
    let (returned, console) = out.split_once("\n<console_output>\n").unwrap();
    assert!(returned.starts_with("The arguments could not be read as JSON: recursion limit exceeded"), "{returned}");
    assert!(
        console.starts_with("store(\"deep\") was not kept: its value could not be read as JSON: recursion limit exceeded"),
        "{console}"
    );
    assert_eq!(fixture.host.stored().into_keys().collect::<Vec<_>>(), [cut, "fine"]);
}

#[test]
fn a_failed_scripts_calls_are_summed_up() {
    assert_eq!(summary(&[]), "No tool calls were made.");
    // Thousands of calls are a line still: the tools are named for those that did not go
    // well, and not more than a few of them.
    let mut calls: Vec<(String, &str)> = (0..5000).map(|_| ("echo".to_string(), "ok")).collect();
    calls.extend((0..10).map(|i| (format!("tool_{i}"), "failed")));
    calls.extend([("hang".to_string(), "cancelled"), ("tool_0".to_string(), "failed"), ("hang".to_string(), "cancelled")]);
    assert_eq!(
        summary(&calls),
        "Tool calls made before the failure (they are not undone): 5000 ok, \
         11 failed (tool_0 ×2, tool_1, tool_2, tool_3, tool_4, tool_5, tool_6, tool_7, and 2 more tools), 2 cancelled (hang ×2)"
    );
}

#[tokio::test]
async fn output_is_cut_to_its_budget_and_ends_at_its_limits() {
    let fixture = Fixture::new();
    let cut = fixture.output("// @options: {\"max_output_tokens\": 10}\ntext('a'.repeat(500) + 'b'.repeat(500))").await;
    let (shown, saved) = cut.rsplit_once("\n\n").unwrap();
    let kept = format!("{}…240 tokens truncated…{}", "a".repeat(20), "b".repeat(20));
    assert_eq!(shown, format!("Warning: truncated output (original token count: 250)\nTotal output lines: 1\n\n{kept}"));
    let path = saved.strip_prefix("[Full output: ").unwrap().strip_suffix(" (read with offset/limit)]").unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), "a".repeat(500) + &"b".repeat(500));

    // Past the hard limits the script fails, also when it catches the error.
    for code in
        ["for (let i = 0; i <= 100000; i++) { try { text('') } catch {} }", "try { text('x'.repeat(16777217)) } catch {}\ntext('more')"]
    {
        let failed = fixture.failure(&format!("// @options: {{\"max_output_tokens\": 1000}}\n{code}")).await;
        assert!(
            failed.contains(
                "RangeError: script output exceeded the limit of 16777216 characters or 100000 text(), image(), and console calls."
            ),
            "{failed}"
        );
    }
}

/// On the one thread of the test's runtime: a script that spins must not hold it.
#[tokio::test]
async fn a_deadline_ends_a_script_that_spins_or_waits() {
    let fixture = Fixture::new();
    for (spin, calls) in [
        ("while (true) {}", "No tool calls were made."),
        ("while (true) await null", "No tool calls were made."),
        ("await tools.hang({})", "Tool calls made before the failure (they are not undone): 1 cancelled (hang)"),
    ] {
        let started = Instant::now();
        let code = format!("// @options: {{\"timeout_ms\": 300}}\ntext('partial');\n{spin}");
        let beat = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            started.elapsed()
        };
        let (failed, beat) = tokio::join!(fixture.failure(&code), beat);
        assert_eq!(failed, format!("partial\nScript error:\nScript timed out: Execution timed out after 300 ms\n\n{calls}"));
        assert!(beat < Duration::from_millis(300) && started.elapsed() < Duration::from_secs(3), "{beat:?} {:?}", started.elapsed());
    }
    // The call the script waited on was told to end.
    assert_eq!(fixture.log(), ["hang called", "hang cancelled"]);
}

#[tokio::test]
async fn stopping_the_run_ends_the_script_and_its_calls() {
    // While it waits on a tool.
    let fixture = Fixture::new();
    let stop = async {
        fixture.called("hang").await;
        fixture.cancel.cancel();
    };
    let (failed, ()) = tokio::join!(fixture.failure("text('partial');\nawait tools.hang({});\ntext('never')"), stop);
    assert_eq!(
        failed,
        "partial\nScript error:\nScript aborted: Execution aborted\n\nTool calls made before the failure (they are not undone): 1 cancelled (hang)"
    );
    assert_eq!(fixture.log(), ["hang called", "hang cancelled"]);
    // Once stopped, a script does not start.
    assert_eq!(fixture.failure("return 1").await, "Script error:\nScript aborted: Execution aborted\n\nThe script did not start.");

    // While it spins.
    let mut fixture = Fixture::new();
    let started = Instant::now();
    let stop = async {
        fixture.called("echo").await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        fixture.cancel.cancel();
    };
    let (failed, ()) = tokio::join!(fixture.failure("text('partial');\nawait tools.echo({});\nwhile (true) {}"), stop);
    assert_eq!(
        failed,
        "partial\nScript error:\nScript aborted: Execution aborted\n\nTool calls made before the failure (they are not undone): 1 ok"
    );
    assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
    // The VM stopped spinning: the next script gets its place.
    fixture.cancel = CancellationToken::new();
    assert_eq!(fixture.output("return 1").await, "1");
}

#[tokio::test]
async fn a_vm_has_limited_memory_and_stack() {
    let fixture = Fixture::new();
    let memory = fixture.failure("const kept = [];\nwhile (true) kept.push(new Uint8Array(16 << 20));").await;
    assert!(memory.starts_with("Script error:\nInternalError: out of memory"), "{memory}");
    // Filled to the last bit there is no memory left for that error, and what is thrown is
    // null: to the script's promise, or, once it has awaited, to nobody at all.
    for awaited in ["", "await tools.echo({});\n"] {
        let code = format!("const kept = [new Uint8Array(240 << 20)];\n{awaited}while (true) kept.push({{ n: kept.length }});");
        let memory = fixture.failure(&code).await;
        assert!(
            memory.starts_with("Script error:\nError: null was thrown: the script most likely ran out of memory (it has 256 MB)\n\n"),
            "{memory}"
        );
    }
    let stack = fixture.failure("const down = () => down();\ndown();").await;
    assert!(stack.starts_with("Script error:\nRangeError: Maximum call stack size exceeded"), "{stack}");
    // Both are the script's to catch, and the next script finds nothing changed.
    assert_eq!(fixture.output("const down = () => down();\ntry { down() } catch (error) { return error.name }").await, "RangeError");
}

#[tokio::test]
async fn a_script_waits_for_a_free_vm_within_its_deadline() {
    let mut fixture = Fixture::new();
    let second = async {
        fixture.called("hang").await;
        let failed = fixture.failure("// @options: {\"timeout_ms\": 50}\nreturn 1").await;
        fixture.cancel.cancel();
        failed
    };
    let (_, failed) = tokio::join!(fixture.run("await tools.hang({})"), second);
    assert_eq!(
        failed,
        "Script error:\nScript timed out: Execution timed out after 50 ms while waiting for a free codemode VM\n\nThe script did not start."
    );
    // The stopped script's VM gives its place back.
    fixture.cancel = CancellationToken::new();
    assert_eq!(fixture.output("return 1").await, "1");
}

#[tokio::test]
async fn a_script_finds_the_tools_it_was_not_told_of() {
    let fixture = Fixture::new();
    let code = r#"
        const found = await searchTools("search documentation");
        const within = await searchTools("page", { namespace: "mcp__docs", limit: 1 });
        return {
            all: ALL_TOOLS.map((tool) => tool.name),
            found: found.map((tool) => tool.name),
            within: within.map((tool) => tool.name),
            described: await describeTool("mcp__docs__search-pages"),
            same: (await describeTool("mcp__docs__search_pages")) === found[0].description,
            namespace: await describeNamespace("mcp__docs"),
            by_server: (await describeNamespace("docs")).name,
            unknown: [await describeTool("nope"), await describeNamespace("nope")],
        };
    "#;
    let expected = json!({
        "all": ["echo", "sleep", "fail", "hang", "read", "bash", "mcp__docs__search_pages", "mcp__docs__fetch"],
        "found": ["mcp__docs__search_pages", "mcp__docs__fetch"],
        "within": ["mcp__docs__search_pages"],
        "described": "Search the documentation.\n\n- Quote exact phrases.\n\ncodemode tool declaration:\n```ts\ndeclare const tools: { mcp__docs__search_pages(args: {\n  // What to look for.\n  query?: string;\n}): Promise<string>; };\n```",
        "same": true,
        "namespace": { "name": "mcp__docs", "description": "The documentation.", "instructions": "Search before you fetch.", "tools": ["mcp__docs__search_pages", "mcp__docs__fetch"] },
        "by_server": "mcp__docs",
        "unknown": [null, null],
    });
    assert_eq!(serde_json::from_str::<Value>(&fixture.output(code).await).unwrap(), expected);
}

#[test]
fn the_spec_lists_the_tools_declared_to_the_model() {
    let fixture = Fixture::new();
    let spec = fixture.tool.spec();
    assert_eq!((spec.name.as_str(), spec.deferred, &spec.input["required"]), ("codemode", false, &json!(["code"])));
    assert!(
        spec.description.contains("\n  bash(args: { command?: string; }): Promise<{ output: string; exit_code: number }>;\n"),
        "{}",
        spec.description
    );
    // An MCP server's tools are for scripts to find: connecting one leaves the text as it was.
    assert!(!spec.description.contains("mcp__docs"));
    assert_eq!(spec, fixture.tool.spec());
}

#[test]
fn the_first_line_may_set_options() {
    assert_eq!(Source::parse("return 1").unwrap(), Source { code: "return 1", max_output_tokens: 10_000, timeout: None });
    let source = Source::parse("  // @options: {\"max_output_tokens\": 5, \"timeout_ms\": 1e3}\r\nreturn 1").unwrap();
    assert_eq!(source, Source { code: "\nreturn 1", max_output_tokens: 5, timeout: Some(Duration::from_secs(1)) });
    for (input, error) in [
        ("  \n", "Expected JavaScript source text"),
        ("return 1\n// @options: {}", "must be the first line of the input; found one on line 2"),
        ("// @options: {}\n ", "must be followed by JavaScript source"),
        ("// @options:\nreturn 1", "@options must be a JSON object"),
        ("// @options: [1]\nreturn 1", "@options must be a JSON object"),
        ("// @options: {max}\nreturn 1", "@options must be valid JSON"),
        ("// @options: {\"timeout\": 1}\nreturn 1", "got `timeout`"),
        ("// @options: {\"timeout_ms\": 0}\nreturn 1", "`timeout_ms` must be a positive integer"),
        ("// @options: {\"max_output_tokens\": -1}\nreturn 1", "`max_output_tokens` must be a non-negative integer"),
    ] {
        let message = Source::parse(input).unwrap_err().to_string();
        assert!(message.contains(error), "{message}");
    }
}
