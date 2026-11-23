# [L] Replay OpenCode compaction, warming, and Anthropic cache TTL

## Goal

`token-max doctor` replays OpenCode's measurable session-lifecycle settings on
recorded OpenCode histories: `warming` (transient keepalive pings),
`compaction` (`auto`, `prune`, `reserved`, `buffer`, `background`, `keep`), and
Anthropic prompt-cache TTL when an OpenCode profile routes requests to
Anthropic. Each check recommends the cheaper setting at `models.dev` API prices
and offers the comment-preserving `opencode.json{,c}` edit when OpenCode
exposes a setting that applies it.

## Plan

Settled decisions (from [the epic](/quest/m1/harnesses/README.md)):

- Cover every measurable replay check OpenCode's transcripts and settings
  support, priced via `models.dev` by `providerID`/`model.id`.
- Apply confirmed edits to `opencode.jsonc` while preserving comments and
  formatting.

Recommendations:

- `warming`: replay recorded idle gaps against OpenCode's `warming` setting
  (default `false`), reusing the keepalive replay model where the request's
  provider has a documented cache TTL and measurable cache-miss cost. Where a
  provider's cache lifetime is undocumented or unobservable, keep the comparison
  unpriced and say so plainly rather than guessing.
- `compaction`: read `compaction` settings (`auto`, `prune`, `reserved`,
  `buffer`, `background`, `keep`) and replay candidate thresholds against
  observed compaction events (`time_compacting` and summary turns) when
  calibration samples exist, extrapolating with `~` and a margin of error as
  Claude Code and Codex window checks do. State plainly when a sub-setting such
  as `prune` cannot be priced from recorded token counts.
- Anthropic cache TTL: when an OpenCode profile sends Anthropic requests with
  cache read/write splits, replay 5m vs 1h TTL using the existing cache replay.
  Verify against the installed OpenCode config schema whether an explicit
  provider or agent TTL setting exists; offer the edit only if documented, and
  otherwise report as measured evidence naming the missing knob.
- Exercise against real local OpenCode histories where applicable and add
  synthetic SQLite fixtures for warming, compaction, and Anthropic TTL cases.
- Document replay assumptions and limits in `docs/reference.md` and
  `docs/semantics.md`.

## Required

- [Discover, price, and audit OpenCode sessions and settings](/quest/m1/harnesses/opencode.md) - adds OpenCode discovery, v1/v2 SQLite reader, multi-provider pricing, and comment-preserving opencode.jsonc edits
