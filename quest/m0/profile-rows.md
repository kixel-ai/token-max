# [M] Give each profile one row per tool

## Goal

`token-max doctor` reports each tool, skill or MCP server once per profile,
from every session that profile's settings govern, so it never offers to
turn off something the same report shows in use.

## Plan

Found 2026-11-14 on the maintainer's host (99,739 requests over 4 days,
`~/.claude` shared by native Claude Code and T3's `claudeAgent`). For
`[~/.claude]` the Measured section printed the Agent tool three times:
"Never used", $0.76; "Used 376 times", $0.06; and `[T3 claudeAgent]`
"Used 376 times", $28. Grep and Glob also appeared twice for `[~/.claude]`
with different costs. "Changes to make" then offered
`"permissions": {"deny": ["Agent"]}` in `~/.claude/settings.json`, which
would turn off subagents for T3 as well. Find the root cause, such as
subagent transcripts or a second scan of the same profile forming their own
row; do not filter duplicates at render time.

A change to a settings file goes by the uses of every profile row that file
governs: one offered for a file that another row shows in use is a bug,
covered by a synthetic test with a shared profile.

## Related

- [Cut doctor's default report to one line each](/quest/m0/report-length.md) - also changes these rows
- [Prove nonuse through unparseable lines](/quest/m0/unreadable-nonuse.md) - also decides never-used rows
