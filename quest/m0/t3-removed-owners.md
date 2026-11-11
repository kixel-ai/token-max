# [S] Price sessions whose T3 owner was removed

## Goal

A native session whose only T3 owner is an instance no longer in T3's
settings is still priced in the bill, but no longer makes any current
profile's TTL, window or idle control check advisory. Profiles that share
a home with such sessions, such as `claudeAgent`, `claude_local` and native
`~/.claude` on the maintainer's host, get priced recommendations again.

## Plan

Decided 2026-11-11 (maintainer, /quest-complete planning the follow-ups of
[kixelated/token-max#50](https://github.com/kixelated/token-max/pull/50)):
✅ plan it in m0 with the rule below (rejected: keep the ambiguity until the
old sessions age out of the window).

- A session whose only T3 owner is a removed instance ran under controls that
  no longer exist. Leave it out of every current profile's control checks,
  and keep its requests in the bill.
- Keep it ambiguous only when a current instance shares the removed one's
  home and driver, which looks like a rename.
- This reverses the deliberate "Preserve ambiguity even if a referenced
  instance was removed from settings" choice in `src/t3.rs`. Update
  `docs/t3.md`'s ownership rules in the same PR.

Evidence, 2026-11-11: `T3 unresolved: 149,210 requests: the native session's
only T3 owner is not a selected instance for this home` this week. The
owners are `claude_personal` (2,286 threads) and `claude_proxy` (505).
