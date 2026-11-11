# [M] Cut the scan's redundant memory

## Goal

The default full-history run keeps no redundant copies: ids, snapshots and
replayed requests are stored once and compactly, and multi-host runs keep one
host's scan at a time. Reports and recommendations stay identical, and the
runtime is no worse than after
[kixelated/token-max#49](https://github.com/kixelated/token-max/pull/49)
(5:57 wall for 1,028,316 requests on the maintainer's host).

## Plan

Decided 2026-11-11 (maintainer, /quest-plan after asking why the scan holds
so much when the reader streams): ✅ land these wins first, then
[the per-session fold](/quest/m0/memory-fold.md) (rejected: the fold alone,
which would make most of these obsolete; these wins alone). ✅ m0 (rejected:
m1).

Candidates, from reading the code on 2026-11-11 (sizes are estimates, not
measured). Measure first: log RSS at each `phase()` in `src/cli.rs`, or use
heaptrack, and keep only the wins that pay for themselves.

- Event ids are 64-character hex `String`s (`src/safe.rs` `identity`) and
  request ids are long `Text`. Store fixed-size digests.
- `ExposureCollector` clears `held_snapshot` on every tool result, and each
  later assistant or token line stores its own `Arc<ResultTotals>`, which is
  O(lines × held tools). `observations.lines` keeps every line, but only the
  lines a request uses matter.
- Exposure `calls` and `results` use `String` keys and per-entry profile
  `Vec`s. Use digests and the interned `Profiles`.
- `legacy` requests and `modern_totals` strings in `src/reader.rs` can become
  digest keys, with requests dropped once resolved.
- `Modeled::to_mut` in `src/compaction.rs` clones whole requests per scenario
  and window candidate. Clone only the replayed parts.
- The idle check builds two whole-history `ReplayIndex`es. Share one.
- `src/cli.rs` collects every host's scan before analysis. Analyze each host
  as it arrives and keep only its report.
- `--since` keeps every pre-window request as `cache_context`. Keep compact
  per-session cache state instead.
