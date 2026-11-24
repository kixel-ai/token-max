# [M] Suggest an advisor model in vibes

## Goal

When a Claude Code profile has no `advisorModel`, `token-max vibes` suggests
an advisor one rank above the main-session default: Opus for a Sonnet or
Haiku main, Fable for an Opus main, and nothing for a Fable main. The card
cites Anthropic's [advisor page](https://code.claude.com/docs/en/advisor):
Claude calls the advisor at decision points, so a faster main model with a
stronger advisor "typically costs less than running the stronger model
throughout". It offers the `advisorModel` edit.

## Plan

Decided 2026-11-23 (maintainer, /quest-plan after a survey of Claude Code
2.1.322 settings worth turning on):

- Card: ✅ an advisor (rejected: see
  [the code intelligence card](/quest/m1/vibes/language-servers.md) for the
  candidates left out).
- Pairing: ✅ one rank above the main default (rejected: always Opus, which
  gives an Opus main only an Opus second opinion).

Recommendations:

- Rank against the default the profile's settings resolve, after any
  `Main-session default` change the user accepted
  in the same walk.
- Say on the card what stops it working: the advisor is experimental, not on
  Bedrock, Agent Platform or Foundry, and needs a feature flag that
  `DISABLE_TELEMETRY` or `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` blocks.
  When the profile sets one of those, say the advisor won't load rather than
  offer the edit. A Fable advisor bills to usage credits on some plans.
- `src/fable.rs` already parses advisor parts, so `doctor` can price advisor
  use once it is on; extend that line if it doesn't show advisor cost yet.
