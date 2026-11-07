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

- Implement evidence-backed input estimates and explicit output/fee cancellation,
  with synthetic coverage of supported, unsupported, and mixed cases. Keep original
  usage and historical spend unknown; estimate only the comparison inputs supported
  by the recorded evidence. Show estimate provenance and sensitivity separately.
- Recheck the committed implementation independently against
  the reviewed plan branch. Round 2 verified all six round-1 fixes but found valid
  empty iteration lists, unavailable-geography sentinels, and diagnostic issues.
  Those fixes pass local checks; their committed head still needs the next
  independent review in a new session after the recorded design change. Keep its review record exact and its
  draft state until the remaining findings settle.
- Attempt the Claude-only `--since 2026-10-30` audit window and report either the
  result or a safe exact blocker. The exploratory audit quoted 1h main/subagent
  $5,494/$1,906 and 5m $8,869/$2,382 for 10-30 through 11-05. No retained script
  establishes its coverage, and a current open-ended scan is not that same frozen
  window. Existing malformed complete Claude and Codex records can prevent a
  complete scan. Preserve strict failure and empty success output; do not skip,
  modify transcripts, or present a selected subset as a complete billing audit.
- Once complete, delete this quest and its links with `quest rm`, run full checks,
  and finish `/quest-review`. Use the plan PR as the implementation base while it
  remains open; merge updated main after it lands, without rebasing.
