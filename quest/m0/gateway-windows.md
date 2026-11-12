# [S] Price unset gateway windows at the declared context size

## Goal

A Claude model ID outside the documented window table, such as a gateway
alias, with `autoCompactWindow` unset and `CLAUDE_CODE_MAX_CONTEXT_TOKENS`
set gets a priced compaction-window comparison at the declared window,
labeled as an assumption, instead of staying unresolved when no compaction
was recorded. The compaction, idle and keepalive checks all use it.

## Plan

Decided 2026-11-11 (maintainer, /quest-complete planning the follow-ups of
[kixelated/token-max#66](https://github.com/kixelated/token-max/pull/66)):
✅ plan it in m0 (rejected: m1; drop). Also decided: ✅ drop #66's other
follow-up, a per-model cap inside the replay for shared window owners
(`CLAUDE_CODE_AUTO_COMPACT_WINDOW`, T3 `config.autoCompactWindow`), which
stay advisory with their stated reason (rejected: plan it in m0).

Claude Code ([model config](https://code.claude.com/docs/en/model-config#which-ids-claude_code_max_context_tokens-applies-to),
checked 2026-11-11): for a provider or custom spelling it cannot resolve to
a known model and that has no `[1m]`, "the variable applies directly and
proactive compaction continues at the declared window". With `[1m]`, the
variable applies only under `CLAUDE_CODE_DISABLE_1M_CONTEXT=1`. For an ID
that resolves to a known model, or a bare `claude-` name, it applies only
with `DISABLE_COMPACT`, which turns compaction off.

- Use the declared value as the tuned default in `controls::tune_windows`,
  after #59's precedence: cap, then observed trigger, then this declared
  window, then the documented table, else unresolved.
- Reuse #67's `Sizing::Declared(n)` owner suffix, so directories that
  declare different values never share a replayed window.
- Apply it only where the docs say the variable applies, using the same
  provider-or-custom-spelling test #66 uses for its declared-value cap. An
  ID with the variable unset stays unresolved: Claude Code assumes 200k for
  an ID it does not recognize, but token-max's model table can lag Claude
  Code's, so it cannot tell whether Claude Code recognizes the ID.
