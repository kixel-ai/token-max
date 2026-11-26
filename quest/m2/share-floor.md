# [XS] No spend share over 100% when the host total is a floor

## Goal

No line shows a share of its host's spend above 100%. When the host's total is
only a lower bound ("at least $X", because some profiles priced none of their
requests), a line's share says it's an upper bound, or is left out when it
would pass 100%, instead of printing an impossible percentage.

## Plan

Planned 2026-11-25 (maintainer, /quest-plan), from
[kixel-ai/token-max#258](https://github.com/kixel-ai/token-max/pull/258)'s
follow-up: ✅ plan it (rejected: drop it until real data shows it). The PR's
synthetic fixture showed a 149% share: the header read "at least $0.18" while
a shared advisor line extrapolated to ~$0.27. No real host has shown it yet.
m2, because only a synthetic fixture has shown it.

`Spend::share` in `src/report.rs` divides by `usd`, which includes the floor.
Since the true total is at least that, `usd / total` is an upper bound on the
true share. Recommendation: write such a share as "at most N%", and leave it
out when it passes 100%. This is not a regression from #258 (the floor
predates it), so no `## Caused by` entry.
