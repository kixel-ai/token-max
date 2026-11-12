# [S] Move the repository to the kixel-ai organization

## Goal

The repository is `kixel-ai/token-max`. Its required check, branch
protection and `crates-io` environment still hold there, every reference
in this repository names the new owner, and moq-dev/quest's quests and
`SETUP.md` point to it. The name stays `token-max`.

## Plan

Decided 2026-11-11 (maintainer, /quest-plan for `doctor`). Decision
prompts, chosen options marked ✅:

- Owner: ✅ kixel-ai (the maintainer: "move to the kixel-ai org"),
  superseding the 2026-11-05 choice of kixelated. The organization exists,
  the maintainer is its admin, and it had no public repositories on
  2026-11-11.
- Name: ✅ token-max (rejected after two rounds of candidates: whatif,
  citation-needed, tuneup, agentdx, agentproof, agentwise, agentdebrief,
  tokdoc, handler; the maintainer: "token-max is actually probably still
  the best").
- Agent posts in kixel-ai: ✅ treated like kixelated and moq-dev, without
  asking first (rejected: ask each time; the maintainer transfers the
  repository by hand).

Move before the first crates.io release: a trusted publisher names the
repository's owner, and `.github/workflows/release.yml` checks
`github.repository`.

Agent's notes, for the implementer to revisit:

- References to update: `Cargo.toml`, the README, the install hint in
  `src/ssh.rs`, `release.yml` and `docs/releasing.md`. Links to merged PRs
  redirect and can stay.
- `CONTRIBUTING.md` says GitHub's merge queue is unavailable for personal
  repositories. That stops being true; reword it, and leave enabling the
  queue as a suggested follow-up.
- Confirm after the transfer that the `Check` requirement, auto-merge and
  the `crates-io` environment's branch restriction survived.

## Waiting

- Maintainer: add kixel-ai beside kixelated and moq-dev in the GitHub rules of the dotfiles `CLAUDE.md`, so sessions post there without asking. An agent cannot do it durably, since `dotfiles-update.timer` rewrites `~/.claude/CLAUDE.md` hourly. Check with `grep kixel-ai ~/.claude/CLAUDE.md`.
