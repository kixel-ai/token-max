# [L] Give each setting one row per settings file

## Goal

`token-max doctor` reports each setting, tool, skill or MCP server once for
each settings file that owns it, combining every transcript source that file
controls, so native Claude Code and T3's claudeAgent share one row for
`~/.claude/settings.json`. Claude's and Codex's files, a T3 instance's own
settings, and each `--ssh` host's files stay separate rows. No change
contradicts a use the report shows.

## Plan

Found 2026-11-14 on the maintainer's host (99,739 requests over 4 days,
`~/.claude` shared by native Claude Code and T3's `claudeAgent`). For
`[~/.claude]` the Measured section printed the Agent tool three times:
"Never used", $0.76; "Used 376 times", $0.06; and `[T3 claudeAgent]`
"Used 376 times", $28. Grep and Glob also appeared twice for `[~/.claude]`
with different costs. "Changes to make" then offered
`"permissions": {"deny": ["Agent"]}` in `~/.claude/settings.json`, which
would turn off subagents for T3 as well.

Decided 2026-11-14 (maintainer, at the pick of the planning PR): ✅ stop
splitting rows by profile: one row per setting in each settings file,
combining every transcript source that file controls (rejected: one row per
profile, which produced the contradiction above; one row per feature across
files, which would need two edits for one row). This amends m0's "resolve
settings and recommendations per profile".

- Key each row by the setting and the owner its `Set in` names for each
  source. Ownership is per setting, not per source: T3 sets `fastMode`
  itself, and its SDK window override outranks the native files
  ([docs/t3.md](../../docs/t3.md)), so T3 claudeAgent joins
  `~/.claude/settings.json`'s tool and cache TTL rows while its fast mode,
  and its window when overridden, are rows of T3's settings. A shell
  variable, project or managed file that owns a setting for some sources
  gets its own row, as it is a step by hand today, and a source whose owner
  is unresolved keeps a row naming that source. A row no setting changes,
  such as a cost or effort line, merges by the same owner as its setting
  would.
- Decide each row from all its sources together: a replay sums each
  candidate value's cost across them, each source keeping its own window,
  and recommends the lower total; a feature's uses and cost are the sum of
  its sources'. Never decide per source and merge the verdicts.
- Count each request, use and dollar once. The run above put the same 376
  uses under both `[~/.claude]` and `[T3 claudeAgent]`, and a second
  `[~/.claude]` Agent row besides. Find why, such as subagent transcripts
  forming their own row or a second scan of one history, rather than
  hiding duplicates at render time.
- Name the file on each row; `--details` breaks a row down by source.
  Percents are of the whole host's spend, as
  [the report quest](/quest/m0/report-length.md) records.
- Cover with synthetic tests: native and T3 sharing `~/.claude`, one source
  using a tool the other never did (one row, no change offered), and T3
  owning `fastMode` (separate rows). Update `docs/reference.md`'s
  "Profiles" and "Applying changes", `docs/semantics.md` where a check
  resolves each profile, and `docs/t3.md` to match.

## Related

- [Cut doctor's default report to one line each](/quest/m0/report-length.md) - renders these rows
- [Walk the changes biggest saving first](/quest/m0/walk.md) - needs this, since it must never offer a change another row shows in use
