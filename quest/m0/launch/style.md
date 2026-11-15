# [L] Make doctor's output pretty

## Goal

`token-max doctor`'s report and walk read cleanly on a color terminal: a
bold headline, colored verdicts and savings, dim secondary text, aligned
columns, and short walk cards, with plain text when piped or under
`NO_COLOR`. The walk's edits show once, as a diff of each settings file,
at the confirm.

## Plan

Decided 2026-11-15 (maintainer, after mockup recordings of three styles:
polished lines, boxed cards, and badges with bars): ✅ polished lines
(rejected: boxed cards; badges and saving bars).

- The header is one line: the tool's name, then the requests, window and
  spend dimmed. The headline counts the changes in their verdict colors.
- A walk card is the counter and the setting's name with its saving in
  dollars and percent at the right, then the current value → the offered
  one with the colored verdict and the claim kind, then "What" (from
  [Say what each setting does](/quest/m0/launch/descriptions.md)) and
  "Why", the evidence. ✅ drop the Edit lines from the card, the file and
  JSON fragment, which repeat the value change (maintainer: "The text is
  too crowded"); ✅ keep Why (rejected: evidence under `--details` only).
  This amends the walk's plan (`quest/m0/walk.md` in git history) that
  each change shows the edit it would make.
- ✅ the confirm shows the accepted changes as a diff of each settings file
  before its one question, so nothing is written without being shown.
- The ranked list marks each row with a colored dot and the section headers
  with a rule; savings are green.

## Required

- [Say what each setting does](/quest/m0/launch/descriptions.md) - the What line on each walk card
