# [S] Bound what an SSH destination can make doctor hold

## Goal

A destination of `doctor --ssh` or `sessions --ssh` can't make this host hold
more than a bound, for each JSON value it sends and for the whole stream.
Each limit follows from what the protocol carries, not from a constant picked
to be large. A destination that passes a limit fails its host's scan with an
error naming the limit, and the other hosts' results still show.
Non-goal: other untrusted input, such as transcripts or settings files.

## Plan

Planned 2026-11-25 (maintainer, /quest-plan), from
[kixel-ai/token-max#263](https://github.com/kixel-ai/token-max/pull/263)'s
follow-up: ✅ bound each line and the whole stream (rejected: each line only,
which still lets a destination send values without end; drop).

Today only the run script's first line is capped (`ssh::found`, at
`PATH_MAX + 16`). After that, `wire::Stream` deserializes each value straight
from the child's stdout with no limit, and #263 made the report one structured
`Outcome::Report` value, so a single value can be any size.

Recommendation, not settled: split what a value carries into pieces whose
size its fields bound (a path at most `PATH_MAX`, a name, numbers), so a
frame limit falls out of the schema. Write session lines through as they
arrive instead of holding them. Bound a report by what the checks can produce
for the profiles the destination found. If a piece has no natural bound, say
so in the PR and choose its limit there.

Tests go in `tests/ssh.rs`, using a destination that sends a value over each
limit.

