# [S] Pass the Codex memories scan test outside CI

## Goal

`exposure::scan_tests::codex_memory_summary_and_memory_tools_count_though_memories_are_off_by_default`
passes on every host that runs `just check`, not only in CI. On a Fedora host
with an XFS home it fails at `src/exposure/scan_tests.rs:2793`: with a Codex
memory store present, the "Codex memories" line no longer reads
`: off. No measured cost, but extraction and consolidation are not measured; never read back or written.`

## Plan

Observed 2026-11-12 on main at `75b4aab`, where CI's Linux, macOS and Windows
jobs pass it. On the failing host it still fails with a cleared environment,
an empty `HOME` and `TZ=UTC`, on the same rustc 1.99.0 as `nix develop`.

Lead to check first: test homes live under the checkout's `.tmp/`, so the
synthetic sessions' working directories sit below the host's real home. On
the failing host those ancestors hold `~/.claude/settings.json` and
`~/.codex/config.toml`, which CI's runners lack. If discovery reads them as
project settings, isolate the tests from the checkout's ancestors, and decide
whether discovery should stop at the user's home, rather than loosening the
assertion.

`recorded_request_shapes_price_each_piece_at_its_cache_position_and_count_its_uses`
fails on the same host and gets its own quest in
[kixelated/token-max#75](https://github.com/kixelated/token-max/pull/75); it
may share the cause, so whichever lands second checks the other.
