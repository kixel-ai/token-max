# [S] Add one total across SSH hosts

## Goal

A `doctor` run over several hosts heads its report with one total across
them, the hosts' spend and savings summed, beside each host's own section.

## Plan

Decided 2026-11-17 (maintainer, /quest-plan): ✅ sum the hosts' figures as
they are (rejected: deduplicating copied histories by request ID across
hosts; summing with a "copied histories may count twice" label). The
maintainer: "who syncs transcripts ??? sum it don't waste CPU deduping".
This reverses m0's 2026-11-08 "amounts across hosts are not added".

Recommended: a figure marked `~` on any host keeps the `~` in the total, and
its margin adds as independent, as the title's total over profiles does
once the extrapolation quest in m0 lands. Say in
`docs/reference.md` that a history copied between hosts counts on each.
