# [S] Catch slash-literal path assertions before Windows CI

## Goal

A test that compares a path it expects with one the code built uses the
platform's separator, and `just check` on Linux fails a new test that asserts
a hard-coded `/` path, so the mistake no longer surfaces only in CI's
`Tests (windows-2025)` job.

## Plan

Found 2026-11-24 during a /quest-run: four PRs failed only on Windows CI for
test setup. Three asserted expected paths with `/`:
[kixel-ai/token-max#203](https://github.com/kixel-ai/token-max/pull/203),
[#205](https://github.com/kixel-ai/token-max/pull/205) and
[#210](https://github.com/kixel-ai/token-max/pull/210).
[#208](https://github.com/kixel-ai/token-max/pull/208) cleared the environment
and lost `SystemRoot`, which its `support::isolated` helper now prevents.
Each cost a CI round trip after local checks passed.

Decided 2026-11-24 (maintainer, /quest-run): ✅ a shared helper plus a local
check (rejected: keep relying on the Windows job).

Recommendations:

- One helper for expected paths built from components, replacing the copies
  of `fn text(path: &Path)` in `src/discovery.rs`, `src/reader.rs`,
  `src/fast.rs` and `tests/cli.rs`, which each normalize separators on
  Windows.
- The check: a small test or `just check` step that scans `src/**/*.rs` and
  `tests/**/*.rs` for an assertion (`assert*!`, `.contains(`, `.ends_with(`,
  `.starts_with(`) whose string literal holds a path segment with `/`, such as
  `"repo/.codex/config.toml"`. Allow URLs and an explicit opt-out comment for
  a literal that really is `/`-separated, such as a settings key.
  Prove it on the three culprit assertions above, restored on a scratch
  branch.
