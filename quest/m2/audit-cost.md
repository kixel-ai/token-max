# [L] Audit build and CI cost

## Goal

Run the `cost` audit: `quest audit cost` prints what it checks, what each finding becomes, and how to plan its next run.

## Plan

Scope: the `Check` workflow (the merge gate), `Release`, the release archives and installers, and the crate. The repository is public, so
GitHub-hosted minutes cost no money; what CI costs is gate latency and the
org's runner concurrency, which macOS hits first.

Baseline from the m1 run (2026-11-20, main at 309b526), before its fixes:

- CI, from each job's own timing over the latest 600 Check runs
  (2026-11-16 to 2026-11-20): 1,993 runner minutes (ubuntu 794, windows-2025
  700, macos-15 500); 1,348 on pull requests, 336 in the merge queue, 309 on
  pushes to main. Source p50 139 s, Windows tests p50 155 s, macOS tests
  p50 101 s.
  A release is about 12 minutes of builds across five runners.
- Slowest test: `tests/ssh.rs::any_failed_destination_fails_the_whole_report`,
  71 to 115 s on the Linux runner and about 20 s on macOS, because each
  `--ssh` run hashes the 43 MB debug binary with an unoptimized sha2. With
  `sha2` at opt-level 3 the whole of `tests/ssh.rs` went from 10.8 s to 1.4 s
  locally (dev profile, default 32 threads, 627 tests in all).
- Build, on a 32-thread host under a load average of 12 to 17: cold
  `cargo test --no-run` 25.3 s wall, the `token-max` lib test (15.7 s) and lib
  (9.3 s) units slowest, then libsqlite3-sys's build script (4.9 s); an edit
  to `src/report.rs`, repeated, rebuilds the tests in 14 to 16 s.
- v0.0.1 assets: x86_64-linux-musl archive 4,567,080 bytes (binary
  15,429,536 unstripped, 12,777,856 stripped), aarch64-linux-musl 4,121,880,
  x86_64-darwin 4,250,884, aarch64-darwin 3,774,108, windows zip 5,599,366,
  source.tar.gz 1,756,394 (walk.gif is 717,546 of it), crate 842,687. Of the
  linux binary's sized symbols, `token_max` is 2.1 MiB, `core` 1.2 MiB, and
  bundled SQLite roughly 2 to 3 MiB of the C code.

Check first: the share of Check minutes spent on reruns of an already-tested
head, and whether quest-only changes still skip the Rust jobs (the m1 run made
the `Plan` job decide that). Compare the release assets with v0.0.1's sizes
above.

Tried and rejected in m1: stripping symbols from release binaries (saves
2.6 MB unpacked but only 330 KB of the 4.57 MB archive, and loses symbolized
panic backtraces); moving macOS tests off the gate (they never failed alone in
15 days, but run beside the slower Windows job, and their p90 queue wait of
180 s is runner concurrency, which removing the reruns relieves); a Cargo
cache (no run on main would save one now that pushes to main don't run
Check); a per-test time limit in the gate (it would depend on runner
wall-clock time).

## Audit

- cost

## Scheduled

- 2026-11-27 - a week after the m1 run, so a fast m1 does not rerun it at once
