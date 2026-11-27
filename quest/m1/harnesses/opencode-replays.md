# [L] Replay OpenCode compaction, warming, and Anthropic cache TTL

## Goal

`token-max doctor` replays OpenCode's measurable session-lifecycle settings on
recorded OpenCode histories: `warming` (transient keepalive pings),
`compaction` (`auto`, `buffer`, `keep.tokens`, `background`), and
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

- `warming`: replay recorded idle gaps against OpenCode's top-level `warming`
  (default `false`) and any provider or model `settings.cache.warming`,
  reusing the keepalive replay model where the request's provider has a
  documented cache TTL and measurable cache-miss cost. Where a provider's
  cache lifetime is undocumented or unobservable, keep the comparison
  unpriced and say so plainly rather than guessing.
- `compaction`: read `auto`, `buffer`, `keep.tokens`, and `background`, plus
  the deprecated `reserved`, `prune`, `tail_turns`, and
  `preserve_recent_tokens` an older config may still set (OpenCode v2.4.1's
  schema), and replay candidate thresholds against observed compaction events
  (`time_compacting` and summary turns) when calibration samples exist,
  extrapolating with `~` and a margin of error as `doctor`'s Compaction window
  line does for Claude Code and Codex. Like that line, weigh cost alone and
  name what the replay cannot measure: a smaller `buffer` or `keep.tokens`
  keeps less recent conversation beside the summary, and the effect of that
  lost detail on the work is not measured. State plainly when a sub-setting
  such as `prune` cannot be priced from recorded token counts.
- Anthropic cache TTL: when an OpenCode profile sends Anthropic requests with
  cache read/write splits, replay 5m vs 1h TTL using the existing cache
  replay. OpenCode v2.4.1's schema has a provider and model
  `settings.cache.ttl`; confirm against OpenCode's source that it reaches
  Anthropic's `cache_control` before offering the edit, and otherwise report
  the line as measured evidence naming the missing knob.
- `Harness::replayed()` in `src/model.rs` keeps OpenCode out of the cache,
  compaction and keepalive replays (and the transcript audit) today, and an
  OpenCode profile's TTL and compaction controls are unset with that reason
  in `src/opencode.rs`'s `profile`; each replay this quest adds lifts that
  exclusion for its own check.
- Exercise against real local OpenCode histories where applicable and add
  synthetic SQLite fixtures for warming, compaction, and Anthropic TTL cases.
- Document replay assumptions and limits in `docs/reference.md` and
  `docs/semantics.md`.
