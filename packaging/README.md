# Packaging, releases and updates

## What an install is (Windows)

`Dirigent-Setup-<version>.exe` (Inno Setup, `windows/dirigent.iss`) installs per user, without admin,
into `%LOCALAPPDATA%\Programs\Dirigent`:

```
dirigent_desktop.exe         the app, with Lilex, the icon and version info built in (build.rs)
dirigent.exe                 the command line (below), put on the user's PATH (a ticked installer task, applied again on each update; uninstalling takes it off)
runtime\dirigent_runtime.exe the bun runtime compiled to one exe (dirigent_runtime/build.ts), no bun needed
runtime\package.json, docs\, …   what a compiled pi reads next to its exe
runtime\node_modules\        pi-ai's codex module as source: the CLIProxyAPI provider extension patches it (build.ts)
runtime\cua-driver.exe       Cua Driver, which computer use runs (below), and its license next to it
licenses\Lilex-OFL.txt
```

plus a Start menu entry, an uninstaller and, unless unticked, "Open in Dirigent" on folders (Windows 11:
under "Show more options"). User data stays where it always was (`%LOCALAPPDATA%\dirigent`, `%APPDATA%\dirigent`)
and survives uninstalling. Agents' shell commands need Git for Windows; the installer says so when it can't find
its bash.

## What an install is (Linux)

`dirigent-<version>-linux-x86_64.tar.gz` holds one directory, `dirigent/`:

```
dirigent_desktop      the app, with Lilex built in (build.rs)
dirigent              the command line (below)
runtime/              the compiled runtime, as on Windows
install.sh            linux/install.sh
dirigent.png          the icon
licenses/Lilex-OFL.txt
```

Installing, on any distro, per user and without root:

```sh
d=$(mktemp -d) && curl -fsSL https://dirigent.sebba.dev/api/v0/download/stable/linux-x86_64 | tar xz -C "$d" && "$d/dirigent/install.sh"
```

`install.sh` copies `dirigent/` to `~/.local/lib/dirigent`, links `~/.local/bin/dirigent` to the command line and
writes `~/.local/share/applications/dirigent.desktop` (the app menu entry; with `MimeType=inode/directory`, file
managers offer it in "Open with" on folders). Built on Ubuntu 22.04, so it needs glibc 2.35 or newer, plus a Vulkan
driver and `libxkbcommon-x11` (which a pure Wayland setup may lack). Uninstalling is removing those three. User data is where it always was (`~/.local/share/dirigent`,
`~/.config/dirigent`).

## What an install is (macOS)

Apple silicon only, and the Linux install as is, minus the app menu entry and the icon:
`dirigent-<version>-macos-aarch64.tar.gz` holds the same `dirigent/` (built on `macos-15`), and the same
`install.sh` (it skips the `.desktop` file on macOS) puts it in `~/.local/lib/dirigent` and links
`~/.local/bin/dirigent`, which isn't on a Mac's default PATH. User data is in the Linux places too
(`~/.local/share/dirigent`, `~/.config/dirigent`), as developer tools on macOS mostly do.

```sh
d=$(mktemp -d) && curl -fsSL https://dirigent.sebba.dev/api/v0/download/stable/macos-aarch64 | tar xz -C "$d" && "$d/dirigent/install.sh"
```

There's no `.app` yet: it starts from a terminal (`dirigent`), and quits with it. Nor a Cua Driver (computer use,
below): macOS gives screen and accessibility access to a signed app, which nothing in the tarball is.

Without an Apple developer account nothing is signed or notarized, and it doesn't need to be: Apple silicon
runs any binary with an ad-hoc signature (`codesign --sign -`, which the release job makes sure of), and
Gatekeeper only checks files marked as quarantined, which browsers do and curl, tar and the app's own updates
don't. A tarball downloaded with a browser does get marked: `xattr -dr com.apple.quarantine <dir>` before
installing, or use the curl line.

None of it is tested by us (no Mac): CI only proves it builds.

## Dirigent Nightly: an app of its own

