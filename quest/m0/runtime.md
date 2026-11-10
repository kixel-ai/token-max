# [M] Restore the large-history runtime

## Goal

A default `token-max` run on the maintainer's host is back to its
[kixelated/token-max#27](https://github.com/kixelated/token-max/pull/27)
runtime or better, with each hot spot measured before and after.

## Plan

Measured 2026-11-09 on the maintainer's host, release builds, `--offline`:

| Run | Requests | Wall | User CPU | Peak RSS |
| --- | ---: | ---: | ---: | ---: |
| #27's binary, default | 1,013,138 | 4:22 | 237 s | 12.6 GB |
| [kixelated/token-max#40](https://github.com/kixelated/token-max/pull/40), default | 1,013,220 | 9:49 | 453 s | 17.1 GB, swapping |
| #40, default profiles without T3 | 1,013,410 | 4:53 | 268 s | 14.3 GB |

#40 removed the quadratic profile clone in `idle::Scopes::window`, which took
the default run from 82 minutes to under 10. What remains is T3: this host's
T3 metadata read fails (see [T3 state](/quest/m0/t3-state.md)), so every
native `.claude` request (648k) stays shared with both T3 Claude instances
and each per-profile check replays it three times, and the failing metadata
query itself costs about 15% of the reader thread. Once T3 metadata joins,
`t3::enrich` gives each joined request a single owner.

After the T3 state quest lands, measure the default run again against #27's
binary on the same history. If shared requests still repeat whole check
replays, share per-profile replays between profiles whose requests and
relevant controls match, keeping `--json` identical.

Decided 2026-11-09 (maintainer, /quest-plan from /quest-complete): ✅ plan
this follow-up (rejected: accept the 12-minute run). ✅ m0 (rejected: m1),
since first-release users with large histories hit it.

## Required

- [T3 state](/quest/m0/t3-state.md) - joined T3 metadata gives shared native requests a single owner
