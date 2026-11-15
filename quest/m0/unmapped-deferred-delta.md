# [S] Stop offering a deny rule for an unmapped deferred-tools delta

## Goal

A Claude `deferred_tools_delta` attachment whose names and lines do not pair
one to one never becomes a tool line of its own, so `token-max doctor` stops
offering to add a meaningless rule such as `deferred_tools_delta:413` to
`permissions.deny`. Its characters still count, as context with no switch.

## Plan

Found 2026-11-15 by `token-max doctor --details` on the maintainer's host,
while adding What lines: one row read `deferred_tools_delta:413 tool on ->
off`, worth considering, with no description. `src/exposure.rs` names an
unpaired delta `{kind}:{line}` in `Category::McpDiscovery`, and
`context::pieces` passes every `McpDiscovery` name to `tool()`, which gives
any name outside `CORE_TOOLS` a `permissions.deny` control.

Recommended: map an unpaired delta to an unswitchable piece, such as the
`Other` kind the skill listing's framing uses, keyed so it joins the "System
prompt and core tools" line, and add a synthetic transcript with an unpaired
delta to the scan tests.
