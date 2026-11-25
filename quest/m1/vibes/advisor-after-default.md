# [S] Rank the advisor card against a default accepted in the same walk

## Goal

When a `token-max vibes` walk accepts the `Main-session default` card's
change, the `Advisor model` card that follows ranks against the newly accepted main
default instead of the one the scan read, so a Sonnet user who takes the
Opus 5.5 default is offered a Fable advisor rather than a second Opus.

## Plan

Decided 2026-11-24 (maintainer, /quest-run): ✅ keep this quest (rejected: drop it).

The advisor card (`advisor_item` in `src/report.rs`) ranks against
`Profile.effort.model`, resolved before the walk, and every card's change is
built before the walk starts (`vibes::fixes`). Recommendations:

- Order the `Main-session default` card before the advisor card, and when its change
  is accepted, rebuild the advisor card from `advisor::pick` of the accepted
  model before asking about it; a declined change leaves the card as built.
- Off a terminal, where nothing is accepted, the card stays ranked against
  the default read, and says the default-model card would change it.
