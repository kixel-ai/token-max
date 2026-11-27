# [S] Price Claude advisor iterations under the request's tier

## Goal

token-max prices every Claude Code advisor iteration at the advisor model's
list rates under the request's effective service tier. An advised session,
such as kixel-ai/quest's advisor A/B arm B, then reports its full cost
instead of a $0 lower bound marked `complete: false`.

Non-goals: how non-advisor parts are priced, and Priority Tier commitments
on the advisor model alone.

## Plan

Decided 2026-11-26 (maintainer, /quest-plan in kixel-ai/quest's /quest-run,
kept from [kixel-ai/quest#934](https://github.com/kixel-ai/quest/pull/934)'s
follow-ups):

- ✅ Plan it in m0 (rejected: m1; drop). Correct API-price replays are m0's
  goal, and quest's advisor trial readout (2026-12-04) reads these costs.
- ✅ When a request records no service tier anywhere, price its advisor
  iteration at standard only when the request ran on a provider Anthropic
  runs and nothing on it shows Priority Tier; otherwise keep it unpriced
  (rejected: always unpriced, which leaves gateway sessions at $0; always
  standard, which guesses for other providers).

Evidence: an advisor iteration records no `service_tier` of its own.
`service_tier` in `src/prices.rs` then borrows the executor iterations'
tier and returns `an advisor call's service tier is not recorded` unless
they all show `default`. On #934's probe sessions through the maintainer's
subscription gateway (Claude Code 2.1.330, Sonnet high with an Opus
advisor, and Opus high with a Fable advisor), `token-max sessions` reported
both advisors' tokens at $0, while Claude Code's own `modelUsage` priced
the Opus advisor at $0.12. That fallback came from
[kixel-ai/token-max#258](https://github.com/kixel-ai/token-max/pull/258);
list it under `## Caused by` if the reproduction confirms it.

Recommended, for the implementer to confirm:

- Reproduce first on a recorded advisor transcript, and find which tier
  evidence the request holds: the executor iterations', the message's
  top-level `usage.service_tier`, or none.
- Take the advisor iteration's tier from the executor iterations, else from
  the request's top-level tier, then apply the no-tier decision above.
- Tests: a fixture shaped like the probe transcript prices its advisor
  iteration at the advisor model's standard rate; a request with Priority
  evidence prices it under that; a tierless request on a provider Anthropic
  doesn't run stays unpriced.
