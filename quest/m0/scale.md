# [M] Scan large histories

## Goal

A default `token-max` run completes on a host with tens of gigabytes of
transcripts, and `--json` and `--details` produce output a person or script
can use.

## Plan

Measured 2026-11-08 on the maintainer's host (21 GB in 16,315 transcripts
across `~/.claude`, `~/.claude-proxy`, `~/.claude-api-credits` and `~/.codex`):

- The default run was killed at 27 GB RSS after about 10 minutes, with or
  without `--since`. Every file is read and every pre-window request is kept
  as cache context, so `--since` does not bound the work.
- One profile for one week (`~/.claude-proxy`, 182,709 requests) took 29
  minutes and 12 GB.
- `--json` on a 321 MB profile failed with `RangeError: Out of memory` in
  `JSON.stringify`, so the report never reaches stdout.
- `--details` on that profile printed 121,083 lines, about 120,000 of them
  one line per idle period.

Profile memory before changing anything, and keep the reader streaming. Cache
context only needs pre-window requests within the longest cache lifetime of
the window, and a file whose last write precedes that point cannot add a
selected request. The JSON report needs either streamed serialization or a
smaller default shape, which changes schema version 1 and needs the
maintainer's call. `--details` should summarize per-period evidence and leave
the full list to JSON.
