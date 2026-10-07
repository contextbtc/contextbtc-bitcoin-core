{
  description = "ContextBTC";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      flake-utils,
    }:
    # Per-system outputs (packages, apps, devShells, formatter).
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        # Toolchain pinned via rust-toolchain.toml if present, otherwise stable.
        rustToolchain =
          if builtins.pathExists ./rust-toolchain.toml then
            pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml
          else
            pkgs.rust-bin.stable.latest.default.override {
              extensions = [
                "rust-src"
                "rust-analyzer"
                "clippy"
                "rustfmt"
              ];
            };

        nativeBuildInputs = with pkgs; [
          pkg-config
        ];

        buildInputs =
          with pkgs;
          [
            openssl
          ]
          ++ lib.optionals stdenv.isDarwin [
            libiconv
            darwin.apple_sdk.frameworks.Security
            darwin.apple_sdk.frameworks.SystemConfiguration
          ];

        # The compiled workspace. Produces the binaries in $out/bin:
        # `contextbtc-server`, `contextbtc-electrs-server`, `contextbtc-client`
        # and `contextbtc-electrum-client-cli`.
        contextbtc = pkgs.rustPlatform.buildRustPackage {
          pname = "contextbtc";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          inherit nativeBuildInputs buildInputs;
          # The e2e tests need external services (bitcoind, a Nostr relay) that
          # aren't available in the sandboxed build. They run in CI instead.
          doCheck = false;
          # `nix run` defaults to the server binary.
          meta.mainProgram = "contextbtc-server";
        };
      in
      {
        packages = {
          default = contextbtc;
          contextbtc = contextbtc;
        };

        apps = {
          default = {
            type = "app";
            program = "${contextbtc}/bin/contextbtc-server";
          };
          server = {
            type = "app";
            program = "${contextbtc}/bin/contextbtc-server";
          };
          electrs-server = {
            type = "app";
            program = "${contextbtc}/bin/contextbtc-electrs-server";
          };
          client = {
            type = "app";
            program = "${contextbtc}/bin/contextbtc-client";
          };
          electrum-cli = {
            type = "app";
            program = "${contextbtc}/bin/contextbtc-electrum-client-cli";
          };
        };

        devShells.default = pkgs.mkShell {
          inherit nativeBuildInputs buildInputs;

          packages = [
            rustToolchain
            pkgs.nak
            # bitcoind for the e2e integration tests (corepc-node finds it via
            # BITCOIND_EXE below). nak provides the local relay (`nak serve`).
            pkgs.bitcoind
            # Package manager, dev server and bundler for the web client in `web/`.
            pkgs.bun
          ];

          env = {
            RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
            # corepc-node resolves the bitcoind binary from this first.
            BITCOIND_EXE = "${pkgs.bitcoind}/bin/bitcoind";
            ELECTRS_EXE = "${pkgs.electrs}/bin/electrs";
          };

          shellHook = ''
            echo "rust dev shell ready: $(rustc --version)"
          '';
        };

        formatter = pkgs.nixfmt;
      }
    )
    # System-independent outputs. NixOS modules must live outside
    # `eachDefaultSystem` because they are not per-system.
    // {
      nixosModules.default =
        {
          config,
          lib,
          pkgs,
          ...
        }:
        let
          cfg = config.services.contextbtc;
        in
        {
          options.services.contextbtc = {
            enable = lib.mkEnableOption "ContextBTC MCP server";

            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.system}.default;
              defaultText = lib.literalExpression "contextbtc.packages.\${system}.default";
              description = "The contextbtc package to run.";
            };

            relayUrls = lib.mkOption {
              type = lib.types.listOf lib.types.str;
              default = [ "ws://localhost:10547" ];
              description = "Nostr relay websocket URLs (sets NOSTR_RELAY_URLS).";
            };

            extraEnvironment = lib.mkOption {
              type = lib.types.attrsOf lib.types.str;
              default = { };
              example = lib.literalExpression ''{ BITCOIN_RPC_URL = "http://127.0.0.1:8332"; }'';
              description = ''
                Extra non-secret environment variables for the service, e.g.
                BITCOIN_RPC_URL. Put secrets (SERVER_NOSTR_SECRET_KEY,
                BITCOIN_RPC_PASSWORD, ...) in `environmentFile` instead.
              '';
            };

            environmentFile = lib.mkOption {
              type = lib.types.nullOr lib.types.path;
              default = null;
              example = "/run/secrets/contextbtc.env";
              description = ''
                Path to an EnvironmentFile with secrets. Read at service start,
                so it is never written to the world-readable Nix store.
              '';
            };
          };

          config = lib.mkIf cfg.enable {
            systemd.services.contextbtc = {
              description = "ContextBTC MCP server";
              wantedBy = [ "multi-user.target" ];
              after = [ "network-online.target" ];
              wants = [ "network-online.target" ];

              environment = {
                NOSTR_RELAY_URLS = lib.concatStringsSep "," cfg.relayUrls;
              }
              // cfg.extraEnvironment;

              serviceConfig = {
                ExecStart = lib.getExe cfg.package;
                EnvironmentFile = lib.mkIf (cfg.environmentFile != null) cfg.environmentFile;
                Restart = "on-failure";
                RestartSec = 5;

                # Run as an unprivileged, isolated dynamic user.
                DynamicUser = true;
                ProtectSystem = "strict";
                ProtectHome = true;
                PrivateTmp = true;
                NoNewPrivileges = true;
              };
            };
          };
        };
    };
}
