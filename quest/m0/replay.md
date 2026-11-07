# [L] Replay and the cache TTL check

## Goal

`token-max` reads every Claude Code and Codex session on the host, one
record per model request (time, session, main or sub-agent, model, input,
cache write by TTL, cache read, output), prices them from models.dev, and
runs its first check: Claude Code's prompt cache TTL, 5m or 1h, separately
for main sessions (`promptCacheTtl`) and sub-agents
(`subagentPromptCacheTtl`). The report format, `--since`, `--json`, and the
nonzero exit when a change is recommended are in place for the later
checks. The reader also exposes available structural compaction, turn, and
client metadata for those checks, separately from model requests, and
estimates compaction charges missing from request usage. It exposes
request attribution, effective service tiers, context exposure, and usage
coverage for the other checks, and supports guidance without priced
savings. Tests run on small synthetic transcripts.

## Plan

The check replays each session's request gaps: under 5m, a gap of 5 to 60
minutes rewrites the cached prefix at the 5m write price instead of reading
it; under 1h, every write costs the 1h price. Unset, Claude Code uses 1h for
the main conversation on a subscription within its plan and 5m otherwise,
and 5m for sub-agents everywhere; `CLAUDE_CODE_PROMPT_CACHE_TTL`,
`CLAUDE_CODE_SUBAGENT_PROMPT_CACHE_TTL`, `ENABLE_PROMPT_CACHING_1H`, and
`FORCE_PROMPT_CACHING_5M` override the settings
([prompt caching](https://code.claude.com/docs/en/prompt-caching), checked
2026-11-05). Codex exposes no cache TTL setting: gpt-6.1-sol's cache lives at
least 30 minutes and every read renews it, so the check covers Claude Code
only.

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
- Resolve prices for the actual model, service tier, context tier, and
  provider. A missing price stays unknown; do not silently substitute a
  public model for internal models such as `codex-auto-review`. OpenAI
  cached input is part of total input, reasoning output is part of output,
  and its catalog's cache-write field is not an additional OpenAI charge.
- Each Claude request records its cache writes by TTL, which dates the last
  TTL change for the default window.
- Keep the reader to request fields and structural metadata, so transcript
  text never stays in memory longer than one line's parse. Expose available
  context sizes, `compact_boundary` metadata (`trigger`, `precomputed`,
  `preTokens`, `postTokens`, `durationMs`), turn timing/tool-wait state, and
  client/version fields without treating a boundary as a billed model
  request. Missing metadata stays unknown; summary text is never retained
  or printed. The consuming check interprets eligibility and reads the
  settings it needs.
- Expose source/trigger identifiers and their coverage without deriving
  background calls from idle time alone. Link Claude `api-request` IDs and
  assistant request references, deduplicating streamed assistant usage by
  message/request identity. Expose response speed/tier and effective
  version, client, and setting records where present; an omitted historical
  setting stays unknown. Each consuming check resolves current settings
  and overrides from the owning harness rather than treating a missing key
  as proof of a historical value.
- For Codex, prefer individual `token_usage_record.usage`, deduplicated by
  `response_id`; its turn/thread usage fields are cumulative. Do not add
  overlapping `event_msg.token_count` accounting again. Support older
  formats with verified cumulative/delta semantics, including repeated
  totals, resets, and inherited session history. Malformed records fail the
  run with location diagnostics that contain no transcript text; exploratory
  scans that excluded bad records are not complete billing audits.
- Expose skill/tool identities, invocation evidence, and the intervals in
  which context components were actually visible. Current Codex
  `world_state.state.host_skills.body` holds a rendered catalog;
  `state.tools` can hold deferred namespace descriptions, while
  `top_level_tools` may contain only hashes. Actual schemas may appear in
  `response_item.additional_tools`. Claude records skill listings,
  instruction files, request shapes, and deferred-tool deltas. Verify these
  formats before making synthetic fixtures; hashes are not schema text.
- Tokenize exposed components during parsing and retain counts, identities,
  fingerprints, and exposure metadata, not their content. Identify the
  tokenizer or approximation and use the request's cache/read/write prices
  for marginal context estimates. When component positions or cache status
  are missing, label the allocation assumption and show a range rather than
  treating all repeated context as uncached. Do not count snapshots as
  requests or assume current installed metadata existed historically.
- Validate compaction identification and cost signals here, before later
  checks use them. Inspect metadata for resumed and final idle periods;
  final periods may record nothing about a summary. Current boundaries
  distinguish `auto` and `manual`, not `idle`. Validate documented idle
  notices in system/informational records and their pairing with boundaries
  before using `precomputed: true`, an undocumented candidate signal. Match
  notices during parsing without retaining or printing their text. Unknown
  compaction reasons stay unknown; synthetic fixtures match verified record
  shapes. A missing boundary on an unresumed session proves nothing about
  summary generation or cost.
- Associate observed compactions with their usage when present. Otherwise
  expose one explicitly estimated summary charge from `preTokens` at the
  applicable input price and `postTokens` as a summary-output proxy, with
  cache/input overhead limits stated. This lets every later check price
  observed summaries once without waiting for the idle-policy check.
  Compactions share the outside-main TTL bucket controlled by
  `subagentPromptCacheTtl`; include these estimated charges when comparing
  that TTL, separately from the sub-agent audit figures. A boundary can be
  logged at resume; neither its timestamp nor `durationMs` establishes when
  the summary was generated. Preserve those distinctions in the metadata.
