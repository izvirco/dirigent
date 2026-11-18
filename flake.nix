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
      lilexDir = "${pkgs.lilex}/share/fonts/truetype";

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
      rustSrc = lib.fileset.toSource {
        root = ./.;
        fileset = lib.fileset.unions [
          ./Cargo.toml
          ./Cargo.lock
          ./changelog.md
          ./dirigent_agent
          ./dirigent_api
          ./dirigent_auth
          ./dirigent_cli
          ./dirigent_desktop
          ./dirigent_protocol
          ./dirigent_service
        ];
      };
      cargoDeps = rustPlatform.fetchCargoVendor {
        name = "dirigent";
        src = rustSrc;
        hash = "sha256-jut3H54VsnOHyDsLBYxHZ0XU7tV6Q3GNDi/WnTNfDLo=";
      };
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
            doCheck = false;
            meta.mainProgram = name;
          }
          // args
        );

      # MUST KILL: rearchitect: one Cua Driver pin that cuaDriver and packaging/cua-driver.sh both read
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

      withTools =
        pkgs':
        let
          name = (builtins.head pkgs').meta.mainProgram;
          # MUST KILL: rename: place copies each binary beside the tools (copyBesideTools), not links it
          place = pkg: ''
            cp ${lib.getExe pkg} $out/libexec/dirigent/
            ln -s ../libexec/dirigent/${pkg.meta.mainProgram} $out/bin/${pkg.meta.mainProgram}
          '';
        in
        pkgs.runCommand "${name}-${version}" { meta.mainProgram = name; } ''
          mkdir -p $out/bin $out/libexec/dirigent
          ln -s ${lib.getExe cuaDriver} $out/libexec/dirigent/cua-driver
          ln -s ${cuaDriverLicense} $out/libexec/dirigent/cua-driver-LICENSE.md
          ln -s ${pkgs.sway-unwrapped}/bin/sway $out/libexec/dirigent/sway
          ln -s ${lib.getExe pkgs.wtype} $out/libexec/dirigent/wtype
          ${lib.concatMapStrings place pkgs'}
        '';

      cli = crate "dirigent_cli" { meta.mainProgram = "dirigent"; };
    in
    {
      packages.${system} = rec {
        default = dirigent_desktop;

        dirigent_desktop = withTools [
          (crate "dirigent_desktop" {
            nativeBuildInputs = [
              pkgs.pkg-config
              pkgs.python3 # stylo's build script runs it (blitz-dom)
            ];
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

        dirigent_service = withTools [
          (crate "dirigent_service" { })
          cli
        ];

        dirigent_api = crate "dirigent_api" { };
      };

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
            pkgs.nodejs
            pkgs.oxlint
            pkgs.oxfmt
            pkgs.pkg-config
            pkgs.cmake
            pkgs.python3 # stylo's build script runs it (blitz-dom)
            pkgs.sccache
            cuaDriver
            pkgs.sway-unwrapped
            pkgs.wtype
            pkgs.imagemagick
            (pkgs.lib.getBin pkgs.sqlite)
            pkgs.util-linux
            pkgs.iproute2
            pkgs.procps
          ]
          ++ gpuiLibs;

          env = {
            RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
            RUSTC_WRAPPER = "sccache";
            SCCACHE_CACHE_SIZE = "50G";
            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath gpuiLibs;
            # Software Vulkan (lavapipe): a private screen has no GPU presentation.
            DIRIGENT_UI_VK_ICD = "${pkgs.mesa}/share/vulkan/icd.d/lvp_icd.x86_64.json";
            DIRIGENT_LILEX_DIR = lilexDir;
          };

          # sccache's server listens on one TCP port for the whole machine by default, so a second
          # user's builds reach the first user's server, which compiles as that user and can't read
          # their files. A hook because `env` is static; the mkdir because sccache won't create the
          # directory.
          shellHook = ''
            mkdir -p ~/.cache
            export SCCACHE_SERVER_UDS=~/.cache/sccache.sock
          '';
        };

        windows = pkgs.mkShell {
          packages = [
            (rustToolchain.override { targets = [ "x86_64-pc-windows-gnu" ]; })
            pkgs.pkgsCross.mingwW64.stdenv.cc
            pkgs.python3 # stylo's build script runs it (blitz-dom)
          ];
          env.DIRIGENT_LILEX_DIR = lilexDir;
        };
      };
    };
}
