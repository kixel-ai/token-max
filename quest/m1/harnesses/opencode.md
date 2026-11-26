# [L] Discover, price, and audit OpenCode sessions and settings

## Goal

`token-max` (`doctor`, `sessions`, T3 Code `opencode` instances, and `--ssh`)
discovers OpenCode profiles, reads both v1 and v2 transcripts from
`opencode.db`, prices requests via `models.dev` by recorded
`providerID`/`model.id`, and reports spend, model/effort and subagent evidence,
and measured MCP, skills, and instructions checks. In `doctor`, confirmed edits
write to `opencode.json` or `opencode.jsonc` while preserving comments and
formatting.

## Plan

Settled decisions (from [the epic](/quest/m1/harnesses/README.md)):

- Cover `doctor`, `sessions` export, T3 `opencode` instances, and `--ssh`.
- Price requests strictly from `models.dev` using recorded
  `providerID`/`model.id` (for example `opencode`, `opencode-go`,
  `opencode/*-free`, `anthropic`, `openai`, `google`); never trust OpenCode's
  stored `cost` field, and leave uncataloged models unknown rather than
  guessing.
- Preserve comments and formatting when `doctor` edits `opencode.jsonc`
  (matching how `toml_edit` preserves Codex's `config.toml`), leaving
  unsupported or unparseable JSONC syntax as recommend-only rather than
  stripping comments.
- Keep all lines strictly measured; no `vibes` cards in this quest.

Recommendations:

- Replace binary `Harness::Claude` vs `else` assumptions across the codebase
  with exhaustive `match harness` dispatch (discovery, reader, pricing, checks,
  exposure, report, doctor, sessions, T3, wire) so Antigravity and later
  harnesses plug in cleanly without duplicate branching.
- Discovery: honor `--opencode-dir`, `$XDG_DATA_HOME/opencode/opencode.db`
  (default `~/.local/share/opencode/opencode.db`),
  `$XDG_CONFIG_HOME/opencode/opencode.json{,c}` (default
  `~/.config/opencode/opencode.json{,c}`), `OPENCODE_CONFIG`, project
  `opencode.json{,c}`, and T3 Code's `opencode` driver instance in `src/t3.rs`.
- SQLite safety: open `opencode.db` read-only with `query_only`,
  `trusted_schema=OFF`, snapshot isolation, and bounded column reads (matching
  `src/t3.rs`), never copying a live WAL database and never retaining prompt,
  reasoning, reply, or tool-output text.
- Reader: support both OpenCode v2 (`session_v2` and `session_message`, where
  `type = 'assistant'` carries `model: { id, providerID, variant }`, `agent`,
  `tokens: { input, output, reasoning, cache: { read, write } }`, `time`, and
  tool calls in `content`) and legacy v1 (`session`, `message`, and `part`
  `step-finish` records). Capture `directory`/`worktree`, `parent_id`, and
  timestamps for `token-max sessions` and subagent cost breakdown. Deduplicate
  shared histories.
- Exposure checks: resolve configured MCP servers in both shapes OpenCode
  accepts, v2's `mcp.servers.<name>` and v1's `mcp.<name>` beside
  `mcp.timeout`, counting only servers whose `enabled` is not `false`; skills
  from the `skills` array (and its deprecated `skills.paths`/`skills.urls`
  object form) plus the discovery directories `~/.config/opencode/skills/`,
  `.opencode/skills/`, `~/.agents/skills/`, `.agents/skills/`,
  `~/.claude/skills/`, and `.claude/skills/`; and instructions (`AGENTS.md`,
  `instructions` paths). A skill directory Claude Code also loads costs each
  harness that loads it, so price it per harness, not once. Measure recurring
  context overhead against recorded uses, following `AGENTS.md` (red/exit 1
  when never used, yellow/exit 0 above `--max-cost-per-use`, green otherwise).
- Exercise against the local host's real `~/.local/share/opencode/opencode.db`
  and `~/.config/opencode/opencode.jsonc` before review, and cover v1, v2, T3,
  `sessions`, and comment-preserving JSONC edits with synthetic fixtures.
- Uncached system prompt (decided 2026-11-25, maintainer, /quest-plan over
  [this post](https://x.com/TekonBurns/status/2123525121168314876)):
  gpt-6.2-sol and gpt-6.2-astra never cache a Responses request's top-level
  `instructions`, but they cache the same text sent as a leading developer
  message ([openai/codex#62312](https://github.com/openai/codex/issues/62312),
  open). The post says OpenCode v2.3.2 moved the prompt there. When the
  latest OpenCode session on a profile ran an older version with a gpt-6.2
  model, price the system prompt's uncached input and recommend upgrading.
  OpenCode's stored token counts don't split out the system prompt, so
  that price is an estimate: mark it `~` with its margin of error, and say
  which part of the prompt it could not size.
  Record OpenCode's version per request for that, and reuse the
  version-gated upgrade advice `doctor` gained in
  [kixel-ai/token-max#247](https://github.com/kixel-ai/token-max/pull/247)
  (`docs/reference.md`'s Harness upgrades).
  Codex itself shows no gap on the maintainer's host: since 2026-11-18,
  gpt-6.2-sol cached 90.2% of input over 1,281 requests, and gpt-6.1-sol
  88.9% over 7,960. So Codex gets no line.
- Document OpenCode discovery, pricing, and caveats in `docs/reference.md`,
  `docs/semantics.md`, and `docs/t3.md`.
