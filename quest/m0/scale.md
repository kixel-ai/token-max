# [M] Scan large histories

## Goal

A default `token-max` run completes on a host with tens of gigabytes of
transcripts, and `--json` and `--details` produce output a person or script
can use.

## Plan

Measured 2026-11-08 on the maintainer's host (21 GB in 16,315 transcripts
across `~/.claude`, `~/.claude-proxy`, `~/.claude-api-credits` and `~/.codex`),
with the TypeScript implementation:

- The default run was killed at 27 GB RSS after about 10 minutes, with or
  without `--since`. Every file is read and every pre-window request is kept
  as cache context, so `--since` does not bound the work.
- One profile for one week (`~/.claude-proxy`, 192,775 requests) took 24
  minutes, 158 s of it scanning. A CPU profile put 800 s in idle's
  per-context-size grouping, which filtered every period once per distinct
  size, 180 s in compaction's predecessor lookup, and 77 s rebuilding the
  cache boundary index for every replay.
- `--json` on a 321 MB profile failed with `RangeError: Out of memory` in
  `JSON.stringify`, so the report never reaches stdout.
- `--details` on that profile printed 121,083 lines, about 120,000 of them
  one line per idle period.

The Rust rewrite indexes those lookups and streams JSON to stdout. Its output
matched the TypeScript's exactly on `~/.claude-api-credits` (JSON, text and
`--details`). Measured the same day with the Rust release build:

- `~/.claude-proxy` for one week: 89 s and 3.4 GB.
- The default run over the whole host (983,585 requests from 47 days): 7.4
  minutes and 12.5 GB.

Remaining: memory still grows with the whole history. Cache context only
needs pre-window requests within the longest cache lifetime of the window,
and a file whose last write precedes that point cannot add a selected
request. Each component keeps one location per request that carried it,
about half the exposure memory. The JSON report repeats per-request
evidence, which may need a smaller default shape; that changes schema
version 1 and needs the maintainer's call. `--details` should summarize
per-period evidence and leave the full list to JSON.
