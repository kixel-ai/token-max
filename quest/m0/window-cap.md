# [XS] Cap explicit windows at the model's context window

## Goal

An explicit Claude `autoCompactWindow` larger than the model's context
window replays at the window Claude Code actually uses, such as 200k for an
explicit 400k under `CLAUDE_CODE_DISABLE_1M_CONTEXT=1` or on Opus 4.6
without `[1m]`, so the compaction, idle and keepalive checks stop pricing a
window that never ran.

## Plan

Decided 2026-11-11 (maintainer, /quest-complete, follow-up suggested by
[kixelated/token-max#59](https://github.com/kixelated/token-max/pull/59)):
✅ m0 (rejected: m1; dropping it).

Claude Code caps any window at the model's context window
([model config](https://code.claude.com/docs/en/model-config#turn-off-1m-context)).
Reuse the cap table #59 added to `src/controls.rs` (`capped`, and its
`CLAUDE_CODE_MAX_CONTEXT_TOKENS` exception for IDs outside the table)
rather than a second table. Label a capped explicit value in the report as
the setting's value and the effective window.
