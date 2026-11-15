# [S] Prove nonuse through unparseable lines

## Goal

A tool, skill or MCP server with no recorded use stays red (exit 1) when
the transcripts its row covers have unparseable lines, unless one of those
lines could hold a call to it.

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

The search proves nonuse only in bytes that were written. A crash can cut a
record off, and the reader skips it as unreadable once a later write ends
its line, often with the next record joined on. Its missing bytes could
hold any call, and a cut inside a name, such as `"name":"Age`, leaves a
prefix that does not contain `Agent`; one record can also hold several
calls. Decode JSON string escapes such as `\u0041gent` before matching.

Decided 2026-11-14 (maintainer, at the pick of the planning PR): ✅ a cut
record counts as a possible use only when its session has a tool result
whose id matches no parsed call, Claude's `tool_use_id` or Codex's
`call_id`, since every call that ran leaves a result (rejected: any cut
record that could hold a call makes every never-used feature yellow, which
could leave the maintainer's host as it is; searching only the surviving
bytes, which can call a used tool unused). A call that never ran is no use.
This rule accepts one miss: a call cut inside its name whose client died
before writing its result counts as unused, though it may have run. The
reason line and the docs say so.
A result does not name its tool, so an unmatched result makes every
never-used feature whose row covers the line's transcript yellow, while a
name found in a line's bytes makes just that feature yellow. Rows are one
per settings file, combining every source it controls, as
[Give each setting one row per settings file](/quest/m0/file-rows.md)
decides. Take results from the same transcript file, from its unparseable lines' bytes as well as parsed records, since the
record joined onto a cut call is often that call's own result. Match ids
across the whole file, not only the `--since` window, so a call just before
the window does not leave its result unmatched.
Report in the PR how many of the maintainer's host's unparseable lines are
cut records and how many sessions have an unmatched result. Cover an escaped
name, a joined pair, and a cut record with and without an unmatched result
with synthetic tests.

## Related

- [Give each setting one row per settings file](/quest/m0/file-rows.md) - decides which transcripts a never-used row covers
