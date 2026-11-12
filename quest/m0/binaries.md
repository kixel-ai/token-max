# [M] Ship prebuilt binaries with cargo-dist

## Goal

Each release attaches prebuilt `token-max` binaries, their checksums, a shell
installer and a PowerShell installer to a GitHub Release, built by
cargo-dist. `cargo binstall token-max` downloads a binary instead of
compiling, and `nix run github:kixel-ai/token-max` works from the flake.
`cargo install` keeps working. Nothing is released by a PR, a tag or a merge:
the release still runs only by dispatch on protected `main`.

## Plan

Decided 2026-11-11 (maintainer, /quest-plan for the follow-ups of
[kixelated/token-max#62](https://github.com/kixelated/token-max/pull/62)).
The tool is for people, so running it should not need a Rust toolchain.
Decision prompts, chosen options marked ✅:

- Goal and milestone: ✅ binaries and a curl-piped installer in m0, so
  0.1.0 ships with them (rejected: m1; binaries with no install script).
- Other channels: ✅ cargo-binstall, ✅ a PowerShell installer, ✅ a Nix
  flake package (rejected: a Homebrew tap, which needs a second repository
  and a token that pushes to it on every release).
- Tooling: ✅ cargo-dist (rejected: a hand-written build matrix and
  installers). It released v0.34.0 on 2026-11-02 and generates the matrix,
  both installers, checksums and build attestations from one config.

The short install URL and `run.sh` are
[their own quest](/quest/m0/install-site.md).

Agent's choices, for the implementer to revisit:

- Targets: Linux x86_64 and aarch64, statically linked with musl; macOS
  aarch64 and x86_64; Windows x86_64. CI already tests on all three systems.
- Fit cargo-dist's generated workflow to the protected path in
  `docs/releasing.md`: dispatch on `main` only, and the job that creates the
  GitHub Release holds `contents: write` only inside an environment
  restricted to `main`. One dispatch should publish the crate and the
  release, or fail both.
- Turn on GitHub build attestations.
- The flake today has a dev shell and no package output. The package builds
  from source, so it needs no release.
- `releases/latest` does not exist before 0.1.0. Validate with cargo-dist's
  local build and plan output; m0's own release work verifies the published
  assets.

The same PR updates `docs/releasing.md` and the README's install section.

## Required

- [Move the repository to the kixel-ai organization](/quest/m0/kixel-ai.md) - the release URLs and the flake reference contain the owner
