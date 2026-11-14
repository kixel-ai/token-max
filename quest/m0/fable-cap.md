# [S] Show Fable's weekly use beside the plan cap

## Goal

`token-max doctor` shows, under Official, each Claude profile's Fable
spend at API prices in each week of the window, beside Anthropic's
documented plan rule: on Max and Ultra, Fable models can use up to half of
the weekly limit, and use that limit faster than other models; on Pro,
they run on usage credits. No percentage of the limit and no verdict, since
no limit is published in tokens or dollars.

The line states beside the figures that they are partial evidence: the cap
counts Fable use on every Claude surface, while the figures cover only this
profile's recorded sessions, and they may include requests paid with usage
credits after the cap was reached.

## Plan

Decided 2026-11-13 (maintainer, /quest-plan after Theo Browne's
[Opus against Fable video](https://www.youtube.com/watch?v=XtykeVoSmP8),
which estimates about $3k a month of Fable against $14k of Opus on the
$200 plan):

- Show it? ✅ yes, in m0 (rejected: m1), as part of the subscription
  framing, whose wording `docs/reference.md`'s "Plans and API prices"
  gives: dollars are API prices, and a provider's rule for a plan is an
  `Official:` sentence with its source.
- Shape? ✅ weekly API-price spend beside the documented cap (rejected:
  Fable's share of weekly spend, which understates its share of the limit
  because Fable draws faster; detecting the plan, which the maintainer
  rejected).

Sources, read 2026-11-13: Anthropic's [pricing](https://claude.com/pricing)
and the help center's
[Fable models on your plan](https://support.claude.com/en/articles/15424964-claude-fable-models-on-your-plan).
Check both again when implementing, since plan terms change.
