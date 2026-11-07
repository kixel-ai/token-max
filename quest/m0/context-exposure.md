# [M] Context exposure measurement

## Goal

The skills and context checks share local component-size estimates,
request-by-request exposure, and skill/MCP invocation evidence with explicit
coverage. They can estimate recurring overhead without uploading content,
retaining prompt text, or interpreting opaque schema hashes as tokens.

## Plan

Split from replay during the 2026-11-06 m0 plan review so the cache, speed,
and background checks do not wait on context analysis. This is shared
measurement for the maintainer's confirmed skills/MCP cost goal.

- Reconstruct exposure from verified snapshots/deltas and request
  references, including compaction, removals, and inherited history.
  Codex `world_state.state.host_skills.body` holds rendered skill listings;
  `state.tools` can hold deferred namespace descriptions. Opaque
  `top_level_tools` hashes are identities, not schema text. Actual provided
  schemas may appear in `response_item.additional_tools`. Claude exposes
  skill/instruction attachments, request shapes, and deferred-tool deltas.
- m0 uses local character-count estimates calibrated to observed model and
  harness token counts where complete text exposure allows calibration.
  Publish the calibration and coverage; without a defensible conversion,
  show characters and guidance with unknown dollars. Recorded component
  token counts take precedence. No remote counting endpoint, model calls,
  or new tokenizer dependency is required. Retain derived counts,
  identities, and fingerprints, never component text beyond parsing.
- Price marginal before/after context using the request's cache positions
  when available; otherwise label an allocation assumption and range.
  Reprice the whole request across context tiers. Observe listing backfill
  and description expansion rather than summing removed entries' sizes.
  Expose separate gross overhead and fixed-behavior net estimates.
- Capture explicit typed skill invocations as well as Skill calls and
  identified file loads. Parse invocation wrappers or mentions in place
  without retaining user text. Codex `$skill` mentions show requested use,
  not proof of successful execution; keep those categories distinct and do
  not classify a requested skill as unused because no tool call followed.
- Capture direct and nested MCP calls from actual call/completion events
  where available, including provider `item_completed` records, MCP
  resources, and prompts. Script references alone are weak evidence. Track
  visibility per harness/client/version, window, and component. Missing
  nested calls or typed-skill visibility blocks automatic unused findings,
  while allowing rankings labelled incomplete.
- Own every context category once: skills and their bodies feed the skills
  check; MCP discovery/schemas/results, instruction files, agent listings,
  hook context, and plugin/app/managed instructions feed the context check.
  Attribute shared plugin overhead once and link its skill/MCP findings.
  Installed metadata or current file sizes cannot establish historical
  exposure. Verify record shapes before writing synthetic fixtures.

## Required

- [Replay and the cache TTL check](/quest/m0/replay.md) - deduplicated requests, prices, profile provenance, and structural context events
