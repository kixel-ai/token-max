# [L] Replay OpenCode compaction and warming

## Goal

`token-max doctor` replays OpenCode's remaining measurable session-lifecycle
settings on recorded OpenCode histories: `warming` (transient keepalive pings)
and `compaction` (`auto`, `buffer`, `keep.tokens`, `background`). Each check
recommends the cheaper setting at `models.dev` API prices and offers the
comment-preserving `opencode.json{,c}` edit when OpenCode exposes a setting
that applies it.

The Anthropic prompt-cache TTL replay (`providers.anthropic.settings.cache.ttl`)
and the reading of OpenCode 2's `compaction` messages as summary requests
already landed with the first half of this quest.

## Plan

Decided 2026-11-27 (maintainer, on
[kixel-ai/token-max#295](https://github.com/kixel-ai/token-max/pull/295)): ✅
land the Anthropic TTL line and the compaction-message spend fix, and park
warming and compaction until a host records OpenCode compactions or Anthropic
requests (rejected: keep the rest a ready quest; hold the PR until both are
built).

Settled decisions (from [the epic](/quest/m1/harnesses/README.md)):

- Cover every measurable replay check OpenCode's transcripts and settings
  support, priced via `models.dev` by `providerID`/`model.id`.
- Apply confirmed edits to `opencode.jsonc` while preserving comments and
  formatting.

What OpenCode v2.4.1's source and this host's database show:

- Warming pings are never stored as messages. `session.generate` records each
  ping only as a `session.usage.recorded` event (source `generate`) and in the
  session's `tokens_*` totals; this host's `event` table is empty. Current ping
  spend and state are therefore unrecorded, as for the keepalive check, and the
  replay adds hypothetical pings on top of recorded activity.
- Warming only pings main sessions (`parentID` unset), from each real request's
  send time, every `interval` until `duration` (default 30 minutes) after it.
  `interval: "auto"` is 90% of the model's `cache.ttl` setting, else 4 minutes;
  an `"auto"` interval at or past `duration` never pings. A provider's or
  model's `settings.cache.warming` overrides the top-level `warming`. Durations
  are Effect duration strings such as `"30 minutes"`.
- Compaction triggers at the model's input window minus `buffer` (a token count
  or a percentage; default the larger of 15% and 16,000 tokens), read from
  models.dev `limit.input`/`limit.context`, which token-max's catalog does not
  load yet. Background summaries that are never used are recorded only as
  usage events, so `background` cannot be priced; `keep.tokens` changes the
  resumed prefix, which recorded counts do not split; `auto: false` cannot be
  replayed.
- On the maintainer's host: 22,459 OpenCode requests, all through `opencode`
  and `opencode-go` with no recorded cache writes, no compaction in any
  session, and warming unset. Neither check would price anything there, so
  both rest on synthetic fixtures.

Recommendations:

- `warming` first: the keepalive replay (`src/keepalive.rs`) runs unchanged
  with no compaction windows, so OpenCode needs no compaction replay for it.
  Add an OpenCode branch for main sessions sent straight to Anthropic, reusing
  the Anthropic TTL line's lifetime, OpenCode's own interval, and each cap as a
  `duration`. Unlike Claude Code's `/loop`, `warming` is a real setting, so the
  line gets Change and Keep verdicts and an edit of `warming` or
  `warming.duration`; the keepalive report text assumes no setting exists and
  needs OpenCode wording. A provider's or model's `cache.warming`, or a project
  file that sets warming, leaves the line advisory. Providers without
  documented cache lifetimes stay unpriced and say so. The walk gains a
  question, so rerun `assets/demo/walk.tape` and `assets/demo/site.py`.
- `compaction`: emit a compaction `StructuralEvent` for each OpenCode
  compaction message, linked to its summary request, so calibration samples
  exist; load model limits into the catalog; then add OpenCode's trigger to
  the compaction-window replay (`src/compaction.rs`, whose OpenCode arms are
  `unreachable!` today) with `compaction.buffer` as the control. Like the
  Compaction window line, weigh cost alone and name what the replay cannot
  measure: a smaller `buffer` or `keep.tokens` keeps less recent conversation
  beside the summary. Read the deprecated `reserved`, `prune`, `tail_turns`
  and `preserve_recent_tokens` an older config may still set, and state plainly
  which cannot be priced.
- `Harness::replayed()` in `src/model.rs` keeps OpenCode out of the
  compaction and keepalive replays (and the transcript audit) today; each
  replay this quest adds lifts that exclusion for its own check.
- Document replay assumptions and limits in `docs/reference.md` and
  `docs/semantics.md`.

## Parked

- until a host records an OpenCode compaction or a request sent straight to Anthropic - both replays would otherwise rest on synthetic fixtures alone; resume when `sqlite3 ~/.local/share/opencode/opencode.db "SELECT count(*) FROM session_message WHERE type = 'compaction' OR json_extract(data, '$.model.providerID') = 'anthropic'"` prints more than 0 on a maintainer host (`every-host` runs it on each)
