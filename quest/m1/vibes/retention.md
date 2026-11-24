# [S] Suggest keeping a year of Claude transcripts in vibes

## Goal

When Claude Code's `cleanupPeriodDays` is under 365, its default of 30
included, `token-max vibes` suggests raising it to 365, so Claude Code stops
deleting the transcripts token-max replays. The card shows how much disk the
profile's transcripts take today and names the maintainer's pick as its
source.

## Plan

Decided 2026-11-23 (maintainer, /quest-plan after a pass over their
token-max transcripts and dotfiles). The maintainer's dotfiles keep
transcripts for 3650 days because on 2026-11-22 the oldest were already being
deleted; `desktop` held 22 GB of them.

Decision prompts, chosen options marked ✅:

- Card: ✅ keep transcripts (rejected: none; picked with the memory and
  window cards).
- Target: ✅ 365 days (rejected: 3650, the maintainer's own value, which
  grows disk without bound for heavy users; 90 days, which loses longer
  comparisons).

Recommendations:

- Say on the card that the setting only stops future deletion: history
  already removed is gone.
- Codex keeps its sessions until deleted by hand, so this card is
  Claude-only. Confirm that against current Codex docs before writing it.
