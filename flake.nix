{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
    rust = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { nixpkgs, rust, ... }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs {
        inherit system;
        overlays = [ rust.overlays.default ];
      };
      rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      # Runtime libs gpui dlopens on Linux (Wayland + wgpu/Vulkan + fonts).
      gpuiLibs = with pkgs; [
        wayland
        libxkbcommon
        libxcb
        vulkan-loader
        fontconfig
        freetype
        openssl
        zstd
      ];
    in
    {
      devShells.${system}.default = pkgs.mkShell {
        packages = [
          rustToolchain
          pkgs.nodejs
          pkgs.bun
          pkgs.oxlint
          pkgs.oxfmt
          pkgs.pkg-config
          pkgs.cmake
          pkgs.sccache
          # Headless UI testing: virtual X display, input, screenshots (see testing.md)
          pkgs.xvfb
          pkgs.xdotool
          pkgs.imagemagick
          pkgs.xclip # clipboard of the test display (`scripts/ui.sh run xclip ...`)
          (pkgs.lib.getBin pkgs.sqlite) # peek into a test session's dbs
        ] ++ gpuiLibs;

        env = {
          RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
          # Every checkout and workspace has its own target dir; sccache shares the compiled
          # dependencies (crates.io, gpui) between them, so a fresh workspace isn't a cold build.
          # Not shared: debug builds of our own crates (incremental), proc-macros, and crates that
          # include build-script output (OUT_DIR). Don't set CARGO_TARGET_DIR: sccache hashes
          # CARGO_* env vars, so it makes every entry private to that path (--target-dir is fine).
          RUSTC_WRAPPER = "sccache";
          SCCACHE_CACHE_SIZE = "50G";
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath gpuiLibs;
          # Software Vulkan (lavapipe): Xvfb has no GPU presentation. scripts/ui.sh sets
          # VK_ICD_FILENAMES to this for the test app only.
          DIRIGENT_UI_VK_ICD = "${pkgs.mesa}/share/vulkan/icd.d/lvp_icd.x86_64.json";
          # The system fonts plus Lilex, the default UI font, for machines (and the test
          # display) that don't have it installed.
          FONTCONFIG_FILE = pkgs.writeText "fonts.conf" ''
            <?xml version="1.0"?>
            <!DOCTYPE fontconfig SYSTEM "urn:fontconfig:fonts.dtd">
            <fontconfig>
              <include ignore_missing="yes">/etc/fonts/fonts.conf</include>
              <dir>${pkgs.lilex}/share/fonts</dir>
            </fontconfig>
          '';
        };
      };
    };
}
