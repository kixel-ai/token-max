# [L] Report auto memory's total cost and advise against it

## Goal

Each Claude and Codex profile shows auto memory on its own line with its
total cost and use count over the window, the setting that turns it off, and
a yellow advisory: memory is not recommended because unreviewed notes the
agent writes for itself encourage slop, so durable guidance belongs in
reviewed, checked-in `CLAUDE.md` or `AGENTS.md`. The advisory exits 0. A
profile with memory off shows it green. MEMORY.md no longer appears under
"No setting turns these off" with advice to shorten it.

## Plan

Decided 2026-11-11 (maintainer, /quest-plan after asking "should we also
measure the cost of memory? IMO it should be checked into the repo (with
review), not ad-hoc"). Decision prompts, chosen options marked ✅:

- Recommendation: ✅ total cost plus a soft "not recommended, encourages
  slop" (rejected: off outright as a firm change; the `--max-cost-per-use`
  rule other features use). Whether memory earns its cost is subjective, so
  cost per use does not apply.
- Exit code: ✅ advisory, exit 0, yellow (rejected: a change with exit 1).
- Cost scope: ✅ loaded context plus memory reads and writes (rejected:
  loaded context with unpriced counts).
- Codex: ✅ same quest (rejected: a separate quest; Claude only).
- AGENTS.md: ✅ add auto memory as the exception to the per-use rule, so
  later work does not reapply it.

Claude Code (2.1.319, [memory docs](https://code.claude.com/docs/en/memory#auto-memory)):

- Control: `autoMemoryEnabled: false` in settings, or
  `CLAUDE_CODE_DISABLE_AUTO_MEMORY=1`. On by default. Storage is
  `<config>/projects/<repo>/memory/`, shared by all worktrees of a repository,
  or `autoMemoryDirectory` from any settings scope.
- Loaded context: the first 200 lines or 25 KB of `MEMORY.md` arrive in the
  `instructions` attachment beside CLAUDE.md files (`src/exposure.rs`
  `attachment()`), today priced as an ordinary `instructions:<path>` piece in
  `src/context.rs` `pieces()`. The system prompt also carries a `# Memory`
  section (about 2.1 KB of a 14.4 KB prompt on the maintainer's host), today
  inside "Claude Code system prompt". Split that section out of the recorded
  system prompt by its heading; when MEMORY.md loads but no section is found,
  say the section's cost is unmeasured rather than guessing.
- Uses: Read results of files under the memory directory move from the Read
  tool's output line to auto memory; Write and Edit inputs to those files are
  estimated at the output price from their recorded characters and labeled
  estimated. Topic files load only through such reads. On the maintainer's
  host, 284 of 300 recent sessions loaded MEMORY.md and they recorded 26
  memory writes.
- The maintainer turned auto memory off on 2026-11-12T01:35Z; sessions after
  that load no MEMORY.md, so the window since the setting changed shows it
  green.

Codex (0.167.0, source `openai/codex` tag `rust-v0.167.0`;
[memories docs](https://developers.openai.com/codex/memories)):

- Control: `[features] memories = false`, the default; `[memories]
  use_memories = false` stops only injection. `session_meta.payload.memory_mode`
  is recorded when set.
- Loaded context: a developer message starting `## Memory` with
  `memory_summary.md`, truncated to 2,500 tokens, added once per full context
  (session start or after compaction), not a world-state field. token-max
  drops developer `response_item` messages today.
- Uses: shell reads of the memories folder, and `memories` namespace tool
  calls (`list`, `read`, `search`, `add_ad_hoc_note`) when `dedicated_tools`
  is on.
- Not measurable: the extraction and consolidation passes run as a bare
  request and an ephemeral sub-agent and record tokens only in OTel; say so
  on the line.
- No local session has ever enabled it, so tests use synthetic records
  derived from the source; note that in the PR.

Docs: update the README sample (`~/.claude/CLAUDE.md` line) and
`docs/semantics.md` instruction-budget notes, which today would still list
MEMORY.md as a fixed instruction file.
