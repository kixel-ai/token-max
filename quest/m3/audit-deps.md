# [M] Audit dependencies

## Goal

Run the `deps` audit: `quest audit deps` prints what it checks, what each finding becomes, and how to plan its next run.

## Plan

Baseline: the m2 run covered releases through 2026-11-27.

Scope: the crates in `Cargo.toml`, the GitHub Actions pinned in
`.github/workflows`, cargo-dist's version in
`dist-workspace.toml` and `.github/workflows/release.yml`, the Rust toolchain
the Nix dev shell provides, and `site/wrangler.jsonc`'s compatibility date.
The flake's nixpkgs, flake-utils and quest inputs are commit pins, so they are
skipped. There is no vendored code or fork. The site serves static assets with
no Worker code, so a new compatibility flag matters only if it changes how
assets are served.

Check first: whether a pinned action's comment names a release tag (the m1 run
found the Nix installer pinned to an untagged `main` commit), and whether a
crate added since the last run turns on default features nothing uses (the m2
run found crossterm's `derive-more` feature pulling in six unused crates).

## Audit

- deps

## Required

- [m2](/quest/m2/README.md) - the audit runs once per milestone, after m2's work

## Scheduled

- 2026-12-04 - a week after the m2 run, so a fast m2 does not rerun it at once