A nightly build (a version with `-nightly.` in it, below; `dirigent_protocol::release::nightly`) is Dirigent
Nightly: it installs and runs next to a stable Dirigent, at the same time, and the two share nothing. Everything
above holds for it with these names in the place of the stable ones:

|                          | stable                                          | nightly                                        |
| ------------------------ | ----------------------------------------------- | ---------------------------------------------- |
| data dir                 | `~/.local/share/dirigent`                       | `~/.local/share/dirigent-nightly`              |
| config dir (pi's too)    | `~/.config/dirigent`                            | `~/.config/dirigent-nightly`                   |
| Windows data, config     | `%LOCALAPPDATA%\dirigent`, `%APPDATA%\dirigent` | `…\dirigent-nightly` in both                   |
| local service port       | 7777                                            | 7776                                           |
| install (Linux, macOS)   | `~/.local/lib/dirigent`                         | `~/.local/lib/dirigent-nightly`                |
| command                  | `dirigent`                                      | `dirigent-nightly`                             |
| app menu entry           | `dirigent.desktop`, Dirigent                    | `dirigent-nightly.desktop`, Dirigent Nightly   |
| window class and title   | `dirigent`, Dirigent                            | `dirigent-nightly`, Dirigent Nightly           |
| install (Windows)        | `…\Programs\Dirigent`, its own `AppId`          | `…\Programs\Dirigent Nightly`, its own `AppId` |
| folder right-click entry | Open in Dirigent                                | Open in Dirigent Nightly                       |
| icon                     | `dirigent.png` / `.ico`                         | `dirigent_nightly.png` / `.ico`                |

The app picks its dirs, port, window class and logo (`release::app`, `dirigent_service::port`); the installers
pick the rest: `install.sh` from the `nightly` file a nightly's tarball has next to it, `dirigent.iss` from the
version. Inside an install the files are named the same in both, but for the command on Windows
(`dirigent-nightly.exe`: both install dirs are on PATH there, which leaves `dirigent_desktop.exe` the one name
they share on it; nothing starts the app that way). So a nightly starts empty, as a first install
does: its own setup, sign-ins, hosts and threads (pi's agent dir is seeded from `~/.pi/agent` if there is one, as
on any first start; never from the stable install's). Its service being on 7776, another desktop reaches it as
`ws://<host>:7776/ws`.

Installing one is the stable line with `nightly` for `stable`:

```sh
d=$(mktemp -d) && curl -fsSL https://dirigent.sebba.dev/api/v0/download/nightly/linux-x86_64 | tar xz -C "$d" && "$d/dirigent/install.sh"
```

## Computer use (Cua Driver, Xvfb)

The `computer` tool (Settings › Agent tools, on by default; `dirigent_runtime/src/computer.ts`) opens a screen for
an agent to run and operate GUI apps on, when it asks for one: nothing runs before that. Two things do the work.

[Cua Driver](https://github.com/trycua/cua) (MIT) operates the screen, as an MCP server of the thread: one
executable, `cua-driver[.exe]`, which the Windows and Linux installs ship in `runtime/`.
`dirigent_runtime/build.ts` downloads it from the driver's GitHub release as it builds the runtime, checks its
sha256 and puts its license next to it; the version and hashes are pinned there (`DRIVER`), and `flake.nix`
(`cuaDriver`) pins the same version for the nix packages and the dev shell: bump both together. The driver moves
fast and drops things on dated notices, so an update is a release to try, not a number to raise: `testing.md` has
how. A private screen offers a named few of its tools and a guide of ours in the place of its own (`PRIVATE_TOOLS`,
`privateGuide` in `computer.ts`), both written against the pinned version: read them against a new one. Where none
is shipped (macOS, a dev checkout outside the dev shell) the runtime takes `cua-driver` from PATH; a machine
without one that starts (`cua-driver --version`, tried once per runtime) has no computer use, and the agent that
asks is told why. On macOS that is the user's own install
(`/bin/bash -c "$(curl -fsSL https://cua.ai/driver/install.sh)"`): its `CuaDriver.app` holds the permissions.
Untested by us, like the rest of the macOS build. Windows is untested too: the driver is what its release ships.
Its telemetry, update check and on-screen cursor are off, and it keeps its state under `<data dir>/screens/`,
which a runtime wipes as it starts. On Linux it drives X11 windows (and XWayland ones in a Wayland session); it
needs `libX11`, `libXi` and `libxkbcommon`, which a desktop has.

`Xvfb` makes a thread's private screen: an X display nobody sees but in the thread. Linux only. The nix packages
ship it next to the runtime; the tarball doesn't, and takes it from PATH (`xvfb` on Debian and Ubuntu,
`xorg-server-xvfb` on Arch, `xorg-x11-server-Xvfb` on Fedora; the dev shell has it). Where there is none (Windows and macOS too), an agent that asks for a
private screen gets the machine's desktop in its place and is told to say so; the thread's panel marks what
happens on the desktop either way. A private screen's apps run in the user's session in every other way (the same
environment and session bus), so an app that is single-instance over D-Bus may open in the copy already running
on the desktop.

## The command line (`dirigent`)

`dirigent_cli`'s binary, a console program next to the app in every install (and `~/.local/bin/dirigent`):

```
dirigent [dir]        start the desktop (a running one gets the folder)
dirigent pi [args…]   the bundled pi's own CLI: `dirigent pi install npm:…`, `dirigent pi config`, a terminal session
dirigent key …        API keys of the embedded service's public listener (below): add <name>, list, revoke <name>
dirigent login        sign this data dir in to the forge with a code (below); `dirigent logout`, `dirigent whoami`
```

`--data-dir` / `--config-dir` go first and apply to both. It finds the desktop and `runtime/` next to its own
(real) path, so it works from an install, a nix package and `target/`.

Pi (the runtime and `dirigent pi` alike) keeps its agent dir (`auth.json`, `settings.json`, installed packages,
extensions, skills…) in `<config dir>/agent`, apart from `~/.pi/agent`. The first start without one copies
`~/.pi/agent` (minus `sessions/` and caches; `dirigent_service/src/agent.rs`), so nothing needs signing in again;
a `--config-dir` instance copies the real install's instead.

## Remote hosts

A remote host runs `dirigent_service` (the nix package below; a desktop's embedded service works the same) and a
desktop lists it in its `hosts.toml` (Settings › Hosts, or `<config dir>/hosts.toml` by hand; the file's header
has the forms). The service's agents reach the hosts in its own `hosts.toml` (the `dirigent` tool). The ways to
reach one:

**Tailscale, no auth.** The service listens on loopback and its tailscale addresses, port 7777 (`DIRIGENT_PORT`;
`DIRIGENT_BIND` replaces the addresses), never on a wildcard: anyone on the tailnet can use it. In `hosts.toml`:
`hetzner = "hetzner"` (or `"ws://hetzner.tail1234.ts.net:7777/ws"`).

**Over ssh, no auth: ssh is the auth.** `box = "ssh://[user@]host[:ssh port][/service port]"` (an ssh_config alias
works; the service port defaults to 7777). Dirigent runs the system `ssh -N -L 127.0.0.1:<free port>:127.0.0.1:7777
… host` (`BatchMode`, keepalives every 15 s, `ExitOnForwardFailure`) and connects through it to the service's
loopback listener, so nothing on the remote listens beyond loopback. `ssh host` has to work without prompts first: a
key (or agent), and the host in `known_hosts`. A desktop and its agents share one ssh per host; when ssh exits its
last stderr line shows in Settings › Hosts (hover the host's `error`) and the next connect starts it again. It
stops when the host is removed and when Dirigent quits (`dirigent_service/src/tunnel.rs`).

**Public, with a reverse proxy: an API key per client.** On the remote, `<config dir>/service.toml` adds a listener
for a TLS proxy to forward to (we don't terminate TLS):

```toml
[public]
listen = "127.0.0.1:7778"
url = "wss://dirigent.example.com/ws"
```

and the proxy, e.g. Caddy (it passes websockets through as is):

```
dirigent.example.com {
    reverse_proxy 127.0.0.1:7778
}
```

Every upgrade on that listener needs `Authorization: Bearer <key>`, else 401; the loopback and tailscale listeners
stay open, whatever address the proxy connects from. Keys live in the service's `service.db`, as their sha256:

```sh
dirigent_service key add laptop   # prints dk_… once
dirigent_service key list
dirigent_service key revoke laptop
```

(`--data-dir` / `--config-dir` first, as for the service; `dirigent key …` for a desktop's embedded service.) A
revoked key is refused from its next connect; connections already open stay until they close (restart the service
to drop them). On the client: `prod = { url = "wss://dirigent.example.com/ws", key = "dk_…" }`, or Settings › Hosts ›
Add host › API key. `hosts.toml` is then a secret: Dirigent writes it owner-only (unix). A service hands its agents
the keys of the hosts its `hosts.toml` lists, so whoever can use a service can use those too.

**Public, with the forge account: no keys to hand around.** A data dir can be signed in to a forge account (one per
data dir: a desktop and its embedded service share it; `dirigent_service/src/forge.rs`). Sign the remote service in
with a device code, and the desktop in Settings › Forge providers (the browser; the only place in the app that signs
in or out: the leaderboard's Join and forge hosts send you there), or with `dirigent login`:

```sh
dirigent_service login    # prints a url and a code to enter there, in any browser
dirigent_service whoami
dirigent_service logout   # forgets it here; revoke Dirigent on the forge's account page to end it there
```

With `[public] url` set, the public listener then also takes forge tokens of **that same account**, minted by the
client for the host's address: `prod = { url = "wss://dirigent.example.com/ws", auth = "forge" }` (or Add host ›
forge account in Settings › Hosts). A token is a ten-minute JWT the forge signs for that address (its audience): the
service checks the forge's signature, the forge, Dirigent's app, the address and the account, so `[public] url`
must be the address clients use. Both sides normalize it the same way (`wss://h/ws`, `wss://h:443/ws` and
`WSS://H/ws` are one), but a different host name or path is a different audience: the 401 says which one the
service expects. The service only takes forge tokens while it is signed in; signed out, API keys only. Revoking
Dirigent on the forge stops new tokens at once; one already out lasts its ten minutes.

The forge sign-in's refresh token is in `<data dir>/forge.json` (owner-only), written at sign-in. It stays good
until Dirigent is revoked on the forge's account page or it goes 90 days unused (each use restarts the 90 days). A
refresh the forge refuses signs the data dir out, said in Settings › Forge providers and in `whoami`, until someone
signs in again. `forge.json` is a credential: a copy acts as that account wherever it is, so have each machine sign
in itself.

A host's CLIProxyAPI (Settings › Proxy) is in `<data dir>/proxy.json` (owner-only): its address and management key.
That is a credential too: the proxy's management API hands out the tokens of every account behind it.
`DIRIGENT_FORGE_URL` and `DIRIGENT_FORGE_CLIENT_ID` point an instance at another forge or app (tests).

## Updates

Release builds check `https://dirigent.sebba.dev/api/v0/updates/<channel>/<platform>` every 5 minutes
(`dirigent_desktop/src/update.rs`), download a newer installer (Linux: the tarball) and its minisign signature,
verify both, and then show the update button in the sidebar. Restarting starts that installer and quits; it waits
for Dirigent to exit, installs, and starts it again:

- Windows: the setup exe, silently (`/UPDATE /WAITPID=<pid>`).
- Linux and macOS: the tarball, unpacked in `<data dir>/updates/`, then its
  `dirigent/install.sh --update <pid> <app args>`, which swaps the new `dirigent/` in for the old one. Its output
  goes to `<data dir>/logs/install.log`.

The new version's installer always does the installing, and the update code ships inside every version, so both
can change in any release. What installs in the field rely on, and must stay compatible:

- the update url and the `Release` JSON (`dirigent_protocol/src/release.rs`; add fields, don't change them),
- the installer's `AppId`s (stable's and nightly's) and the flags `update.rs` passes it,
- the tarball's layout (`dirigent/install.sh` at its root; a nightly's `dirigent/nightly`) and `install.sh --update`'s
  arguments,
- the names in the table above (`release::app`, the nightly port): an install's data is found by them,
- the signing keys (`release::KEYS`; rotate by shipping the new key next to the old one first),
- the leaderboard api (`join`, `usage`, `me`, `leaderboard` under `/api/v0/` and their statuses;
  `dirigent_protocol/src/leaderboard.rs`'s types: add fields),
- Dirigent's forge app id (`dirigent_auth::CLIENT_ID`) and its redirect URI `http://127.0.0.1/forge/callback`:
  desktops in the field sign in as it.

The leaderboard broke this once, on purpose: 0.0.7 dropped joining through the api. 0.0.6 desktops lose the
leaderboard (their uploads get 401, they forget their player, Join fails; the board still shows) until they update,
sign in to the forge and Join again; their usage in between is back-filled, up to 30 days back
(`UPLOAD_DAYS` in `dirigent_desktop/src/leaderboard.rs`).

## Making a release

Master is always the next version, in draft: `[workspace.package] version` in `Cargo.toml` (semver) is the version
of the top `changelog.md` section, `## <version> (draft)`. Each section is a one-line summary, then a glanceable
list: a short line per change, ending in a link to its forge change (`([#101])`, with `[#101]: <change url>` under
the section's list). The app shows the changelog after updating (`dirigent_desktop/src/changelog.rs`).

**A nightly**, any time and as often as you like: Actions › Release › Run workflow, channel `nightly` (the
default). Nothing to edit first. It is a prerelease of the draft version named by the time of the run (UTC),
`0.0.10-nightly.202611151432`: semver puts each above the last one and all of them below `0.0.10` itself, and the
next version's nightlies (`0.0.11-nightly.…`) above those. Its changelog is the draft section as it stands. That
version is also what makes the build Dirigent Nightly (above).

**A stable release**, once per version:

1. Make the section final (`## <version> (draft)` becomes `## <version>`) and push to GitHub. The workflow
   refuses `stable` without a final section.
2. Actions › Release › Run workflow, channel `stable`.
3. Right after, bump the version for the next release (with `Cargo.lock` and the `cargoDeps` hash in `flake.nix`)
   and open its `## <version> (draft)` section.

Channels are independent: each is a build of its own (the update url is baked in) with its own versions, and an
install only updates within its channel. A machine can have both; neither becomes the other. (The first installs
were on `beta`, which gets no more releases: they move to `stable` by installing its installer by hand.)

The workflow (`.github/workflows/release.yml`) works the version out once (its `version` job), then has a job per
platform. Each builds the desktop with that version (`DIRIGENT_BUILD_VERSION`, which becomes
`dirigent_protocol::release::VERSION`; other builds are the workspace version) and the channel's update url baked
in (`DIRIGENT_BUILD_UPDATE_URL`; dev builds have none and never check), compiles the runtime,
builds the installer (Windows) or the tarball (Linux, macOS) and runs `dirigent_api publish`, which signs it and
uploads it to the bucket, the channel file last. If one job fails, re-run it alone ("Re-run failed jobs": it keeps
the run's version; re-running all jobs makes a new nightly). Not once a newer nightly is out: the re-run would point
its platform's channel back at the older one, so run a new nightly then. Testers download the latest from
`https://dirigent.sebba.dev/api/v0/download/<channel>/<windows-x86_64|linux-x86_64|macos-aarch64>`. Unsigned by Authenticode for
now: SmartScreen asks once ("More info", "Run anyway"); updates aren't downloaded by a browser, so they don't ask.

Repository settings: variables `S3_ENDPOINT`, `S3_REGION`, `S3_BUCKET`; secrets `S3_ACCESS_KEY_ID`,
`S3_SECRET_ACCESS_KEY` and `DIRIGENT_SIGNING_KEY` (the text of the secret key file below).

**The signing key.** `dirigent_api keygen <file>` writes a new secret key and prints its public key, which goes
into `release::KEYS`. The current secret key was generated on Seb's machine into
`~/.config/dirigent-release.key`: keep a copy somewhere safe. Losing it means shipping a new key to every
install with a release signed by the old one, which is then impossible.

## The update api (`dirigent_api`)

The update endpoints hold no secrets: they read the public bucket. Run it behind the TLS proxy for
`dirigent.sebba.dev`, routing `/api/v0/` to it:

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

**The leaderboard** (the desktop's Usage modal, `dirigent_api/src/leaderboard.rs`) runs in the same api when it
has a db: a writable `DIRIGENT_API_DB`, the one piece of state the server has, worth a backup. On NixOS it's on,
the db in `/var/lib/dirigent-api/`. Without a db it's off and the desktops show it can't be reached. It holds no
secret: desktops sign in to the forge themselves (Dirigent's public app, `dirigent_auth::CLIENT_ID`) and send a
forge JWT for the api's url (`DIRIGENT_API_URL`, default `https://dirigent.sebba.dev`: the url desktops use, no
trailing slash), which the api checks against `FORGE_URL`'s keys (default `https://forge.sebba.dev`) and
Dirigent's app (`DIRIGENT_FORGE_CLIENT_ID` overrides the compiled id). Its `sub`, the forge user id, is the
player. An upload or leave with a valid token of someone not on the board is 403 (the desktop forgets its player
then, and only then: a refused token is 401, no forge keys 503). Desktops use `https://dirigent.sebba.dev` (all
builds, dev and nix too), or `DIRIGENT_API_URL` at run time for tests.

## Nix (x86_64-linux)

`flake.nix` packages all three: `dirigent_desktop` (the default), `dirigent_service` for remote hosts, and
`dirigent_api`, e.g. `nix profile install <flake url>#dirigent_service`. The first two come with the `dirigent`
command line (`dirigent pi` signs in and installs extensions on a remote host too) and are laid out like an
install: the binaries in `libexec/dirigent/` next to the compiled `runtime/` (with Cua Driver in it, patched to find its libraries in the store, and Xvfb), symlinked into `bin/`. Nix builds never self-update (no update url is baked in). The desktop needs a Vulkan
driver: on NixOS `hardware.graphics.enable`; elsewhere nix's vulkan-loader may not find the system's drivers
(nixGL).

Two hashes in `flake.nix` follow the lockfiles: `cargoDeps` (Cargo.lock) and `nodeModules` (bun.lock). After
changing a lockfile, set the hash to `lib.fakeHash` and run `nix build .#dirigent_service`, which needs both but
doesn't build gpui; it prints the right hash.

Not built yet: the standalone `dirigent_service` for remote hosts could ship as a tarball the same way; an AUR
package could repackage the Linux tarball (pacman would then update it, so its installs shouldn't self-update).

## The icon

`dirigent_desktop/assets/dirigent.png` (1000 px) is the source; `dirigent.ico` is made from it with PNG entries
from 64 px up and bitmaps below. The nightly's source is `dirigent_nightly.svg` (the welcome screen draws it, as it
does `dirigent.svg`): `magick -background none dirigent_nightly.svg -resize 1000x1000 -depth 8 -strip
dirigent_nightly.png`, then its `.ico` the same way (`PNG32:` keeps the entries RGBA, which an image of few
colours wouldn't be):

```sh
for s in 256 128 96 64 48 40 32 24 20 16; do magick dirigent.png -filter Lanczos -resize ${s}x$s -strip PNG32:i$s.png; done
icotool -c -o dirigent.ico --raw=i256.png --raw=i128.png --raw=i96.png --raw=i64.png i48.png i40.png i32.png i24.png i20.png i16.png
```
