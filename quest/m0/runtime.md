# [M] Restore the large-history runtime

## Goal

A default `token-max` run on the maintainer's host is back to its
[kixelated/token-max#27](https://github.com/kixelated/token-max/pull/27)
runtime or better, with each hot spot measured before and after.

## Plan

Measured 2026-11-09 with the release build after
[kixelated/token-max#37](https://github.com/kixelated/token-max/pull/37):
`token-max --plain --since 2026-11-01` read 320,548 requests across four
Claude/Codex profiles and four T3 instances and took about 12 minutes. #27
measured the whole host, 983,585 requests from 47 days, at 7.4 minutes and
12.5 GB, so the T3, Codex skill and background control PRs since then
regressed it.

`perf` samples during that run:

- Around 3 minutes in: `compaction::simulate`, mostly SipHash `write`,
  `memmove` and `memcmp`.
- Later: malloc and free, then `drop_glue` of
  `indexmap::Bucket<String, Vec<BackgroundControl>>`, the per-directory
  `background_directories` maps, which suggests profiles or their maps are
  cloned per request or per directory.

#37 already replaced the quadratic request and event lookups in
`idle::t3_comparison`, which spent over 25 CPU minutes in `memcmp` on
the same data.

Record the before and after time and peak RSS of the default run and of the
`--since` run in the PR. Keep output identical: compare `--json` before and
after on the same history.

Decided 2026-11-09 (maintainer, /quest-plan from /quest-complete): ✅ plan
this follow-up (rejected: accept the 12-minute run). ✅ m0 (rejected: m1),
since first-release users with large histories hit it.
