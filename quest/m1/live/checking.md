# [M] Tally the savings while checking settings

## Goal

While a terminal `doctor` run checks settings, the progress line shows how
many checks are done and tallies two figures as each lands: what the current
settings already save against the defaults, and what the changes found so
far would save. When checking ends, the tallies are the report's "Compared
with defaults" figure and the total its headline gives for every change.

## Plan

Settled in the [epic](/quest/m1/live/README.md): the tallies end on the
report's own figures, and each SSH host's figures join the line when its
result arrives. `vibes` shows no tally, since its advice is unmeasured.

Recommendations:

- Reuse the figures that already exist rather than a second calculation:
  every contribution to the credit in `src/defaults.rs`, including the
  context rows for a tool, skill or MCP server already turned off, not only
  `defaults::check`'s per-check baselines; and the all-changes total
  `src/doctor.rs` puts under the report's headline.
- Keep their structure: "up to" savings and costs, estimated features and
  unmeasured settings stay apart, as the credit line keeps them. When a
  qualifier won't fit the row, drop that figure rather than flatten it into
  one `~` sum. The tally need not repeat the overlap caveat on one row.
- `analyze` in `src/replay.rs` runs its checks in sequence, but each line's
  default baseline is computed later, while the report is formatted. Compute
  each figure once, where the tally and the report can both read it, rather
  than format the report twice.
- Join each SSH host's figures from the `Totals` its reply already carries
  (`src/totals.rs`), whose offered savings and credit sum across hosts
  before rounding, rather than add fields to the wire.
- Something like `Checking settings ⠋ 7/15  saving ~$945/mo vs defaults,
  found ~$310/mo more`, fitted to the row as the reading line is.
- Validate on a real host that the final tallies match the report's lines.
