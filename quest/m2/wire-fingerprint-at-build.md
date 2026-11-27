# [S] Hash the wire fingerprint's sources at build time

## Goal

The released binary no longer embeds the source text that `wire::build()`
hashes for the `--ssh` stream's build fingerprint, and the fingerprint stays
identical across the platforms one commit builds for, since a coordinator
compares it with a destination's installed release build.

## Plan

`wire::build()` hashes thirteen source files, tests included, with
`include_str!` at run time, so the binary carries their text: 1.50 MB at
104ec46, against 0.92 MB at v0.0.1, measured by the m2 `cost` audit. That
is about 8% of the 19.5 MB `dist` binary and roughly 300 KB of its 5.5 MB
`xz` archive, and it grows with every edit to `report.rs`, `doctor.rs` or
`vibes.rs`.

A build script would hash the same bytes, in the same order, and pass the
digest in with `cargo:rustc-env`, printing `cargo:rerun-if-changed` for each
file. Add `/build.rs` to the crate's `include` list, and check that the digest
matches the one the current code computes for the same commit.

Decided 2026-11-27 (maintainer, /quest-run): ✅ hash the sources in a build
script with `sha2` as a build-dependency (rejected: keep embedding the
sources; drop the quest).
