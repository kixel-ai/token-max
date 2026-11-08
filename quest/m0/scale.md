# [M] Make full-history JSON practical

## Goal

A default `token-max` run completes on a host with tens of gigabytes of
transcripts, and `--json` and `--details` produce output a person or script
can use.

## Plan

The Rust rewrite and this quest's memory work let the full history complete.
Consecutive component lists share their prefixes, replay emits one request's
parts at a time, and structural events share session and pricing metadata.
The reader releases temporary indexes before analysis and avoids spare capacity
in its final vectors. Text details group repeated idle periods, cache witnesses,
usage estimates and modeled compaction geometry; JSON retains the full evidence.

Remaining: JSON schema version 1 repeats the complete component lists, request
evidence and modeled histories. A frozen whole-host run on 2026-11-08 produced
26,543,011,046 JSON bytes for roughly one million requests. Streaming prevents
a second output-sized allocation, but the file is still impractical to inspect
or load in ordinary JSON consumers. Keeping every request and event also means
memory remains proportional to the full history, including before `--since`.
Changing those arrays or their selected-window coverage changes the public
report schema.

Do not discard old files from their modification times or a cache-TTL cutoff:
they may carry component catalogs, pricing context, conversation aliases and
compaction calibration used by selected requests. Design the compact report and
its evidence opt-in before removing history from the serialized scan.

## Waiting

- Maintainer: choose the JSON evidence contract. Recommend a new schema version
  with aggregate checks and component costs by default and an explicit opt-in
  for full per-request evidence; the alternative keeps version 1's complete
  arrays and its multi-gigabyte output. The original implementation brief
  reserved schema changes for this decision, so it cannot be automated without
  choosing a public API change. Check: a maintainer decision recorded on the PR
  selects the default fields and the full-evidence opt-in.
