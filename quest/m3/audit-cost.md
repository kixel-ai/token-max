# [L] Audit build and CI cost

## Goal

Run the `cost` audit: `quest audit cost` prints what it checks, what each finding becomes, and how to plan its next run.

## Plan

Scope: the `Check` workflow (the merge gate), the `macOS` workflow (after each
push to `main`), `Release`, the release archives and installers, and the
crate. The repository is public, so GitHub-hosted minutes cost no money; what
CI costs is gate latency and the org's runner concurrency, which macOS hits
first, since every kixel-ai repository shares its macOS runners.

Baseline from the m2 run (2026-11-27, main at 104ec46), before its fixes:

- CI, from each job's own timing over all 662 Check runs from 2026-11-20 20:13
  to 2026-11-27 08:31 UTC: 2,583 runner minutes (ubuntu 962, windows-2025 941,
  macos-15 680); 1,970 on pull requests and 612 in the merge queue. No run
  retested a head that already had a finished run. Successful jobs: Source
  p50 146 s (25 s on the quest-only path, 57 runs), Windows p50 184 s
  (p90 216), macOS p50 114 s (p90 143). Gate latency from run creation to the
  last test job: p50 220 s and p90 925 s on pull requests, p50 208 s and p90
  1,918 s in the merge queue.
- Runner waits: macOS p90 978 s and up to 84 minutes, while in 52 of the 65
  waits over 5 minutes no token-max macOS job was running, so other
  repositories held the org's macOS runners. Windows p90 33 s, ubuntu 57 s.
  Two successful queue runs took 69 and 74 minutes, past the ruleset's
  60-minute check timeout, and their heads never landed.
- Windows is now the gate's critical path (last in 145 of 265 full runs). Its
  build takes 2 m 17 s, and its lib tests 46 s against 1.8 s on Linux and
  8.8 s on macOS; the `t3` tests' SQLite fixtures, which synced every insert,
  were 4.4 of the lib tests' 7 s serial time locally.
- `tests/ssh.rs` took 30.9 s in Source (9.3 s on macOS), because CI built
  with full debug info: a 195 MB test binary against 54 MB without, which
  every `--ssh` run reads, hashes and copies.
- Build, on a 24-thread host under a load average of 5 to 9, with this host's
  `~/.cargo/config.toml` (incremental off, `debug = false`): cold
  `cargo test --no-run` 27.0 s wall (36.9 s with CI's then-default incremental
  and full debug info). The `token-max` lib test unit (20.6 s) and lib
  (12.2 s) are slowest, then libsqlite3-sys's build script (3.5 s). A
  repeated edit to `src/report.rs` rebuilds the tests in 19.4 to 20.2 s, the
  lib test unit (19.5 s) being the critical path. Since m1, `src/` grew from
  82,434 to 116,035 lines and tests from 622 to 855, matching the build's
  growth of about 30%.
- Tests, `cargo test --locked` at the dev profile with libtest's default
  threads on that host: 854 tests; lib 0.34 s, `tests/ssh.rs` 2.0 s, every
  other target under 0.4 s.
- Sizes, `dist` profile, `x86_64-unknown-linux-gnu` (the host; the release
  builds musl), v0.0.1 against 104ec46: binary 15,720,520 to 19,478,696 bytes
  unstripped, 13,056,384 to 16,161,280 stripped, `xz -9e` 4,611,660 to
  5,499,380. Of the sized symbols, `token_max` grew 2.24 to 3.19 MB,
  `core`/`alloc` monomorphizations 1.83 to 2.40 MB, and unnamed data
  2.74 to 4.64 MB, which includes the 1.50 MB of source text `wire::build`
  embeds (0.92 MB at v0.0.1). Crate 842,687 to 1,228,231 bytes; the GitHub
  source archive 1,755,977 to 2,616,729, `walk.gif` 717,546 to 985,957 of it.
  No release has shipped since v0.0.1, so its asset sizes in the m1 stub
  remain the last measured.

Check first: the gate latency and Windows job time after the m2 fixes, whether
the `macOS` workflow failed on `main` and filed its issue, and the release
assets against the sizes above.

Tried and rejected in m1: stripping symbols from release binaries (saves
2.6 MB unpacked but only 330 KB of the 4.57 MB archive, and loses symbolized
panic backtraces); a Cargo cache (no run on main would save one now that
pushes to main don't run Check); a per-test time limit in the gate (it would
depend on runner wall-clock time).

Tried and rejected in m2: `debug = "line-tables-only"` locally (cold build
30.9 s against 27.0 s with no debug info, so CI drops debug info entirely);
moving `quest check` into the Plan job so a dangling link skips the platform
jobs (it would add about 20 s of Nix install to every run to save about
5 runner minutes a week); `gifsicle -O3` on `walk.gif` (saves 11%, 106 KB, but
adds a step to every rerun of the recording); making the platform jobs wait
for Source so one compile error fails one job (adds Source's 146 s to the
gate).

## Audit

- cost

## Required

- [After m2](/quest/m2/README.md) - this audit runs once per milestone

## Scheduled

- 2026-12-04 - a week after the m2 run, so a fast m2 does not rerun it at once
