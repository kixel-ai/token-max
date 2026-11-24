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

Decided 2026-11-24 (maintainer, /quest-run): ✅ a Codex comparison priced at an
assumed tier gives a verdict only when every priced pair agrees in sign
(rejected: keep withholding every such verdict; assume the default tier).

Recommendations:

- A service tier multiplies a whole request's price, so it scales both sides
  of a paired difference alike when one tier bills the whole pair. Keep each
  pair's sign: when every priced pair of the winner saves, or every one loses,
  no assignment of a tier to each pair reverses it, and the line gives its
  verdict with the saving still marked as priced at an assumed tier. Mixed
  signs stay withheld, as now.
- A pair that spans requests, such as an added summary and the request it
  shrinks, can be billed at two tiers if the tier changed between them, which
  could reverse its sign. Say the rule assumes one tier within a pair, or
  bound each such pair over the documented multipliers of its requests
  apart and give the verdict only when the sign holds at every bound.
- Bound the saving over the documented tier multipliers rather than report
  the default tier's figure alone.
- `src/prices.rs` holds the rule (`TIER_SENSITIVITY`, `tier_sensitive`) and
  `Pairs` the paired differences; `src/compaction.rs`, `src/turn_end.rs` and
  `src/keepalive.rs` read it.

## Related

- [New releases](/quest/m1/releases.md) - a Codex release that records the billed tier would settle this at the source
