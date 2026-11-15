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

Added 2026-11-16 (maintainer, after running the merged walk on macOS:
"no coloring on my mac. Also maybe add some padding so it looks better"):
the walk today prints only bold, while the list after it is colored.

- ✅ color the walk like the report: the verdict in its color, savings
  green, and secondary text dim, wherever the report colors (not only on
  Linux terminals; macOS Terminal and iTerm2 handle the same escapes).
- ✅ padding: indent the walk and the list by two spaces, and leave a blank
  line around the headline and between cards. Replace the three
  "token-max: Scanning..." progress lines with one line that clears itself.
- The headline and the card counter disagreed: that run said "73 changes to
  review" and then "[1/2]", since 72 small changes shared one question.
  [Group the walk's small changes by kind](/quest/m0/launch/group-walk.md)
  owns the fix.

## Required

- [Say what each setting does](/quest/m0/launch/descriptions.md) - the What line on each walk card
