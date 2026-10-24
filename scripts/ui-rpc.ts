// `scripts/ui.sh rpc`: one protocol request to a test session's embedded service, its reply
// printed. EXPR, if given, is JavaScript over the reply `r`; strings print raw, arrays one
// item per line, anything else as JSON. See testing.md.
//
//   bun scripts/ui-rpc.ts PORT JSON [EXPR]

const [port, json, expr] = process.argv.slice(2) as [string, string, string?];
const fail = (msg: string) => {
  console.error(msg);
  process.exit(1);
};
let req: Record<string, unknown> = {};
try {
  req = JSON.parse(json);
} catch {
  fail(`not JSON: ${json}`);
}
const id = req.id ?? 1;
const ws = new WebSocket(`ws://127.0.0.1:${port}/ws`);
ws.onopen = () => ws.send(JSON.stringify({ ...req, id }));
ws.onerror = () => fail(`no service on :${port} (is the session started?)`);
ws.onclose = () => fail(`the service on :${port} closed the connection (did the app exit?)`);
// Pushes (and the `hello`) carry no `reply_to`.
ws.onmessage = (e) => {
  const r = JSON.parse(String(e.data));
  if (r.reply_to !== id) return;
  if (r.type === "error") fail(r.message);
  let v = r;
  try {
    if (expr) v = new Function("r", `return (${expr})`)(r);
  } catch (err) {
    fail(`EXPR failed: ${err}`);
  }
  const line = (x: unknown) => (typeof x === "string" ? x : JSON.stringify(x));
  console.log(Array.isArray(v) ? v.map(line).join("\n") : line(v));
  process.exit(0);
};
setTimeout(() => fail(`no reply to ${req.type} within 30s`), 30_000);
