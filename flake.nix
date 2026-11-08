{
  description = "anipv development shells";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
    }:
    let
      inherit (nixpkgs) lib;
      # The release targets.
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      pkgsFor = lib.genAttrs systems (
        system:
        import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        }
      );
      forAllSystems = f: lib.mapAttrs (_: f) pkgsFor;

      # Cargo.toml's rust-version, e.g. "1.99". Raising it past what the locked
      # rust-overlay knows needs `nix flake update rust-overlay`.
      rustVersion = (lib.importTOML ./Cargo.toml).package.rust-version;

      # rust-toolchain.toml's stable with rustfmt and clippy, as rustup has it,
      # minus rust-docs (700 MiB nothing here reads), plus rust-src
      # (rust-analyzer reads std's source from the toolchain on PATH).
      stableFor =
        pkgs:
        let
          toolchain = (lib.importTOML ./rust-toolchain.toml).toolchain;
        in
        pkgs.rust-bin.fromRustupToolchain (
          toolchain
          // {
            profile = "minimal";
            components = toolchain.components ++ [ "rust-src" ];
          }
        );
    in
    {
      # The newest release of rust-version, as rustup's `+1.99` and CI's "1.99"
      # pick it; stable itself while that is the MSRV (each toolchain is
      # 170+ MiB of its own). Not in the shell: `just msrv` builds it.
      packages = forAllSystems (
        pkgs:
        let
          stable = stableFor pkgs;
        in
        {
          msrv =
            if lib.versions.majorMinor stable.version == rustVersion then
              stable
            else
              pkgs.rust-bin.fromRustupToolchain {
                channel = rustVersion;
                profile = "minimal";
              };
        }
      );

      devShells = forAllSystems (
        pkgs:
        let
          default = pkgs.mkShell {
            packages = [
              (stableFor pkgs)
              pkgs.just
              pkgs.cargo-deny # just check
              pkgs.cargo-nextest # what CI runs the tests with
              pkgs.cargo-insta # just update-fixtures
              pkgs.jq # just release
              # The tape's `Set Shell "bash"` (just record) takes bash from
              # PATH, and stdenv's (without readline) prints the prompt's
              # `\[ \]` literally. Here, not only in .#record: under direnv,
              # `just record` with the system's vhs runs in this shell.
              pkgs.bashInteractive
            ];
            # `just msrv` runs this instead of rustup's `cargo +1.99` (with the
            # flag, as direnv's `use flake` doesn't need flakes in nix.conf).
            ANIPV_MSRV_CARGO = "nix --extra-experimental-features 'nix-command flakes' shell .#msrv --command cargo";
            # mpv and ffmpeg come from the system: a Nix mpv would shadow it
            # for `just run`, without the host's GPU drivers. The real-mpv
            # test skips itself without them.
          };
        in
        {
          inherit default;

          # `nix develop .#record --command just record`: vhs brings Chromium,
          # so it has a shell of its own.
          record = default.overrideAttrs (old: {
            nativeBuildInputs = old.nativeBuildInputs ++ [
              pkgs.vhs
              pkgs.ttyd
            ];
          });
        }
      );

      # `nix flake check`: what CI and the flake.lock updates check.
      checks = forAllSystems (
        pkgs:
        let
          inherit (pkgs.stdenv.hostPlatform) system;
          shell = self.devShells.${system}.default;
        in
        {
          # The default shell has every tool the justfile and CI call, and
          # `just msrv` the MSRV.
          tools = pkgs.runCommand "anipv-shell-tools" { inherit (shell) nativeBuildInputs; } ''
            cargo fmt --version
            cargo clippy --version
            cargo deny --version
            cargo nextest --version
            cargo insta --version
            just --version
            jq --version
            ${self.packages.${system}.msrv}/bin/cargo --version | grep -E '^cargo ${lib.escapeRegex rustVersion}[. ]'
            touch $out
          '';
          fmt = pkgs.runCommand "flake-fmt" { } ''
            ${lib.getExe self.formatter.${system}} --check ${./flake.nix}
            touch $out
          '';
        }
      );

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
