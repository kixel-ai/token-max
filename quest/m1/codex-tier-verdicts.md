# [M] Give Codex cost comparisons a verdict without a recorded service tier

## Goal

`doctor`'s Codex cost comparisons, the auto-compact window, turn-end
compaction and keepalive lines, reach a verdict on native Codex transcripts
whenever no service tier could reverse the winner. Today each is "not enough
evidence" on every native Codex host: Codex's `token_usage_record` holds no
billed service tier, so every request is priced at an assumed tier and a
verdict that rests on one is withheld
([reference](/docs/reference.md#turn-end-compaction)).

## Plan

Found 2026-11-23 while measuring turn-end compaction: on the maintainer's
host the line priced a threshold at ~$142 a window saved and still gave no
verdict, as the window line beside it does.

Recommendations:

- A service tier multiplies a whole request's price, so it scales both sides
  of a paired difference alike. Keep each pair's sign: when every priced pair
  of the winner saves, or every one loses, no assignment of tiers to requests
  reverses it, and the line gives its verdict with the saving still marked as
  priced at an assumed tier. Mixed signs stay withheld, as now.
- Bound the saving over the documented tier multipliers rather than report
  the default tier's figure alone.
- `src/prices.rs` holds the rule (`TIER_SENSITIVITY`, `tier_sensitive`) and
  `Pairs` the paired differences; `src/compaction.rs`, `src/turn_end.rs` and
  `src/keepalive.rs` read it.

## Waiting

- Maintainer: decide how a Codex comparison priced at an assumed tier may
  reach a verdict. Options: a verdict only when every pair agrees in sign
  (recommended: no tier could reverse it, so nothing is guessed); keep
  withholding every such verdict (leaves these lines without a verdict on
  native Codex); assume the default tier (a guess the rule exists to avoid).

## Related

- [New releases](/quest/m1/releases.md) - a Codex release that records the billed tier would settle this at the source
