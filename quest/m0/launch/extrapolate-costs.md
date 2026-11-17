# [M] Mark extrapolated feature and subagent costs with ~ and a margin

## Goal

Every cost a line shows over requests that were only partly priced reads the
way a replay's saving does since
[#143](https://github.com/kixel-ai/token-max/pull/143): extrapolated to all
of its requests, prefixed with `~`, and followed by its 95% margin of error,
such as "costs ~$2,880 ± $40". That covers what a tool, skill or MCP server
costs, the subagent, effort and Fable weekly lines, "System prompt and core
tools", and Auto memory over requests that recorded no context. No such line
reads "at least" because some of its requests could not be priced or record
their context; one with no priced requests stays unknown.

## Plan

Found while extrapolating the replays (#143), on the desktop run the
maintainer complained about: the replays and the headline now read
"~$70,050 ± $209", while the general-purpose subagent line still reads
"costs at least $2,856 (47,245 of 47,300 requests priced)", and a skill's
cost is already scaled from its priced requests, yet shows no `~`, so the
report marks the same kind of figure two ways.

Recommended:

- Reuse `prices::Sample` and `report::approx`: accumulate each row's
  per-request cost and its square where context costs are summed
  (`context.rs`, `row.cost.priced`), and in the subagent, effort and Fable
  sums, and extrapolate with `Sample::estimate`.
- A parent row's own cost is today its total minus its children's, then
  restored; give it the margin of its own total rather than combining the
  children's, since they come from the same requests.

Decided 2026-11-17 (maintainer, /quest-run on #143): ✅ extrapolate Auto
memory's "at least" too, assuming requests that recorded no context loaded
memory as often as those that did (rejected: keep "at least").
