# [S] Skills catalog check

## Goal

`token-max` reads the skills catalog Codex sends with each session and flags
entries listed more than once and a catalog over `skills.max_context_tokens`,
naming the folders the duplicates come from and what the extra entries cost.

## Plan

Added by the agent while planning; the maintainer can drop it in review.
The 2026-11-05 audit found Codex listing 182 skills per request, 104 of them
four copies of the claude.ai skills Claude Code syncs into
`~/.claude/skills/synced/`, which the maintainer's `~/.agents/skills` linked
([kixelated/dotfiles#47](https://github.com/kixelated/dotfiles/pull/47)
fixed it). At Sol's cached input price that was about $130 a month, but the
catalog has a fixed token budget, so duplicates can push useful skills out.

## Required

- [Replay and the cache TTL check](/quest/m0/replay.md) - the reader, prices, and report it adds to
