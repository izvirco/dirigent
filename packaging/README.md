# Packaging, releases and updates

## What an install is (Windows)

`Dirigent-Setup-<version>.exe` (Inno Setup, `windows/dirigent.iss`) installs per user, without admin,
into `%LOCALAPPDATA%\Programs\Dirigent`:

```
dirigent_desktop.exe         the app, with Lilex, the icon and version info built in (build.rs)
dirigent.exe                 the command line (below), put on the user's PATH (a ticked installer task, applied again on each update; uninstalling takes it off)
dirigent_service.exe         the service on its own, for a PC that is a remote host (below)
cua-driver.exe               Cua Driver, which computer use runs (below)
licenses\                    Lilex-OFL.txt, cua-driver-LICENSE.md
```

plus a Start menu entry, an uninstaller and, unless unticked, "Open in Dirigent" on folders (Windows 11:
under "Show more options"). User data stays where it always was (`%LOCALAPPDATA%\dirigent`, `%APPDATA%\dirigent`)
and survives uninstalling. Agents' shell commands need Git for Windows; the installer says so when it can't find
its bash.

The task "Run the Dirigent service when I sign in" (unticked) makes the PC a remote host: a `Run` value of the
user's (`HKCU\…\CurrentVersion\Run`, `Dirigent Service`) starts `dirigent_service.exe` at sign-in, and the
installer starts it right away. Per user and without admin, as the install is, so nothing runs before someone
signs in: a PC nobody sits at needs automatic sign-in. The exe has no console window (a release build is a
Windows-subsystem program), so it prints nothing: its commands are the command line's there (`dirigent key …`,
`dirigent login`), and its log is `%LOCALAPPDATA%\dirigent\logs\service.*.log`. A desktop started on that PC uses
the running service. Written without a Windows machine: the task and a service's own update (below) are untested.

## What an install is (Linux)

`dirigent-<version>-linux-x86_64.tar.gz` holds one directory, `dirigent/`:

```
dirigent_desktop      the app, with Lilex built in (build.rs)
dirigent              the command line (below)
dirigent_service      the service on its own, for a remote host (below)
cua-driver            Cua Driver, which computer use runs (below)
install.sh            linux/install.sh
dirigent.png          the icon
licenses/             Lilex-OFL.txt, cua-driver-LICENSE.md
```

Installing, on any distro, per user and without root:

```sh
d=$(mktemp -d) && curl -fsSL https://dirigent.sebba.dev/api/v0/download/stable/linux-x86_64 | tar xz -C "$d" && "$d/dirigent/install.sh"
```

`install.sh` copies `dirigent/` to `~/.local/lib/dirigent`, links `~/.local/bin/dirigent` to the command line and
`~/.local/bin/dirigent_service` to the service, and
writes `~/.local/share/applications/dirigent.desktop` (the app menu entry; with `MimeType=inode/directory`, file
managers offer it in "Open with" on folders). Built on Ubuntu 22.04, so it needs glibc 2.35 or newer, plus a Vulkan
driver and `libxkbcommon-x11` (which a pure Wayland setup may lack). Uninstalling is removing those four. User data is where it always was (`~/.local/share/dirigent`,
`~/.config/dirigent`).

**A remote host** takes the same tarball, installed with `--headless`: the service alone, kept running.

```sh
d=$(mktemp -d) && curl -fsSL https://dirigent.sebba.dev/api/v0/download/stable/linux-x86_64 | tar xz -C "$d" && "$d/dirigent/install.sh" --headless
```

