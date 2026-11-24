# [M] Cut the wait after the reading bar fills

## Goal

The step between the reading bar filling and "Checking settings" starting
takes a fraction of what it does now, measured on the same host before and
after, with an unchanged report.

## Plan

Found 2026-11-24 while planning the [live stats epic](/quest/m1/live/README.md);
the maintainer chose to plan it. On the maintainer's `fedora` host at main
3c30ba7 (`doctor --defaults --offline`, 9.3 GB, 338,298 requests), the bar
filled after about 36s and "Scanning local histories..." then spun 69s, the
longest phase of the run, before 43s of checking.

What those 69s spend is not known: profile it first (`samply` or `perf`
from Nix) rather than guess. Candidates are the work after the read loop in
`scan` (`src/reader.rs`), the window pass, and T3's SQLite snapshot. If part
of it is unavoidable, give it its own phase name on the progress line so
the wait says what it is doing.

The PR's `## Validation` gives the before and after timings and shows the
report is unchanged. Compare both builds on the same inputs: a frozen copy
of the transcripts and, since `--claude-dir` and `--codex-dir` turn off T3
discovery, of the T3 state too (`--t3-dir`), with the same `--prices-file`
and `--since`, and the analysis clock pinned, since it is read after the
scan and scales every monthly figure. No option pins it today; add the
smallest way to, or compare through a harness that calls `render` with a
fixed `now`, rather than mask the monthly figures. Compare the `--details`
reports, which show the scan's coverage, normalizing only the snapshot start
and finish times the scan records as it runs.
