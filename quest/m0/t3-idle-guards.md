# [L] Rebuild historical T3 idle guards

## Goal

T3's `beforeCacheExpires` idle mode is priced for past idle gaps, not just
the current state, so the T3 idle check can compare all three modes.

## Plan

Decided 2026-11-11 (maintainer, /quest-complete planning the follow-ups of
[kixelated/token-max#50](https://github.com/kixelated/token-max/pull/50)):
✅ plan it in m0 (rejected: m1, which would leave `beforeCacheExpires`
advisory in 0.1.0; drop).

- T3 projects only the current lifecycle guards, so every gap before a
  later run in the same thread stays unpriced for `beforeCacheExpires`.
- Rebuild each gap's guards from T3's `orchestration_events`: thread
  archive, settle and snooze events, pending runtime requests and
  merge-backs, as of the gap.

Found 2026-11-11 while starting the quest
([kixelated/token-max#68](https://github.com/kixelated/token-max/pull/68)):
T3 prunes its event log, so the plan cannot reach the Goal for most histories.

- At the pinned revision, [`pruneEventStore`](https://github.com/pingdotgg/t3code/blob/c4ce7b0fd74f6beb37ed6034f5c8416e5aa7e165/apps/server/src/orchestration-v2/ProjectionMaintenance.ts)
  runs every six hours and keeps at least one day of events. Older ones
  survive only as `thread.created`, `run.updated` with status `running`, and
  each stream's newest event; the boundary is the `event-log-pruned-through`
  row of `orchestration_v2_projection_metadata`.
- Events are full snapshots of one entity, so a gap after the boundary is
  rebuildable once every guard entity has a retained event at or before it,
  or none after the boundary at all (its projection row is then unchanged).
  Gaps before the boundary are not.
- Settling is the guard that matters and has no other record: a new message
  [unsettles a thread](https://github.com/pingdotgg/t3code/blob/c4ce7b0fd74f6beb37ed6034f5c8416e5aa7e165/apps/server/src/orchestration-v2/Orchestrator.ts),
  and [auto-settle](https://github.com/pingdotgg/t3code/blob/c4ce7b0fd74f6beb37ed6034f5c8416e5aa7e165/apps/server/src/orchestration-v2/ThreadSettlementService.ts)
  settles on merge or after days, so settled spells between two runs are
  common and the current row cannot stand in for them.
- The rebuild must mirror [`idleCompactionWindow`](https://github.com/pingdotgg/t3code/blob/c4ce7b0fd74f6beb37ed6034f5c8416e5aa7e165/apps/server/src/orchestration-v2/IdleCompaction.ts)
  at both points the [service](https://github.com/pingdotgg/t3code/blob/c4ce7b0fd74f6beb37ed6034f5c8416e5aa7e165/apps/server/src/orchestration-v2/IdleCompactionService.ts)
  checks it: each `run.updated` completed event that arms the timer, and the
  fire time two minutes before expiry. Beyond the guards listed above, that
  covers the latest started run, the active Claude provider thread and
  instance, the latest usage, snooze expiry without an event, and
  [pending background work](https://github.com/pingdotgg/t3code/blob/c4ce7b0fd74f6beb37ed6034f5c8416e5aa7e165/packages/shared/src/orchestrationV2PendingBackgroundWork.ts)
  from provider-thread tasks and active background turn items.
- Turn-item event payloads hold tool output, observed up to 640 KB, so
  reading their fields needs an `octet_length` guard that leaves an
  oversized item unknown rather than failing the whole read on the 1 MiB
  value bound.

## Waiting

- Maintainer: decide the scope now that T3 keeps about one day of events;
  done once a `Decided` line in the Plan records the pick.
  1. Recommended: rebuild guards after the prune boundary and limit the T3
     idle check's window to it, so all three modes are priced on the same
     retained day or more and extrapolated monthly. `off` and `onReturn`
     lose their longer comparison window.
  2. Rebuild after the boundary only, keeping the full-history window:
     older gaps stay unpriced with a pruning reason, so `beforeCacheExpires`
     stays unpriced for any history older than about a day, nearly always.
  3. Ask T3 upstream to keep the lifecycle events the predicate reads; this
     posts outside the `kixelated`, `kixel-ai` and `moq-dev` orgs, so it needs approval
     and t3code's AI policy check, and old history stays pruned.
  4. Drop the quest: `beforeCacheExpires` stays advisory, and its unpriced
     reason names T3's event pruning.
