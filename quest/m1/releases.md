# [M] Check new Claude Code and Codex releases for settings to measure or suggest

## Goal

Every two weeks, read the Claude Code and Codex release notes since the
versions the last run checked, and open a PR that plans what they mean for
token-max: a quest for each new or changed setting `doctor` can measure, a
`vibes` card quest for each worth suggesting that it can't, and an edit or
fix quest for each existing check a release changes, such as a new default,
a renamed key or a new version gate. A run that finds nothing still opens
the PR, carrying the next run.

## Plan

Decided 2026-11-23 (maintainer, /quest-plan): ✅ a recurring release check
every two weeks (rejected: weekly, where most runs would find nothing;
monthly, which leaves advice stale for a month). Claude Code ships almost
daily and Codex weekly, and
doctor's upgrade offer for newer token-max releases only helps once token-max
itself has caught up.

Each run:

- Reads, from the last checked versions up: Claude Code's changelog
  ([anthropics/claude-code](https://github.com/anthropics/claude-code)
  `CHANGELOG.md`) and settings and environment variable references, and
  Codex's releases ([openai/codex](https://github.com/openai/codex)), config
  reference and `codex features list`.
- Sorts each change that touches cost or capability: measurable from
  transcripts goes to `doctor`, not measurable to `vibes`, and anything an
  existing check reads gets checked against that check. Skips what an earlier
  run rejected, listed below, unless the release changed it.
- Diffs the Code intelligence card's `SERVERS` table (`src/lsp.rs`) against
  the language server entries of
  [anthropics/claude-plugins-official](https://github.com/anthropics/claude-plugins-official)'s
  `.claude-plugin/marketplace.json`: each entry's `lspServers` command and
  `extensionToLanguage`, and any `defaultEnabled` it sets, since the card
  assumes none does. A renamed binary, a new typed language or a changed
  extension is a fix quest.
- Plans through `/quest-plan` rules, so the maintainer picks which new
  quests to keep when the PR lands.
- Finishes by updating this file in its own PR: the baseline below, the
  rejected list, and `Scheduled` moved 14 days on. The next run stays at this
  path.

The recurrence ends when the maintainer drops this quest.

Baseline, from the 2026-11-23 survey that planned the
[vibes cards](/quest/m1/vibes/README.md): Claude Code 2.1.322, codex-cli
0.167.1. Rejected then, so not to propose again unless changed: Codex
`features.code_mode` (the model catalog already forces code mode on for every
OpenAI model, T3's included), `includeGitInstructions: false` (right only
for users with their own git workflow), spend caps (`workflowSizeGuideline:
small`, Codex `goals.max_goal_token_budget`), telemetry opt-outs (privacy,
not cost or capability), `CLAUDE_CODE_IDLE_COMPACT_MIN_TOKENS` tuning
(decided 2026-11-17 in [m0](/quest/m0/README.md)).

Check first: whether Codex's catalog still forces code mode, and whether
code mode's `wait` polling
([openai/codex#38495](https://github.com/openai/codex/issues/38495)) became
user-controllable, which would make it a `doctor` line.

## Scheduled

- 2026-12-07 - two weeks after the baseline survey
