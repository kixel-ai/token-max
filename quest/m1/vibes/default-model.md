# [M] Suggest a default main-session model and effort in vibes

## Goal

`token-max vibes` suggests a main-session default of Claude Opus 5.5 at
`medium` effort for Claude Code, and Sol 6.2 at `high` for Codex, whenever a
settings file's default is neither the pick nor an accepted alternative:
Opus 5.5 at `high`, or Sol 6.2 at `xhigh`. It offers one confirmed edit to
the file that sets the default, writing the model and the effort together
when both differ, or one edit per file when two files set them. The card names the maintainer's pick as its
source, cites the provider guides the effort warning cites today, and shows
the effort line's measured cost by model and effort.

## Plan

Decision prompts, chosen options marked ✅ (maintainer, 2026-11-23):

- Item: ✅ a suggested default, first offered as a fix for effort above
  `high` (rejected for now: cache habits).
- Target: ✅ Opus 5.5 at `medium` and Sol 6.2 at `high`, the maintainer's
  own answer (rejected: `high` for both, the warning's threshold; one step
  down, as the offer [#192](https://github.com/kixel-ai/token-max/pull/192)
  removed did for `max`).
- Trigger: ✅ whenever the default model or effort differs from the pick
  (rejected: only above `high`, which never reaches a Fable or Sonnet
  default). `vibes` is opt-in advice, so a small difference costs one "n".
  The maintainer then added: don't fire for Opus 5.5 at `high` or Sol 6.2 at
  `xhigh`, which are fine defaults too.

Recommendations:

- This card replaces the effort warning that
  [the command quest](/quest/m1/vibes/command.md) moves into `vibes`, so Sol
  6.2 at `xhigh` no longer gets OpenAI's xhigh caution. That is deliberate:
  the maintainer accepted it as a fine default, and a caution against it
  would contradict that. Every other default above `high` gets this card,
  which still cites the guidance. `doctor`'s effort line stays evidence only.
- Keep the picks and accepted alternatives in one table, so changing them is
  a one-line edit.
- Confirm Sol 6.2's model ID before writing it. This host's Codex sessions
  record `gpt-6.1-sol` and `gpt-6.2-astra`, but no Sol 6.2. The Codex
  0.167.1 model catalog (`~/.codex/models_cache.json`) lists `gpt-6.2-sol`
  first, so that is the likely ID.
- Write the setting that `doctor`'s effort line already names as owning the
  default: Claude's `model` and effort setting, or Codex's `model` and
  `model_reasoning_effort`. #192 deleted the effort-target machinery
  (`EffortCheck.also`, `EffortControl.supported`); bring back only what the
  offer needs.

## Required

- [Vibes command](/quest/m1/vibes/command.md) - the `vibes` walk this card joins
