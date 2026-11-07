# [L] Replay and the cache TTL check

## Goal

Finish the Node-compatible TypeScript replay core and Claude main/outside-main
cache-TTL check, preserving local read-only processing, safe diagnostics, explicit
unknowns, and aggregate estimated net costs. Complete the independent review and
report the audit-window validation result without inventing a complete host total.

## Plan

The implementation in [kixelated/token-max#3](https://github.com/kixelated/token-max/pull/3)
replaces the Rust stub with TypeScript/Bun tooling and a Node-compatible build,
streamed frozen snapshots, Claude/Codex parsers, public-price adapters, structural
metadata, minimal text/JSON output, and the TTL check. Billing rules, assumptions,
and metadata limits are documented in `docs/semantics.md`. The rest of m0 remains
in its existing quests; this quest does not publish, change settings, or add SSH.

Settled 2026-11-07: the maintainer chose option 1 in
[the Design change decision](https://github.com/kixelated/token-max/pull/3#issuecomment-6709040500).
Estimate unlinked API attempts where recorded evidence supports an estimate, show
provenance and sensitivity, and keep unsupported cases and their affected
comparisons unpriced. Never exclude attempts or treat missing usage as zero.
Unknown fixed costs that demonstrably cancel do not block a known difference.
The aggregate estimated net winner decides; sensitivity is not an all-scenarios
veto. This resolves the earlier Waiting item.

Remaining work:

- Resolve the pre-window unknown cache-chain finding after the required design
  checkpoint. An unsupported attempt before a default window currently disappears
  from the replay chain when its speed is unknown, or clears the prior state when
  speed is recorded. Both paths can produce a priced recommendation without
  provenance or sensitivity. The synthetic case in the PR gives opposite winners
  from that metadata difference. Carry unsupported predecessor evidence into an
  affected comparison rather than silently choosing a cache history; address the
  related ambiguity between measured null-speed and recorded-speed requests.
- Complete a new independent review after the checkpoint and fix. The third round
  after the maintainer Design change scored 8/10 at
  [c6c8252](https://github.com/kixelated/token-max/commit/c6c8252).
  Every earlier fix was verified, including the growing-retry write correction,
  client provenance, pre-window estimated-seed sensitivity, fixed-cost cancellation,
  empty iterations, geography, diagnostics, managed ownership and discovery.
  Full checks and 46 synthetic tests pass on Bun and Node 22/24. Keep each recheck
  in a fresh delegate and leave the PR draft until all findings settle.
- Preserve the audit limitation. The Claude-only `--since 2026-10-30` attempt
  fails safely with zero report bytes at complete malformed record
  `agent-aac1432197b4da0d2.jsonl:194`; several histories contain other malformed
  records. The exploratory audit quoted 1h main/subagent
  $5,494/$1,906 and 5m $8,869/$2,382 for 10-30 through 11-05. No retained script
  establishes its coverage, and a current open-ended scan is not that same frozen
  window. Existing malformed complete Claude and Codex records can prevent a
  complete scan. Preserve strict failure and empty success output; do not skip,
  modify transcripts, or present a selected subset as a complete billing audit.
- Once complete, delete this quest and its links with `quest rm`, run full checks,
  and finish `/quest-review`. Use the plan PR as the implementation base while it
  remains open; merge updated main after it lands, without rebasing.

## Waiting

- Maintainer: choose the implementation approach at the required third-round
  design checkpoint. `quest context` requires stopping before fixes when every
  third review round reports findings, so an agent cannot choose this checkpoint
  itself. The estimation policy above is settled. Recommend rethinking cache-chain
  tracking as explicit known/estimated/unknown state with predecessor provenance,
  carrying missing-speed ambiguity across possible cohorts and the window boundary.
  This addresses the shared class of evidence disappearing at replay transitions.
  Alternatively, continue the current map with a targeted unknown/taint flag
  (smaller patch, more implicit transitions), or drop replay (not recommended;
  its goal remains useful and the rest is implemented). Record the chosen approach
  in a Design change comment, then start a new review session after implementation.
