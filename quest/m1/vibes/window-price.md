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

Decided 2026-11-24 (maintainer, /quest-run): ✅ keep this quest (rejected:
drop it). ✅ Price the cap but never let it win, so no verdict changes
(rejected: price it as a normal candidate; don't price it). Add it as a
candidate the line prices but never recommends: the winner stays the best of
the evidence-derived candidates, and the cap's scenario is kept for the card
and `--details`.
