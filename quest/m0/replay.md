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

Remaining work:

- Settle handling of API request records with no linked assistant usage, then
  implement the chosen estimate/coverage policy and synthetic tests. Current code
  keeps them unknown and blocks a priced TTL winner because their token usage can
  alter the difference. An older attempt is not evidence of zero billing. The
  independent reviewer retained this concern after the parent goal was cited.
- After the decision, recheck the committed implementation independently against
  the reviewed plan branch. Round 2 verified all six round-1 fixes but found valid
  empty iteration lists, unavailable-geography sentinels, and diagnostic issues.
  Those fixes pass local checks; their committed head still needs the next
  independent review once the decision clears. Keep its review record exact and its
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

## Waiting

- Maintainer: choose how to handle unlinked API attempts. Options: (a) estimate
  from defensible request/neighbor metadata and expose bounds and sensitivity
  alongside the aggregate winner; (b) exclude them with incomplete-coverage
  warnings; (c) retain unknown usage and withhold priced recommendations for the
  affected bucket. Recommend (a) only with an explicit evidence-backed estimator;
  age or a missing assistant alone cannot establish billing or a zero count.
  Exclusion hides unknown spend, and (c) can leave realistic windows advisory.
  The round-2 reviewer retained this policy disagreement, so the implementation
  agent cannot settle it by repeating the earlier refutation. Record the user's
  choice in the PR/plan and verify it with synthetic incomplete-usage cases.
