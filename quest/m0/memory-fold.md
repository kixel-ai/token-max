# [XL] Fold the scan per session

## Goal

Peak memory grows with the largest session plus a small fixed record per
request, not with the whole history, and a multi-host run holds one host's
scan at a time. Reports and recommendations stay
identical, and the runtime is no worse than after
[kixelated/token-max#49](https://github.com/kixelated/token-max/pull/49).

## Plan

Decided 2026-11-11 (maintainer, /quest-plan): ✅ after cutting the
redundant memory in
[kixelated/token-max#56](https://github.com/kixelated/token-max/pull/56)
(rejected: the fold alone; the cheap wins alone). ✅ m0 (rejected: m1). ✅ the
implementer chooses between one pass and two by measurement against the
runtime bound (rejected: one pass only; two passes regardless of time).

The reader already streams lines, but `scan()` keeps every deduplicated
request, event and exposure observation, sorts them, and hands the whole
`Scan` to `analyze()`, which the `Report` keeps. Most checks need only one
session at a time:

- TTL (`CacheChain`), idle (`Profiled::replay`), keepalive and background
  already replay per session. Fast and Bash are per request.
- Global steps produce small outputs: the default window from setting-change
  times (`default_since`), the compaction calibration medians, cross-file
  dedup of requests and tool calls, and the T3 owner joins.

Options for the implementer:

- Two passes: the first keeps only compact keys, timestamps, samples and T3
  ids; the second re-reads the frozen snapshots by session and folds into
  per-check totals. It costs a second read of about 26 GB on the
  maintainer's host, where runs are mostly I/O-bound.
- One pass: fold each session as it completes, keep per-session partial
  results small enough to fix up once the global facts are known.
- Either way, copies of one request can land in different session groups,
  and `equivalent()` must still detect copies that disagree. Assign each
  request to one owner session, or merge outputs by request id.

Rough floor from reading the structs: 100-150 bytes per request and 60-100
per event, about 0.3-0.4 GB for 1M requests, against about 5 KB per request
today. The largest transient is now T3 enrichment's SQLite joins, about 0.9 GB
on the maintainer's host.

Multi-host runs moved here from the cheap wins, decided 2026-11-11
(maintainer, on [kixelated/token-max#56](https://github.com/kixelated/token-max/pull/56)):
✅ keep the coordinator receiving every scan before analysis until this fold
shrinks what each host sends (rejected: a `ready` notice with each scan left
on its destination until its turn; gating each transfer on a request with
an idle-connection watch; spilling received scans to temporary files). A
scan left on its destination can fail unseen after `ready`, and a dropped
connection then fails a run that holding every scan would finish. Buffering
received bytes instead costs more than the loaded scan: 4.74 GB over SSH
against 3.45 GB in memory on the maintainer's host.
