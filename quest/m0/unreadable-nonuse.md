# [S] Prove nonuse through unparseable lines

## Goal

A tool, skill or MCP server with no recorded use stays red (exit 1) when a
profile's transcripts have unparseable lines, unless one of those lines
could hold a call to it.

## Plan

Decided 2026-11-14 (maintainer, /quest-plan after a local doctor run):
✅ search each unparseable line's raw bytes for the feature's name; only a
line that contains it makes the verdict yellow (rejected: keep any
unparseable line making every never-used feature yellow; red under a
fraction of unparseable lines, an arbitrary cutoff). On the maintainer's
host, 12 unparseable lines in 10 files of about 100k requests made all 13
never-used tools yellow, so the run said "No setting changes recommended"
and exited 0.

This refines #64's rule, "a recurring cost with no use across a fully
visible window stays red": a line that cannot hold the name hides no use of
it. Keep the bytes only long enough to search them, as with prompt and
reply text, which are never printed or kept. Match the name as a call
records it, such as `mcp__server__tool` or a skill's name, so a mention in
prose counts as a possible use rather than none. Update the reason line,
`docs/reference.md`'s "Exit codes and unreadable lines" and
`docs/semantics.md` to match.

## Related

- [Give each profile one row per tool](/quest/m0/profile-rows.md) - also decides never-used rows
