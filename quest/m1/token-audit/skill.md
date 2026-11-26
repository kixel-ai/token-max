# [M] Ship the token-audit skill, prompt command, and skill installer

## Goal

Users can install the `/token-audit` skill via `token-max skill install` or
`npx skills add kixel-ai/token-max -g`, invoke it in Claude Code, Codex, or any
Agent Skills harness, and have the agent run `token-max skill audit` and
`token-max audit` to diagnose cache-busting actions, excessive/bloated tool
usage, useless context, and missing context—locally and across `--ssh` hosts—and
present a ranked, dollar-attributed report with interactive approval before
applying any edit.

## Plan

Implements the skill, live prompt command, and installer decided in the
[token-audit epic](/quest/m1/token-audit/README.md).

### Thin stub and repository layout

- Add `skills/token-audit/SKILL.md` at the repository root following the
  [Agent Skills specification](https://agentskills.io/specification) so
  `npx skills add kixel-ai/token-max` discovers it natively.
- Keep `SKILL.md` as a thin stub (matching `quest`'s pattern) whose YAML
  frontmatter names `token-audit` and opens its `description` with when it
  should and should not run (in the words a user's request would use—distinguishing
  transcript efficiency audits from `token-max doctor`'s settings checks).
- The stub body instructs the agent to run `token-max skill audit` and follow
  its output, with a fallback hint when `token-max` is not on `PATH`
  (`npx @kixel/crabx token-max skill audit` or `https://max.kixel.me`).
- Embed `skills/token-audit/SKILL.md` into the Rust binary via `include_str!`
  so the repo copy and `token-max skill install` never drift.

### `token-max skill audit` prompt

Follow `quest guide prompting` and the official Claude Code, Codex, and Agent
Skills guidance when writing the embedded prompt (concise, key instructions
first, no fluff, check current official caching/context/skills docs before
finalizing wording):

1. **Gather metrics first**: run `token-max audit` (current repo by default;
   pass `--all` if the user asked for a whole-host audit, `--since` if given,
   and `--ssh <host>` for any requested remote hosts). Note that settings-level
   tuning belongs to `token-max doctor` / `vibes` and should be mentioned only as
   a one-line pointer rather than re-audited.
2. **Filtered, bounded slice inspection**:
   - Never read whole JSONL transcripts or dump raw JSONL lines directly into
     the audit session's tool output (raw lines contain private user prompts,
     assistant replies, and huge tool payloads that would be retained in the
     audit session's own history).
   - Inspect only the top flagged `(host, file, line)` windows plus a bounded
     sample of early exploration turns from the highest-cost sessions, piping
     each local read—and each remote `ssh <host>` read—through a read-only
     filter (such as `jq`) *before* output reaches the agent. The filter strips
     user prompt prose, assistant reply text, and raw file/env/tool-output
     bodies, emitting only safe structured evidence: record type, timestamp,
     model/effort, tool names, truncated tool targets (e.g., command
     executable/flags or file path with secret-like env/token values redacted),
     exit code / error status, and result byte/line counts.
   - If a hotspot genuinely requires reading raw prompt, reply, or tool-output
     text to diagnose, leave that inspection to the user by citing its
     `(host, file, line)` pointer rather than pulling raw text into the audit
     session.
   - Never copy secrets, tokens, or raw transcript quotes into reports or
     committed files; paraphrase findings with counts and dollar figures.
3. **Distilled best-practice audit checklist** (Claude Code + Codex + agent
   context engineering):
   - **Cache invalidations**: commands/scripts/builds outlasting cache TTL (move
     to background, shorten, or cap), mid-session `CLAUDE.md`/`MEMORY.md` edits,
     mid-session model/effort/fast-mode toggles, tool/MCP churn, cold resumes on
     bloated windows (suggest `/clear` vs. `/compact`).
   - **Tool & command waste**: noisy/unfiltered build, test, or log outputs
     carried across many turns (recommend quiet flags, targeted commands,
     `PreToolUse` filters, or file offloading), repeated reads of the same files,
     debug/retry spirals ("3-strike" reset rule), unused code-intelligence/LSP
     where agents brute-force grep.
   - **Useless context**: bloated always-loaded `CLAUDE.md`/`AGENTS.md`, stale
     auto-memory, heavy MCP schemas, or multi-task mega-sessions carrying dead
     history—recommend moving specialized instructions to skills or path-scoped
     rules, trimming dead rules, and splitting tasks with `/clear`.
   - **Missing context (what *should* be context)**: repeated across-session
     rediscovery (build/test commands, repo layout, recurring command failures,
     worktree/env quirks) that would pay for itself as a concise `AGENTS.md`
     line, skill, hook, or script—checking existing `AGENTS.md`/`CLAUDE.md`
     first so nothing is duplicated or bloated past harness budgets.
4. **Output and interactive fixes**:
   - Present findings ranked by measured or estimated dollar impact, separating
     measured CLI figures from qualitative estimates, citing session/event counts
     and concrete root causes.
   - Propose concrete, minimal edits (e.g., `AGENTS.md`/`CLAUDE.md` trims or
     additions, hook/script filters, skill splits, workflow habit notes) and ask
     before applying any file edit. Never edit transcripts.

### `token-max skill install`

- Default (global install): write `token-audit/SKILL.md` into each detected
  harness's global skills directory on this host:
  - Claude Code: `<claude-dir>/skills/token-audit/SKILL.md` (default
    `~/.claude`, honoring `--claude-dir` and `CLAUDE_CONFIG_DIR`),
  - Codex: `<codex-dir>/skills/token-audit/SKILL.md` (default `~/.codex`,
    honoring `--codex-dir` and `CODEX_HOME`),
  - Shared Agent Skills: `~/.agents/skills/token-audit/SKILL.md` when
    `~/.agents` already exists (deduplicating real paths so symlinks such as
    `~/.claude/skills -> ~/work/dotfiles/skills` are written once).
- `--project`: install into `.agents/skills/token-audit/SKILL.md` (and
  `.claude/skills/token-audit/SKILL.md` if `.claude` exists and is not symlinked
  to the same target) in the current working directory.
- Safe atomic writes: create parent directories as needed; if an existing
  `token-audit/SKILL.md` matches the embedded stub, report up to date; if it
  differs from a `token-max`-managed stub, refuse to overwrite without `--force`
  (printing the path and diff hint) so user-customized files are never silently
  clobbered. Print every written or skipped path cleanly.

### Verification

- Unit and integration tests in `tests/` covering:
  - `token-max skill audit` stdout and exit code,
  - `token-max skill install` for global dirs, `--claude-dir`/`--codex-dir`,
    symlink deduplication, `--project`, idempotent re-run, and refusal to
    overwrite modified files without `--force`,
  - a test asserting `skills/token-audit/SKILL.md` matches the embedded stub.
- Update `src/cli.rs` help text and `docs/reference.md`.
