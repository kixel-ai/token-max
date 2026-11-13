# [M] Advise subscription users, measured at API prices

## Goal

`token-max doctor` speaks to Claude and ChatGPT subscribers first: its
header, the README, `--help`, and each check's evidence say what a change
means for a plan's usage limits, and show API-price dollars as the measure,
instead of disclaiming subscription bills. A fact that differs by plan is
stated conditionally, such as "on Max or Ultra", since token-max never reads
which plan a profile is signed in to.

Non-goal: modelling a plan's limits in tokens or dollars, which no provider
publishes.

## Plan

Decided 2026-11-13 (maintainer, /quest-plan after reading Anthropic's
[model guide](https://claude.dev/blog/which-claude-model-should-you-build-with/)
and Theo Browne's
[Opus against Fable video](https://www.youtube.com/watch?v=XtykeVoSmP8)):

- Audience? ✅ subscription users, with API prices as the measure shown
  (rejected: both audiences equally). This supersedes m0's "API-price
  equivalents are not a claim about subscription bills".
- Read the signed-in plan? ✅ no (rejected: reading Claude Code's
  `~/.claude.json` `oauthAccount`, whose `billingType` and
  `userRateLimitTier`, such as `default_claude_max_5x`, are undocumented,
  and Codex's plan from its auth token).
- Milestone? ✅ m0 (rejected: m1).

Agent's choices, for the implementer to revisit:

- Audit each check for what it means on a plan, citing the provider where
  it documents how usage counts against limits, such as fast mode drawing
  extra usage, and saying plainly where it does not. Where nothing is
  documented, say API price is the closest public measure of a request's
  share of the limit.
- Replace the "not your subscription bill" lines in `README.md`,
  `docs/reference.md`, `docs/semantics.md`, `src/cli.rs`, and
  `src/report.rs`, and keep the claim that dollars are API-price
  equivalents, not an invoice.
