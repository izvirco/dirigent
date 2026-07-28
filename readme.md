# Dirigent

Dirigent is a native desktop workspace for running and managing multiple [Pi coding agent](https://github.com/earendil-works/pi) sessions across projects.

## Configuration and themes

On first launch, Dirigent creates `~/.config/dirigent/config.toml` and sample themes in `~/.config/dirigent/theme/`. Set `theme` in `config.toml` to a theme filename without `.toml`. Set `font` to any installed font family, for example `font = "Iosevka"`. Changes to the config or selected theme are reloaded automatically.

Theme colors accept `#RRGGBB` (opaque) or `#RRGGBBAA`, with the final byte controlling opacity. For example, `surface = "#15181e80"` uses 50% opacity. The equivalent `0xRRGGBB` and `0xRRGGBBAA` forms are also accepted. When `background` has an alpha value below `ff`, Dirigent exposes a transparent window surface to the compositor.

Niri can blur behind the transparent surface with a window rule:

```kdl
blur {
    on
}
window-rule {
    match app-id="^dirigent$"
    background-effect {
        blur true
    }
}
```

Dirigent follows `XDG_CONFIG_HOME` on Unix. On Windows, configuration is stored in `%APPDATA%\dirigent`.

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

The Nix development shell sets `LILEX_FONT_DIR` for the build. Outside Nix, set it to a directory containing the static Lilex TTF files before enabling `bundled-lilex`. The font files are copied into Cargo's build output and embedded in the binary; they are not stored in this repository. Builds without the feature continue to use system fonts. Distribute `licenses/Lilex-OFL.txt` with bundled release artifacts.

## Run on M$ Windows

Install Rust with `rustup`, Node.js 22.19+, Git for Windows, and Visual Studio Build Tools with **Desktop development with C++** and a Windows SDK. Then, in PowerShell:

```powershell
npm install -g --ignore-scripts @earendil-works/pi-coding-agent
pi --version
cargo run --release
```

After that realize that you want to use NixOS
