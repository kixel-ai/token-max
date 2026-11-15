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

The search proves nonuse only in bytes that were written. A crash can cut a
record off, and the reader skips it as unreadable once a later write ends
its line, often with the next record joined on. Its missing bytes could
hold any call, and a cut inside a name, such as `"name":"Age`, leaves a
prefix that does not contain `Agent`. So the rule to implement is: read
each unparseable line, and a deferred unterminated final line, as the
records it holds; it makes a feature's verdict yellow when its bytes
contain the feature's name as a call records it, or when it holds a record
cut off before its JSON closes, unless the cut record's surviving bytes
rule out a call to that feature, such as a record type that cannot hold a
call, a call name that survived whole, or a cut inside a call name that
the feature's name does not start with. Otherwise the feature stays red.
This applies the decision above to cut records (found in review of the
planning PR, listed in its body for the maintainer's pick). Cover a cut
record, a cut name and a joined pair with synthetic tests.

## Related

- [Give each profile one row per tool](/quest/m0/profile-rows.md) - also decides never-used rows
