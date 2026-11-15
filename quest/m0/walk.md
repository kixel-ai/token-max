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
pager, a full-screen TUI and a ranked y/n walk): ✅ the walk to decide, then the ranked list to confirm (rejected:
rank and page through `$PAGER`; a full-screen TUI). The walk explains why
each change makes sense, which a one-line row cannot.

- Order the changes by monthly saving, largest first, across every profile.
- Each change shows its current and offered value, its saving per month in
  dollars and as a percent of its profile's spend over the window (the
  percent base [the report quest](/quest/m0/report-length.md) records), its
  claim kind, and the stats behind it, such as the requests replayed, uses
  and cost per use, or the resume gaps a cache TTL replay counted, then the
  edit it would make.
- ✅ a labeled running total after each yes: the sum of the accepted
  changes' own savings, in dollars and percent, saying that overlapping
  changes may save less, such as "accepted so far: about $11,400/mo, 31% of
  T3 claudeAgent; overlapping changes may save less" (rejected: a joint
  replay of the accepted set, which the replay cannot do yet; no total).
  This amends m0's 2026-11-08 rule of no summed savings, for this total
  only; the report's rows still each hold everything else constant.
- Changes saving under $5/mo each are offered together after the rest, as
  one "take all?" question that can list them one by one. On the
  maintainer's host that cuts the walk from 15 questions to 8.
- ✅ then review and confirm (maintainer, 2026-11-14: "C for deciding
  what to change, then A with a confirm write these changes"): print the
  ranked one-line list, the accepted changes first with their savings and
  the labeled total, then the rows the walk did not offer, as the report
  quest describes, and ask once whether to write the accepted changes,
  naming each file. The y/n answers in the walk only select; nothing is
  written before that yes. No pager, since a question follows the list.
  Without a terminal, doctor prints that report and asks nothing, as today.

## Required

- [Cut doctor's default report to one line each](/quest/m0/report-length.md) - the ranked rows, percents and the list printed after the walk
- [Give each profile one row per tool](/quest/m0/profile-rows.md) - the walk must never offer a change another row shows in use
