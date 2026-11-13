# [S] Count cache uncertainty in equal compaction parts

## Goal

A compaction window comparison counts a part whose cache charge is unknown
in either replay, even when both replays give the part the same usage, so
the window's savings and coverage don't claim a known result the cache
replay could not establish.

## Plan

Found 2026-11-13 by CodeRabbit reviewing
[#87](https://github.com/kixel-ai/token-max/pull/87#discussion_r4563027628).
Main has the same check, and #87 keeps reports identical to main, so the fix
lands on its own.

- `Candidate::compare` in `src/compaction.rs` skips a part when the current
  window's and the candidate's replayed `BillingPart`s are equal. Cache
  certainty is not part of `BillingPart`: it is `ReplayedPart::cache_unknown`.
- `CacheChain::replay` in `src/cache.rs` gives a part without an anchor the
  usage of `replay(None)` whether the replay is known or unknown. When the
  candidate's modeled boundaries change the predecessor histories, one side
  can be unknown with the same usage, and its unknown cache charge never
  reaches the window totals or coverage.
- Return early only when `cache_unknown` matches as well, and add a
  synthetic test in which a candidate's boundary leaves an anchorless part
  unknown on one side only.
- This changes reports where it applies: the pair is counted, adds its
  unknown reason and lowers the window's coverage.
