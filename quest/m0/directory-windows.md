# [L] Tune unset windows per recorded directory

## Goal

Idle and keepalive pricing resolve an unset Claude compaction window in each
recorded working directory's settings, not only the profile's, so sessions
run in a recorded directory get priced estimates instead of staying
advisory. Each tuned window is keyed by the setting that actually sets its
value, so two directories never share a replayed window they do not share
in Claude Code.

## Plan

Decided 2026-11-11 (maintainer, /quest-complete on
[kixelated/token-max#59](https://github.com/kixelated/token-max/pull/59)):
✅ land #59 as is and plan this follow-up (rejected: extend #59 first; land
the partial fix). ✅ m0 (rejected: m1), since m0 promises priced idle and
keepalive recommendations.

Why it is open: `controls::tune_windows` (from #59) fills only the
profile-wide controls, while idle and keepalive read each recorded
directory's own controls (`src/idle.rs` `Scopes::window`, `src/keepalive.rs`).
An unset window there stays unresolved. This predates #59;
[kixelated/token-max#58](https://github.com/kixelated/token-max/pull/58)
extends it to inherited T3 instances.

Starting point: branch `window-defaults-directories` (commits `0f8aae7`,
`251e41d`) tunes each directory's window, but replay keys windows by owner
(`current_windows` in `src/idle.rs`), and #59's review found two collisions:

- A per-model `modelSettings.<model>.autoCompactWindow` owner names the same
  row in every directory, so one directory's tuned window replays for
  another with a different cap or trigger.
- `CLAUDE_CODE_MAX_CONTEXT_TOKENS` was compared by whether it is set, not
  by its value.

Key each tuned window by what sets its value: the per-model row together
with the directory's `CLAUDE_CODE_DISABLE_1M_CONTEXT` cap and
`CLAUDE_CODE_MAX_CONTEXT_TOKENS` value, so directories that resolve to
different windows get distinct owners and recommendations. Keep #59's
precedence (cap, then observed trigger, then documented default, else
unresolved). Expect a design checkpoint early: settle the owner key before
patching individual collisions.
