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
report is byte-identical on the same transcripts.
