# JSON reports and migration

`--json` emits compact schema version 2. Replace an existing `--json` invocation
with `--json-full` to keep schema version 1 and its complete per-request evidence.
Both forms use the same analysis, costs, recommendations and exit codes. Neither
contains prompt, reply, tool argument, tool output or component text.

Version 2 keeps the version 1 field names for unchanged fields. In particular,
`context` retains every component cost, request/pricing coverage, use count,
control and verdict. Every check keeps its discriminator, owning profile and
setting, history profiles, window, costs (including partial known subtotals and
unknown reasons), coverage, assumptions, uncertainty and sensitivities. Savings
from independent checks still must not be added.

Keepalive checks have the same complete shape in both versions, including every
cap's ping cost, recorded-work savings, net savings and immediate-close sensitivity.
Their period coverage, unknown current ping state, unpriced periods and unresolved
comparisons remain explicit. The sequential replay uses complete historical inputs
before either output projection, including when the scan came from SSH.

These evidence fields change shape:

| Field | Version 2 |
| --- | --- |
| `scan.requests`, `scan.events` | Selected record counts |
| `scan.cacheContext.requests`, `.boundaries`, `.prompts` | Pre-window record counts |
| `scan.snapshots` | File count, frozen bytes, deferred bytes and skipped lines |
| `scan.exposure.components`, `.requests`, `.invocations` | Record counts |
| `scan.exposure.visibility` | Groups by component name/category and coverage conditions across revisions/windows |
| `exposure.calibration[].samples` | Sample count; cohort, ratio, range and assumptions remain |
| `compactions` | Evidence groups and aggregate cost, separately for each TTL |
| Check `usageEstimates` | Groups by evidence, reason and assumptions |
| Check/scenario `cacheEvidence` | Groups by TTL, state, outcome and reason; separate predecessor groups |
| Window scenario `added` | Count, token ranges and aggregate summary/prefix costs |
| Idle scenario `periods` | Groups of outcome, status, reason and activation conditions; aggregate summary cost |
| Idle scenario `byContext` | Size count, token range, unknown-size count, period count and summed delta |

Grouped fields have `count` and `groups`; every group has its own `count`.
Cache predecessor counts count witness occurrences, including a predecessor
appearing in multiple replays. Unknown sizes have no numeric token range. A
summed delta or cost stays null if any contributing value is unknown. Empty
cost aggregates are known zero; empty ranges have null endpoints. No fixed
row limit drops distinct conditions. Request/event identifiers, individual file
boundaries, component catalogs/locations and calibration witnesses remain in
`--json-full`. Compact visibility groups identify each component by name/category and count
coverage records, not requests. Names that identify an individual rendered message,
tool result or unmapped delta are null; those records group by semantic targets,
category and provenance instead. This preserves feature coverage without listing
every message/call. Per-setting request coverage stays in `context` and checks.

Full evidence is streamed to stdout without building a serialized copy of the
history in memory. It can still produce many gigabytes. Compact output releases
evidence before building its JSON representation, but scanning and analysis still
retain historical metadata in memory. `--since` selects billing and coverage;
it does not discard older catalogs, ownership, aliases, calibration or cache
context needed to interpret the selected requests. Neither form changes or
writes transcripts or settings.

With `--ssh`, the envelope is `{"schemaVersion": 2, "hosts": [...]}` for
`--json`, and version 1 for `--json-full`. Each `hosts[]` contains `ssh` (null
for this host) and `report` in that same schema. The private SSH scan protocol
still transfers the complete metadata required for analysis with shared values;
the output choice is applied by the coordinator. Destinations need the same
build, including its scan-schema fingerprint. Costs are independent per host.

T3 adds optional `scan.t3` state coverage/snapshot bounds and `profiles[].t3`
instance ownership, plus opaque `requests[].t3`/`events[].t3` historical run
metadata in full JSON. `requestedSpeed` remains separate from each billing
part's `tier`/`speed`. The `t3-idle-compaction` check keeps all three policy costs
in both JSON forms; its `currentSettings.periods` and
`afterRecommendations.periods` use the same compact grouping as native idle
periods. Unsupported providers and auxiliary costs remain explicit exclusions.
