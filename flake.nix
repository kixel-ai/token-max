{
  description = "token-max: recommends cheaper agent settings from your Claude Code and Codex transcripts";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    # The quest CLI, which serves the quest guide and the skills the stubs in
    # .claude/skills and .agents/skills call. Tracks main; flake.lock pins the
    # revision, so `nix flake update quest` upgrades it.
    quest = {
      url = "github:moq-dev/quest";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.flake-utils.follows = "flake-utils";
    };
  };

  outputs =
    {
      nixpkgs,
      flake-utils,
      rust-overlay,
      quest,
      ...
    }:
    flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ] (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };
        rust = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      in
      {
        packages.quest = quest.packages.${system}.default;

        devShells.default = pkgs.mkShell {
          packages = [
            rust
            pkgs.just
            quest.packages.${system}.default
          ];
        };
      }
    );
}
