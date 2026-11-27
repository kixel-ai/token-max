# [M] Audit test coverage

## Goal

Run the `tests` audit: `quest audit tests` prints what it checks, what each finding becomes, and how to plan its next run.

## Plan

Baseline (m2 run, 2026-11-27): the one package, `token-max`, covers 95.75% of
lines (82,296 of 85,951), 95.64% of functions and 95.04% of regions, every
suite included. The least-covered files are `upgrade.rs` (71.8%, see below),
`tui.rs` (82.6%), `cli.rs` (84.4%, mostly the piggy loading panel and the TUI
walk) and `descriptions.rs` (65.8%, static strings).

Measure: CI keeps no coverage report. Use `cargo-llvm-cov` from nixpkgs with a
rustup stable toolchain and `llvm-tools-preview` installed under `.tmp`
(`RUSTUP_HOME` and `CARGO_HOME` there, rustup itself from nixpkgs), run inside
`nix develop` for the linker and SQLite; the dev shell's Nix rustc segfaults in
its profiler runtime. `tests/support`'s `isolated` keeps `LLVM_PROFILE_FILE`,
so the binary the integration tests spawn writes its profiles beside the test
binaries' (check no `default_*.profraw` lands in the checkout). A few of those
profiles come out truncated, from destinations a cancel test kills mid-exit,
and fail the merge: collect with `cargo llvm-cov --no-report`, set aside each
profile `llvm-profdata show` rejects, then run `cargo llvm-cov report`. What coverage cannot see: the `cfg(windows)`
branches on Linux, `tests/ssh.rs` off Unix, and a process that `exec`s, such
as doctor's rerun after an upgrade, so `upgrade.rs`'s staging reads as
uncovered though `tests/cli.rs` drives it.

Mirrors to recheck, each against what it copies:

- `test/fixtures/t3-schema.sql`: every index the pinned T3 revision's
  migrations create over its columns, verbatim, parsed so a statement spread
  over several lines counts (the m2 run found one the m1 copy missed).
- `test/fixtures/codex.jsonl` and the `claude*.jsonl` fixtures: their records'
  key paths against a heavy user's real transcripts, comparing key names and
  types only, never values. `compactMetadata.precomputed` is a real,
  feature-gated Claude Code field this host never wrote.
- `test/fixtures/prices.json`: its shape against models.dev's `api.json`.
- `test_support`'s OpenCode database and Antigravity root: their tables'
  columns, and their records' key paths and types, against a real
  `opencode.db` and `~/.gemini` transcripts.

Check first: `report.rs` (429 uncovered lines), `doctor.rs`'s background
feature edits when nothing sets the variable (the `{variable} (default)`
owner, never run), and the guards no test can yet reach: an SSH destination's
result arriving before its request, and doctor's re-read after taking Claude
Code's state-file lock.

## Audit

- tests

## Required

- [m2](/quest/m2/README.md) - the audit runs once per milestone, after m2's work

## Scheduled

- 2026-12-04 - a week after the m2 run, so a fast m2 does not rerun it at once
