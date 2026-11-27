{
  description = "token-max: recommends cheaper agent settings from your Claude Code and Codex transcripts";

  inputs = {
    # quest's nixpkgs, so every repository's dev shell gets the same C compiler and mbx
    # shares build scripts, proc-macros, and the crates built on them across repositories.
    nixpkgs.follows = "quest/nixpkgs";
    flake-utils.url = "github:numtide/flake-utils";
    # The quest CLI, which serves the quest guide and the skills the stubs in
    # .claude/skills and .agents/skills call. Tracks main; flake.lock pins the
    # revision, so `nix flake update quest` upgrades it.
    quest = {
      url = "github:kixel-ai/quest";
      inputs.flake-utils.follows = "flake-utils";
    };
  };

  outputs =
    {
      nixpkgs,
      flake-utils,
      quest,
      ...
    }:
    flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ] (
      system:
      let
        pkgs = import nixpkgs { inherit system; };
      in
      {
        # `nix run github:kixel-ai/token-max` builds this from source.
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "token-max";
          version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              ./build.rs
              ./Cargo.toml
              ./Cargo.lock
              ./src
              ./skills/token-audit/SKILL.md
              ./tests/ssh_windows.rs
              ./tests/upgrade_windows.rs
            ];
          };
          cargoLock.lockFile = ./Cargo.lock;
          # nixpkgs' Cargo setup hook makes rusqlite link the system SQLite, so
          # the build needs it and pkg-config, bundled feature or not.
          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [ pkgs.sqlite ];
          # CI runs the tests; running the tool should not.
          doCheck = false;
          meta.mainProgram = "token-max";
        };

        packages.quest = quest.packages.${system}.default;

        devShells.default = pkgs.mkShell {
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.clippy
            pkgs.rustfmt
            pkgs.rust-analyzer
            pkgs.just
            pkgs.jq
            quest.packages.${system}.default
          ];
        };
      }
    );
}
