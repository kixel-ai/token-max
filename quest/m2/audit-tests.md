# [M] Audit test coverage

## Goal

Run the `tests` audit: `quest audit tests` prints what it checks, what each finding becomes, and how to plan its next run.

## Plan

Baseline (m1 run, 2026-11-20): the one package, `token-max`, covers 96.44% of
lines (56,723 of 58,815), 96.07% of functions and 95.87% of regions, every
suite included. The least-covered files are `ssh.rs` (88.6%), `mcp.rs` (88.9%),
`doctor.rs` (92.9%) and `descriptions.rs` (65.4%, static strings).

Measure: CI keeps no coverage report. Use `cargo-llvm-cov` from nixpkgs with a
rustup stable toolchain and `llvm-tools-preview` installed under `.tmp`
(`RUSTUP_HOME` and `CARGO_HOME` there); the dev shell's Nix rustc segfaults in
its profiler runtime while writing value-profile data at exit. `tests/cli.rs`
and `tests/ssh.rs` clear the environment of the binary they run, so its
profiles land as `default_*.profraw` in the checkout's root: move them into
`target/llvm-cov-target` before `cargo llvm-cov report`, or `cli.rs` reads as
17% covered. A Linux run cannot see the `cfg(windows)` branches, and
`tests/ssh.rs` runs only on Unix.

Mirrors to recheck, each against what it copies:

- `test/fixtures/t3-schema.sql`: every index the pinned T3 revision's
  migrations create over its columns, verbatim (the m1 run found two
  mis-copied and 13 missing).
- `test/fixtures/codex.jsonl` and `claude.jsonl`: their records' key paths
  against a heavy user's real transcripts, comparing key names and types
  only, never values (the m1 run found the Codex tier in `turn_context`,
  where Codex never writes it).
- `test/fixtures/prices.json`: its shape against models.dev's `api.json`.

Check first: `report.rs` (351 uncovered lines) and `idle.rs` (174), which the
m1 run did not reach, and the guards no test can yet reach: an SSH
destination's result arriving before its request, and doctor's re-read after
taking Claude Code's state-file lock.

## Audit

- tests

## Scheduled

- 2026-11-27 - a week after the m1 run, so a fast m1 does not rerun it at once
