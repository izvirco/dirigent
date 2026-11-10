// Builds the runtime into a standalone executable, for installs (which have no bun):
//
//   bun run build [--target bun-windows-x64] [--out DIR]
//
// DIR (default: `dist/runtime` at the repo root) gets `dirigent_runtime[.exe]` and the files
// a compiled pi reads from next to its executable (`getPackageDir` in pi's config): its
// package.json, README, docs and examples (the system prompt points the model at them),
// themes, photon's wasm (image resizing) and pi-ai's codex module as source (below). Installs
// put DIR next to the desktop binary as `runtime/`, where the service finds it (`runtime_cmd`
// in dirigent_service).

import { $ } from "bun";
import { cp, mkdir, rm } from "node:fs/promises";
import { isBuiltin } from "node:module";
import { dirname, join } from "node:path";

const args = process.argv.slice(2);
const option = (name: string) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : undefined;
};
// A bun target (`bun-windows-x64`, `bun-linux-x64`), default this machine's.
const target = option("--target");
const out = option("--out") ?? join(import.meta.dir, "..", "dist", "runtime");
const windows = target ? target.includes("windows") : process.platform === "win32";

await rm(out, { recursive: true, force: true });
await mkdir(out, { recursive: true });
const exe = join(out, windows ? "dirigent_runtime.exe" : "dirigent_runtime");
const targetArgs = target ? ["--target", target] : [];
await $`bun build --compile --no-compile-autoload-bunfig --no-compile-autoload-dotenv ${targetArgs} ${join(import.meta.dir, "src", "index.ts")} --outfile ${exe}`;

// The package's entry is `dist/index.js`: its root is two levels up.
const pi = dirname(dirname(Bun.resolveSync("@earendil-works/pi-coding-agent", import.meta.dir)));
for (const name of ["package.json", "README.md", "CHANGELOG.md", "docs", "examples"]) {
  await cp(join(pi, name), join(out, name), { recursive: true });
}
await cp(join(pi, "dist", "modes", "interactive", "theme"), join(out, "theme"), {
  recursive: true,
});
const photon = dirname(Bun.resolveSync("@silvia-odwyer/photon-node", pi));
await cp(join(photon, "photon_rs_bg.wasm"), join(out, "photon_rs_bg.wasm"));

// The CLIProxyAPI provider extension (`@router-for-me/pi-cliproxyapi-provider`) makes its
// stream by patching the source of pi-ai's codex module and importing the result, which
// then imports that module's own imports from disk. A compiled pi has no source on disk, so
// ship that one module and all it reaches (some 40 small files) as `node_modules/` next to
// the executable, one of the places the extension looks. Flat: these packages have one
// version each.
const scanner = new Bun.Transpiler({ loader: "js" });
const queue = [
  join(dirname(Bun.resolveSync("@earendil-works/pi-ai", pi)), "api", "openai-codex-responses.js"),
];
const copied = new Set<string>();
while (queue.length > 0) {
  const file = queue.pop()!;
  if (copied.has(file)) continue;
  copied.add(file);
  // `<root>/<package>/<path>`, the root being the last `node_modules` of the install's layout.
  const [, root, name, path] =
    file.match(/^(.*[\\/]node_modules)[\\/]((?:@[^\\/]+[\\/])?[^\\/]+)[\\/](.*)$/) ?? [];
  if (!root || !name || !path) throw new Error(`not in node_modules: ${file}`);
  await cp(file, join(out, "node_modules", name, path));
  // The package.json says how the package's files load (`type`, `exports`).
  queue.push(join(root, name, "package.json"));
  if (!/\.[cm]?js$/.test(file)) continue;
  for (const { path: specifier } of scanner.scanImports(await Bun.file(file).text())) {
    if (!isBuiltin(specifier)) queue.push(Bun.resolveSync(specifier, dirname(file)));
  }
}

console.log(`built ${out}`);
