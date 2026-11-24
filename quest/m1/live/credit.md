# [S] Open the walk with what the settings already save

## Goal

On a terminal, `doctor`'s opening header line is followed by the "Compared
with defaults, your settings save about $X/mo" line before any question, so
someone whose settings are already good sees that credit first, including
on a run with nothing to change. The report after the walk keeps the line
where it is.

## Plan

The line and its wording come from `src/defaults.rs`, unchanged: the same
sum, the same "up to" and unmeasured counts, the same overlap caveat. Move
nothing else into the opening. With several hosts it covers what the header
covers, one line for all of them, as the header's total does.

Under `--defaults` every setting is its default, so the opening skips the
line rather than say nothing differs.

Rerun `assets/demo/walk.tape` and `assets/demo/site.py`, since doctor's
opening changes.

The 0.1.0 release quest (`quest/m1/release-0.1.md`) requires this one
(decided 2026-11-24, see the [epic](/quest/m1/live/README.md)); whichever of
this quest's planning PR and
[kixel-ai/token-max#226](https://github.com/kixel-ai/token-max/pull/226),
which adds that quest, lands second adds this quest to its `Required`.
