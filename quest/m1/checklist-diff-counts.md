# [S] Count the checklist diff's files and changes exactly

## Goal

The checklist's `Review exact edits` header names the number of settings files
the diff below it writes and every change it makes, so an edit to both
`settings.json` and `~/.claude.json` no longer reads "across 1 file".

## Plan

- ✅ Kept by the maintainer at [kixel-ai/token-max#286](https://github.com/kixel-ai/token-max/pull/286#issuecomment-7216566816)'s pick, 2026-11-27 (rejected: drop it).
- `preview_measured_events` and `preview_vibes_events` in `src/cli.rs` add one
  to `file_count` per host with any diff, not per file, and `change_count`
  counts only `Choose { yes: true }` and `Cleanup` events, skipping the MCP
  removals and `deniedMcpServers` entries.
- A local host's `Plan` holds its files; an SSH destination's review sends
  only the shown diff, so its count has to come back over the wire with it
  (a stream type change, which the build fingerprint already versions).
