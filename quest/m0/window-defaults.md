# [M] Price unset Claude windows from documented defaults

## Goal

A Claude profile that leaves `autoCompactWindow` unset gets a priced
compaction-window comparison against Claude Code's documented default for
each model, instead of an advisory "tuned model default is not established".
When the effective window is larger than a priced candidate, the report also
cites Anthropic's guidance that quality degrades as context grows, without
putting a dollar value on quality.

## Plan

Decided 2026-11-11 (maintainer, after asking whether token-max recommends
smaller auto-compact windows and whether 1M windows cost quality and money):
✅ documented defaults plus context-rot guidance in m0 (rejected: defaults
only; not planning it).

Documented defaults
([model config](https://code.claude.com/docs/en/model-config#default-auto-compact-thresholds),
checked 2026-11-11):

- Native-1M models (Fable, Sonnet 5 and later, Haiku 5.5, Opus 4.7 and
  later) compact at about 967k tokens.
- Opus 4.6 and Sonnet 4.6 without `[1m]`, any model under
  `CLAUDE_CODE_DISABLE_1M_CONTEXT=1`, and model ids Claude Code does not
  recognize compact at 200k.
- The tuned value may come from the server (2.1.288 changelog), so "about
  967k" can move without a release. Prefer the trigger observed in the
  profile's own compactions for that model when there is one, and label the
  documented value as an assumption otherwise. On the maintainer's host,
  unset-window compactions triggered at 967k-974k.
- Replace the rule in `src/controls.rs` and `docs/semantics.md` ("Unknown
  tuned defaults are not guessed") with the documented table and its source.

Guidance:

- Cite the [context windows](https://platform.claude.com/docs/en/build-with-claude/context-windows)
  doc ("As token count grows, accuracy and recall degrade, a phenomenon known
  as context rot") and the
  [compaction overview](https://platform.claude.com/docs/en/build-with-claude/compaction).
  The guidance is advisory and adds no size cutoff of its own. No public
  per-length quality results exist for the 5.x models as of 2026-11-11.
- Haiku 5.5 bills its whole request at 5x once input passes 100k
  ([pricing](https://platform.claude.com/docs/en/about-claude/pricing#long-context-pricing)).
  The existing tier repricing already covers this; check that its candidate
  includes the 100k boundary.

Not in scope: a model with no compaction sample of its own, such as Fable 5.2
on the maintainer's host before
[kixelated/dotfiles#65](https://github.com/kixelated/dotfiles/pull/65), stays
unpriced. Borrowing summary geometry from another model would be guessing.
