{
  description = "A Nix binary cache server";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";

    crane = {
      url = "github:ipetkov/crane";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.flake-compat.follows = "flake-compat";
      inputs.flake-utils.follows = "flake-utils";
    };

    flake-compat = {
      url = "github:edolstra/flake-compat";
      flake = false;
    };

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.flake-utils.follows = "flake-utils";
    };

    cargo2nix = {
      url = "github:cargo2nix/cargo2nix/main";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.flake-utils.follows = "flake-utils";
      inputs.rust-overlay.follows = "rust-overlay";
    };
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
    crane,
    cargo2nix,
    rust-overlay,
    ...
  }: let
    supportedSystems = flake-utils.lib.defaultSystems;

    makeCranePkgs = pkgs: let
      craneLib = crane.mkLib pkgs;
    in
      pkgs.callPackage ./crane.nix {inherit craneLib;};
  in
    flake-utils.lib.eachSystem supportedSystems (system: let
      pkgs = import nixpkgs {
        inherit system;
        overlays = [
          cargo2nix.overlays.default
          (import rust-overlay)
        ];
      };
      rustPkgs = pkgs.rustBuilder.makePackageSet {
        packageFun = import ./Cargo.nix;
        rustChannel = "stable";
        rustVersion = "latest";
        packageOverrides = pkgs:
          pkgs.rustBuilder.overrides.all
          ++ [
            (pkgs.rustBuilder.rustLib.makeOverride {
              name = "cxx";
              overrideAttrs = old: {
                postInstall = ''
                  mkdir -p $out/include/rust
                  cp include/cxx.h $out/include/rust/cxx.h
                '';
              };
            })
            (pkgs.rustBuilder.rustLib.makeOverride {
              name = "attic";
              overrideAttrs = old: {
                buildInputs =
                  old.buildInputs
                  ++ [
                    pkgs.nix
                    pkgs.boost
                  ];
              };
            })
          ];
      };

      inherit (pkgs) lib;
    in rec {
      packages =
        {
          default = packages.attic;

          attic = rustPkgs.workspace.attic {};
          attic-client = rustPkgs.workspace.attic-client {};
          attic-server = rustPkgs.workspace.attic-server {};
          attic-queue = rustPkgs.workspace.attic-queue {};

          attic-nixpkgs = pkgs.callPackage ./package.nix {};

          # TODO: Make this work with Crane
          attic-static =
            (pkgs.pkgsStatic.callPackage ./package.nix {
              nix = pkgs.pkgsStatic.nix.overrideAttrs (old: {
                patches =
                  (old.patches or [])
                  ++ [
                    # To be submitted
                    (pkgs.fetchpatch {
                      url = "https://github.com/NixOS/nix/compare/3172c51baff5c81362fcdafa2e28773c2949c660...6b09a02536d5946458b537dfc36b7d268c9ce823.diff";
                      hash = "sha256-LFLq++J2XitEWQ0o57ihuuUlYk2PgUr11h7mMMAEe3c=";
                    })
                  ];
              });
            })
            .overrideAttrs (old: {
              nativeBuildInputs =
                (old.nativeBuildInputs or [])
                ++ [
                  pkgs.nukeReferences
                ];

              # Read by pkg_config crate (do some autodetection in build.rs?)
              PKG_CONFIG_ALL_STATIC = "1";

              "NIX_CFLAGS_LINK_${pkgs.pkgsStatic.stdenv.cc.suffixSalt}" = "-lc";
              RUSTFLAGS = "-C relocation-model=static";

              postFixup =
                (old.postFixup or "")
                + ''
                  rm -f $out/nix-support/propagated-build-inputs
                  nuke-refs $out/bin/attic
                '';
            });

          attic-client-static = packages.attic-static.override {
            clientOnly = true;
          };

          attic-ci-installer = pkgs.callPackage ./ci-installer.nix {
            inherit self;
          };

          book = pkgs.callPackage ./book {
            attic = packages.attic;
          };
        }
        // (lib.optionalAttrs pkgs.stdenv.isLinux {
          attic-server-image = pkgs.dockerTools.buildImage {
            name = "attic-server";
            tag = "main";
            copyToRoot = [
              # Debugging utilities for `fly ssh console`
              pkgs.busybox
              packages.attic-server

              # Now required by the fly.io sshd
              pkgs.dockerTools.fakeNss
            ];
            config = {
              Entrypoint = ["${packages.attic-server}/bin/atticd"];
              Cmd = ["--mode" "api-server"];
              Env = [
                "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
              ];
            };
          };
        });

      devShells = {
        default = pkgs.mkShell {
          inputsFrom = with packages; [attic book];
          nativeBuildInputs = with pkgs;
            [
              rustc

              rustfmt
              clippy
              cargo-expand
              cargo-outdated
              cargo-edit
              tokio-console

              sqlite-interactive

              editorconfig-checker

              flyctl

              wrk

              cargo2nix.packages.${system}.cargo2nix
            ]
            ++ (lib.optionals pkgs.stdenv.isLinux [
              linuxPackages.perf
            ]);

          NIX_PATH = "nixpkgs=${pkgs.path}";
          RUST_SRC_PATH = "${pkgs.rustPlatform.rustcSrc}/library";

          ATTIC_DISTRIBUTOR = "dev";
        };

        demo = pkgs.mkShell {
          nativeBuildInputs = [
            packages.default
          ];

          shellHook = ''
            >&2 echo
            >&2 echo '🚀 Run `atticd` to get started!'
            >&2 echo
          '';
        };
      };
      devShell = devShells.default;
    })
    // {
      overlays = {
        default = final: prev: let
          cranePkgs = makeCranePkgs final;
        in {
          inherit (cranePkgs) attic attic-client attic-server;
        };
      };

      nixosModules = {
        atticd = {
          imports = [
            ./nixos/atticd.nix
          ];

          services.atticd.useFlakeCompatOverlay = false;

          nixpkgs.overlays = [
            self.overlays.default
          ];
        };
      };
    };
}
