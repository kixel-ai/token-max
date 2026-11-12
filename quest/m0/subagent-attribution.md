# [S] Share subagent attribution between the report and session export

## Goal

The report's Subagents section and `token-max sessions` read each
subagent's session, parent, kind, model and effort through one shared
reader path, so both resolve a Codex subagent's parent the same way and a
fix to one cannot miss the other.

## Plan

Decided 2026-11-11 (maintainer, /quest-complete planning the follow-up of
[kixelated/token-max#63](https://github.com/kixelated/token-max/pull/63)):
✅ plan it in m0 (rejected: m1; drop).

- #63 records per-file subagent attribution in `Scan.subagents`;
  [kixelated/token-max#69](https://github.com/kixelated/token-max/pull/69)
  parses the same Codex `session_meta` fields again in `src/sessions.rs`.
- The copies already disagree. #63's review fixed a nested spawn taking the
  root `session_id` instead of its immediate spawning thread (the root is
  right only for native sources such as `review`), while `src/sessions.rs`
  still prefers `session_id` over `parent_thread_id` when it names a
  session's parent. Check whether that misattributes a nested spawn's usage
  in the export, and fix it through the shared path.
- Keep one parser in the reader and have both consumers read its result;
  delete the second path rather than wrapping it.
