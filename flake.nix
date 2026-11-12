# alforria — Nix flake
#
# Run:      nix run github:alforria-ai/alforria
# Install:  nix profile install github:alforria-ai/alforria
#
# NixOS: add `github:alforria-ai/alforria` as a flake input and import
# `nixosModules.default` to install it system-wide.
#
# Nixpkgs: `overlays.default` provides `pkgs.alforria`.
{
  description = "alforria — agentic coding tool with TUI, HTTP server and web UI";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f (import nixpkgs { inherit system; }));

      version = (nixpkgs.lib.importTOML ./Cargo.toml).workspace.package.version;

      mkAlforria =
        pkgs:
        pkgs.rustPlatform.buildRustPackage {
          pname = "alforria";
          inherit version;
          src = self;

          cargoLock.lockFile = ./Cargo.lock;
          cargoBuildFlags = [
            "-p"
            "alforria"
          ];

          nativeBuildInputs = [
            pkgs.makeBinaryWrapper
            pkgs.pkg-config
          ];
          buildInputs = [ pkgs.openssl ];

          # alforria shells out to git (snapshots, worktrees, vcs) and to a
          # POSIX shell; guarantee both are on PATH regardless of the
          # user's environment.
          postFixup = ''
            wrapProgram $out/bin/alforria \
              --prefix PATH : ${
                pkgs.lib.makeBinPath [
                  pkgs.bash
                  pkgs.git
                ]
              }
          '';

          # The test suite spawns servers and PTYs and is exercised by CI;
          # it does not fit the derivation sandbox.
          doCheck = false;

          meta = {
            description = "Supervise a fleet of AI coding agents from the terminal, browser or CLI";
            homepage = "https://github.com/alforria-ai/alforria";
            license = pkgs.lib.licenses.mit;
            mainProgram = "alforria";
            platforms = [
              "x86_64-linux"
              "aarch64-linux"
              "aarch64-darwin"
            ];
          };
        };
    in
    {
      packages = forAllSystems (pkgs: rec {
        alforria = mkAlforria pkgs;
        default = alforria;
      });

      overlays.default = final: _prev: {
        alforria = mkAlforria final;
      };

      nixosModules.default = { pkgs, ... }: {
        environment.systemPackages = [
          self.packages.${pkgs.stdenv.hostPlatform.system}.alforria
        ];
      };

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = [
            pkgs.bun
            pkgs.cargo
            pkgs.cargo-nextest
            pkgs.clippy
            pkgs.git
            pkgs.openssl
            pkgs.pkg-config
            pkgs.rust-analyzer
            pkgs.rustc
            pkgs.rustfmt
          ];
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
