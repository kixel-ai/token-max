# [M] Audit the quest tree

## Goal

Run the `tree` audit: `quest audit tree` prints what it checks, what each finding becomes, and how to plan its next run.

## Plan

Baseline from the m1 run (2026-11-20): 8 quests, all in m1, and the m0 README
holding the project's decision record. The tree was small enough to read in one
pass without sub-agents.

Check first: the facts the `Waiting` quests and the m0 decision record cite,
against the live state they name (crates.io, max.kixel.me, the repository's
environments and secrets), and pointers to finished quests, which should say
they are in git history.

## Audit

- tree

## Scheduled

- 2026-11-27 - a week after the m1 run, so a fast m1 does not rerun it at once
