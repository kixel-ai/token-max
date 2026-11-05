# [L] Replay and the cache TTL check

## Goal

`token-max` reads every Claude Code and Codex session on the host, one
record per model request (time, session, main or sub-agent, model, input,
cache write by TTL, cache read, output), prices them from models.dev, and
runs its first check: Claude Code's prompt cache TTL, 5m or 1h, separately
for main sessions (`promptCacheTtl`) and sub-agents
(`subagentPromptCacheTtl`). The report format, `--since`, `--json`, and the
nonzero exit when a change is recommended are in place for the later
checks. Tests run on small synthetic transcripts.

## Plan

The check replays each session's request gaps: under 5m, a gap of 5 to 60
minutes rewrites the cached prefix at the 5m write price instead of reading
it; under 1h, every write costs the 1h price. Codex exposes no cache TTL, so
the check covers Claude Code only.

The maintainer's 2026-11-05 audit did this replay by hand over 10-30 to
11-05 at Opus 5.5's models.dev prices ($5 per million for a 5m write, $8
assumed for a 1h write, $0.20 for a read). 1h cost $5,494 for main sessions
and $1,906 for sub-agents; 5m would have cost $8,869 and $2,382, so the 1h
TTL saves about $17k a month at API prices (38% and 20%). The scripts are
not kept; run the finished check on the maintainer's host and compare with
these figures in the PR.

Recommendations for the implementer:

- Fetch models.dev's `api.json` with a cached copy for offline runs, and
  name the price an estimate assumed when the catalog lacks one, such as the
  1h write.
- Each Claude request records its cache writes by TTL, which dates the last
  TTL change for the default window.
- Keep the reader to the fields above, so the transcript text never reaches
  memory longer than one line's parse.
