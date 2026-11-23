# [M] Offer to run Claude's Explore subagent on Haiku in vibes

## Goal

When Claude's built-in Explore ran off Haiku, `token-max vibes` offers
Anthropic's [model guide](https://claude.dev/blog/which-claude-model-should-you-build-with/)
tip: a user agent named `Explore` with `model: haiku`. The card shows what
Explore cost and the same tokens on Haiku, as the warning does today, and
cites the guide.

## Plan

Decided 2026-11-23 (maintainer, /quest-plan): ✅ an Explore-on-Haiku item in
`vibes` (rejected for now: cache habits).

Recommendations:

- An agent file named `Explore` replaces the built-in, so it must restate the
  built-in's tool restriction and system prompt (docs/reference.md, Subagent
  models). Offer the file only if a documented source gives both. Otherwise
  the card gives the step by hand and asks nothing.
- Don't offer `CLAUDE_CODE_SUBAGENT_MODEL_FORCE`: it moves every subagent to
  Haiku, not just Explore.
- This card replaces the Explore warning that
  [the command quest](/quest/m1/vibes/command.md) moves into `vibes`.

## Required

- [Vibes command](/quest/m1/vibes/command.md) - the `vibes` walk this card joins
