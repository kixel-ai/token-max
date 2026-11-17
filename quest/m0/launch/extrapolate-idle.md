# [M] Extrapolate the idle compaction replays over unpriced sessions

## Goal

The idle compaction and T3 idle compaction checks extrapolate over sessions
they could not price, the way the cache TTL, compaction window, keepalive and
background replays do since
[#143](https://github.com/kixel-ai/token-max/pull/143): a figure with `~` and
its 95% margin of error, and a verdict whenever any session was priced. A few
unpriced requests no longer leave either line "Not enough evidence"; gaps that
are not pricing, such as no idle gap long enough to trigger or unresolved T3
ownership, still do.

## Plan

Found while extrapolating the replays (#143). Both idle checks total each
mode's cost over every session and give up when any part of the total is
unknown (`idle.rs`: `delta_usd` is None whenever `unknown` is non-empty, and
the T3 check needs every mode's `total_usd`), so one unpriced request blocks
the whole comparison.

Recommended:

- Pair the on and off runs per session, as `prices::Pairs` pairs the TTL
  replays per part: `idle.rs` already runs every mode one session at a time,
  so take each session's adjustment difference and add it to a `Pairs`, and
  extrapolate with its `savings`. Keep whole-replay errors that are not about
  one session's price, such as an unresolved window, as blockers.
- Do the same for the T3 modes, per session, and carry the margin into
  `CheckBase::margin` so the row and walk show it.
