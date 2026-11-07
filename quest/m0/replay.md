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

Settled 2026-11-07: the maintainer chose B1 in
[the cache-state Design change](https://github.com/kixelated/token-max/pull/3#issuecomment-6710296074).
Replace the implicit predecessor map with explicit known, estimated and unknown
cache states and predecessor provenance. Carry unsupported pre-window attempts
and missing-speed ambiguity across every possible cohort and window boundary;
reject the narrow flag-only patch. A comparison is priced only when recorded
constraints establish its cache geometry, including when all possible histories
have the same geometry. This resolves the mandatory third-round checkpoint.

The opposite-winner case is corrected: both missing and recorded speed preserve
the unknown predecessor and leave the affected 5m comparison unpriced. New tests
cover possible cohorts, lifetime boundaries and prefix extrema, invariant geometry,
known-state recovery, zero-cache requests and copied compaction boundaries. Full
checks pass with 53 synthetic tests on Bun and Node 24; built tests also pass on
Node 22. Main through the protected squash configuration PR is merged locally.

Remaining work:

- Complete a new independent review session against main after the B1 change.
  Every earlier fix must remain verified: growing-retry writes, client provenance,
  pre-window estimated-seed sensitivity, fixed-cost cancellation, empty iterations,
  geography, diagnostics, managed ownership and discovery. Each recheck uses a new
  delegate; leave the PR draft until checks and review are clean.
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
  and finish `/quest-review`. The plan PR has merged; review against main and merge its updates without
  rebasing. Landing workflow changes belong to the separate repository setup PR.
