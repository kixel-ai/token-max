# [M] Build each replay's profile history and lockstep inputs once

## Goal

Compaction, turn-end compaction, idle compaction and keepalive read a
profile's requests, boundaries and events through one helper, and the two
lockstep replays build their inputs through one more. Every check's figures
are unchanged, which the existing replay tests and a before/after run of
`doctor --details` on the synthetic fixtures show.

## Plan

The m2 code audit found the same block about six times:
`compaction.rs` (around `1862`), `turn_end.rs` (`105`), `idle.rs` (`2116` and
`2405`) and `keepalive.rs` (`681` and `742`). It chains
`cache_context.requests` with `scan.requests`, keeps one group's, sorts by time
then `locale_compare`, and does the same for boundaries and events. The only
real difference is that idle sorts its events.

`compaction.rs` (around `1974`) and `turn_end.rs` (`200`) both rebuild
`summary_ids`, then `fixed` (unmatched summaries plus `server_cost`), `last`
and `no_estimates`, before building `Lockstep`.

`exposure::calibrate` runs three times on the same input: in `replay::analyze`,
`memory::memory_checks` and `context::context_costs`. Compute it once in
`analyze` and pass it down.

These are core replay paths, so verify a second way: compare the full
`--details` output of every fixture before and after.
