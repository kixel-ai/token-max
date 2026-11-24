# [M] Tally the savings while checking settings

## Goal

While a terminal run checks settings, the progress line shows how many checks
are done and tallies two sums as each lands: what the current settings
already save against the defaults, and what the changes found so far would
save. When checking ends, the tallies are exactly the report's "Compared
with defaults" figure and the total its headline gives for every change.

## Plan

Settled in the [epic](/quest/m1/live/README.md): the tallies end on the
report's own figures, and each SSH host's figures join the line when its
result arrives.

Recommendations:

- Reuse the sums that already exist rather than a second calculation: the
  credit's measured total from `src/defaults.rs`, and the all-changes total
  `src/doctor.rs` puts under the report's headline. Each sums savings that
  may overlap, as those lines say; the tally carries the same `~` and need
  not repeat the caveat on one row.
- `analyze` in `src/replay.rs` runs its checks in sequence, but each line's
  default baseline is computed later, while the report is formatted
  (`defaults::check`). Compute each figure once, where the tally and the
  report can both read it, rather than format the report twice.
- Something like `Checking settings ⠋ 7/15  saving ~$945/mo vs defaults,
  found ~$310/mo more`, fitted to the row as the reading line is.
- Validate on a real host that the final tallies match the report's lines.
