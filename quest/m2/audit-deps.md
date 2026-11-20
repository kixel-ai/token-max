# [M] Audit dependencies

## Goal

Run the `deps` audit: `quest audit deps` prints what it checks, what each finding becomes, and how to plan its next run.

## Plan

Baseline: the m1 run covered releases through 2026-11-20.

Scope: the crates in `Cargo.toml`, the GitHub Actions pinned in
`.github/workflows` (Pullfrog's pin follows the org standard in dotfiles'
`repo-settings`, so bump it there), cargo-dist's version in
`dist-workspace.toml` and `.github/workflows/release.yml`, the Rust toolchain
the Nix dev shell provides, and `site/wrangler.jsonc`'s compatibility date.
The flake's nixpkgs, flake-utils and quest inputs are commit pins, so they are
skipped. There is no vendored code or fork.

Check first: whether a pinned action's comment names a release tag (the m1 run
found the Nix installer pinned to an untagged `main` commit), and whether the
Nix installer still supports upstream Nix (`determinate: false`).

## Audit

- deps

## Scheduled

- 2026-11-27 - a week after the m1 run, so a fast m1 does not rerun it at once

## Required

- [m1](/quest/m1/README.md) - runs once per milestone
