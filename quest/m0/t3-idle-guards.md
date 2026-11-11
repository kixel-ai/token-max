# [L] Rebuild historical T3 idle guards

## Goal

T3's `beforeCacheExpires` idle mode is priced for past idle gaps, not just
the current state, so the T3 idle check can compare all three modes.

## Plan

Decided 2026-11-11 (maintainer, /quest-complete planning the follow-ups of
[kixelated/token-max#50](https://github.com/kixelated/token-max/pull/50)):
✅ plan it in m0 (rejected: m1, which would leave `beforeCacheExpires`
advisory in 0.1.0; drop).

- T3 projects only the current lifecycle guards, so every gap before a
  later run in the same thread stays unpriced for `beforeCacheExpires`.
- Rebuild each gap's guards from T3's `orchestration_events`: thread
  archive, settle and snooze events, pending runtime requests and
  merge-backs, as of the gap.
- Once this lands with the two required quests, this is the only remaining
  blocker for that mode.

## Required

- [Price sessions whose T3 owner was removed](/quest/m0/t3-removed-owners.md) - removed-instance sessions stop making current profiles advisory
