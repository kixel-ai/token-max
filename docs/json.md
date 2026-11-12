# JSON reports and migration

`--json` emits schema version 2, an aggregate report with the same analysis,
costs, recommendations and exit codes as the text report. It contains no prompt,
reply, tool argument, tool output or component text.

`--json-full` and schema version 1, which listed every request, event, file and
component sighting, are removed: token-max summarizes settings and recommends
changes rather than exporting evidence. Replace `--json-full` with `--json`; the
table below lists what each removed evidence field became. Nothing replaces the
per-request lists.

Version 2 keeps the version 1 field names for unchanged fields. In particular,
`context` retains every component cost, request/pricing coverage, use count,
control and verdict. Every check keeps its discriminator, owning profile and
setting, history profiles, window, costs (including partial known subtotals and
unknown reasons), coverage, assumptions, uncertainty and sensitivities. Savings
from independent checks still must not be added.

Each `context[]` row now also has `details` and `instructionSize`.
MCP server `details` contain `mcp-tool` and `mcp-discovery` rows with the same
cost and coverage fields; these costs are already included in the server total.
Do not sum parent and detail rows. Each estimate uses its own priced-request
coverage, so differing coverage can also make detail estimates sum to a
different amount than the parent estimate. A tool's `control` blocks just that
tool; recommendations to disable a whole server cover its tool recommendations in
the text report and settings snippet. A covered tool's `verdict` is null, while
its cost, use count and control remain available. `instructionSize` is null without
recorded text, otherwise `{ "bytes": N, "lines": N }` for the latest recorded
revision. Codex sizes measure rendered text, including any wrappers, and do
not establish the source-file chain size. Explanatory guidance is generated only by the
text renderer.

`historical[]` includes `pricedRequests`, `conditionalRequests`
and `tierSensitivity` (served-tier scenarios with full `Cost` values). A numeric
conditional estimate does not increase `scan.coverage.tierKnown`. Context rows
include `conditionalRequests`, `tierUnknownRequests` (including unpriced requests)
and pricing `assumptions`; their verdict stays null
when an unrecorded tier could reverse keep/off. Compaction and keepalive scenario
dollars remain available, but recommendation savings and winners stay unresolved
when affected costs depend on missing tiers. Unsupported tier
scenarios have a null total and explicit unknown reasons, never a zero total.

Keepalive checks keep their complete shape, including every
cap's ping cost, recorded-work savings, net savings and immediate-close sensitivity.
Their period coverage, unknown current ping state, unpriced periods and unresolved
comparisons remain explicit. The sequential replay uses complete historical inputs
before the output projection, including when the scan came from SSH.

These version 1 evidence fields change shape:

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
boundaries, component catalogs and calibration witnesses are not reported. Visibility groups identify each component by name/category and count
coverage records, not requests. Names that identify an individual rendered message,
tool result or unmapped delta are null; those records group by semantic targets,
category and provenance instead. This preserves feature coverage without listing
every message/call. Per-setting request coverage stays in `context` and checks.

The report releases evidence before building its JSON representation, but
scanning and analysis still retain the historical metadata the checks read.
`--since` selects billing and coverage; it does not discard older catalogs,
ownership, aliases, calibration or cache context needed to interpret the selected
requests. The report never changes or writes transcripts or settings.

With `--ssh`, the envelope is `{"schemaVersion": 2, "hosts": [...]}`. Each
`hosts[]` contains `ssh` (null for this host) and `report` in that same schema.
The private SSH scan protocol transfers the metadata the analysis needs with
shared values; the coordinator analyzes and reports it. Destinations need the
same build, including its scan-schema fingerprint. Costs are independent per host.

A compaction check has `unset: true` when no setting names its window, so its
`current` is Claude Code's tuned window for the model: the latest observed
automatic-compaction trigger or the documented default. Its owner is then the
model's `modelSettings` row, and Claude `profiles[].compaction.tuned` lists
the tuned window and reason for every model it used.

T3 adds optional `scan.t3` state coverage/snapshot bounds and `profiles[].t3`
instance ownership. The `t3-idle-compaction` check keeps all three policy costs;
its `currentSettings.periods` and
`afterRecommendations.periods` use the same compact grouping as native idle
periods. Unsupported providers remain explicit exclusions. `scan.t3[].textGeneration`
names the resolved text-generation `selection`, its `owner` setting, its
`resolution`, narrower `overrides` and the count of `projectOverrides`; its
cost is unmeasured. `scan.t3[].unresolved` counts the window's requests whose
control owner stays unresolved, keyed by cause. `scan.t3[].removed` counts the window's
requests whose every T3 owner was removed from settings, keyed by instance;
their cost groups name the profile id `t3:<state>:<instance>`, which no
current profile has.
