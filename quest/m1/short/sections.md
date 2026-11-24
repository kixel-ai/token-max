# [L] Group changes into sections across profiles

## Goal

On one host, the walk and the report share one grouping: each change joins a
section by kind and reason, spanning every profile, and the section is one
row in the report and one question in the walk, such as "Never-used skills
19 → typed only  saves ~$310/mo". The main-agent and subagent cache TTLs are
one section; a setting with no kind, such as auto memory, is its own.

## Plan

Settled in the [epic](/quest/m1/short/README.md). Today the walk groups a
tool, skill or MCP server by kind and reason within one profile
(`docs/reference.md`, "A tool, skill or MCP server's change joins a group"),
and the report lists each change on its own row.

Recommendations:

- Keep the reasons apart (never used, no use recorded, over
  `--max-cost-per-use`, redundant catalog copy), so a yes to never-used
  features never removes one in use.
- `s` splits a section into its members, as it splits a group now; a member
  row names its profile when several were read. `--details` lists members
  under the section row.
- The report's section row carries the combined saving, its `~` and margin,
  and the member count; its verdict is the strongest member's.
- Keep the under-$1/mo rule, applied to the section's total.
- Update `docs/reference.md` and rerun `assets/demo/walk.tape`.

## Related

- [Hosts](/quest/m1/short/hosts.md) - extends the sections across `--ssh` hosts
