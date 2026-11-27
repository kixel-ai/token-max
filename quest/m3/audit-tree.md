# [M] Audit the quest tree

## Goal

Run the `tree` audit: `quest audit tree` prints what it checks, what each finding becomes, and how to plan its next run.

## Plan

Baseline from the m2 run (2026-11-27): 18 quests across m0 to m2, seven of
them audit stubs, and the m0 README holding the project's decision record.
The tree was small enough to read in one pass without sub-agents.

Check first:

- The facts the `Waiting` quests and the m0 decision record cite, against the
  live state they name (crates.io, max.kixel.me, the repository's
  environments and secrets, kixel-ai/crabx's tags and npm versions).
- Pointers to finished quests, which should say they are in git history.
  `quest check` catches only links; the m2 run found bare file names
  (`advisor-after-default.md`) and kixel-ai/crabx quest links that had gone
  stale. Search the tree for `\.md` outside links, and fetch each
  cross-repository quest path.
- Notes that a PR "ships in a later release" or that work "waits on" a
  release, against what has since merged or shipped.

## Audit

- tree

## Scheduled

- 2026-12-04 - a week after the m2 run, so a fast m2 does not rerun it at once

## Required

- [m2](/quest/m2/README.md) - runs once per milestone
