# [M] Compaction window check

## Goal

`token-max` recommends Claude Code's `autoCompactWindow` (per model, under
`modelSettings`) and Codex's `model_auto_compact_token_limit` from the spend
on requests above each candidate context size, net of the compaction
requests a lower window would add.

## Plan

Context on the maintainer's host, 2026-11-05: Claude sessions that peaked
above 400k (2.6% of sessions) were 35% of cost, and auto-compaction ran at a
median of 967k. Claude now compacts at 400k for Opus and Codex at 240k, half
its window.

Recommendations for the implementer:

- A compaction is one request that reads the whole context and writes a
  summary, after which the next request writes the summary as a new cache
  prefix; price both.
- Compaction also loses detail an agent may re-read later. Say so in the
  output beside the saving.
- Evaluate windows after the TTL check's recommended values. Use replay's
  observed or estimated compaction charges once in the current baseline;
  the separate idle check evaluates their on/off policy after the window
  recommendation. Report each saving relative to that order, rather than
  adding independent savings that may overlap.
