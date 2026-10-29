// Builds the runtime into a standalone executable, for installs (which have no bun):
//
//   bun run build [--target bun-windows-x64] [--out DIR]
//
// DIR (default: `dist/runtime` at the repo root) gets `dirigent_runtime[.exe]` and the files
// a compiled pi reads from next to its executable (`getPackageDir` in pi's config): its
// package.json, README, docs and examples (the system prompt points the model at them),
// themes, and photon's wasm (image resizing). Installs put DIR next to the desktop binary
// as `runtime/`, where the service finds it (`runtime_cmd` in dirigent_service).

import { $ } from "bun";
import { cp, mkdir, rm } from "node:fs/promises";
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

console.log(`built ${out}`);
