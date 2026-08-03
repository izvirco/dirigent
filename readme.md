# Dirigent

Dirigent is a native desktop workspace for running and managing multiple [Pi coding agent](https://github.com/earendil-works/pi) sessions across projects.

Dirigent requires Pi 0.82.1 or newer. It launches Pi with a bundled, private extension that exposes session-tree navigation to the UI; nothing is installed into the user's Pi configuration.

## Turn diffs

Open the right-hand Changes sidebar from the thread or with `Space d`. Dirigent freezes the net workspace diff when each agent turn settles, supports unified and split views, and keeps completed turn diffs across restarts. Select diff text and use **Add reference to composer** to insert an editable code-review reference directly into the next prompt.

Diff capture respects repository ignore files and excludes common generated directories. It records workspace changes made while a turn is running; concurrent edits made by another program during that interval are therefore included as well.

## Linux (Nix)

```
nix develop
cargo run
```

To produce a release binary with Lilex embedded:

```
nix develop
cargo build --release --features bundled-lilex
```

The Nix development shell sets `LILEX_FONT_DIR` for the build. Outside Nix, set it to a directory containing the static Lilex TTF files before enabling `bundled-lilex`. The font files are copied into Cargo's build output and embedded in the binary; they are not stored in this repository. Builds without the feature continue to use system fonts. Distribute the `licenses/` directory with release artifacts; it contains the Lilex, tree-house, and bundled grammar licenses.

## Run on M$ Windows

Install Rust with `rustup`, Node.js 22.19+, Git for Windows, and Visual Studio Build Tools with **Desktop development with C++** and a Windows SDK. Then, in PowerShell:

```powershell
npm install -g --ignore-scripts @earendil-works/pi-coding-agent
pi --version
cargo run --release
```

After that realize that you want to use NixOS

