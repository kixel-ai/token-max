# [M] Suggest a smaller compaction window in vibes

## Goal

`token-max vibes` suggests capping the compaction window at 400K tokens for
Claude Code and 240K for Codex, for output quality rather than cost. It fires
when the window, after any `doctor` change the user accepted, is above the
cap, including an unset window on a native-1M Claude model, which compacts
near 967K. The card names the maintainer's pick as its source and shows
`doctor`'s replayed cost of the cap when the window line priced it.

## Plan

Decided 2026-11-23 (maintainer, /quest-plan after a pass over their
token-max transcripts). The maintainer, 2026-11-11: "I feel like 1M is mostly
a marketing thing and results in worse output (and higher cost) at high
windows". Their dotfiles set `autoCompactWindow: 400000` and Codex's
`model_auto_compact_token_limit = 240000`.

Decision prompts, chosen options marked ✅:

- Card: ✅ a smaller window (rejected: none; picked with the memory and
  transcript-retention cards).
- Target and overlap: ✅ 400K for Claude and 240K for Codex, firing on the
  window left after `doctor`'s accepted changes and showing `doctor`'s
  replayed cost of the cap (rejected: Claude only).

Recommendations:

- Cite provider guidance on long-context quality if a current page states it;
  otherwise the card rests on the maintainer's pick alone and says so.
- Write the setting `doctor`'s window line names as the owner, through the
  same edit, so `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` and per-model rows stay
  consistent. Where that line would leave the change by hand, so does this
  card.
- A model whose context is at or under the cap never fires.
