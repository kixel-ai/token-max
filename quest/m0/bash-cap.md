# [S] Bash cap check

## Goal

`token-max` flags Claude Code's `BASH_MAX_TIMEOUT_MS` when it is at or above
the cache TTL, since a foreground wait that long lets the cache expire, and
reports Bash calls that hit the cap or poll in loops, with what their cache
misses cost.

## Plan

`BASH_MAX_TIMEOUT_MS` caps only the timeout the model sets on one
foreground Bash call (default 600000). Background commands have their own
limit, and the user's own processes are unaffected. Sub-agents use the same
cap; Claude Code's sub-agent stall timer does not count time spent waiting
on a tool call. The maintainer's host sets 3300000, 55 minutes under the 1h
TTL.

Codex's `background_terminal_max_timeout` (default 5 minutes) bounds one
poll of a background terminal. Each poll reads the cached context, which is
cheaper than a miss, so the check does not recommend raising it.
