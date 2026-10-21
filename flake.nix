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
          # Headless UI testing: virtual X display, input, screenshots (see testing.md)
          pkgs.xvfb
          pkgs.xdotool
          pkgs.imagemagick
        ] ++ gpuiLibs;

        env = {
          RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath gpuiLibs;
          # Software Vulkan (lavapipe): Xvfb has no GPU presentation. scripts/ui.sh sets
          # VK_ICD_FILENAMES to this for the test app only.
          DIRIGENT_UI_VK_ICD = "${pkgs.mesa}/share/vulkan/icd.d/lvp_icd.x86_64.json";
        };
      };
    };
}
