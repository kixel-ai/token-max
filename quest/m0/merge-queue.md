# [S] Land PRs through GitHub's merge queue

## Goal

PRs into `main` land through GitHub's merge queue, which tests each one
against the current base, so landing one PR no longer forces every other
open PR to merge `main` again and rerun its checks. `CONTRIBUTING.md`
describes the new landing path.

## Plan

Decided 2026-11-11 (maintainer, /quest-complete planning the follow-up of
[kixelated/token-max#62](https://github.com/kixelated/token-max/pull/62)):
✅ plan it in m0 (rejected: drop). #62 suggested it only if stale-branch
reruns became a cost; in that session, main's "branch must be up to date"
rule made each of about 13 landings send the other open PRs back for a
merge of `main` and a fresh CI run, about 10 extra merges in all.

- Enable the queue on `main` with squash merges and the `Check` job as its
  required check, and run `Check` on `merge_group` events in
  `.github/workflows/`. Drop the up-to-date requirement in its favor.
- `quest merge` enqueues a PR on a base with a merge queue; update
  `CONTRIBUTING.md`'s landing steps to match, including whether
  `quest land watch` runs beside the queue.
