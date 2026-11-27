# [S] Price OpenCode and Gemini models that carry models.dev's legacy context field

## Goal

An OpenCode or Antigravity request on a model whose models.dev entry has both
a context `tiers` list and the older `context_over_200k` field, such as
`opencode/claude-sonnet-4-5`, `opencode-go/grok-4.7` or
`google/gemini-3.1-pro-preview`, is priced at the catalog's tier rates. Today
every such request is unpriced, whatever its size, with "legacy context
threshold is undocumented for this model".

## Plan

Found by the m2 tests audit. `prices::selected_rates` refuses any model whose
entry sets `context_over_200k` unless `prices::threshold` documents the
threshold for that provider and model, and `threshold` knows only `openai`
and `anthropic`. On 2026-11-27 models.dev lists the legacy field on 19
`opencode`, 5 `opencode-go` and 6 `google` models, each beside a `tiers` entry
that names its threshold. A scratch test confirmed the refusal for
`opencode/claude-sonnet-4-5` at 1,000 input tokens.

Recommended: require a documented threshold only when the legacy field is the
model's only context pricing. With `tiers` present, price by them, as
[the epic's](/quest/m1/harnesses/README.md) decision to price OpenCode
through models.dev by its recorded provider and model says. Keep the check
that a tier contradicting a documented threshold leaves the request unpriced.
Push the "legacy `context_over_200k` resolved at documented 272000" assumption
only when the legacy rates were the ones chosen, and name the documented
threshold rather than 272000, which is wrong for `claude-haiku-5-5`'s 100k.

Test with a catalog entry shaped like models.dev's
`opencode/claude-sonnet-4-5` (base rates, one 200k tier, and the legacy
field): a request at 200,000 input tokens prices at the base rates, one above
at the tier's.
