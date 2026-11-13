# [XL] Fold the scan per session

## Goal

Peak memory grows with the largest session plus a small fixed record per
request, not with the whole history. Reports and recommendations stay
identical, and the runtime is no worse than after
[kixelated/token-max#49](https://github.com/kixelated/token-max/pull/49).

## Plan

Decided 2026-11-11 (maintainer, /quest-plan): ✅ after cutting the
redundant memory in
[kixelated/token-max#56](https://github.com/kixelated/token-max/pull/56)
(rejected: the fold alone; the cheap wins alone). ✅ m0 (rejected: m1). ✅ the
implementer chooses between one pass and two by measurement against the
runtime bound (rejected: one pass only; two passes regardless of time).

Multi-host runs are done:
[kixelated/token-max#78](https://github.com/kixelated/token-max/pull/78)
has each SSH destination analyze its own scan and send only its report, so
the coordinator never holds more than its own scan. Destinations run the same
`scan()` and `analyze()`, so this fold applies to them unchanged.

The reader already streams lines, but `scan()` keeps every deduplicated
request, event and exposure observation, sorts them, and hands the whole
`Scan` to `analyze()`, which the `Report` keeps. On synthetic histories peak
memory grows linearly: about 4.4 KB per request before
[#87](https://github.com/kixel-ai/token-max/pull/87) (2.83 GB for 640,000
requests), about 2.8 KB after it (1.83 GB peak RSS).

### What #87 left

#87 shrank what each record holds and stopped the checks keeping whole
replays, without the fold: requests and events share their recorded-alike
fields, and the TTL, compaction and idle replays total their parts as they
stream. Heap on the 160,000-request synthetic history, measured with a
counting allocator: 169 MB live when analysis starts (1.06 KB per request:
events 65 MB at 200 B each, billing parts 38 MB, requests 26 MB, exposure
28 MB), then these peaks above it:

- Idle, 481 MB, the run's peak. The report keeps every `IdlePeriod` (352 B,
  one per request gap, in both scenarios: about 190 MB with their costs)
  though it renders only counts and groups, and each owner's scenario holds
  its periods, prepared gaps and the point runs' per-period charges.
  Recommendation: keep counts per (status, return timing, outcome, reason)
  in the report, and give the tests, which check single periods, their own
  way to reach them.
- Reading, 285 MB: the scan holds every request and event until it ends.
- Compaction, 294 MB: the current window's replay keeps one 240 B record per
  part, and each simulation copies every request it changes.

The fold below removes the per-request floor; the idle item is independent
of it and the largest single step left.

### One pass, by measurement

A sessions-only pass (`token-max sessions`) took 1.1 s of user CPU against
1.5 s for the full run on a 0.6 GB history, so the scan is about 70% of the
CPU and a second full read would put the runtime near 1.7 times the bound, on
top of reading about 26 GB twice on the maintainer's mostly I/O-bound host. Agent's
recommendation: one pass that folds each session as it completes into a
compact per-request record, then finishes the checks from those records once
the global facts are known.

### What crosses sessions

Surveyed 2026-11-12 on main at `edc08ed`. The checks run as a chain of
sweeps, each needing facts only the previous one finishes over the whole
history:

1. Compaction charges, which mutate requests before anything prices them.
2. Global facts: the default window from setting changes
   (`default_since`), cohorts and tuned windows, the compaction calibration
   samples and medians, candidate and current windows with their owners, and
   the subagent directories.
3. TTL verdicts (`CacheChain`).
4. Compaction, which reads the TTL verdicts.
5. Idle: an activation-cohort pass, then the adaptive break-even bisection,
   up to 52 runs, each needing the history-wide delta.
6. Keepalive.

Each later sweep can run from per-request records, but the record must hold
what each sweep reads, and 5 is the costliest to keep small.

Other joins span sessions or files and need a key that survives the fold:

- Copies of one request can land in different sessions, and `equivalent()`
  must still detect copies that disagree: assign each request one owner, or
  merge by request id.
- Event origins, `event.request`, compaction charge request ids, subagent to
  parent, Codex fork owners found through location files, `NativeIds`
  parents, call and invocation ids, and refs.
- Events sort by time with ties in arrival order, so the fold needs a global
  arrival key in place of the vector index.
- T3 enrichment is two-phase over every profile (`ambiguous_profiles`,
  `removed`, `t3_ids`); its SQLite joins are the largest transient, about
  0.9 GB on the maintainer's host.
- Exposure keeps request shapes by hash and each component's `first_seen`
  across the whole history.
- Every first-seen ordered set or list, such as a cost's unknown reasons and
  assumptions, needs a global ordinal so a merged list keeps today's order.
- Cost totals need no ordering: `Cost`, `PairedSavings`, session, context and
  memory costs and keepalive's per-cap totals add `Usd`, an integer count of
  2^-64 dollars, so partial sums merge exactly in any order. Values derived
  from them, such as per-month rates, are f64 and are taken after the merge.

Rough floor from reading the structs: 100-150 bytes per request and 60-100
per event, about 0.3-0.4 GB for 1M requests.
