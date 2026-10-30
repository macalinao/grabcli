{
  description = "grabcli: command-line client for Grab services (GrabFood first)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-parts = {
      url = "github:hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };
    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    git-hooks = {
      url = "github:cachix/git-hooks.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];

      imports = [
        inputs.treefmt-nix.flakeModule
        inputs.git-hooks.flakeModule
      ];

      perSystem =
        {
          config,
          self',
          pkgs,
          ...
        }:
        {
          # The whole workspace builds from Cargo.lock; only the `grab` binary
          # (crate grabcli) is installed. Tests run here too, so `nix flake
          # check` covers them.
          packages.grab = pkgs.rustPlatform.buildRustPackage {
            pname = "grab";
            version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;
            src = pkgs.lib.cleanSource ./.;
            cargoLock.lockFile = ./Cargo.lock;
            cargoBuildFlags = [
              "--package"
              "grabcli"
            ];
            cargoTestFlags = [ "--workspace" ];
            meta.mainProgram = "grab";
            meta.license = pkgs.lib.licenses.asl20;
          };
          packages.default = self'.packages.grab;

          checks.grab = self'.packages.grab;

          # One formatter for Rust + Nix; `nix fmt` works.
          treefmt = {
            projectRootFile = "flake.nix";
            programs.nixfmt.enable = true;
            programs.rustfmt.enable = true;
          };

          # Format on commit; clippy is a full typecheck, so only before push.
          pre-commit.settings.hooks = {
            treefmt = {
              enable = true;
              package = config.treefmt.build.wrapper;
            };
            clippy = {
              enable = true;
              stages = [ "pre-push" ];
              packageOverrides = {
                inherit (pkgs) cargo clippy;
              };
              settings = {
                extraArgs = "--workspace --all-targets";
                denyWarnings = true;
              };
            };
          };

          devShells.default = pkgs.mkShell {
            packages = [
              pkgs.cargo
              pkgs.rustc
              pkgs.clippy
              pkgs.rustfmt
              pkgs.rust-analyzer
              config.treefmt.build.wrapper
            ];
            shellHook = config.pre-commit.installationScript;
          };
        };
    };
}
