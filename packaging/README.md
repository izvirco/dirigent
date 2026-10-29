# Packaging, releases and updates

## What an install is (Windows)

`Dirigent-Setup-<version>.exe` (Inno Setup, `windows/dirigent.iss`) installs per user, without admin,
into `%LOCALAPPDATA%\Programs\Dirigent`:

```
Dirigent.exe                 dirigent_desktop, with Lilex, the icon and version info built in (build.rs)
runtime\dirigent_runtime.exe the bun runtime compiled to one exe (dirigent_runtime/build.ts), no bun needed
runtime\package.json, docs\, …   what a compiled pi reads next to its exe
licenses\Lilex-OFL.txt
```

plus a Start menu entry, an uninstaller and, unless unticked, "Open in Dirigent" on folders (Windows 11:
under "Show more options"). User data stays where it always was (`%LOCALAPPDATA%\dirigent`, `%APPDATA%\dirigent`)
and survives uninstalling. Agents' shell commands need Git for Windows; the installer says so when it can't find
its bash.

## Updates

Release builds check `https://dirigent.sebba.dev/api/v0/updates/<channel>/<platform>` every 5 minutes
(`dirigent_desktop/src/update.rs`), download a newer installer and its minisign signature, verify both, and then
show the update button in the sidebar. Restarting runs that installer silently (`/UPDATE /WAITPID=<pid>`): it
waits for Dirigent to exit, installs, and starts it again.

The new version's installer always does the installing, and the update code ships inside every version, so both
can change in any release. What installs in the field rely on, and must stay compatible:

- the update url and the `Release` JSON (`dirigent_protocol/src/release.rs`; add fields, don't change them),
- the installer's `AppId` and the flags `update.rs` passes it,
- the signing keys (`release::KEYS`; rotate by shipping the new key next to the old one first).

## Making a release

1. Bump `[workspace.package] version` in `Cargo.toml` (semver; a version is published once per channel) and push to
   GitHub.
2. Actions › Release › Run workflow, pick the channel (`beta`), optionally notes.

Channels are independent: each is a build of its own (the update url is baked in) with its own versions. Publishing
0.2.0 to `beta` and later the same commit to `stable` gives both a 0.2.0. A machine only updates within its channel:
moving one to another channel is installing that channel's installer by hand.

The workflow (`.github/workflows/release.yml`) builds `Dirigent.exe` with the channel's update url baked in
(`DIRIGENT_BUILD_UPDATE_URL`; dev builds have none and never check), compiles the runtime, builds the installer
and runs `dirigent_api publish`, which signs it and uploads it to the bucket, the channel file last. Testers
download the latest from `https://dirigent.sebba.dev/api/v0/download/beta/windows-x86_64`. Unsigned by
Authenticode for now: SmartScreen asks once ("More info", "Run anyway"); updates aren't downloaded by a browser,
so they don't ask.

Repository settings: variables `S3_ENDPOINT`, `S3_REGION`, `S3_BUCKET`; secrets `S3_ACCESS_KEY_ID`,
`S3_SECRET_ACCESS_KEY` and `DIRIGENT_SIGNING_KEY` (the text of the secret key file below).

**The signing key.** `dirigent_api keygen <file>` writes a new secret key and prints its public key, which goes
into `release::KEYS`. The current secret key was generated on Seb's machine into
`~/.config/dirigent-release.key`: keep a copy somewhere safe. Losing it means shipping a new key to every
install with a release signed by the old one, which is then impossible.

## The update api (`dirigent_api`)

Holds no secrets: it reads the public bucket. Run it behind the TLS proxy for `dirigent.sebba.dev`, routing
`/api/v0/` to it:

```sh
S3_ENDPOINT=https://fsn1.your-objectstorage.com S3_REGION=fsn1 S3_BUCKET=dirigent \
DIRIGENT_API_BIND=127.0.0.1:7790 dirigent_api serve
```

It logs every update check with the client's version (`Dirigent/<version> (<platform>)`), which is the only
usage signal there is.

Bucket layout: `releases/<channel>/<version>/<file>` (+ `.minisig`), never overwritten;
`channels/<channel>/<platform>.json`, the channel's current release. `S3_PREFIX=test/` puts all of it under `test/`,
for trying things out without touching real releases (`cargo run -p dirigent_api -- publish …` with the bucket keys
and a signing key).

On NixOS: import the flake's `nixosModules.dirigent_api` and set `services.dirigent-api` (`enable`, `s3.endpoint`,
`s3.region`, `s3.bucket`; `bind` defaults to `127.0.0.1:7790`).

## Nix (x86_64-linux)

`flake.nix` packages all three: `dirigent_desktop` (the default), `dirigent_service` for remote hosts, and
`dirigent_api`, e.g. `nix profile install <flake url>#dirigent_service`. The desktop
and service are laid out like an install: the binary in `libexec/dirigent/` next to the compiled `runtime/`,
symlinked into `bin/`. Nix builds never self-update (no update url is baked in). The desktop needs a Vulkan
driver: on NixOS `hardware.graphics.enable`; elsewhere nix's vulkan-loader may not find the system's drivers
(nixGL).

Two hashes in `flake.nix` follow the lockfiles: `cargoDeps` (Cargo.lock) and `nodeModules` (bun.lock). After
changing a lockfile, set the hash to `lib.fakeHash` and run `nix build .#dirigent_service`, which needs both but
doesn't build gpui; it prints the right hash.

## Linux (not built yet)

Planned to fit the same pieces: a tarball (`dirigent_desktop` + `runtime/`, built by `dirigent_runtime/build.ts`
for `bun-linux-x64`) with an install script for `~/.local`, a `.desktop` file with `MimeType=inode/directory` so
file managers offer it for folders, and hicolor icons from `dirigent_desktop/assets/dirigent.png`. `update.rs`
then gets a Linux `apply`: unpack the new version next to the old, swap, restart (Linux lets a running binary be
replaced, so no installer is needed). The release workflow gains a `linux-x86_64` job publishing to the same
channels; the standalone `dirigent_service` for remote hosts can ship the same way.

## The icon

`dirigent_desktop/assets/dirigent.png` (1000 px) is the source; `dirigent.ico` is made from it with PNG entries
from 64 px up and bitmaps below:

```sh
for s in 256 128 96 64 48 40 32 24 20 16; do magick dirigent.png -filter Lanczos -resize ${s}x$s -strip i$s.png; done
icotool -c -o dirigent.ico --raw=i256.png --raw=i128.png --raw=i96.png --raw=i64.png i48.png i40.png i32.png i24.png i20.png i16.png
```
