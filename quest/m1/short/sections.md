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
- A section's members share one offered effect: a skill set to typed only
  and one whose sync is turned off, which removes typed use too, are
  separate sections, so a yes never approves an effect the row didn't name.
- When the TTL section's members would get different values, the row names
  each, such as `main 5m → 1h, subagent 1h → 5m`, never one value for both.
- Keep an "up to" saving, such as an unset main-agent TTL under unknown
  billing, apart from the measured one within the TTL section's row, as the
  walk's running total already does.
- The unsplit card names a count and the few largest members, not all of
  them; `s` lists each.
- `vibes` cards that say doctor skipped a change under $1/mo, such as auto
  memory, apply the section's total, not each member's, so they never
  re-offer what the walk asked.
- `s` splits a section into its members, as it splits a group now; a member
  row names its profile when several were read. `--details` lists members
  under the section row.
- The report's section row carries the combined saving, its `~` and margin,
  and the member count; its verdict is the strongest member's.
- Keep the under-$1/mo rule, applied to the section's total.
- Update `docs/reference.md` and rerun `assets/demo/walk.tape`.

## Related

- [Hosts](/quest/m1/short/hosts.md) - extends the sections across `--ssh` hosts
