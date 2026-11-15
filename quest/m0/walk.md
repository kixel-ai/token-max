# [L] Walk the changes biggest saving first

## Goal

On a terminal, `token-max doctor` walks through the changes it offers,
biggest saving first. Each one explains why it makes sense, with the
numbers behind it in dollars and percent. A running total shows what the
accepted changes save so far, and small changes are offered together.
Nothing is written until a final ranked list of the accepted changes gets
one confirming yes.

## Plan

Decided 2026-11-14 (maintainer, after mockup videos of a ranked report in a
pager, a full-screen TUI and a ranked y/n walk): ✅ the walk to decide, then
the ranked list to confirm (rejected: rank and page through `$PAGER`; a
full-screen TUI). The walk explains why each change makes sense, which a
one-line row cannot.

Today `doctor` prints the whole report, then asks
`Apply? [y]es, [n]o, [q]uit` for each change it can make, recommended ones
first and then suggested ones, in report order, and writes every accepted
edit once the last answer is in (`ask` and `write` in `src/doctor.rs`). This
quest replaces that order, the report printed before the questions, and the
write without a confirm:

- On a terminal, the walk comes first, after a headline counting the
  changes; the report follows it as the list below, not before it.
- ✅ order the changes by monthly saving, largest first, across every
  settings file, recommended and worth-considering alike, each marked with
  its verdict (maintainer, at the pick of the planning PR; rejected:
  recommended first). ✅ a change whose saving was not measured comes after
  those saving $5/mo or more, one question each, saying its saving was not
  measured (rejected: in the small "take all?" batch).
- Each change shows its current and offered value, its saving per month in
  dollars and as a percent of the host's spend over the window (the
  percent base [the report quest](/quest/m0/report-length.md) records), its
  claim kind, and the stats behind it, such as the requests replayed, uses
  and cost per use, or the resume gaps a cache TTL replay counted, then the
  edit it would make.
- ✅ a labeled running total after each yes: the sum of the accepted
  changes' own savings, in dollars and percent, saying that overlapping
  changes may save less, such as "accepted so far: about $11,400/mo, 31% of
  this host's spend; overlapping changes may save less". One total, with no
  subtotal for each profile, since every percent shares the host's spend as
  its base (rejected: a joint replay of the accepted set, which the replay
  cannot do yet; no total).
  An accepted change whose saving was not measured is counted beside the
  total, not added to it, such as "plus 1 change whose saving was not
  measured". This amends m0's rule of "no summed or joint savings total",
  for this total only; the report's rows still each hold everything else
  constant.
- Changes saving under $5/mo each are offered together after the rest, as
  one "take all?" question that can list them one by one. On the
  maintainer's host that cuts the walk from 15 questions to 8.
- `q` ends the walk, leaving the rest unselected, and goes on to the list;
  end of input is a `q` during the walk and a no at the confirm. Ctrl-C
  still exits 2 with nothing written.
- ✅ then review and confirm (maintainer, 2026-11-14: "C for deciding
  what to change, then A with a confirm write these changes"): print the
  ranked one-line list, the accepted changes first with their savings and
  the labeled total, then the declined changes, unselected, the steps to
  take by hand, and every other row, as the report quest describes, and ask
  once whether to write the accepted changes, naming each file. The y/n
  answers in the walk only select; nothing is written before that yes, and
  a no says nothing was written. No pager, since a question follows the
  list. Either answer exits 0, as a walk does today. Without a terminal,
  doctor prints that report and asks nothing, as today.

Update `docs/reference.md`'s "Applying changes" and the README's walk
excerpt and the paragraph on what `doctor` writes to match.

## Required

- [Cut doctor's default report to one line each](/quest/m0/report-length.md) - the ranked rows, percents and the list printed after the walk
- [Give each setting one row per settings file](/quest/m0/file-rows.md) - the walk must never offer a change another row shows in use
