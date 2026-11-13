# [M] Check each profile's default effort

## Goal

`token-max doctor` gives each profile's main-session reasoning effort its
own line: what each model and effort pair cost over the window, how often
it ran, the current default (Claude Code's `effortLevel` or
`CLAUDE_CODE_EFFORT_LEVEL`, Codex's `model_reasoning_effort`), and the
setting that changes it.

- A `max` default is a change: doctor offers to set it to `xhigh`, and the
  report exits nonzero. Its evidence cites Anthropic's
  [model guide](https://claude.dev/blog/which-claude-model-should-you-build-with/):
  past `high`, test the next model up at a lower effort.
- A `low` default is worth considering raising, under Opinion.
- An `xhigh` default cites the same guidance under Official, with no
  verdict.

The effort's cost effect is never repriced, since a different effort
changes the output tokens; the line says so.

## Plan

Decided 2026-11-13 (maintainer, /quest-plan after reading Anthropic's
model guide and Theo Browne's
[Opus against Fable video](https://www.youtube.com/watch?v=XtykeVoSmP8)):

- Scope? ✅ the main session's default effort (rejected: none). This amends
  m0's 2026-11-06 exclusion of model and effort selection for the default
  effort only; recommending a model stays out, and the
  [subagent lines](/quest/m0/subagent-guidance.md) stay evidence.
- `max`? ✅ a change to `xhigh`. Anthropic's guide never recommends it, and
  Theo finds `max` forces long thinking on simple tasks and Fable 5.2
  scores worse at `max` than at `xhigh`.
- `low`? ✅ an Opinion, worth considering (rejected: no line). Theo calls
  Opus at low "garbage"; Anthropic measured it about 8 points below `high`
  on SWE-bench Pro for about a third of the cost.
- `xhigh`? ✅ cited under Official with no verdict, as part of the goal the
  maintainer confirmed (rejected: worth considering), since token-max
  cannot measure what a lower effort would have changed.
- Milestone? ✅ m0 (rejected: m1).

Agent's choices, for the implementer to revisit:

- Read each request's recorded effort (`output_config.effort` for Claude,
  already parsed in `src/sessions.rs`) and Codex's turn context. Apply the
  same rules to Codex where it offers `max`.
- Resolve the current default per profile the way the other checks do,
  including a project setting or environment variable that would override
  the user file, listed to change by hand.

## Related

- [Subscription framing](/quest/m0/subscriptions.md) - the wording this
  line's evidence follows
