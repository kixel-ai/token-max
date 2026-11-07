# [S] Fast-mode check

## Goal

`token-max` checks Claude Code and Codex speed settings and recommends fast
mode off, naming the effective value, its owner, and the standard-speed
replacement. Where transcripts identify premium requests and prices, it
reports the premium paid over the selected window and per month.

## Plan

Decided 2026-11-06 (maintainer, /quest-plan): ✅ add the check in m0
(rejected: m1). The maintainer explicitly wants fast mode off. This is a
cost preference; faster responses may still be useful to someone else.

- Resolve Claude `fastMode`, `fastModePerSessionOptIn`, and applicable
  session/environment overrides, including `CLAUDE_CODE_DISABLE_FAST_MODE`
  and managed `fastMode: false`, and Codex `service_tier`, feature gates,
  profiles, and session overrides. Warn on an effective premium speed tier,
  including supported ultrafast modes. Do not flag a saved value that an
  effective override disables or claim an unobserved session override.
- Reprice observed premium usage at the same model's standard API rates,
  preserving tokens, cache categories, and context tiers. Use actual
  response tier/speed when present; a requested tier is not proof that it
  was billed. Distinguish observed premiums, estimates from known effective
  settings, and requests with unknown tier or price.
  Holding recorded cache categories constant is a rate-only estimate:
  Claude fast and standard modes use separate caches, so label excluded
  switching effects or replay them where cache history supports it.
- Missing historical billing metadata does not prevent the settings
  warning, but prevents an unsupported dollar claim. An unset setting uses
  that version's documented default. The observed settings window must not
  turn old premium sessions into a recommendation against a current
  standard-speed configuration.
- Label dollars as API-price equivalents, not subscription-rate-limit or
  purchased-credit multipliers. Note that this replay holds work constant
  and does not value the latency difference.

Sources checked 2026-11-06: [Claude fast
mode](https://code.claude.com/docs/en/fast-mode), [Claude
settings](https://code.claude.com/docs/en/settings-reference#fastmode),
[Codex speed controls](https://learn.chatgpt.com/docs/agent-configuration/speed),
and [OpenAI API tier billing](https://developers.openai.com/api/docs/guides/fast-mode).

## Required

- [Replay and the cache TTL check](/quest/m0/replay.md) - request usage, effective tiers, prices, and reporting
