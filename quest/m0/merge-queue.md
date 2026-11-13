# [S] Land PRs through GitHub's merge queue

## Goal

PRs into `main` land through GitHub's merge queue, which tests each one
against the current base, so landing one PR no longer forces every other
open PR to merge `main` again and rerun its checks. `CONTRIBUTING.md`
describes the new landing path.

## Plan

Decided 2026-11-11 (maintainer): plan it in m0, after about 13 landings
in one session sent the other open PRs back for about 10 extra merges of
`main` under the "branch must be up to date" rule.

[kixel-ai/token-max#88](https://github.com/kixel-ai/token-max/pull/88)
made `Check` run on `merge_group` events. What remains, now that it is on
`main`:

- Add a `merge_queue` rule (squash, `Check` required) to the `main`
  ruleset (id 26764302) with `gh api -X PUT`, and set
  `strict_required_status_checks_policy` to false in the same call, since
  the queue tests against the current base. Revert by PUTting the old rules
  back: deletion, non_fast_forward, and required_status_checks with `Check`
  (integration 15368) and the strict policy true.
- Rewrite `CONTRIBUTING.md`'s landing section: `quest merge <pr> --squash
  --land <sha>` enqueues the head (or arms auto-merge until checks pass);
  no merging of `main` unless the queue refuses the head. This repository
  does not run `quest land watch`, so nothing runs beside the queue.
