# [S] Show set Codex compaction settings when no request is selected

## Goal

When a Codex profile sets `model_auto_compact_token_limit` or
`model_post_turn_compact_threshold_percent` but the scan selects none of its
requests (no history, or `--since` excludes it), `doctor` still gives each its
line: the value, the file that sets it, and that nothing in the window can
measure it. Today both lines disappear and the report reads as if no setting
differs from its default, which AGENTS.md rules out ("say plainly when a cost
can't be measured rather than hiding the setting").

## Plan

Found 2026-11-24 while reviewing
[kixel-ai/token-max#210](https://github.com/kixel-ai/token-max/pull/210):
CodeRabbit flagged the turn-end line's early return, and the reply showed the
window line takes the same one, so the fix belongs to both.

Decided 2026-11-24 (maintainer, /quest-run): ✅ plan it (rejected: leave the
lines hidden with no requests).

Recommendations:

- The early exits are `let Some(first_selected) = ... else { continue; }` in
  `src/compaction.rs` and `first_selected ... ?` in `src/turn_end.rs`. Return
  a no-verdict result carrying the current value and owner instead, as the
  other lines do for a setting with nothing to replay.
- Only a value that is set gets the line; an unset default with no requests
  stays out, as now, so a Codex home with no sessions doesn't grow two empty
  rows.
- No verdict, no offer and no exit-code effect: there is nothing measured.
- The lines stay in `doctor`, not `vibes`: AGENTS.md sends a setting with
  nothing to compare there, but these replay whenever the window holds
  requests, and only this window lacks them.
