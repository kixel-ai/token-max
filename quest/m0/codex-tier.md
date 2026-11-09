# [M] Codex costs without recorded billing tiers

## Goal

Codex histories with measured token usage but no recorded effective service tier
show useful API-equivalent dollar estimates with explicit pricing assumptions.
Recorded billing evidence remains authoritative, and uncertain assumptions never
become claims about actual subscription bills or historical billed tiers.

## Plan

Recommended during /quest-complete on 2026-11-08 after
[kixelated/token-max#26](https://github.com/kixelated/token-max/pull/26),
with the same gap independently reported by the keepalive PR.
The maintainer authorized "do anything you recommend". ✅ Plan this in m0
(rejected: leave all such Codex usage unpriced; defer beyond first release),
because missing tiers currently suppress every Codex cost check despite
measured usage and public model prices.

- Extend the shared pricing owner, not individual checks. Prefer a recorded
  effective tier. When it is absent, present a clearly conditional standard-rate
  API equivalent or a supported requested-tier estimate, preserving the
  difference between requested and effective tiers. Current config cannot
  establish historical request settings.
- Keep the assumption and coverage visible in text, both JSON projections,
  comparisons and SSH host reports. Retain unknown results for unsupported
  models/providers, contradictory evidence and missing dimensions that cannot
  be estimated. Do not turn unknown usage into zero or claim an unconditional
  winner when tier sensitivity could reverse it.
- Verify current provider and Codex semantics before implementation. OpenAI
  project defaults can select a nonstandard tier when service_tier is omitted
  or auto, and requested tier can differ from the actual response tier. Native
  parsers own recorded billing evidence; the T3 quest owns joining its requested
  selection, not a second pricing implementation.
- Use synthetic histories to cover absent, recorded, requested and conflicting
  tier evidence, context thresholds and cache categories. Reuse existing rate
  selection and report assumptions.

Sources checked 2026-11-08:
[OpenAI project tier defaults](https://developers.openai.com/api/reference/resources/admin/subresources/organization/subresources/projects/subresources/service_tier_settings/methods/retrieve)
and [Responses service tier](https://developers.openai.com/api/reference/resources/responses/methods/create).
