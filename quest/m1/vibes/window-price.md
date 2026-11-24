# [S] Price the vibes window cap in doctor's replay

## Goal

The `vibes` auto-compact window card quotes what `doctor`'s replay measured
for the maintainer's cap (400K for Claude Code, 240K for Codex) on every line
whose replay can price it, instead of saying the cost is not measured.

## Plan

The card quotes the line's replay of the cap only when the window line
happened to price it, and its candidates come from the evidence (the current
window, the provider minimum, price-tier boundaries and resumed-prefix
geometry), so neither 400K nor 240K is usually among them. Adding the cap to
the candidates of a line whose window is above it costs one more replay
stream per summary scenario, and only on those lines.

Recommendation: add it as a candidate the line prices but never recommends,
so the maintainer's pick changes no verdict: the winner stays the best of the
evidence-derived candidates, and the cap's scenario is kept for the card and
`--details`.

## Waiting

- Maintainer: whether `doctor`'s replay may carry a candidate that exists only
  because of the maintainer's pick. Options: price it without letting it win
  (recommended, keeps every verdict evidence-derived); price it as a full
  candidate that may win on measured cost; leave the card's cost unmeasured
  and delete this quest.
