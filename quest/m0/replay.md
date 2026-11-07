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
request attribution, effective service tiers, structural context events,
and usage coverage for the other checks, and supports guidance without
priced savings. The context-exposure quest owns component estimates and
exposure reconstruction. Tests run on small synthetic transcripts.

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

- Discover the default and environment-selected roots plus recognizable
  local profiles such as `~/.claude*/projects`; accept repeatable
  `--claude-dir` and `--codex-dir` overrides for arbitrary locations. Print
  discovered, selected, excluded, and inaccessible roots so detection gaps
  cannot silently look like a whole-host report. Explicit selection limits
  the run; inaccessible selected roots fail it.
- Scan main and nested subagent transcripts. Canonicalize directories and
  deduplicate request identities across files and profiles. Distinct config
  roots can share one projects directory; read that history once, preserve
  the aliases, and resolve each profile's settings and billing/provider
  conditions separately. Assign historical costs to a profile only with
  provenance; ambiguous shared history stays a labelled shared bucket, not
  a full-cost copy under every alias. Current-setting replays name the
  chosen profile and assumption. Apply these rules to every later check.
- Fetch models.dev's `api.json` with a cached copy for offline runs, and
  name the price an estimate assumed when the catalog lacks one, such as the
  1h write.
- Resolve prices for the actual model, service tier, context tier, and
  provider. A missing price stays unknown; do not silently substitute a
  public model for internal models such as `codex-auto-review`. OpenAI
  cached input is part of total input, reasoning output is part of output,
  and its catalog's cache-write field is not an additional OpenAI charge.
- Resolve contradictory catalog fields against the provider's documented
  threshold with a dated source in the price adapter. In particular,
  `tiers[].size` and the legacy `context_over_200k` name must not silently
  choose different thresholds. Unsupported model/tier combinations remain
  unpriced, including fast-mode entries without long-context rates. Price
  whole baseline and counterfactual requests again when a change crosses a
  context tier; do not multiply removed tokens by the old marginal rate.
- Verify whether Claude top-level usage includes `usage.iterations` before
  using either; never add overlapping totals. Account for recorded
  `inference_geo`/regional pricing and public server-tool fees when
  applicable, otherwise expose unpriced charges. Keep these distinct from
  client-side tool calls and from compaction boundaries.
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
  totals, resets, and inherited session history. Freeze file lengths at run
  start; defer an unterminated final line as in progress and report the
  snapshot boundary. Exclude data provably outside the selected window
  before validation when safe; a file's start date alone is insufficient.
  Complete malformed selected records fail the run with location
  diagnostics containing no transcript text. Keep the workspace's strict
  failure policy: no success report or recommendations from a failed scan,
  and no skip-malformed mode in m0. Earlier exploratory partial scans are
  not complete billing audits.
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
