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

The initial metadata survey covered only the default Claude profile and
mostly SDK/IDE clients; it cannot establish whether interactive background
generation is logged or what this host spends on it. Alternate profiles
hold most of its Claude data. Before relying on an attribution rule,
capture eligible interactive sessions with suggestions/recaps enabled and
disabled, including generation without a later user turn. Determine what
is recorded and validate those shapes before adding synthetic fixtures.

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
- Observed feature costs take precedence. With known eligible client and
  feature state, a documented trigger may supply the activation estimate,
  such as a suggestion request after a completed response. Use verified
  context/output proxies from controlled capture where available; state
  the deciding assumptions and sensitivity as with idle compaction. Idle
  time alone does not establish eligibility. Without estimable inputs show
  unknown dollars. A modelled trigger is not a measured feature cost.
- If calls or their cache refreshes are not logged, report that baseline
  coverage gap for every affected replay, not just this feature's total.
- Compare the lower aggregate estimated net cost, including observable
  follow-on work and any cache warming lost when requests disappear. For
  suggestions and recaps, a priced fixed-behavior net estimate decides the
  recommendation; unobservable acceptance or whether a person read a recap
  is a caveat, not a veto. Attributed gross spend alone is insufficient.
  Removing goal check-ins can remove autonomous work, so keep that
  recommendation conditional unless follow-on work supports a net
  comparison. Without a priced comparison, give a conditional suggestion
  to disable unused UI generation and explain the tradeoff; do not infer
  that the person never uses it.
- Idle compaction belongs to its existing check. Do not charge it here or
  conflate goal check-ins with scheduled tasks, explicit user requests,
  guardian approval reviews, or every subagent request.

Sources checked 2026-11-06: [Claude background token
usage](https://code.claude.com/docs/en/costs#background-token-usage),
[recap and suggestion settings](https://code.claude.com/docs/en/settings-reference),
[goal check-in control](https://code.claude.com/docs/en/env-vars), and
[Codex recap setting](https://learn.chatgpt.com/docs/config-file/config-reference).

## Related

- [Idle compaction check](/quest/m0/idle-compaction.md) - owns idle-summary costs and the on/off comparison
