{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
    rust = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      nixpkgs,
      rust,
      ...
    }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs {
        inherit system;
        overlays = [ rust.overlays.default ];
      };
      rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
    in
    {
      devShells.${system}.default = pkgs.mkShell {
        nativeBuildInputs = [ pkgs.pkg-config ];

        buildInputs = [
          pkgs.fontconfig
          pkgs.freetype
          pkgs.libxkbcommon
          pkgs.vulkan-loader
          pkgs.wayland
        ];

        packages = [
          rustToolchain
          pkgs.cargo-edit
          pkgs.nerd-fonts.lilex
        ];

        env = {
          RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [
            pkgs.fontconfig
            pkgs.freetype
            pkgs.libxkbcommon
            pkgs.vulkan-loader
            pkgs.wayland
          ] + ":/run/opengl-driver/lib";
        };
      };
    };
}
