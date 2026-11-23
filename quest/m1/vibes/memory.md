# [M] Suggest turning auto memory off in vibes

## Goal

When Claude Code's auto memory or Codex memories are on, and most main
sessions in the window ran in a directory with a checked-in `CLAUDE.md` or
`AGENTS.md`, `token-max vibes` suggests turning memory off: Claude's
`autoMemoryEnabled: false` or Codex's `features.memories = false`. The card
names the maintainer's pick as its source and gives the reason: a lasting
fact about a project belongs in its repository, where it is reviewed and
every host and teammate sees it, not saved silently on one machine. It shows
`doctor`'s measured memory line (total cost, reads and writes) as evidence.

## Plan

Decided 2026-11-23 (maintainer, /quest-plan after a pass over their
token-max transcripts). The maintainer, 2026-11-12: "IMO it should be checked
into the repo (with review), not ad-hoc", and "don't use memory if you're
mostly working in repos with AGENTS.md/CLAUDE.md". Their dotfiles set
`autoMemoryEnabled: false` for that reason. `doctor` once gave memory a
yellow "not recommended (encourages slop)" advisory;
[#192](https://github.com/kixel-ai/token-max/pull/192) dropped it as too much
opinion, and `vibes` is now where opinions go.

Decision prompts, chosen options marked ✅:

- Card: ✅ auto memory off (rejected: none; picked with the window and
  transcript-retention cards).
- Trigger: ✅ only when most main sessions ran in a directory with a
  `CLAUDE.md` or `AGENTS.md` (rejected: whenever memory is on, which tells
  people who work outside repos to drop their only place for a fact).

Recommendations:

- Count sessions, not requests, and read the instruction files the scan
  already resolves at recorded working directories. Say on the card what share
  of sessions qualified.
- `doctor`'s memory line stays as it is: measured, red when never used. When
  `doctor` already offered memory off, `vibes` doesn't ask again.
- Reuse the memory edit `doctor` already makes, including its by-hand cases
  for named profiles and untrusted projects.
