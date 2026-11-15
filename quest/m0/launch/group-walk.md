# [M] Group the walk's small changes by kind

## Goal

The walk offers its changes under $5/mo in groups of one kind and reason,
such as "never-used skills → typed only" or "MCP servers over $1 per use →
off", one question each that names every member and gives the group's
combined saving in dollars and percent. A group saving under $1/mo in total
is not asked; one dim line says how many changes were skipped and that the
list after the walk shows them. Changes of $5/mo or more stay one question
each, and the list keeps one line per row.

## Plan

Decided 2026-11-16 (maintainer, /quest-plan after running the merged walk
on macOS, whose single "take all?" question listed 72 changes one per line,
most saving under $0.01/mo: "Can we group these instead of costing per
skill? ... who cares about <0.01 month stuff?"):

- ✅ group by kind and reason, such as never-used skills apart from skills
  over `--max-cost-per-use` (rejected: kind only, where one yes would cover
  features in use).
- ✅ skip a group saving under $1/mo in total, with one dim line such as
  "41 more changes save under $1/mo together; they're in the list below"
  (rejected: say nothing).
- ✅ keep y/n/o, where o goes through the group's members one by one
  (rejected: y/n only).
- ✅ the list after the walk stays one line per row, and each group
  question names all of its members (rejected: grouped list rows, which
  the maintainer first picked and then withdrew: "don't group the list,
  name all of the skills/mcps we want to disable"). AGENTS.md's "every
  setting and feature on its own line" stands.

The headline counts what the walk asks, replacing the counter wording in
[Make doctor's output pretty](/quest/m0/launch/style.md).

## Related

- [Make doctor's output pretty](/quest/m0/launch/style.md) - the same walk's look
- [Offer to remove unused MCP servers](/quest/m0/launch/remove-mcp.md) - adds a question to the MCP server groups
