# OpenCode and Antigravity support

## Goal

`token-max` (`doctor`, `sessions`, T3 Code instances, and `--ssh`) discovers,
prices, and audits OpenCode and Antigravity alongside Claude Code and Codex,
reporting API-equivalent spend and applicable measured setting recommendations.
Once all child quests finish, update `README.md`, `AGENTS.md`, `docs/`, and
[the recurring release check](/quest/m1/releases.md) so top-level framing and
release tracking include both harnesses.

## Plan

Decided 2026-11-23 (maintainer, `/quest-plan` after asking whether to support
Antigravity, OpenCode, and Grok). Both OpenCode and Antigravity are installed
and actively used on the maintainer's host and in T3 Code, with per-request
token transcripts and public API prices in `models.dev`.

Decision prompts, chosen options marked ✅:

- Goal: ✅ extend `token-max` (`doctor` and `sessions`, including T3 Code
  instances and `--ssh`) to discover, price, and audit additional local coding
  harnesses for spend and applicable measured setting recommendations, while
  keeping unmeasurable advice in `vibes` and leaving other harnesses (such as
  Cursor or Pi) out of scope (rejected: narrow to spend totals and `sessions`
  only; narrow to OpenCode only; drop).
- Harnesses: ✅ OpenCode + Antigravity (rejected: all three including Grok
  Build; OpenCode + Antigravity now with Grok Build gated on a local install
  and transcripts; Antigravity only). Grok Build (`grok`) is dropped for now:
  it is not installed locally to validate against real transcripts, logs
  per-turn (`turn_completed`) rather than per-request, and exposes no
  cache-TTL or compaction-window replay levers.
- Surfaces: ✅ `doctor`, `sessions` export, T3 instances, and `--ssh` together
  (rejected: `doctor`, T3, and `--ssh` only; `sessions` first, `doctor` checks
  later).
- Check depth: ✅ all measurable checks each harness's transcripts and settings
  support (rejected: spend, evidence, and MCP/skills/instructions only; spend
  and model/effort/subagent evidence only).
- OpenCode pricing: ✅ price via `models.dev` by recorded
  `providerID`/`model.id`, never trusting OpenCode's stored `cost` (rejected:
  always map to upstream provider API rates; fall back to stored `cost`).
- OpenCode `opencode.jsonc` edits: ✅ preserve comments and formatting when
  editing `.jsonc`, leaving unparseable or unsupported syntax as recommend-only
  rather than stripping comments (rejected: edit `.json` only and keep `.jsonc`
  recommend-only; recommend-only for all OpenCode config).
- Antigravity surfaces: ✅ Antigravity CLI (`agy`) and T3 `antigravity-acp`,
  plus `--antigravity-dir` (rejected: also discover Antigravity IDE / 2.0
  Desktop App `~/.gemini/antigravity/`; T3 `antigravity-acp` only).
- `vibes` boundary: ✅ keep this epic strictly measured (`doctor` + `sessions`),
  deferring OpenCode and Antigravity `vibes` cards until `vibes` lands and
  opinions are ready (rejected: include `vibes` cards now). `vibes` has since
  landed ([kixel-ai/token-max#205](https://github.com/kixel-ai/token-max/pull/205));
  the boundary stands, and OpenCode or Antigravity cards are a later quest
  once a provider or the maintainer has advice for them.

Recommended 2026-11-26 (`/quest-iterate` takeover, for review):
`token-max audit`
([kixel-ai/token-max#273](https://github.com/kixel-ai/token-max/pull/273)),
which landed after this plan, stays Claude Code and Codex only in this epic.
Its hotspot buckets rest on each harness's tool and cache records, which the
child quests only start to read; extend it once both readers ship.

## Related

- [Check new Claude Code and Codex releases](/quest/m1/releases.md) - extend
  the two-week release check to OpenCode and Antigravity once both ship
