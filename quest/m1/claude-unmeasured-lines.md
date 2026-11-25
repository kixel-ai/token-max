# [S] Show a set Claude autoCompactWindow when no request is selected

## Goal

When a Claude profile sets `autoCompactWindow` (top level, or under
`modelSettings.<model>`) but the scan selects none of its requests (no
history, or `--since` excludes it), `doctor` still gives the setting its line:
the value, the file that sets it, no verdict, no offer, and that nothing in
the window can measure it. Today the line disappears, as the Codex limit's
did before
[kixel-ai/token-max#225](https://github.com/kixel-ai/token-max/pull/225).

## Plan

Found 2026-11-24 in [kixel-ai/token-max#225](https://github.com/kixel-ai/token-max/pull/225).
Decided 2026-11-24 (maintainer, /quest-run): ✅ plan it as its own quest
(rejected: widen #225 to fix Claude too; leave the Claude line hidden).

- The early exit is `let Some(first_selected) = ... else { continue; }` in
  `compaction_checks` (`src/compaction.rs`). `unmeasured` there returns `None`
  for a non-Codex profile, and `CheckBase::unmeasured` (`src/model.rs`) builds
  the no-verdict base.
- The problem is that Claude's window is per model: `window_control(.., Some(model))`
  resolves a `modelSettings.<model>` row, an alias or a dated ID before the
  top-level value, and the replay's lines are per model. With no selected
  request there is no model to resolve against, and `window_control(.., None)`
  reads only the top-level value, which would hide a `modelSettings` row.
- Recommended: one line per set row, not per model. List each
  `modelSettings.<model>.autoCompactWindow` row and the top-level value from
  `compaction.models` and `compaction.default`, each with its own owner (the
  `row_key` form the measured lines use). That shows every set value without
  guessing which model runs. Environment overrides
  (`CLAUDE_CODE_AUTO_COMPACT_WINDOW`, `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE`) that
  outrank a row must show on the line that they outrank, since a value an
  override outranks would otherwise read as live.
- A value that is unset, or `"auto"`, gets no line, as the Codex lines do.
- No verdict, no offer and no exit-code effect; same `--details` rule as #225
  (no replay fields for a line with no scenarios).
- Tests: a top-level value, a model row, a row an override outranks, and an
  unset default with no requests.
