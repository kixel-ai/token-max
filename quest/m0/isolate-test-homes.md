# [S] Isolate test homes from the host's real config

## Goal

Every scan test passes on every host that runs `just check`, not only in CI,
because no test reads config outside its own synthetic home. Two fail today
on agent hosts and pass in CI's Linux, macOS and Windows jobs:

- `exposure::scan_tests::codex_memory_summary_and_memory_tools_count_though_memories_are_off_by_default`,
  at `src/exposure/scan_tests.rs:2793`: the "Codex memories" line no longer
  reads `: off. No measured cost, but extraction and consolidation are not measured; never read back or written.`
- `exposure::scan_tests::recorded_request_shapes_price_each_piece_at_its_cache_position_and_count_its_uses`,
  at `src/exposure/scan_tests.rs:854`: the strict report's "MCP server docs"
  line no longer reads `: on. Costs ...` followed by
  `Turn off: "permissions": {"deny": ["mcp__docs"]}.`

## Plan

Observed 2026-11-12 on main at `75b4aab` on a Fedora host with an XFS home.
The Codex test still fails there with a cleared environment, an empty `HOME`
and `TZ=UTC`, on the same rustc 1.99.0 as `nix develop`.

Likely cause: test homes live under the checkout's `.tmp/`, so the synthetic
sessions' working directories sit below the host's real home, whose
`~/.claude/settings.json` and `~/.codex/config.toml` CI's runners lack.
Confirm that discovery reads those ancestors as project settings, then fix it
at the root: isolate test homes from real config in ancestor directories, and
decide whether discovery itself should stop at the user's home. Never loosen
either assertion. Both tests must pass on an agent host afterwards.
