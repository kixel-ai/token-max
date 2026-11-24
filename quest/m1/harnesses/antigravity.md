# [L] Discover, price, and audit Antigravity CLI and T3 sessions

## Goal

`token-max` (`doctor`, `sessions`, T3 Code `antigravity` instances, and `--ssh`)
discovers Antigravity CLI (`agy`) and T3 `antigravity-acp` profiles, reads
their per-request transcripts and conversation metadata, prices requests via
`models.dev`, and reports spend, model/effort and subagent evidence, and
measured MCP, skills/plugins/rules, and tool-permission checks. In `doctor`,
confirmed edits write to the owning `mcp_config.json` or `settings.json`.

## Plan

Settled decisions (from [the epic](/quest/m1/harnesses/README.md)):

- Discover Antigravity CLI (`~/.gemini/antigravity-cli/`) and T3 Code's
  `antigravity` driver
  (`~/.t3/userdata/providers/antigravity/<hash>/antigravity-acp/`), plus
  explicit `--antigravity-dir`. Antigravity IDE and 2.0 Desktop App
  (`~/.gemini/antigravity/`) are out of scope until local transcripts exist to
  validate against.
- Cover `doctor`, `sessions` export, T3 instances, and `--ssh`.
- Keep all lines strictly measured (no `vibes` cards). Antigravity's
  `settings.json` has no cache-TTL, compaction-window, fast-mode, or keepalive
  setting, so emit no fabricated replay lines.

Recommendations:

- Discovery: resolve Antigravity CLI (`~/.gemini/antigravity-cli/settings.json`
  and `~/.gemini/config/mcp_config.json`), T3 `antigravity` instances in
  `src/t3.rs` (`antigravity-acp/settings.json` and `../config/mcp_config.json`),
  and `--antigravity-dir`.
- Transcript reader: stream
  `brain/<id>/.system_generated/logs/transcript.jsonl` (`PLANNER_RESPONSE`
  records) without keeping prompt, thinking, or reply text. Verify token
  accounting against real logs: `input_tokens` is uncached input (distinct from
  `cache_read_tokens`, which grows monotonically above `input_tokens`),
  `output_tokens` is output, and Gemini implicit caching has no cache-write
  surcharge. Split the recorded model slug into model and effort (for example
  `gemini-4-carbon-high` becomes model `gemini-4-carbon`, effort `high`) and
  price against `models.dev` (`google/<model>`).
- Metadata and subagents: read `conversation_summaries.db` (`workspace_uris`,
  `parent_conversation_id`, `agent_name`, `nesting_depth`) read-only with
  `src/t3.rs`'s SQLite safety flags, plus `conversations/<id>.meta` (`cwd`), to
  supply `cwd`, repositories, and parent/subagent linkage for `sessions` export
  and subagent spend breakdown.
- Missing `model` on T3 ACP rows: on the local host (2026-11-23),
  `antigravity-cli` recorded `model` on 759 of 759 `PLANNER_RESPONSE` rows,
  while `antigravity-acp` omitted `model` on 55 of 60 transcripts. Check
  whether `conversations/<id>.db` (`gen_metadata` or
  `trajectory_metadata_blob`) or T3's `statev2.sqlite`
  (`orchestration_v2_projection_provider_sessions` and `provider_threads`)
  uniquely proves the model for those sessions; keep any request without proven
  model identity unpriced and extrapolate with `~` and margin where rules
  allow, never guessing.
- Cache lifetime: Gemini's implicit cache lifetime is undocumented. One
  public report from T3 logs never saw a cache hit after a gap longer than
  14m 39s, and saw a switch from `high` to `medium` effort miss 104.6k cached
  tokens. Any line that needs a lifetime, such as a T3 idle-compaction
  replay on an `antigravity` instance (first check whether T3 runs idle
  compaction there), should take it from the user's own logs: the longest
  gap still followed by a cache hit. Don't assume 15 minutes. Treat a change
  of the slug's effort suffix as a model change that starts with a cold
  cache.
- Exposure and edits: audit configured MCP servers (`mcp_config.json`),
  skills/plugins/rules (`~/.gemini/antigravity/skills/`, `.agents/skills/`,
  `AGENTS.md`, `GEMINI.md`), and tool usage (`tool_calls` names) against
  recurring context overhead, offering JSON edits to `mcp_config.json` and
  `settings.json` in `doctor`.
- Exercise against the local host's real `antigravity-cli` and T3
  `antigravity-acp` histories before review, and cover CLI, T3 ACP, subagents,
  missing-model handling, `sessions`, and JSON edits with synthetic fixtures.
- Document Antigravity discovery, accounting, and caveats in
  `docs/reference.md`, `docs/semantics.md`, and `docs/t3.md`.

## Required

- [Discover, price, and audit OpenCode sessions and settings](/quest/m1/harnesses/opencode.md) - generalizes Harness dispatch, discovery, T3 driver routing, and exposure checks past the two-harness Claude/Codex assumption
