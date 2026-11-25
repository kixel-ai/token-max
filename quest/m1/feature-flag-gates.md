# [S] One rule for Claude Code's provider and feature-flag variables

## Goal

Every line that asks whether Claude Code fetches feature flags or which cloud
provider it sends requests to reads the variables the same way. The advisor
(`src/advisor.rs`, from
[#217](https://github.com/kixel-ai/token-max/pull/217)) counts
`DO_NOT_TRACK`, `CLAUDE_CODE_DISABLE_FEATURE_FLAGS` and `DISABLE_GROWTHBOOK`,
the Mantle endpoint and any true value; the background features' gate
(`src/controls.rs`), `effort::claude_provider` and `CLAUDE_PROVIDERS` in
`src/discovery.rs` check fewer variables and only `"1"`. So with
`DO_NOT_TRACK=1` the advisor card says the flag fetch is off while prompt
suggestions are priced as running, and `CLAUDE_CODE_USE_BEDROCK=true` is
Bedrock on the card but the Anthropic API in pricing.

## Plan

Recommendation: one shared predicate and variable list in `src/controls.rs`
(beside `env_flag`), checked against Claude Code's
[env-vars page](https://code.claude.com/docs/en/env-vars) and used by all four.
