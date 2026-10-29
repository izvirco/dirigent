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
      self,
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
      inherit (pkgs) lib;
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
      # Lilex, the default font, is built into the desktop (`dirigent_desktop/build.rs`): from
      # here, rather than downloaded, so builds work offline (and in the nix sandbox).
      lilexDir = "${pkgs.lilex}/share/fonts/truetype";

      # Packages: `nix build .#dirigent_desktop` (the default), `.#dirigent_service`,
      # `.#dirigent_api`. They never self-update (no `DIRIGENT_BUILD_UPDATE_URL`).
      #
      # Two hashes to bump: `cargoDeps` when Cargo.lock changes, `nodeModules` when bun.lock
      # does. Set it to `lib.fakeHash` and build: nix prints the right one.
      version = (lib.importTOML ./Cargo.toml).workspace.package.version;
      # Without the extensions: with rust-src installed, rustc puts its store path into std's
      # panic locations, which drags the whole toolchain into every package's closure.
      rustPlatform =
        let
          toolchain = rustToolchain.override { extensions = [ ]; };
        in
        pkgs.makeRustPlatform {
          cargo = toolchain;
          rustc = toolchain;
        };
      # Only the Rust workspace: editing the runtime doesn't rebuild gpui.
      rustSrc = lib.fileset.toSource {
        root = ./.;
        fileset = lib.fileset.unions [
          ./Cargo.toml
          ./Cargo.lock
          ./dirigent_api
          ./dirigent_desktop
          ./dirigent_protocol
          ./dirigent_service
        ];
      };
      # Vendored once for all three crates.
      cargoDeps = rustPlatform.fetchCargoVendor {
        name = "dirigent";
        src = rustSrc;
        hash = "sha256-yx6Xuv/DQajVEtm4Hmoj1CWVAJYL/zXvoru/tR0jLgo=";
      };
      # One crate's binary, in `bin/`.
      crate =
        name: args:
        rustPlatform.buildRustPackage (
          {
            pname = name;
            inherit version cargoDeps;
            src = rustSrc;
            cargoBuildFlags = [
              "-p"
              name
            ];
            # `cargo test --workspace` is the check (AGENTS.md); the package only builds.
            doCheck = false;
            meta.mainProgram = name;
          }
          // args
        );

      # The runtime's node_modules, fixed-output: `bun install` needs the network.
      nodeModules = pkgs.stdenvNoCC.mkDerivation {
        pname = "dirigent_runtime-node_modules";
        inherit version;
        src = lib.fileset.toSource {
          root = ./.;
          fileset = lib.fileset.unions [
            ./package.json
            ./bun.lock
            ./dirigent_runtime/package.json
          ];
        };
        nativeBuildInputs = [
          pkgs.bun
          pkgs.writableTmpDirAsHomeHook
        ];
        buildPhase = ''
          bun install --frozen-lockfile --production --ignore-scripts --no-progress
        '';
        # Both trees: bun's isolated layout links the workspace's packages into the root's.
        installPhase = ''
          mkdir -p $out/dirigent_runtime
          cp -R node_modules $out/
          cp -R dirigent_runtime/node_modules $out/dirigent_runtime/
        '';
        # A fixed output can't refer to the store (patchShebangs would).
        dontFixup = true;
        outputHashMode = "recursive";
        outputHash = "sha256-msyNGZdNCr0ggm2dU6Vnq4uOHzDjrH9wFNDvGfw/5zQ=";
      };

      # The runtime compiled to one executable, plus the files pi reads next to it
      # (`dirigent_runtime/build.ts`): what installs ship as `runtime/`, so no bun at run time.
      runtime = pkgs.stdenvNoCC.mkDerivation {
        pname = "dirigent_runtime";
        inherit version;
        src = lib.fileset.toSource {
          root = ./.;
          fileset = lib.fileset.unions [
            ./package.json
            ./bun.lock
            ./dirigent_runtime/package.json
            ./dirigent_runtime/build.ts
            ./dirigent_runtime/src
          ];
        };
        nativeBuildInputs = [
          pkgs.bun
          pkgs.writableTmpDirAsHomeHook
        ];
        buildPhase = ''
          cp -R ${nodeModules}/. .
          bun dirigent_runtime/build.ts --out $out
        '';
        dontInstall = true;
        # `bun build --compile` appends the bundle to a copy of bun: stripping or patching the
        # executable would cut it off.
        dontStrip = true;
        dontPatchELF = true;
      };

      # A binary laid out as installs have it, with `runtime/` next to it, where the service
      # finds it (`runtime_cmd` in dirigent_service): that's next to the executable's real
      # path, so `bin/` symlinks to it. Copied, not linked: a symlink would resolve to the
      # crate's own store path, which has no `runtime/`.
      withRuntime =
        pkg:
        let
          name = pkg.meta.mainProgram;
        in
        pkgs.runCommand "${name}-${version}" { meta.mainProgram = name; } ''
          mkdir -p $out/bin $out/libexec/dirigent
          cp ${lib.getExe pkg} $out/libexec/dirigent/
          ln -s ${runtime} $out/libexec/dirigent/runtime
          ln -s ../libexec/dirigent/${name} $out/bin/${name}
        '';
    in
    {
      packages.${system} = rec {
        default = dirigent_desktop;

        dirigent_desktop = withRuntime (
          crate "dirigent_desktop" {
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs = gpuiLibs;
            env.DIRIGENT_LILEX_DIR = lilexDir;
            # gpui dlopens these. After fixup, whose rpath shrinking would drop them again.
            postFixup = ''
              patchelf --add-rpath ${
                lib.makeLibraryPath [
                  pkgs.wayland
                  pkgs.vulkan-loader
                  pkgs.libglvnd # wgpu's GL fallback (EGL)
                ]
              } $out/bin/dirigent_desktop
            '';
          }
        );

        # The service for remote hosts (the desktop embeds its own).
        dirigent_service = withRuntime (crate "dirigent_service" { });

        dirigent_api = crate "dirigent_api" { };
      };

      # `services.dirigent-api`: the update api (`dirigent_api serve`), to put behind the TLS
      # proxy (packaging/README.md). It holds no secrets and writes nothing: it reads the
      # public bucket. More environment (`S3_PREFIX`, `DIRIGENT_LOG`) goes into
      # `systemd.services.dirigent-api.environment`.
      nixosModules.dirigent_api =
        { config, ... }:
        let
          cfg = config.services.dirigent-api;
          str =
            description:
            lib.mkOption {
              type = lib.types.str;
              inherit description;
            };
        in
        {
          options.services.dirigent-api = {
            enable = lib.mkEnableOption "the Dirigent update api";
            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${system}.dirigent_api;
              description = "The dirigent_api package.";
            };
            bind = lib.mkOption {
              type = lib.types.str;
              default = "127.0.0.1:7790";
              description = "Address to listen on (`DIRIGENT_API_BIND`).";
            };
            s3 = {
              endpoint = str "The bucket's S3 endpoint, e.g. `https://fsn1.your-objectstorage.com`.";
              region = str "The bucket's region, e.g. `fsn1`.";
              bucket = str "The bucket's name.";
            };
          };

          config = lib.mkIf cfg.enable {
            systemd.services.dirigent-api = {
              description = "Dirigent update api";
              wantedBy = [ "multi-user.target" ];
              after = [ "network.target" ];
              environment = {
                DIRIGENT_API_BIND = cfg.bind;
                S3_ENDPOINT = cfg.s3.endpoint;
                S3_REGION = cfg.s3.region;
                S3_BUCKET = cfg.s3.bucket;
              };
              serviceConfig = {
                ExecStart = "${lib.getExe cfg.package} serve";
                DynamicUser = true;
                Restart = "on-failure";
              };
            };
          };
        };

      devShells.${system} = {
        default = pkgs.mkShell {
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
          ]
          ++ gpuiLibs;

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
            DIRIGENT_LILEX_DIR = lilexDir;
          };
        };

        # `nix develop .#windows`: checks that the Windows build compiles (AGENTS.md). MinGW's
        # gcc compiles the bundled sqlite; Windows itself builds with MSVC.
        windows = pkgs.mkShell {
          packages = [
            (rustToolchain.override { targets = [ "x86_64-pc-windows-gnu" ]; })
            pkgs.pkgsCross.mingwW64.stdenv.cc
          ];
          env.DIRIGENT_LILEX_DIR = lilexDir;
        };
      };
    };
}
