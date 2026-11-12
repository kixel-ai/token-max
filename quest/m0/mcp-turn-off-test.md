# [XS] Fix the failing MCP server turn-off assertion on main

## Goal

`just check` passes on main again:
`exposure::scan_tests::recorded_request_shapes_price_each_piece_at_its_cache_position_and_count_its_uses`
fails at `src/exposure/scan_tests.rs:854`, where the strict report's
"MCP server docs" line no longer reads `: on. Costs ...` followed by
`Turn off: "permissions": {"deny": ["mcp__docs"]}.`

## Plan

Reproduce it on origin/main, find whether the report text or the test
drifted, and fix the root cause rather than loosening the assertion.
