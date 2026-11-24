# [M] Count sessions, requests, tokens and spend while reading

## Goal

While a terminal run reads transcripts, the progress line counts up the
sessions, requests, tokens and API-price spend read so far beside the
reading bar, and keeps the final counts on the line through the phases that
follow reading. When reading ends, the counts are exactly the ones the
report's header gives.

## Plan

Settled in the [epic](/quest/m1/live/README.md): the counts end on the
header's numbers, and each SSH host's totals join the line when its result
arrives.

Recommendations:

- Count as the header counts: requests de-duplicated across profiles that
  share a history (native Claude Code and T3's `claudeAgent` both read
  `~/.claude`) and across streamed chunks of one response, only those in the
  window, and sessions with each subagent counted in the session that spawned
  it. If a count can only be settled after reading, settle the live one the
  same way rather than show a number that later drops.
- Spend follows the header's rule: priced requests at public API prices,
  extrapolated per profile over the unpriced ones and marked `~`. Prices load
  alongside the read; until they arrive the line shows the counts without
  spend, then prices everything read so far.
- Tokens are every token billed (input, cache writes, cache reads and
  output), with a short suffix such as `1.9B`.
- `Reading` in `src/reader.rs` already carries the byte counters to the
  `Meter` thread in `src/cli.rs`; the counts can travel the same way. The
  line must still fit one row: drop the least useful fields first on a narrow
  terminal, as the bar's track already narrows.
- Unit tests build the line from fixed counts and widths, with time mocked
  as the existing bar tests do. Validate on a real host that the last live
  counts match the header.
