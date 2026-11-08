# [M] Skills catalog and usage check

## Goal

`token-max` reports the recurring discovery-context cost and observed usage
of Claude Code and Codex skills. It flags duplicate entries and over-budget
catalogs, and ranks unused or rarely used skills for disabling, reducing
visibility, or scoping to relevant projects. Each finding shows exposure,
usage coverage, the setting's owner, and estimated dollars where supportable;
otherwise it gives an applicable provider-guidance warning.

## Plan

Expanded 2026-11-06 (maintainer, /quest-plan): ✅ usage and recurring cost
for both harnesses in m0 (rejected: defer to m1). The maintainer wants
unused/rarely used skills identified and the cost of leaving them enabled
quantified where possible. Recommendations stay read-only.

Decided 2026-11-07 (maintainer): ✅ recommend `skillOverrides` set to
`user-invocable-only` for a skill whose listing cost more than $1 per model
call (rejected: leave the call to the user), per the
[context quest](/quest/m0/context.md). Typed uses still work when hidden, so
only model calls count. Progress: Claude listing members are priced and judged
per skill; Codex catalogs, duplicates and budget checks remain.

The 2026-11-05 audit found Codex listing 182 skills per request, 104 of them
four copies of the claude.ai skills Claude Code syncs into
`~/.claude/skills/synced/`, which the maintainer's `~/.agents/skills` linked
([kixelated/dotfiles#47](https://github.com/kixelated/dotfiles/pull/47)
fixed it). At Sol's cached input price that was about $130 a month, but the
catalog has a fixed token budget, so duplicates can push useful skills out.

Recommendations for the implementer:

- Track each skill's actual rendered listing, exposed sessions/requests,
  observed invocations or loads, and last observed use in the selected
  window. Resolve names, paths, namespaces, and duplicate sources. Direct
  Skill calls and explicit typed invocations are strong evidence; shell
  reads of SKILL.md or inherited instructions may be weaker evidence.
  Requested use, confirmed loads, and successful invocations stay distinct.
  Count deduplicated calls and explain
  gaps, rather than interpreting a missing invocation event as never used.
- Report "no observed use during N exposed sessions" and the window, not
  "never used". Show use count/rate and recurring cost to rank infrequent
  skills without an arbitrary rarity threshold. Recently installed skills,
  unknown historical catalogs, and incomplete invocation visibility need
  separate labels. A useful rare skill may belong in a project or remain
  manually invocable instead of being disabled everywhere.
  Automatic unused recommendations require complete exposure and invocation
  visibility across the window, positive recurring overhead, and a finding
  that still applies to the current effective configuration. Unused
  findings for components introduced during the window and all other
  low-use rankings are advisory under the milestone's exit-code rules.
- Price the listing actually sent, including name, description, and path
  where present, across its request exposures. Skill bodies and references
  loaded on use are a separate attributed cost. Use marginal before/after
  catalog token counts: budget backfill or description expansion can consume
  freed space, so removed entries' gross cost is not always net savings.
  Include relevant cache behavior and disclose unmeasured extra work.
- Keep the existing duplicate-folder and Codex
  `skills.max_context_tokens` checks, including its documented maximum.
  Include Claude `skillListingBudgetFraction` and
  `skillListingMaxDescChars`. Use the harness's effective budget and
  truncation/omission evidence; do not assume every installed skill is sent
  or the full skill body is loaded on every request.
- Recommend supported visibility controls: Claude `skillOverrides` can use
  `name-only`, `user-invocable-only`, or `off`; plugin skills are controlled
  through the plugin, not that setting. Codex supports per-skill enablement
  and catalog budgets. Name the owning file/tool and supported version, and
  do not claim disabling implicit invocation saves catalog tokens unless
  that harness actually removes the metadata.

Sources checked 2026-11-06: [Claude skills and unused-skill
reports](https://code.claude.com/docs/en/skills#find-unused-skills),
[Codex skill loading and budgets](https://learn.chatgpt.com/docs/build-skills),
and [Codex metadata guidance](https://learn.chatgpt.com/blog/rethinking-skills-and-prompts-for-gpt-6-astra).
Claude's `/skill-doctor` already reports unused skills and context cost;
this check adds cross-harness transcript replay and API-price estimates.
