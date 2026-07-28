# Dirigent

Dirigent is a native desktop workspace for running and managing multiple [Pi coding agent](https://github.com/earendil-works/pi) sessions across projects.

## Configuration and themes

On first launch, Dirigent creates `~/.config/dirigent/config.toml` and sample themes in `~/.config/dirigent/theme/`. Set `theme` in `config.toml` to a theme filename without `.toml`. Changes to the config or selected theme are reloaded automatically.

Dirigent follows `XDG_CONFIG_HOME` on Unix. On Windows, configuration is stored in `%APPDATA%\dirigent`.

## Linux (Nix)

```
nix develop
cargo run --release
```

## Run on M$ Windows

Install Rust with `rustup`, Node.js 22.19+, Git for Windows, and Visual Studio Build Tools with **Desktop development with C++** and a Windows SDK. Then, in PowerShell:

```powershell
npm install -g --ignore-scripts @earendil-works/pi-coding-agent
pi --version
cargo run --release
```

After that realize that you want to use NixOS
