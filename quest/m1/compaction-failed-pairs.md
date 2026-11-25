# [S] Keep a failed window replay's summaries out of its priced savings

## Goal

A compaction window candidate whose replay of a session fails prices none of
that session's summaries: no doctor saving, `vibes` cap figure or margin
counts a recorded summary against a candidate that could not model the
session.

## Plan

Found by review on [kixel-ai/token-max#235](https://github.com/kixel-ai/token-max/pull/235)
and not yet reproduced. `Lockstep::runs` in `src/compaction.rs` pairs each
request's summaries under the current window against the candidate's. Once
a session's replay fails (`uncertain`), its later parts pair as unknown, but
a summary the current window recorded or added in that session still pairs
against the candidate's zero. For example, a 650K request compacting to 410K
prices under a 600K window but cannot resume under a 400K one. That would
count the baseline summary as a saving the candidate never modeled.

Start with a synthetic test that shows it. If it holds, make the failed
session's summary pairs unknown like its parts, so they fall into the
unpriced share and its extrapolation. This applies to every candidate, so
doctor's verdicts can move.
