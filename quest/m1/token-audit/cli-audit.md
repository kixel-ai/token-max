# [L] Add token-max audit forensic subcommand with SSH support

## Goal

`token-max audit` streams local and `--ssh` Claude Code and Codex transcripts
(including T3 Code instances), scopes to the current Git repository by default
(or all repositories with `--all`), and prints structured JSON of priced
transcript inefficiency hotspots with `(host, file, line)` pointers so the
`token-audit` skill can inspect only the highest-leverage transcript slices
without reading raw logs wholesale. It never prints or retains prompt, reply, or
raw command/argument text—locally or over `--ssh`.

## Plan

Implements the deterministic forensic engine decided in the
[token-audit epic](/quest/m1/token-audit/README.md).

### CLI interface and scope

- Subcommand: `token-max audit [options]` alongside `doctor`, `vibes`, and
  `sessions`, keeping `doctor` unaffected (`AGENTS.md`: machine-readable output
  lives in its own command).
- Selection flags matching `sessions` / `doctor`: `--since YYYY-MM-DD|ISO`
  (recommend defaulting to the last 14 days when omitted so audits stay focused
  and fast, while recording the resolved window in the output), `--claude-dir`,
  `--codex-dir`, `--t3-dir`, `--ssh HOST` (repeatable), `--offline`,
  `--prices-file`, `--price-cache`.
- Repository scoping: default to the Git repository of the current working
  directory (matching sessions by canonical repository URL / checkout root using
  the same resolution `src/sessions.rs` and `src/repository.rs` use; if outside a
  Git repository, error clearly and point to `--all` or `--repo PATH`). Support
  `--all` for whole-host audits and `--repo PATH` to target another checkout.
- Output: bounded, deterministic JSON on stdout (diagnostics/errors on stderr as
  JSON, matching `sessions`), with exit 0 on success and exit 2 on scan,
  pricing, argument, or cancellation failure.

### Forensic buckets (all priced at public API rates)

Reuse `src/reader.rs`, `src/cache.rs`, `src/prices.rs`, `src/exposure.rs`, and
`src/ssh.rs` rather than building a second parser or pricer:

1. **Cache invalidations and prefix busts**: every request with material lost
   cache reuse relative to the preceding request's eligible prefix—measuring
   the drop in cached reads against the prior eligible prefix (`prev_prefix -
   cache_read`, bounded by the overlapping rewritten/uncached tokens, matching
   `src/bash.rs`'s `miss()` and `src/cache.rs`) and pricing only that rewritten
   or uncached overlap at its marginal rate rather than checking whether total
   prefix size shrank (since a full-prefix TTL rewrite keeps `read + write`
   unchanged or growing). Classify each event by observable cause:
   - `tool_wait_ttl`: foreground tool/Bash call wait exceeded the active cache
     TTL (reusing `src/bash.rs`'s wait-linking logic).
   - `idle_ttl`: user idle gap exceeded TTL on a large context (cold resume).
   - `control_change`: mid-session model switch, effort/thinking switch,
     fast-mode toggle, or compaction/truncation.
   - `prefix_bust`: unexplained in-TTL lost cache reuse with no
     model/effort/speed/compaction change (e.g., mid-session
     `CLAUDE.md`/`MEMORY.md` edits, tool/MCP schema churn, or volatile prefix
     shifts).
   Emit ranked hotspots with aggregate and per-event USD cost, rewritten tokens,
   preceding tool names (whitelisted names only, never arguments), gap duration,
   and `(host, file, line)` pointers.
2. **Tool-output bloat and carried-context tax**: estimate each tool result's
   token injection (from recorded token deltas / result byte lengths, keeping
   size only and discarding content during streaming) and price its carry over
   subsequent turns before compaction or session end by reusing request-level
   context pricing (`src/context.rs` and `src/prices.rs::prefix_spans`) so
   tokens that fall outside a cache hit (such as uncached Codex turns) are not
   underpriced at the cache-read rate. Where an individual result's placement
   across cached vs. uncached spans on later turns is ambiguous, leave its
   individual USD cost unknown/unpriced (matching `token-max`'s "unknown, never
   zero" rule) while still emitting its estimated token size, turns carried,
   tool name, and `(host, file, line)` alongside priced per-tool aggregates.
3. **Repetitive/failing tool loops and session-shape outliers**:
   - Sessions and spans with high tool-call counts, high error/retry rates, or
     long streaks of the same tool name (e.g., repeated reads/greps/shell
     retries).
   - Session-shape outliers: highest-cost sessions, lowest cache-hit-ratio
     sessions, oversized multi-turn mega-sessions (high cumulative context
     without compaction or `/clear`), high starting-prefix sessions (heavy
     always-loaded instructions/MCP/memory), and subagent spawn overhead.
   - Include early-turn `(host, file, line)` pointers for top sessions so the
     skill can sample recurring startup exploration ("information that should be
     context").

### Privacy, bounds, and SSH

- Never retain or emit prompt text, reply text, tool inputs/commands, tool
  outputs, or file contents—only whitelisted tool names, counts, timestamps,
  prices, session/repo metadata, and `(host, file, line)` pointers (recording
  1-based JSONL line numbers during streaming).
- Over `--ssh`, follow `src/ssh.rs`'s model: each destination runs the same
  `token-max` build, filters and aggregates locally with this host's price
  catalog, and sends only bounded JSON hotspot summaries back (respecting any
  stream bounds in `src/ssh.rs`). Merge and re-rank hotspots across hosts with
  each pointer tagged by `host` (`null` for local, destination string for SSH).
- Cap each hotspot list (e.g., top N per category, configurable via `--limit`)
  so the JSON output stays compact for an agent's context window.

### Verification

- Synthetic Claude Code and Codex JSONL fixtures in `tests/` covering:
  - each cache-miss class (`tool_wait_ttl`, `idle_ttl`, `control_change`,
    `prefix_bust`), including a full-prefix TTL rewrite whose total prefix size
    (`read + write`) stays constant or grows,
  - tool-output carried-context pricing (including uncached Codex spans and
    unpriced ambiguous placement) and line-number accuracy,
  - repetitive/error tool loops and mega-session outliers,
  - repo-scoped vs. `--all` filtering and outside-repo error behavior,
  - `--ssh` aggregation across hosts,
  - an explicit no-text-leak test asserting sentinel strings placed inside
    prompts, replies, tool arguments, and tool outputs never appear anywhere in
    `token-max audit` stdout or stderr.
- Document `token-max audit` in `docs/reference.md` and `docs/semantics.md`
  (naming every assumption, such as byte-to-token estimation for individual tool
  outputs when transcripts only record per-turn totals).
