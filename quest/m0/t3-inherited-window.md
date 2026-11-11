# [S] Resolve the inherited T3 Claude window

## Goal

A T3 Claude instance without `autoCompactWindow`, whose launch arguments
cannot change the window, gets the native profile's effective window
instead of being marked unresolved. Its window and idle checks become
priced.

## Plan

Decided 2026-11-11 (maintainer, /quest-complete planning the follow-ups of
[kixelated/token-max#50](https://github.com/kixelated/token-max/pull/50)):
✅ plan it in m0 with the rule below (rejected: keep inherited windows
unresolved).

- Without `autoCompactWindow`, T3 takes the window from a `--settings` launch
  argument, then local, project and user settings, the same files
  token-max's native scopes already read.
- Treat the inherited window as native when the launch arguments contain no
  `--settings` or `--setting-sources`. `--chrome` qualifies.
- Today `src/t3.rs` reads only whether `launchArgs` is present, so it needs
  to read the arguments themselves. Arguments it cannot parse stay
  unresolved.
- On the maintainer's host this prices `claude_api_credits`.
