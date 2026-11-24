# [M] Count sessions, requests, tokens and spend while reading

## Goal

While a terminal run reads transcripts, the progress line counts up the
sessions, requests, tokens and API-price spend read so far beside the
reading bar, and keeps the counts on the line through the phases that follow
reading. In `doctor`, the counts the line shows when it clears are exactly
the report header's.

## Plan

Settled in the [epic](/quest/m1/live/README.md): the counts end on the
header's numbers, and each SSH host's totals join the line when its result
arrives.

Recommendations:

- Some figures only settle after the read loop: a Codex response near
  `--since` can link to a request outside the window and drop out, T3 can
  move a shared request to another profile, which changes the group spend
  is extrapolated in, and SSH hosts reply late. So count while reading as
  the header would, then replace the counts with the settled ones once
  reconciliation and pricing finish, and with each host's `Totals`
  (`src/totals.rs`, which already carries requests, tokens, sessions and
  spend) as it arrives. The line before it clears must match the header.
- Count as the header counts: requests de-duplicated across profiles that
  share a history (native Claude Code and T3's `claudeAgent` both read
  `~/.claude`) and across streamed chunks of one response, only those in the
  window, and sessions with each subagent counted in the session that spawned
  it.
- Spend follows the header's rule: priced requests at public API prices,
  extrapolated per profile over the unpriced ones and marked `~`. Prices load
  alongside the read; until they arrive the line shows the counts without
  spend, then prices everything read so far.
- Tokens are as `Totals::tokens` counts them, cache reads and writes
  included, with a short suffix such as `1.9B`.
- `vibes` shares the line but its SSH replies (`Outcome::Vibes` in
  `src/wire.rs`) carry no usage, so under `--ssh` it shows this host's
  counts, labelled as this host's, rather than add usage to its wire.
  `vibes` prints no usage header, so its last counts are checked against the
  local scan's settled totals instead.
- `Reading` in `src/reader.rs` already carries the byte counters to the
  `Meter` thread in `src/cli.rs`; the counts can travel the same way. The
  line must still fit one row: drop the least useful fields first on a narrow
  terminal, as the bar's track already narrows.
- Unit tests build the line from fixed counts and widths, with time mocked
  as the existing bar tests do. Validate on a real host that the last live
  counts match the header.
