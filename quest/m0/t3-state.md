# [M] Read real T3 state

## Goal

On a host whose T3 `statev2.sqlite` is tens of gigabytes, `token-max` reads
the thread metadata it needs within a budget that does not grow with the
database, so the T3 idle-compaction and fast-mode lines carry evidence
instead of "unknown". When the read still fails, the report names the cause:
schema, budget, or access.

## Plan

Measured 2026-11-09 on the maintainer's host (T3 0.1.7): `statev2.sqlite`
is 20 GB with a 15 MB WAL. Every run reports `T3 state: ... unavailable:
incompatible/malformed schema, metadata resource bound, or read-only
database/WAL access failed`. As a result, `T3 idle compaction` (all three
Claude instances, `idleCompaction = onReturn`) and `Fast mode` (every T3
instance) say their current selection or lifecycle evidence is unavailable.

The reader in `src/t3.rs` maps every error to that one message. It caps each
snapshot at 10,000 progress callbacks of 1,000 SQLite VM steps, which is the
likely cause on this database, but the run does not say. Find the actual
cause first, then make the queries use T3's indexes or narrow them to the
selected requests' threads, so the cost follows the selected history, not
the database.

Settled: T3 metadata stays optional, and a failed read stays a named
evidence gap that keeps native usage, never a failed run (2026-11-07
decision in the m0 README).

Decided 2026-11-09 (maintainer, /quest-plan, asking which T3 Code settings to
audit): ✅ plan this quest; ✅ text-generation model as its own
quest, since reported (rejected: the T3
`contextWindow` option, which T3 offers only for older 4.6 Claude profiles and
hides for Claude, while the per-instance auto-compact window check covers
current models; auto-resume of limited or interrupted threads, which only
continues work the user would do anyway). ✅ m0 (rejected: m1).
