# [S] Apply recommended cache TTLs the same way in every replay

## Goal

Every check that replays "after the recommendations" uses the same cache
TTLs, worked out by one function.

## Plan

Four places work out the TTLs after the cache TTL check's recommendation:

- keepalive (`src/keepalive.rs`) and idle (`src/idle.rs`) start from
  `profile.ttl.*.value` and apply `recommended` only when the check's action
  is `Change`.
- T3 idle (`src/idle.rs`) starts from `.one().value` and also requires
  `Change`.
- compaction (`src/compaction.rs`) applies `recommended` whatever the action,
  including `Advisory`. `src/replay.rs` sets `Advisory` with a `recommended`
  value whenever the client version is unsupported, so compaction replays a
  TTL that the user was told not to rely on.

Replace them with one `ttls_after(profile, checks)` that applies
`recommended` only when the action is `Change`, as keepalive, idle and T3 idle
do (maintainer, 2026-11-20; rejected: apply Advisory too). Compaction's figures
then change on lines whose TTL check is Advisory, matching what `doctor` offers
to change.
