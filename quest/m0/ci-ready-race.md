# [S] Keep the ready-for-review Check from losing to a draft edit

## Goal

Marking a PR ready with `quest ready` always leaves the required `Check` on
its head reporting the real source and platform results, so auto-merge lands
it without anyone converting the PR to draft and back.

## Plan

Observed 2026-11-10 on
[kixelated/token-max#41](https://github.com/kixelated/token-max/pull/41): `quest ready` edits the
PR body (dropping `## Waiting`) while the PR is still a draft, then marks it
ready. `.github/workflows/check.yml` runs on both `edited` and
`ready_for_review`, sharing one `cancel-in-progress` concurrency group per
ref. The `edited` run's payload has `draft: true`, so its source and platform
jobs skip and its `Check` fails by design, while the `ready_for_review` run
was cancelled. The ruleset's required `Check` stayed failed. Re-running the
cancelled run produced a passing `Check`, but the PR stayed blocked until it
was converted to draft and back, which also dropped auto-merge. The same
draft-run failure appeared on #37, #38, #40 and #42.

Keep the intended behaviour: a draft's skipped jobs must never satisfy the
required `Check`. Options to weigh, then pick the simplest that holds:

- Recommended: key the concurrency group by draft state as well as ref, or
  stop draft-payload runs from cancelling ready ones, so the ready run always
  completes after the draft edit.
- Drop `edited` from the trigger types if nothing needs it (check why it was
  added in [kixelated/token-max#4](https://github.com/kixelated/token-max/pull/4)).
- Change `quest ready` upstream (moq-dev/quest) to mark the PR ready before
  editing its body. Asking outside `kixelated` needs the maintainer's approval.

Verify on a real PR that a `quest ready` flip leaves a passing required
`Check` and that auto-merge lands it.

Planned 2026-11-10 by /quest-complete while the maintainer was away (/afk),
using the agent's recommendations: ✅ plan this follow-up (rejected: drop it).
✅ m0 (rejected: m1), since every m0 PR lands through this gate. The
maintainer has not confirmed either choice yet.