It installs `dirigent/` without the desktop, the command line and the icon (the service has the command line's
`key` and `login` itself), links `~/.local/bin/dirigent_service`, and takes the desktop's links and menu entry
away if an install had them. Where systemd has a user session it then writes the user unit
`~/.config/systemd/user/dirigent.service`, enables and (re)starts it, and turns lingering on
(`loginctl enable-linger`), so that the service starts with the machine and outlives the login. Without root, as
the install: a machine that refuses lingering to a user runs the service while they are logged in, and the script
says what an admin has to run. Without systemd it says to start `dirigent_service` by hand. The unit starts the
service through the user's login shell (`$SHELL -l -c`), so that the commands agents run find what the user's
login profile puts on `PATH` (not what only an interactive shell's rc file adds): systemd's own environment has
little of one. It restarts a service that failed, not one that was stopped. A service that starts with the
machine can be up before tailscale has addresses, and then listens on loopback alone until it is restarted
(`systemctl --user restart dirigent`): `ssh://`, the way in the script names, needs loopback only (Remote hosts,
below). Uninstalling: `systemctl --user disable --now dirigent`, then the unit, the link and the directory.
Needs glibc 2.35 or newer, and nothing of a desktop's; private screens for computer use need `Xvfb` (below).

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

`install.sh --headless` makes a Mac a remote host as it does a Linux machine, with launchd in systemd's place: the
LaunchAgent `~/Library/LaunchAgents/dev.sebba.dirigent.plist` starts the service through the user's login shell
when they log in (`RunAtLoad`), and again if it fails (`KeepAlive`, not after a clean exit). The script loads it
into the user's login session at once (`launchctl bootstrap gui/<uid>`), or says it starts at the next login when
there is none (over ssh on a Mac nobody is logged in to). An agent is per user and needs no root, but only runs
while that user is logged in: a Mac nobody sits at needs automatic login. Uninstalling:
`launchctl bootout gui/$(id -u)/dev.sebba.dirigent`, then the plist, the link and the directory.

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
| config dir               | `~/.config/dirigent`                            | `~/.config/dirigent-nightly`                   |
| Windows data, config     | `%LOCALAPPDATA%\dirigent`, `%APPDATA%\dirigent` | `…\dirigent-nightly` in both                   |
| local service port       | 7777                                            | 7776                                           |
| install (Linux, macOS)   | `~/.local/lib/dirigent`                         | `~/.local/lib/dirigent-nightly`                |
| command                  | `dirigent`                                      | `dirigent-nightly`                             |
| service command          | `dirigent_service`                              | `dirigent_service-nightly`                     |
| systemd user unit        | `dirigent.service`                              | `dirigent-nightly.service`                     |
| LaunchAgent              | `dev.sebba.dirigent`                            | `dev.sebba.dirigent-nightly`                   |
| Windows `Run` value      | `Dirigent Service`                              | `Dirigent Nightly Service`                     |
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
does: its own setup, proxy, hosts and threads (nothing comes from the stable install's). Its service being on 7776, another desktop reaches it as
`ws://<host>:7776/ws`.

Installing one is the stable line with `nightly` for `stable`:

```sh
d=$(mktemp -d) && curl -fsSL https://dirigent.sebba.dev/api/v0/download/nightly/linux-x86_64 | tar xz -C "$d" && "$d/dirigent/install.sh"
```

## Computer use (Cua Driver, sway)

The `computer` tool (Settings › Agent tools, on by default; `dirigent_agent/src/tools/computer.rs`) opens a screen for
an agent to run and operate GUI apps on, when it asks for one: nothing runs before that. Two things do the work.

[Cua Driver](https://github.com/trycua/cua) (MIT) operates the screen, as an MCP server of the thread: one
executable, `cua-driver[.exe]`, which the Windows and Linux installs ship next to the binaries.
`packaging/cua-driver.sh` downloads it from the driver's GitHub release for the release workflow, checks its
sha256 and fetches its license; the version and hashes are pinned there, and `flake.nix` (`cuaDriver`) pins the
same version for the nix packages and the dev shell: bump both together. The driver moves
fast and drops things on dated notices, so an update is a release to try, not a number to raise: `testing.md` has
how. A private screen offers a named few of its tools and a guide of ours in the place of its own (`PRIVATE_TOOLS`,
`private_guide` in `computer.rs`), both written against the pinned version: read them against a new one. The agent
looks for the driver next to its own executable, then on PATH (macOS, a dev checkout: the dev shell has it); a
machine without one that starts (`cua-driver --version`) has no computer use, and the agent that asks is told
why. On macOS that is the user's own install
(`/bin/bash -c "$(curl -fsSL https://cua.ai/driver/install.sh)"`): its `CuaDriver.app` holds the permissions.
Untested by us, like the rest of the macOS build. Windows is untested too: the driver is what its release ships.
Its telemetry and update check are off, and it keeps its state under `<data dir>/screens/`,
which a service wipes as it starts. On Linux it needs `libX11`, `libXi` and `libxkbcommon`, which a desktop has.

In a Wayland session (`WAYLAND_DISPLAY` set) the driver is started with its Wayland backend on
(`CUA_DRIVER_RS_ENABLE_WAYLAND=1`): by itself it leaves that off and reaches X11 windows alone, through XWayland.
A value of that variable in the host's environment stays as it is, so `0` there turns the backend off again. The
backend works with what the compositor offers: on sway, wlroots' protocols and `wtype` to type (looked for on
PATH, so the driver's PATH gets the directory of the one we ship, where it isn't on it). On sway, input aimed at a window is
refused in the background; in the foreground, a click at a window without the focus only brings it forward, and
the next one lands. And while an X11 window is open there, the driver lists every window without pid or size, so
nothing can be read or aimed at by window: screenshots and input by the screen's pixels still work, and `0` in
that variable brings the X11 windows back. We ran it on sway alone: the driver's docs
(`concepts/platform-support`) say what it does on GNOME, KDE and Hyprland. In an X11 session it drives X11
windows, as before.

The driver draws a cursor of its own on the screen, over the window it acts on, and leaves the user's pointer
where it is. On the desktop it shows unless Settings › Agent tools turns it off (`computer_cursor` in
`config.toml`, an option in a prompt's `tools`); on a private screen it never does (`--no-overlay`): the thread's
panel draws its own there. The switch is read when a screen opens, so a desktop that is open keeps what it had.
A click waits for the cursor to get there, about a second. On an X11 desktop without a compositor the cursor
covers the window, and the pictures the agent and the panel get of it are black where the cursor rests. In a
Wayland session the compositor draws the cursor into the output, so those pictures have it in them (seen on
sway). On macOS `mcp` is a client of the user's own driver, whose settings decide, not the switch.

`sway` makes a thread's private screen: a headless Wayland display of the thread's own, with one output of the
size asked for, which nobody sees but in the thread. Linux only. It runs on a config of ours (windows tile and
have no borders; no XWayland, so an app that needs X11 doesn't start there) in a directory of its own under the
temp dir (`dirigent-screen-<id>`, the owner's alone), which has its sockets and goes with it. The driver there is
given that display and nothing of the user's own: `HYPRLAND_INSTANCE_SIGNATURE` and `NIRI_SOCKET` are emptied
(it takes them for the compositor to talk to, and then sees nothing of the screen), and `DISPLAY`, so that
nothing it launches opens on the user's X display. It types with `wtype`. The nix
packages ship `sway` and `wtype` next to the binaries; the tarball doesn't, and takes them from PATH (packages of
those names on Debian, Ubuntu, Arch and Fedora; the dev shell has them). Where one is missing (Windows and macOS
too), an agent that asks for a private screen gets the machine's desktop in its place and is told to say so; the
thread's panel marks what happens on the desktop either way. A private screen's apps run in the user's session
in every other way (the same environment and session bus), so an app that is single-instance over D-Bus may open
in the copy already running on the desktop. Untried on a GNOME or KDE host, where the driver also looks for those
desktops' own helpers on that bus.

## The command line (`dirigent`)

`dirigent_cli`'s binary, a console program next to the app in every install (and `~/.local/bin/dirigent`):

```
dirigent [dir]        start the desktop (a running one gets the folder)
dirigent key …        API keys of the embedded service's public listener (below): add <name>, list, revoke <name>
dirigent login        sign this data dir in to the forge with a code (below); `dirigent logout`, `dirigent whoami`
```

`--data-dir` / `--config-dir` go first and apply to all of them. It finds the desktop next to its own (real)
path, so it works from an install, a nix package and `target/`.

## The agent's files

The agent is part of the service (`dirigent_agent`): an install ships nothing for it but the binaries. What it
reads is the user's, in the config dir:

```
agent.toml   compaction, MCP servers (`dirigent_agent/src/config.rs`); owner-only
AGENTS.md    the user's instructions, in every thread's system prompt
skills/      a directory per skill, each with its SKILL.md
```

and, in the data dir, `service.db` (the threads), `mcp-auth.json` (MCP sign-ins), `proxy.json` (the proxy),
`jobs/` (the logs of the commands agents run) and caches. Settings writes `agent.toml` and `proxy.json`; editing
them by hand works too, and applies to the next request.

`agent.toml` holds no key of Dirigent's, so a user can keep it in a repo and share it between machines. The keys
of an MCP server can stay out of it too: the values of `env` and `headers` may name an environment variable
(`${TOKEN}`) or a command (`!pass show token`).

Models come through a [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI) the user runs, and only that
way: the setup and Settings › Proxy ask for its address and API key. Dirigent neither installs nor starts one.
The proxy is the host's and not the user's, so it is in the data dir: `proxy.json` has its `url`, its `api_key`
and, for Usage, its `management_key`.

0.0.10 kept the address and the API key in `agent.toml` (`[proxy]`, with `url` and `key`), and the management key
in `proxy.json` as `key`, which is still read as `management_key`. A start moves a `[proxy]` it finds in
`agent.toml` to `proxy.json` and takes it out of the file (`move_proxy` in `dirigent_agent/src/config.rs`), through
a link if the file is one. A `proxy.json` that already has an API key stays as it is, and the `[proxy]` only goes;
a management key 0.0.10 left there stays if it is for the same address. A save of `agent.toml` makes the move first,
should a start not have made it (the file did not parse then).

Up to 0.0.9 Dirigent ran pi, as a bundled runtime, and 0.0.10 brought an install over on its first start. That
migration is gone: a `service.db` as pi left it is refused with a line that says to start 0.0.10 on it once
(`Storage::open` in `dirigent_service/src/storage.rs`), and pi's agent dir (`<config dir>/agent`) is not read.
What is left of it in an install is the threads 0.0.10 could not convert whole. They are locked: they open and
fork, but take no more messages. And the backups 0.0.10 made beside the data and the config dir (`<dir>.bck`, with
a `migration.txt`): nothing removes them, they are the user's to delete.

A `--config-dir` instance without an `agent.toml` copies the real install's `agent.toml`, `AGENTS.md` and
`skills/`, and with them, if it has no proxy of its own, the proxy's address and API key into its own `proxy.json`,
the management key not.

## Remote hosts

A remote host runs `dirigent_service` (`install.sh --headless` or the Windows installer's task, above; the nix
package below; a desktop's embedded service works the same) and a
desktop lists it in its `hosts.toml` (Settings › Hosts, or `<config dir>/hosts.toml` by hand; the file's header
has the forms). The service's agents reach the hosts in its own `hosts.toml` (the `dirigent` tool). The ways to
reach one:

**Tailscale, no auth.** The service listens on loopback and its tailscale addresses, port 7777 (`DIRIGENT_PORT`;
`DIRIGENT_BIND` replaces the addresses), never on a wildcard: anyone on the tailnet can use it. It asks tailscale
for its addresses once, as it starts. In `hosts.toml`:
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

A host's CLIProxyAPI (Settings › Proxy) is in `<data dir>/proxy.json` (owner-only): its address, API key and
management key. That is a credential too: the proxy's management API hands out the tokens of every account behind it.
`DIRIGENT_FORGE_URL` and `DIRIGENT_FORGE_CLIENT_ID` point an instance at another forge or app (tests).

## Updates

Release builds check `https://dirigent.sebba.dev/api/v0/updates/<channel>/<platform>` every 5 minutes
(`dirigent_desktop/src/update.rs`, with `dirigent_service/src/update.rs`, which asks, downloads and verifies),
download a newer installer (Linux: the tarball) and its minisign signature,
and verify both; a download that stalls for 30 seconds fails and is retried at the next check. The update button
shows in the sidebar as soon as a newer release is found, its menu showing the download's progress until the
installer is verified. Restarting starts that installer and quits; it waits for Dirigent to exit, installs, and
starts it again:

- Windows: the setup exe, silently (`/UPDATE /WAITPID=<pid>`).
- Linux and macOS: the tarball, unpacked in `<data dir>/updates/`, then its
  `dirigent/install.sh --update <pid> <app args>`, which swaps the new `dirigent/` in for the old one. Its output
  goes to `<data dir>/logs/install.log`.

**A service on its own** (`dirigent_service`: a remote host) checks nothing by itself: a desktop does the asking.
It asks every host it is connected to, at connect and every 5 minutes, for the newer release that host could
install (`ClientMessage::CheckUpdate`; the service asks the update api of its own channel and platform then), and
lists the ones that have one in the update button's menu: `Update <host>`. A click tells the host to install
(`InstallUpdate`), at once or, while agents work there, when they are done, as the desktop's own restart waits
(the desktop has to stay open until then; a host that drops meanwhile is told when it is back). The service
downloads the installer to `<data dir>/updates-service/`, verifies it as the desktop does, and installs it. It
doesn't look at its threads again: one started while it downloads is cut off by the restart.

- Linux and macOS: the tarball's `dirigent/install.sh --update-service`, with `DIRIGENT_PREFIX` set to the prefix
  of the install it runs from. That swaps the directory, a headless install staying headless, and leaves the
  systemd unit or LaunchAgent as it is. The service then shuts down as for a stop and becomes the new
  executable (`exec`): the same process, arguments and environment, so systemd, launchd or the terminal that
  started it keep it.
- Windows: the setup exe, unseen (`/VERYSILENT /SERVICE /WAITPID=<pid>`). The service exits, and the setup starts
  the new one, without arguments: a service started with `--data-dir` or `--config-dir` doesn't update this way.
  A desktop open on that PC is closed by the setup, and stays closed.

The host is gone for a moment and comes back as the new version; a failure is a toast with the service's reason.
Only a service from a release's install updates this way. The service a desktop embeds is updated with the
desktop, a nix package by nix, and a `dirigent_service` started from elsewhere (a build, an unpacked tarball) has
no install to update: these answer that they have nothing, and so does a service from before `CheckUpdate`.
Where a desktop is installed beside a service that runs on its own, each updates by itself: after the desktop's
update the menu offers the service's (on Windows the setup has restarted it already, with the task ticked). A
desktop gives a local service that isn't its own 2 seconds to come back before it starts the embedded one in its
place (`STANDALONE_LOCAL_SERVICE_RESTART_GRACE`), which is what a service restarting into its update needs.
A desktop can't tell a host anything across a change of the protocol (`dirigent_protocol::PROTOCOL`): for a
release that has one, update the hosts from the menu before the desktop.

The new version's installer always does the installing, and the update code ships inside every version, so both
can change in any release. What installs in the field rely on, and must stay compatible:

- the update url and the `Release` JSON (`dirigent_protocol/src/release.rs`; add fields, don't change them),
- the installer's `AppId`s (stable's and nightly's) and the flags the two `update.rs` pass it (a service's:
  `/SERVICE`),
- the tarball's layout (`dirigent/install.sh` at its root; a nightly's `dirigent/nightly`), `install.sh --update`'s
  arguments and `install.sh --update-service` with its `DIRIGENT_PREFIX`,
- an install's layout, `<prefix>/lib/<app>/` with `install.sh` in it: a service finds the install it runs from by
  it,
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
platform. Each builds the desktop, the command line and the service with that version (`DIRIGENT_BUILD_VERSION`,
which becomes `dirigent_protocol::release::VERSION`; other builds are the workspace version) and the channel's update url baked
in (`DIRIGENT_BUILD_UPDATE_URL`; dev builds have none and never check), fetches Cua Driver
(`packaging/cua-driver.sh`; not on macOS), builds the installer (Windows) or the tarball (Linux, macOS) and runs
`dirigent_api publish`, which signs it and uploads it to the bucket, the channel file last. If one job fails, re-run it alone ("Re-run failed jobs": it keeps
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

```text
dirigent_api serve
dirigent_api publish --channel stable --platform windows-x86_64 --version 0.1.0 [--notes TEXT] FILE
dirigent_api keygen FILE     a new signing key: the secret one to FILE, the public one printed
```

`publish` also takes `S3_ACCESS_KEY_ID`, `S3_SECRET_ACCESS_KEY` and `DIRIGENT_SIGNING_KEY` (the minisign secret
key file's text). `DIRIGENT_LOG` sets the log filter (default `info`).

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
command line and are laid out like an install: the binaries in `libexec/dirigent/`, next to Cua Driver (patched
to find its libraries in the store), sway and wtype, and symlinked into `bin/`. Nix builds never self-update (no update url is baked in). The desktop needs a Vulkan
driver: on NixOS `hardware.graphics.enable`; elsewhere nix's vulkan-loader may not find the system's drivers
(nixGL).

One hash in `flake.nix` follows the lockfile: `cargoDeps` (Cargo.lock). After changing it, set the hash to
`lib.fakeHash` and run `nix build .#dirigent_service`, which doesn't build gpui; it prints the right hash.

Not built yet: an AUR package could repackage the Linux tarball (pacman would then update it, so its installs
shouldn't self-update).

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
