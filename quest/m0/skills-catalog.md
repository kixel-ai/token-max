# [M] Codex skills catalog

## Goal

`token-max` gives each skill in a Codex catalog its own line with its
recurring listing cost, observed uses and the `config.toml` entry that turns
it off, under the same cost-per-use verdict Claude skills get. It flags a
catalog that lists the same skill more than once.

## Plan

Expanded 2026-11-06 (maintainer, /quest-plan): ✅ usage and recurring cost
for both harnesses in m0 (rejected: defer to m1). Decided 2026-11-07
(maintainer): ✅ recommend hiding a skill whose listing cost more than $1
per model call, per the [context quest](/quest/m0/context.md).

Done for Claude: listing members are priced per skill with a verdict;
`skillOverrides` applies only to local and bundled skills, synced
`anthropic-skills:*` skills share one `syncClaudeAiSkills` line, and plugin
skills get no settings switch. What remains is Codex.

The 2026-11-05 audit found Codex listing 182 skills per request, 104 of them
four copies of the claude.ai skills Claude Code syncs into
`~/.claude/skills/synced/`, which the maintainer's `~/.agents/skills` linked
([kixelated/dotfiles#47](https://github.com/kixelated/dotfiles/pull/47)
fixed it). At Sol's cached input price that was about $130 a month, but the
catalog has a fixed token budget, so duplicates can push useful skills out.

Codex pieces are not priced yet: Codex requests have no wire shape, so
pricing them needs the chronological exposure, a token estimate and a
`config.toml` snippet, which the context quest builds for every Codex piece.
This quest adds the skill-specific parts on top.

Recommendations for the implementer:

- Codex records the rendered catalog as `world_state.state.host_skills.body`
  when skills are on (`includeInstructions: true`), as of Codex 0.167. Its
  entries end in `(file: rN/<dir>/SKILL.md)`, where the body's leading
  `` - `rN` = `/abs/root` `` lines define the root aliases; plugin skills are
  named `<plugin>:<skill>`. Resolve each entry to its absolute `SKILL.md`.
- The control is `[[skills.config]]` with `path` set to that file and
  `enabled = false`
  ([build skills](https://learn.chatgpt.com/docs/build-skills)). It removes
  the skill entirely, so typed `$name` requests count as uses alongside the
  model's SKILL.md loads. `allow_implicit_invocation: false` is not
  documented to remove the catalog entry; do not claim it saves tokens.
- Codex does not merge same-named skills; both copies are listed. A name
  listed more than once is a duplicate: recommend disabling every copy but
  one by path, even when the dollars are unknown, and note that a symlinked
  copy is better removed at its source.

## Waiting

- Maintainer: drop the listing budget checks (`skills.max_context_tokens`,
  `skillListingBudgetFraction`, `skillListingMaxDescChars`) from this quest,
  or keep them. Recommendation: drop. Per-skill rows already price the listing
  as rendered after any budget, and the only "violation" is an out-of-range
  value the harness itself rejects or clamps, so the checks put no dollar on
  anything. Check: this item is removed and the Plan says which.

## Required

- [MCP and loaded-context check](/quest/m0/context.md) - prices Codex pieces and renders `config.toml` controls
