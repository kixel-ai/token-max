# [S] Open the walk with what the settings already save

## Goal

On a terminal, after the scan, `doctor`'s opening header line is followed by
the "Compared with defaults, your settings save about $X/mo" line before the
first change question, so someone whose settings are already good sees that
credit first, including on a run with nothing to change. The report after
the walk keeps the line where it is.

## Plan

The line and its wording come from `src/defaults.rs`, unchanged: the same
sum, the same "up to" and unmeasured counts, the same overlap caveat. Move
nothing else into the opening. The "How is ~/.claude paid for?" questions
come before the scan and stay first, since an answer can change the main
agent's default TTL that the credit is priced against.

With several hosts it is one line for all of them, as the header's total is.
SSH replies carry `Totals` (`src/totals.rs`), which has the offered savings
but not the credit, so add the credit there as structured figures that sum
across hosts before rounding. A destination is the same token-max version,
so this wire change needs no compatibility path.

Under `--defaults` every setting is its default, so the opening skips the
line rather than say nothing differs.

Rerun `assets/demo/walk.tape` and `assets/demo/site.py`, since doctor's
opening changes. The README's `walk.gif` shows the credit, since
`assets/demo/home.py` sets non-default values; the site's demo runs
`doctor --defaults` on purpose, to show what a default user would save, so
it shows no credit.

The 0.1.0 release quest (`quest/m1/release-0.1.md`) requires this one
(decided 2026-11-24, see the [epic](/quest/m1/live/README.md)); whichever of
this quest's planning PR and
[kixel-ai/token-max#226](https://github.com/kixel-ai/token-max/pull/226),
which adds that quest, lands second adds this quest to its `Required`.
