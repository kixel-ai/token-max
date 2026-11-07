# [M] Background-generation check

## Goal

`token-max` reports the cost of Claude Code prompt suggestions, session
recaps, and goal check-ins, and Codex automatic recaps, where attribution
supports it. It recommends changes from estimated net costs when possible,
or gives provider-guidance warnings with unknown dollars. Each feature is
reported separately, with its effective setting, owner, and coverage.

## Plan

Decided 2026-11-06 (maintainer, /quest-plan): ✅ include in m0 (rejected:
m1). The maintainer asked whether disabling background generation matters
in their own transcripts and wanted a dollar amount if computable. Do not
turn missing attribution into a zero-cost result or a made-up saving.

The metadata-only host survey over 2026-10-30 through 11-06 local time found
no separately attributed recap/suggestion usage in the inspected Claude
main-session or Codex records. Some malformed historical records were
excluded, so this was feasibility research, not a complete billing audit.
Future versions or other hosts may expose more; validate actual record
shapes before adding synthetic fixtures.

- Resolve Claude `promptSuggestionEnabled`, `awaySummaryEnabled`,
  `CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION`,
  `CLAUDE_CODE_ENABLE_AWAY_SUMMARY`, and
  `CLAUDE_CODE_GOAL_CHECKIN_MINUTES`; Codex exposes `tui.auto_recap`.
  Respect documented client/provider/version conditions. Current settings
  do not establish that a feature fired historically.
- Match explicit source/trigger metadata to deduplicated request usage.
  Charge input, cached input/writes, and output once at the actual model
  and tier. Token counts without a feature identity remain unattributed;
  idle gaps, terminal focus guesses, and short replies are not proof of a
  recap or suggestion request.
- If costs can be estimated from verified activation/context evidence,
  state the model and assumptions and show sensitivity. Otherwise show
  unknown dollars. Neither a speculative per-idle-period charge nor all
  unexplained spend is a measured feature cost.
- Compare the lower aggregate estimated net cost, including observable
  follow-on work and any cache warming lost when requests disappear. Goal
  check-ins may advance work; suggestion acceptance or whether a person
  read a recap may be unobservable. Attributed spend alone does not prove
  a feature should be disabled. With no priced comparison, give a
  conditional suggestion to disable unused UI generation and explain the
  tradeoff; do not infer that the person never uses it.
- Idle compaction belongs to its existing check. Do not charge it here or
  conflate goal check-ins with scheduled tasks, explicit user requests,
  guardian approval reviews, or every subagent request.

Sources checked 2026-11-06: [Claude background token
usage](https://code.claude.com/docs/en/costs#background-token-usage),
[recap and suggestion settings](https://code.claude.com/docs/en/settings-reference),
[goal check-in control](https://code.claude.com/docs/en/env-vars), and
[Codex recap setting](https://learn.chatgpt.com/docs/config-file/config-reference).

## Required

- [Replay and the cache TTL check](/quest/m0/replay.md) - source attribution, request costs, and coverage-aware reporting

## Related

- [Idle compaction check](/quest/m0/idle-compaction.md) - owns idle-summary costs and the on/off comparison
