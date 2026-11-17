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

      # Packages: `nix build .#dirigent_desktop` (the default), `.#dirigent_service` (for a
      # remote host), `.#dirigent_api`; the first two come with the `dirigent` command line.
      # They never self-update (no `DIRIGENT_BUILD_UPDATE_URL`).
      #
      # One hash to bump: `cargoDeps`, when Cargo.lock changes. Set it to `lib.fakeHash` and
      # build: nix prints the right one.
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
      # Only the Rust workspace: editing docs or scripts doesn't rebuild gpui.
      rustSrc = lib.fileset.toSource {
        root = ./.;
        fileset = lib.fileset.unions [
          ./Cargo.toml
          ./Cargo.lock
          ./changelog.md # compiled into the desktop
          ./dirigent_agent
          ./dirigent_api
          ./dirigent_auth
          ./dirigent_cli
          ./dirigent_desktop
          ./dirigent_protocol
          ./dirigent_service
        ];
      };
      # Vendored once for all crates.
      cargoDeps = rustPlatform.fetchCargoVendor {
        name = "dirigent";
        src = rustSrc;
        hash = "sha256-VYxhEPlf7rqnF/Z7PVAlRGW1oAIlKFfa0EIPGPZ4y4s=";
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

      # Cua Driver, what computer use runs (`dirigent_agent`'s `computer` tool): the release
      # binary installs ship (`packaging/cua-driver.sh` pins the same version: bump both
      # together), patched to find its libraries here. In the dev shell too, where the agent
      # takes it from PATH.
      cuaDriver = pkgs.stdenvNoCC.mkDerivation rec {
        pname = "cua-driver";
        version = "0.51.3";
        src = pkgs.fetchurl {
          url = "https://github.com/trycua/cua/releases/download/cua-driver-rs-v${version}/cua-driver-rs-${version}-linux-x86_64-binary.tar.gz";
          hash = "sha256-BERm9l2zl2+h7zAqNCfqEtXU7dlpJjeI0vwW15O5Xd4=";
        };
        # The archive has no directory of its own.
        sourceRoot = ".";
        nativeBuildInputs = [ pkgs.autoPatchelfHook ];
        buildInputs = [
          pkgs.libx11
          pkgs.libxi
          pkgs.libxkbcommon
          pkgs.stdenv.cc.cc.lib
        ];
        installPhase = ''
          install -Dm755 cua-driver $out/bin/cua-driver
        '';
        meta = {
          mainProgram = "cua-driver";
          license = lib.licenses.mit;
        };
      };
      # Its license, which goes wherever it is shipped (the archive has none).
      cuaDriverLicense = pkgs.fetchurl {
        url = "https://raw.githubusercontent.com/trycua/cua/cua-driver-rs-v${cuaDriver.version}/LICENSE.md";
        hash = "sha256-wHeSkMHUeDFpqj2/tV/rUF5WPvigBLv1UpjO/8+9qNk=";
      };

      # Binaries laid out as installs have them, side by side with what computer use runs
      # (Cua Driver, and Xvfb, which makes a thread's private screen): the agent looks next to
      # its executable before PATH, and `dirigent` finds the desktop there. That is next to
      # the executable's real path, so `bin/` symlinks to them. Copied, not linked: a symlink
      # would resolve to the crate's own store path, which has none of the others. The first
      # package names the result.
      withTools =
        pkgs':
        let
          name = (builtins.head pkgs').meta.mainProgram;
          place = pkg: ''
            cp ${lib.getExe pkg} $out/libexec/dirigent/
            ln -s ../libexec/dirigent/${pkg.meta.mainProgram} $out/bin/${pkg.meta.mainProgram}
          '';
        in
        pkgs.runCommand "${name}-${version}" { meta.mainProgram = name; } ''
          mkdir -p $out/bin $out/libexec/dirigent
          ln -s ${lib.getExe cuaDriver} $out/libexec/dirigent/cua-driver
          ln -s ${cuaDriverLicense} $out/libexec/dirigent/cua-driver-LICENSE.md
          ln -s ${pkgs.xvfb}/bin/Xvfb $out/libexec/dirigent/Xvfb
          ${lib.concatMapStrings place pkgs'}
        '';

      # The `dirigent` command line (`dirigent [dir]`, `dirigent key`, `dirigent login`).
      cli = crate "dirigent_cli" { meta.mainProgram = "dirigent"; };
    in
    {
      packages.${system} = rec {
        default = dirigent_desktop;

        dirigent_desktop = withTools [
          (crate "dirigent_desktop" {
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
          })
          cli
        ];

        # The service for remote hosts (the desktop embeds its own).
        dirigent_service = withTools [
          (crate "dirigent_service" { })
          cli
        ];

        dirigent_api = crate "dirigent_api" { };
      };

      # `services.dirigent-api`: the update api (`dirigent_api serve`), to put behind the TLS
      # proxy (packaging/README.md). It holds no secrets: updates read the public bucket, and the
      # leaderboard (on, its db in the state dir) checks the forge's signed tokens. More
      # environment (`S3_PREFIX`, `DIRIGENT_LOG`) goes into
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
            url = lib.mkOption {
              type = lib.types.str;
              default = "https://dirigent.sebba.dev";
              description = "Where the api is reached (`DIRIGENT_API_URL`): the audience of the forge tokens the leaderboard takes.";
            };
            leaderboard = {
              forgeUrl = lib.mkOption {
                type = lib.types.str;
                default = "https://forge.sebba.dev";
                description = "The forge players sign in with (`FORGE_URL`).";
              };
            };
          };

          config = lib.mkIf cfg.enable {
            systemd.services.dirigent-api = {
              description = "Dirigent update api";
              wantedBy = [ "multi-user.target" ];
              after = [ "network.target" ];
              environment = {
                DIRIGENT_API_BIND = cfg.bind;
                DIRIGENT_API_URL = cfg.url;
                DIRIGENT_API_DB = "/var/lib/dirigent-api/leaderboard.db";
                FORGE_URL = cfg.leaderboard.forgeUrl;
                S3_ENDPOINT = cfg.s3.endpoint;
                S3_REGION = cfg.s3.region;
                S3_BUCKET = cfg.s3.bucket;
              };
              serviceConfig = {
                ExecStart = "${lib.getExe cfg.package} serve";
                DynamicUser = true;
                # /var/lib/dirigent-api: the leaderboard's db.
                StateDirectory = "dirigent-api";
                Restart = "on-failure";
              };
            };
          };
        };

      devShells.${system} = {
        default = pkgs.mkShell {
          packages = [
            rustToolchain
            pkgs.nodejs # `scripts/ui-rpc.mjs`
            pkgs.oxlint
            pkgs.oxfmt
            pkgs.pkg-config
            pkgs.cmake
            pkgs.sccache
            # Computer use in a dev checkout (`dirigent_agent`'s `computer` tool): the driver, and
            # Xvfb for a thread's private screen, where the UI is tested too (testing.md)
            cuaDriver
            pkgs.xvfb
            pkgs.xdotool # `scripts/ui.sh` makes the test app's window fill its screen
            pkgs.imagemagick # crop a picture of the test app to the part to look at (testing.md)
            (pkgs.lib.getBin pkgs.sqlite) # peek into a test session's dbs
            # What `scripts/ui.sh` runs besides (setsid, ss, pgrep / pkill): not on every user's PATH
            pkgs.util-linux
            pkgs.iproute2
            pkgs.procps
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

          # sccache's server listens on one TCP port for the whole machine by default, so a second
          # user's builds reach the first user's server, which compiles as that user and can't read
          # their files. A socket in the home dir gives every user their own server, shared by all
          # their checkouts and agents. Not in /tmp: another user could own the path first. Not in
          # $XDG_RUNTIME_DIR: a service user has none. Builds that start together while no server
          # runs each start one (a new server replaces the socket file); the extras exit when idle.
          # A hook because `env` is static; the mkdir because sccache won't create the directory.
          # `~`, not $HOME: bash resolves it even when HOME is unset.
          shellHook = ''
            mkdir -p ~/.cache
            export SCCACHE_SERVER_UDS=~/.cache/sccache.sock
          '';
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
