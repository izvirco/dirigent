// Builds the runtime into a standalone executable, for installs (which have no bun):
//
//   bun run build [--target bun-windows-x64] [--out DIR] [--no-driver]
//
// DIR (default: `dist/runtime` at the repo root) gets `dirigent_runtime[.exe]` and the files
// a compiled pi reads from next to its executable (`getPackageDir` in pi's config): its
// package.json, README, docs and examples (the system prompt points the model at them),
// themes, photon's wasm (image resizing) and pi-ai's codex module as source (below). And, on
// Linux and Windows, Cua Driver (`cua-driver[.exe]`), which computer use runs
// (`src/computer.ts`): downloaded from its release, unless `--no-driver` (the nix package
// brings its own). Installs put DIR next to the desktop binary as `runtime/`, where the
// service finds it (`runtime_cmd` in dirigent_service).

import { $ } from "bun";
import { createHash } from "node:crypto";
import { chmod, cp, mkdir, rm, writeFile } from "node:fs/promises";
import { isBuiltin } from "node:module";
import { dirname, join } from "node:path";
import { gunzipSync, inflateRawSync } from "node:zlib";

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
// The codemode worker is a second entrypoint, which a compiled pi finds as
// `./src/extensions/codemode/worker.ts` from the root, hence `--root`.
const entries = ["index.ts", "extensions/codemode/worker.ts"].map((f) =>
  join(import.meta.dir, "src", f),
);
await $`bun build --compile --no-compile-autoload-bunfig --no-compile-autoload-dotenv ${targetArgs} --root ${import.meta.dir} ${entries} --outfile ${exe}`;

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

/**
 * The Cua Driver release installs ship (https://github.com/trycua/cua, MIT): its version and
 * the sha256 of each platform's archive, which holds the one executable we take. `flake.nix`
 * (`cuaDriver`) names the same version: bump both together. Not on macOS: the system gives
 * screen and accessibility access to a signed app, which a bare executable in our tarball
 * isn't, so computer use there runs the user's own install (`packaging/README.md`).
 */
const DRIVER = {
  version: "0.51.3",
  archives: {
    "linux-x64": {
      name: "linux-x86_64-binary.tar.gz",
      sha256: "044466f65db3976fa1ef302a3427ea12d5d4edd969263788d2fc16d793b95dde",
    },
    "windows-x64": {
      name: "windows-x86_64-binary.zip",
      sha256: "3b6c81a8b4063b1eadea4cdc9a35481085e355ee1a6ffedb52903d354add1ae7",
    },
  } as Record<string, { name: string; sha256: string }>,
};

/** The file `name` out of a tar archive: 512-byte headers, each followed by its file's blocks. */
function untar(tar: Buffer, name: string): Buffer | undefined {
  for (let at = 0; at + 512 <= tar.length;) {
    const header = tar.subarray(at, at + 512);
    const path = header.subarray(0, 100).toString().replace(/\0.*$/, "");
    if (!path) return undefined;
    const size = parseInt(header.subarray(124, 136).toString(), 8);
    if (path.replace(/^\.\//, "") === name) return tar.subarray(at + 512, at + 512 + size);
    at += 512 + Math.ceil(size / 512) * 512;
  }
  return undefined;
}

/** The file `name` out of a zip archive, by its central directory. */
function unzip(zip: Buffer, name: string): Buffer | undefined {
  const end = zip.lastIndexOf(Buffer.from([0x50, 0x4b, 0x05, 0x06]));
  if (end < 0) return undefined;
  let at = zip.readUInt32LE(end + 16);
  for (let n = zip.readUInt16LE(end + 10); n > 0; n--) {
    const [method, packed] = [zip.readUInt16LE(at + 10), zip.readUInt32LE(at + 20)];
    const lengths = [28, 30, 32].map((o) => zip.readUInt16LE(at + o));
    const [nameLength, extraLength, commentLength] = lengths as [number, number, number];
    if (zip.subarray(at + 46, at + 46 + nameLength).toString() === name) {
      // The file's own header repeats the name and has an extra field of its own length.
      const local = zip.readUInt32LE(at + 42);
      const data = local + 30 + zip.readUInt16LE(local + 26) + zip.readUInt16LE(local + 28);
      const bytes = zip.subarray(data, data + packed);
      if (method !== 0 && method !== 8) throw new Error(`${name}: zip method ${method}`);
      return method === 8 ? inflateRawSync(bytes) : bytes;
    }
    at += 46 + nameLength + extraLength + commentLength;
  }
  return undefined;
}

const platform = target
  ? target.replace(/^bun-/, "").replace(/-(baseline|modern)$/, "")
  : `${process.platform === "win32" ? "windows" : process.platform}-${process.arch}`;
const archive = DRIVER.archives[platform];
if (args.includes("--no-driver")) {
  // Whoever builds puts one next to the runtime, or computer use takes one from PATH.
} else if (!archive) {
  console.log(`no Cua Driver for ${platform}: computer use will need one on PATH`);
} else {
  const release = `https://github.com/trycua/cua/releases/download/cua-driver-rs-v${DRIVER.version}`;
  const url = `${release}/cua-driver-rs-${DRIVER.version}-${archive.name}`;
  const response = await fetch(url);
  if (!response.ok) throw new Error(`${url}: ${response.status}`);
  const bytes = Buffer.from(await response.arrayBuffer());
  const sha256 = createHash("sha256").update(bytes).digest("hex");
  if (sha256 !== archive.sha256) throw new Error(`${url}: sha256 is ${sha256}`);
  const file = windows ? "cua-driver.exe" : "cua-driver";
  const driver = windows ? unzip(bytes, file) : untar(gunzipSync(bytes), file);
  if (!driver) throw new Error(`${url}: no ${file} in it`);
  await writeFile(join(out, file), driver);
  await chmod(join(out, file), 0o755);
  // Its MIT license goes along with it.
  const license = await fetch(
    `https://raw.githubusercontent.com/trycua/cua/cua-driver-rs-v${DRIVER.version}/LICENSE.md`,
  );
  if (!license.ok) throw new Error(`Cua Driver's license: ${license.status}`);
  await writeFile(join(out, "cua-driver-LICENSE.md"), await license.text());
}

console.log(`built ${out}`);
