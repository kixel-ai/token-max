# [M] Respect the availableModels allowlist in the model cards

## Goal

`token-max vibes` never offers a model change Claude Code's `availableModels`
allowlist would block: the `Main-session default` card's model and the
`Advisor model` card's suggestion, and a set `advisorModel` the allowlist
excludes gets a card saying the advisor is disabled, naming the setting that
excludes it. A profile with no allowlist sees no change.

## Plan

Found by CodeRabbit on [#217](https://github.com/kixel-ai/token-max/pull/217)
(Sonnet 5.5 main with a managed `availableModels: ["sonnet"]` gets an `opus`
advisor offer that Claude Code would not use). No card reads `availableModels`
today, so the main-session default card has the same gap.

Claude Code's rules ([model-config](https://code.claude.com/docs/en/model-config),
"Restrict model selection"), which the resolver must follow rather than
approximate:

- An entry is a family (`sonnet`), a version prefix that also matches later
  IDs extending it by a segment (`claude-fable-5` permits 5, 5.1 and 5.2), or a
  full model ID. A specific entry in a family disables that family's wildcard.
- A managed list alone applies and is never merged across managed sources;
  otherwise user, project and local lists concatenate. An empty list blocks
  every named selection.
- On the Anthropic API, an alias the list blocks resolves to the newest
  permitted model of its family, with a notice, so an alias suggestion may
  still run, on another version.
- A blocked `advisorModel` disables the advisor for the session.

Recommendations:

- Resolve the effective list in discovery beside effort and the advisor, from
  the same scopes, and keep where it came from for the card's text.
- Advisor card: a set `advisorModel` the list blocks becomes a blocker
  (`<owner>:availableModels excludes it`); a suggestion the list blocks is not
  offered, and the card says why.
- Main-session default card: a pick the list blocks is not offered.
- A list from a managed source token-max can't read (the admin console)
  leaves both cards advisory; say so in the text.

Decided 2026-11-25 (maintainer, /quest-run): ✅ keep it (rejected: drop
it); ✅ 0.1.0 does not wait for it (rejected: add it to `release-0.1.md`'s
Required), since an allowlist is uncommon (mostly an organization's managed
setup, though user, project and local settings can set one too), and an
`advisorModel` it blocks adds no billable calls.
