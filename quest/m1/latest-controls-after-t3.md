# [M] Resolve the latest session's Claude controls after T3 attribution

## Goal

A native Claude profile's main-session effort, auto memory and advisor are
read from the settings of the directory its own latest main-session request
ran in, never from a T3 Code thread's project that shares its history.

## Plan

`discover_claude_controls` (`src/discovery.rs`) picks the latest main-agent
request with `memory::deciding_request` before `t3::enrich` attributes
requests to T3 instances, so when native Claude Code and T3 share a home, a
T3 thread can be that latest request. Its project's settings then decide the
native profile's `effort`, `memory` and `advisor`, and `latest_model` picks the
main model the effort line and the advisor card rank against from it.
`discover_inbound` already runs after attribution for the same reason.

Recommendations:

- Move the latest-directory resolution of effort, memory and advisor into a
  pass after `t3::enrich`, as `discover_inbound` is, reading each native
  profile's attributed requests; the per-directory speed, idle and background
  controls stay where they are.
- A request T3 left unsettled between owners counts for neither, so the
  latest settled native request decides; say on the line when none does.
