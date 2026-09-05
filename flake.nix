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
      rustPlatform = pkgs.makeRustPlatform {
        cargo = rustToolchain;
        rustc = rustToolchain;
      };
      runtimeLibraries = [
        pkgs.fontconfig
        pkgs.freetype
        pkgs.libxkbcommon
        pkgs.vulkan-loader
        pkgs.wayland
      ];
      dirigent = rustPlatform.buildRustPackage {
        pname = "dirigent";
        version = "0.0.0";
        src = pkgs.lib.cleanSource ./.;

        cargoHash = "sha256-SWRJcQIvJHTiuCLdHHOFd91KzdNUbNJy+pq6W4l9iQg=";
        cargoBuildFlags = [
          "--package"
          "dirigent_desktop"
          "--features"
          "bundled-lilex"
        ];

        nativeBuildInputs = [
          pkgs.patchelf
          pkgs.pkg-config
          pkgs.removeReferencesTo
        ];
        buildInputs = runtimeLibraries;

        LILEX_FONT_DIR = "${pkgs.lilex}/share/fonts/truetype";

        postFixup = ''
          patchelf --add-rpath "${pkgs.lib.makeLibraryPath runtimeLibraries}:/run/opengl-driver/lib" "$out/bin/dirigent"
          remove-references-to -t ${rustToolchain} "$out/bin/dirigent"
        '';

        meta = {
          description = "Native desktop workspace for Pi coding agent sessions";
          homepage = "https://forge.sebba.dev/izvir/dirigent";
          mainProgram = "dirigent";
          platforms = [ "x86_64-linux" ];
        };
      };
    in
    {
      packages.${system} = {
        default = dirigent;
        inherit dirigent;
      };

      devShells.${system}.default = pkgs.mkShell {
        nativeBuildInputs = [ pkgs.pkg-config ];

        buildInputs = runtimeLibraries;

        packages = [
          rustToolchain
          pkgs.cargo-edit
          pkgs.lilex
        ];

        env = {
          RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
          LILEX_FONT_DIR = "${pkgs.lilex}/share/fonts/truetype";
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
