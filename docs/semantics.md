# Replay semantics

The reader freezes every selected file's length during discovery before parsing
any transcript. It streams one newline-terminated JSON record at a time. Raw
objects live only through that line's normalization; retained data contains
counts, opaque identities, times, and whitelisted structural metadata. A file
that changes identity or shrinks fails. Appended bytes are outside the snapshot.
The JSON result records the discovery interval, each byte boundary, and unfinished
final-line sizes. Metadata storage grows with requests/events, not transcript text.

Records whose timestamp proves they precede `--since` stay outside selected
billing and usage coverage. Claude requests and compaction boundaries are retained
separately in `scan.cacheContext` for predecessor, bracket and summary-link evidence.
`scan.since` records the billing selection; checks inherit it, and a comparison
cannot expand an already filtered scan to an earlier billing window.
Invalid pre-window usage becomes an unknown cache state, never a billed estimate or
silently discarded attempt; it does not fail selected billing validation. Selected
usage errors fail the scan. Requests linked across the window use their recorded
start time, so a completion after the boundary does not move old work into billing.
Context records still establish subsequent model/client metadata. File dates and
modification times cannot prove that all records precede the window. A complete
line that is not a JSON object cannot prove its timestamp either; it is skipped and
counted, and its usage is missing, wherever it falls. Each counts as one unpriced
pair in its profile's paired comparisons, so many such lines block a verdict. Real histories contain such
lines from truncated Codex records that were later rewritten and from interleaved
concurrent Claude writes. Diagnostics never quote JSON parser errors. Unexpected normalization
exceptions remain internal errors, distinct from malformed JSON/UTF-8 and explicit
invalid metadata. The CLI sanitizes internal failures.

Claude assistant entries sharing a message identity are usage snapshots. They are
merged, never summed. `api-request.id` links through assistant `requestRef` to
source and actual request-start time. Unlinked API requests remain explicitly
unpriced: they might have failed, still be running, or have hidden usage. Their
absence of assistant usage is not zero spend. Nested subagent filenames distinguish
separate conversations even when their parent `sessionId` is the same.

Claude's top-level usage overlaps message iterations and omits compaction and
advisor iterations. An absent or empty iteration list uses the top-level usage;
an empty list is a valid older client shape, not zero spend. When nonempty
iterations exist, pricing uses that list alone, with
each iteration's model. Advisor service tier is independent; an omitted tier stays
unknown. Unsupported iteration kinds stay unpriced. These semantics come from
[threshold compaction](https://platform.claude.com/docs/en/build-with-claude/compaction-threshold)
and [advisor billing](https://platform.claude.com/docs/en/agents-and-tools/tool-use/advisor-tool),
checked 2026-11-07. The metadata-only local sample had 4,180 usage records whose
top-level counters equalled their nonempty message-iteration counters. That
sample missed valid empty lists and does not establish a complete host audit.
All committed fixtures are synthetic.

Codex `token_usage_record.usage` is individual response usage, deduplicated by
`response_id`. `turn_token_usage` and `thread_token_usage` are cumulative.
Overlapping `event_msg.token_count` records are removed by matching cumulative
thread usage, not added again. Older records use `last_token_usage` where present;
otherwise only verified differences between successive cumulative counters are
used. Repeated totals add nothing. A first total or reset without a per-request
last usage stays unknown. Inherited response identities deduplicate across files. Legacy records with a
recorded turn identity also deduplicate copied counters across changed session
metadata. Without a response/turn identity, copied legacy history cannot be
proved identical from timestamps and token counts alone; the session-scoped
fallback does not claim a complete fork-history deduplication.
The [Codex protocol](https://github.com/openai/codex/blob/main/codex-rs/protocol/src/protocol.rs),
inspected 2026-11-07, defines these counters and their accumulation.

Counts are measured evidence where individually recorded; cumulative differences
are estimated and unverifiable deltas are unknown. Costs are estimates at the
catalog's present public API rates. Missing costs have both a known subtotal and
a null total, with reasons. An internal model is never replaced with a public one.
A Claude profile with a custom `ANTHROPIC_BASE_URL`, such as a gateway or proxy,
still speaks the Anthropic API and is priced as the public Anthropic API
equivalent. Unsupported providers/tiers are not assigned standard rates.
Without historical endpoint metadata, a standard Claude profile uses an explicitly
labelled public Anthropic API equivalent. Conflicting provider conditions on a
shared history stay unknown. Historical spend is never assigned to all aliases.

OpenAI input includes cached reads and writes; output includes reasoning. On
GPT-5.6 and later GPT families, recorded writes replace ordinary input at total
rates of 1.25x for 30m and 2.5x for Responses-only 12h. Missing write counters stay
unknown. An unsplit recorded write count uses an explicit default-30m assumption;
12h attribution is unknown, not derived from the catalog's single write field.
Earlier GPT models have no additional write charge. The
[OpenAI caching guide](https://developers.openai.com/api/docs/guides/prompt-caching),
checked 2026-11-07, documents the subset and lifetime rules. For catalog entries
with the legacy `context_over_200k` field, supported GPT-5.4/5.5/5.6 and GPT-6 models
use the documented 272,000-input threshold, including reads and writes. A
contradictory explicit tier fails pricing. The full request is repriced when a
counterfactual crosses a tier. See [OpenAI pricing](https://developers.openai.com/api/docs/pricing).

Claude ordinary input, reads, and writes are disjoint in the wire format; the
reader normalizes their sum as total input. Five-minute writes use the catalog;
one-hour writes derive the documented 2x ordinary-input rate. US inference uses
the documented 1.1x multiplier. Supported fast Opus rates use the documented 2x
factor across the full context window; other speed/tier combinations remain
unknown. Absent geography and the older `not_available` sentinel use the labelled
global API-equivalent assumption, without claiming an observed region. Server web search adds its public per-search fee. Web fetch has no extra
fee. Code-execution charges remain unknown without runtime, tool version, and
free-allowance evidence. Client-side calls add no separate server-tool charge.
These rules and Haiku 5.5's 100,000-input threshold are from
[Claude pricing](https://platform.claude.com/docs/en/about-claude/pricing), checked
2026-11-07. A missing service tier is not silently assigned a price.

The Claude TTL check independently replays main and outside-main requests at 5m
and 1h. In 5-to-60-minute gaps, a 1h prefix would need rewriting at 5m; a 5m rewrite
can read the surviving prefix at 1h. Reuse is limited to the preceding cached
prefix; newly appended tokens still need writing. Recorded 5m reads in such a gap
are preserved as evidence of an externally warmed prefix. First visible requests
keep observed reads. Borrowed TTL splits on estimated attempts never establish
observed warmth; replay applies the chosen lifetime to their estimated geometry.
Cache histories are separate by model, speed, provider,
conversation, agent and part kind; compaction boundaries reset them, including copied
boundary origins. The lower aggregate estimate decides the winner,
even if most periods prefer the other TTL. A cold-prefix sensitivity and numeric
reuse break-even accompany the estimate and do not veto it.

Scenarios are compared part by part. The decision sums the difference over parts
priced under both options and treats every other part as able to differ by at
most the largest priced difference. A winner is recommended only when that bound
cannot flip the sign; otherwise the check stays unknown. A handful of failed or
in-flight requests therefore cannot hide a clear winner, while a close call stays
unresolved. `coverage` gives the priced and total part counts, and the TTL result
adds `oneHourSavingsUsd` so a kept setting can state its margin. Scenario totals
still keep null and the unknown reasons.

The maintainer's [B1 cache-state decision](https://github.com/kixelated/token-max/pull/3#issuecomment-6710296074)
uses explicit known, estimated and unknown predecessor states. A recorded cache
touch establishes a known prefix in its fully resolved cohort; supported attempt
estimates establish estimated prefixes. Unsupported attempts preserve the previous
state and add an unknown possible touch, rather than silently clearing or renewing
it. Missing model/provider/speed/kind/agent spans every compatible cohort. All parts
inform cache state before the selected bucket is charged; unknown attribution
can affect either bucket when its role is unresolved. A known subagent remains in
the outside-main bucket even if its iteration kind is unknown. Selected parts with unresolved bucket attribution remain
unpriced in both conditional comparisons, whose request counts/fixed costs must
not be added. A later touch in a fully resolved cohort supersedes earlier alternatives
in that cohort, while a zero-cache
request cannot renew or erase an older cached prefix. Pre-window states are replayed
without charging them or adding them to the selected request count.

`cacheEvidence` reports predecessor request/part IDs, times, state and the outcome
of each uncertain or estimated transition. Each predecessor separately records
`prefixEvidence` (measured/estimated/unknown) and `cohortKnown`, so an estimated
prefix with missing cohort metadata retains both facts. Witnesses cover the latest possible
expired/recent predecessors and the prefix extrema in the 5-to-60-minute range;
they are not a duplicate inventory of every attempt. Every original attempt stays
in the scan. Unknown prefix lengths are evaluated at the conversion extrema, not
assigned a billed point estimate. If possible predecessors imply different cache
reads/writes, only that part's comparison stays unpriced, with no invented cache
subtotal for the request. If the geometry is invariant, the comparison can be
priced while predecessor attribution remains unknown. This is an evidence test,
not a demand that every sensitivity scenario favor the aggregate winner.

`comparisonCost` contains cache charges. `fixedCost` contains ordinary input,
output, server fees and unmatched summary proxies. TTL changes preserve total
input/context tier and output under the fixed-behavior assumption, so these fixed
terms cancel even when their price or token count is unknown. Full scenario totals
retain null and the reason. Recorded zero cache reads/writes need no cache price;
missing cache counters cannot be cancelled as if they were zero. An unsupported
model/context tier needed for nonzero cache charges keeps the comparison unpriced.
Mixed agent/source TTLs preserve the available history; a transition is inferred
only within a source/agent/model cohort, with no conflicting latest cohort TTLs.

The maintainer's [unlinked-attempt policy](https://github.com/kixelated/token-max/pull/3#issuecomment-6709040500)
allows evidence-backed estimates with sensitivity and preserves unsupported cases.
An API attempt can receive a comparison-only input estimate from its immediately
adjacent measured requests in the same conversation/file. Identical input/cache
geometry supports copying that geometry. Growing input and cacheable lengths also
support a conditional retry estimate when the entire bracket spans less than the
shortest documented cache lifetime, five minutes, and the next measured read is
positive and contains at least the preceding cached prefix. That preceding prefix supplies
warm reads; the additional next-request reads supply estimated writes at the TTL
being replayed. The next request's own new writes are excluded from the attempt's
input, with its ordinary-input count assumed unchanged. Thus estimated cacheable
length equals the next measured read, not its read plus new write. Estimated total
input must not shrink below the preceding input. It does not invent an observed
write split. The lifetime is a cache constraint, not a claimed retry timeout or
proof of retry identity, actual billing or absence of another cache warmer.

Model, source, provider, agent, version, client entrypoint and profile provenance
must agree, and recorded pricing hints must not contradict the measured requests.
Recorded compaction boundaries interrupt estimation; entrypoints are compared
directly. Claude does
not record a complete settings-change history; matching observed metadata cannot
prove that unrecorded controls stayed unchanged. Consecutive unlinked attempts,
missing brackets, shrinking geometry, inconsistent warm reads, expired growing-context brackets and
differing attribution remain unsupported. `usageEstimates` names the bracket
request IDs, estimated input geometry, recorded output limit when available,
assumptions, and reasons. Equal counts do not prove equal content or prefix identity.

The point estimate conditionally assumes one billed attempt with the bracketed
input geometry. Output is not imputed; its unknown charge stays fixed. The original
request usage, missing-usage coverage and historical spend remain unknown. Separate
no-billed/cache-affecting-attempt and cold-cache-input sensitivities replay the
whole sequence, including subsequent reuse. They show how billing/cache assumptions
can change the winner, not guaranteed bounds on hidden work, retries or changed
context. Estimated pre-window cache seeds that affect a selected request remain
in provenance and sensitivity; their own charges and request counts stay outside
the selected window. Sensitivity does not veto the aggregate estimate. Unsupported cache inputs
remain unknown and block only their affected comparison. No attempt is silently
excluded or assigned observed zero billing.

Current TTL resolution follows force-5m, bucket environment, bucket setting,
enable-1h, then defaults. Shell overrides apply only to the active config root;
each profile's settings environment is read separately. Main subscription billing
within plan versus credits is not inferred from credential files, so its unset
default can be unknown. An API key in the environment does not prove that Claude
Code accepted API billing. User settings and the platform's system managed JSON
file retain the supplying file as owner, including settings-provided environment
variables. Other managed delivery mechanisms are not inspected; this is a
conditional replay of the observed files, not proof of effective organization
policy. See [managed file locations](https://code.claude.com/docs/en/managed-settings),
checked 2026-11-07. Agent frontmatter, project/CLI controls, cloud/gateway
support, and server-controlled helpers can override a profile default. The latest
recorded client version is the labelled applicability assumption; versions absent
or before 2.1.242 stay advisory. See
[Claude cache controls](https://code.claude.com/docs/en/prompt-caching), checked
2026-11-07. There is no Codex TTL-setting recommendation.

Compaction boundaries are structural events, never billed requests. Explicit
request references associate usage; a unique summary since the preceding boundary
can receive labelled sequence attribution. Linked usage is already in request
costs and is never added again. An unmatched Claude boundary gets one separate
estimate using `preTokens` as ordinary summary input and `postTokens` as an output
proxy, with the preceding model/tier/provider assumption from that file's
metadata, even before `--since`. Pre-window boundaries consume their summary
claims and update attribution limits in the same order as a full scan; only selected
boundary charges are emitted. Stable boundary identities deduplicate copied
boundaries across changed session metadata; their observed conversation/location
aliases remain available for attribution and cache invalidation. Warm reads can cost less;
cold writes can reach 2x input, before unknown system/tool/instruction overhead.
The proxy is included once in each outside-main TTL scenario, separately from
subagent request totals. Unknown timing and missing write splits prevent inventing
a TTL-dependent summary split. Missing proxy sizes remain unknown.

The earlier local metadata scan found auto/manual boundaries but no idle notices.
The native v2.1.316 informational-notice shape is now verified from the installed
provider binary and recognized as described below. `precomputed` remains an undocumented candidate. Boundary time and
duration cannot establish generation time; absence of a boundary in a final idle
period does not prove that no summary was generated or charged. Later checks must
keep these limits when interpreting eligibility and final periods. No observed
idle fixture or reproducible historical billing total is claimed.

## Compaction windows

Each effective setting key gets one aggregate comparison across its affected
models, so shared global/environment controls cannot receive conflicting changes.
Window comparisons reuse the TTL check's cache replay, input estimates, price
components, deduplicated histories and summary attribution. They run after the
recommended TTLs. Their baseline applies the current explicit window to the same
selected requests; it does not present savings from an older, larger historical
window as savings from today's smaller setting. The comparison starts at explicit
`--since`, or available selected history after any inferred TTL transition.
Pre-window usage may calibrate geometry and cache evidence but is never charged.

Claude's current environment, user file and managed file resolve to the owning
window control. Per-model values override a global value within one scope; a
higher scope's global value beats a lower scope's per-model value. Dated native
model IDs share the undated row, with exact matches preferred. Unknown tuned
defaults are not guessed. The latest recorded client version is the applicability
assumption. Per-model controls require v2.1.288; environment suffixes require
v2.1.304. Project, launch and per-agent overrides remain an explicit limitation of
the observed-file analysis. See [Claude model configuration](https://code.claude.com/docs/en/model-config#context-window-and-auto-compaction)
and [settings reference](https://code.claude.com/docs/en/settings-reference#modelsettings),
checked 2026-11-07. Disabled compaction stays advisory.

Codex's explicit `model_auto_compact_token_limit` is a total-context threshold by
default. An unset limit, unresolved named profile, or `body_after_prefix` scope
stays advisory; the latter is unpriced because a total-context replay would model
the wrong control. See [Codex configuration](https://developers.openai.com/codex/config-reference),
checked 2026-11-07. Compacted output can retain items beyond the summary, so summary
output tokens are not used as the resumed prefix. See [OpenAI compaction](https://developers.openai.com/api/docs/guides/compaction),
checked 2026-11-07.

Candidate windows use catalog tier boundaries, the current explicit limit,
and cohort median resumed-prefix sizes plus ordinary input, within provider bounds.
Claude also includes its documented 100k minimum. Candidate count follows
pricing/cohort metadata rather than the number of distinct input counts. This finite empirical search does not claim the optimum over every
integer threshold. It evaluates only earlier compactions; lost historical context
cannot be reconstructed to price larger windows. Every observed compaction remains
once, whether linked usage or an unmatched proxy, including when an earlier pass
might replace it. Those replacement effects are not claimed as observed savings.

Before an affected request, a modeled pass reads the entire estimated context at
ordinary input rates and writes the median observed summary output in a matching
model/provider/agent/tier/speed/geography cohort. Its resumed cache prefix uses the
median first post-boundary prefix, which can contain instructions, retained items
and new context. Added passes require linked billed summary output. Claude `postTokens` remains
a proxy only for unmatched observed charges in the baseline; it cannot
establish output size for calibrating additional passes. Disagreeing summary-model
attribution leaves the affected comparison unpriced. JSON gives each supporting
boundary and resumed request identity. The new prefix is cold on the resumed
request, whose charge already includes it; `prefixCost` is attribution, not another
charge to add. Subsequent growing prefixes reuse only within the chosen Claude
lifetime or the documented default 30m billed Codex lifetime. A new prefix cannot
inherit external warming of the replaced transcript.

Observed boundaries reset modeled reductions. Ambiguous multipart geometry,
missing affected usage, a missing compaction sample, model/agent/provider changes,
unexplained context decreases and unsupported cache retention leave the rest of
that session unpriced under the candidate. Other sessions stay priced and the
same part-by-part decision rule as the TTL check applies; an unresolved current
window control still leaves the comparison unpriced. Unchanged unknown costs
may cancel; affected input/cache prices and context tiers are repriced, including
output when its unit rate changes. Full totals keep unknown charges even when the
comparison is priced. Bracketed attempt estimates retain their witnesses and never
turn original missing billing into measured usage.

The largest estimated aggregate net saving selects the recommendation. Cohort
maximum summary/prefix sizes and cold-summary charges are conditional sensitivities,
not vetoes or guaranteed bounds. Extra work from lost detail, file re-reads, tool
calls and changed reasoning is unmeasured. The report gives the extra dollar cost
that would erase the modeled saving. Window savings are incremental to their named
TTL baseline; separate model/check savings overlap and must not be summed. The idle
compaction on/off policy is evaluated afterward as described below.

## Native idle compaction

`idle-compaction` compares on/off at current TTL/window settings and again after
applicable TTL/window changes. Each owning idle toggle gets an aggregate delta,
on cost minus off cost, across request gaps and final observation periods.
Negative favors on, positive favors off, and exact zero retains the current
value. Only the latter configuration decides the toggle. Independent savings
are not additive. API-price equivalents do not compare subscription bills.

Current idle controls resolve user, project, local and managed settings, with
the active profile's environment applied to environment controls. Project files
use the nearest current Git root, or the recorded working directory for a
non-Git project. Absent directories or working-directory metadata leave ownership
unresolved. Project TTL/window owners are preserved when a profile-level change
would not override them; a conversation with changing effective policies stays
conditional. Unrecorded launch overrides and historical file contents are not
reconstructed. A historical setting event is evidence for that time, never a
current owning file. Without a recorded toggle transition, the window is explicit
`--since` or available history, labelled as replay under today's controls.

The [native settings reference](https://code.claude.com/docs/en/settings-reference#idlecompaction)
and [cache behavior](https://code.claude.com/docs/en/prompt-caching#compacting-the-conversation),
checked 2026-11-07, establish the true default, v2.1.290 toggle support,
pre-v2.1.314 feature-flag prerequisite, enabled auto-compaction, 1h main TTL and
200k default idle threshold with a 100k environment floor. Threshold tuning is
outside this check. A 5m main TTL or window at/below the threshold removes the
eligible band. SDK/headless/subagent/hosted-remote runs and active tool waits do
not become interactive opportunities. `cli` needs `repl_main_thread` provenance;
`claude-vscode` and `ssh-remote` identify supported native hosts. A Desktop label
alone does not prove local execution. Unknown versions and pre-314 flag state
stay unknown. T3 and SSH adapters are not introduced.

The installed Anthropic v2.1.316 binary was inspected without opening transcript
content. Its native notice constructor emits `type: system`, `subtype:
informational`, `level: notice`, and either documented idle notice followed by
the exact toggle suffix. The v2.1.319 transcript writer also verifies `parentUuid`
lineage through compact-summary and attachment records. Only that shape and an
explicit lineage to an automatic boundary attribute an applied idle. Missing,
conflicting, cyclic or ordinary-message ancestry stays unattributed, rather than
tagging an older compaction by timestamp; raw notice text is discarded. `precomputed` also
appears on non-idle background compaction and cannot attribute idle generation.
No new native historical-settings or feature-flag record shape is invented.
The normalized analyzer can consume explicit historical settings evidence; absent
such evidence, native histories use the documented-trigger fallback rather than
reporting today's settings as measured past on/off outcomes.

The public trigger is about one hour, before expiry, with an open session and
awake host. Native v2.1.316 arms at 90% of the cache lifetime and refuses late
execution. Replay uses that inspected timing as its labelled point estimate and
one hour as timing sensitivity, not a configurable or documented 55-minute
policy. Request start controls cache age; active requests spanning the trigger
are excluded. Near-expiry returns retain a timing range. Boundary time and
`durationMs` never establish generation time. A recent final period is censored;
no session-end marker or closure is inferred.

Recorded applied returns use their actual summary usage/proxy and resumed reads
and writes. Known-on returned nonapplications are applied-outcome evidence,
not proof of no discarded generation. Their first request remains the baseline;
a merely long gap does not create an observed on charge. For unobserved
activation, comparable known-on returned applied fractions transfer only within
identical version/client/flag state. Finals use the same fraction, charged once
without multiplying by a second unknown stayed-open probability. Without a
comparable cohort, one open-and-awake activation is the deciding assumption.
Unlogged/discarded summary charges remain explicitly unobserved and can change
the conditional estimate; no missing boundary establishes a known zero charge.

Hypothetical summaries require linked billed output and first resumed-prefix
samples in the same model/provider/agent/tier/speed/geography/client/version cohort.
Median output and full prefix, plus measured subsequent growth, form the point
estimate; cohort maxima form size sensitivity. The full prefix includes
system/project context, retained items and new input. `postTokens` remains only
an unmatched observed summary-output proxy and cannot calibrate that full prefix.
Summary instruction overhead uses observed summary-input excess over preTokens;
unknown framing/changed behavior remains a limitation. Summary input reads the
warm preceding cache prefix and writes new context at the outside-main TTL.
A different summary cache cohort must write cold input. New summary text is
not assumed already cached on the first resume.

Actual resumed cache evidence takes precedence over gap-only expiry inference,
including possible same-directory warming. On an altered prefix, calibration
estimates the unchanged leading system/project footprint from post-compaction
reads, capped below summary output; observed reads can establish warmth for
that unchanged footprint. Other replacement context reuses only after a modeled
request has written/read it within its lifetime. Timing is estimated from the
trigger and outside-main TTL, never the applied boundary. Context reductions
and retained off-history carry through later requests, repricing context tiers
and later summary input until an observed compaction converges the paths.
Multipart/cohort changes or unexplained input decreases leave affected costs
unpriced. Each linked or unmatched observed summary is charged once.

Uncertain activations branch weighted context states, merging equal states after
replacement or convergence; the expected cost does not compact a fractional
number of tokens across price tiers. Final summaries receive their activation
weight with no return savings. Full on/off totals retain common unknown charges;
unchanged unknown components may cancel in the comparison only. Closed/open,
maximum-summary and late-trigger scenarios explain uncertainty without vetoing
the aggregate point winner. When closed/open endpoints bracket a crossing,
a common uncertain activation fraction is replayed to find a break-even, including
sequential effects. No bracket is not proof that no internal crossing exists.
Extra re-read cost per expected resumed summary would erase on's saving; it is a
fixed-behavior caveat, not an unbounded recommendation input. Context-size totals
include summary-only final periods.

## Shared component measurements

`scan.exposure` adds historical component revisions, request views, invocation
evidence, and client/version/window visibility. Components retain Unicode
character counts, opaque identities and fingerprints, source locations, one
owner (`skills` or `context`), and a server/namespace attribution key, or a plugin key where the record explicitly
identifies it. Listing entry spans are members of their aggregate;
member sizes must not be added to the aggregate. Listing replacement captures
backfill and description expansion as a new aggregate revision. It does not
assume removing one entry shrinks the next listing by that entry's size.

Codex full/delta `world_state` records supply rendered host skill listings,
namespace descriptions, instruction text, removals and opaque schema identities.
Structured skill entries and policy flags retain unknown sizes. In particular,
`top_level_tools` hashes are never tokenized. `response_item` items of type
`additional_tools` supply actual provided schemas. Claude attachments supply
rendered listings, instruction files, prompt/tool snapshots, deferred-tool and
agent-listing deltas, surfaced schemas, and server instructions. Deferred schema
records are distinguished from exposed definitions. Unclassified rendered
attachments stay visible as context evidence with unknown category.

Chronological attachment/world state is candidate exposure, not proof of exact
request input. Claude request `base`/`keep`/`tail` digest chains reconstruct
identity sequences where their base is available, without interpreting hashes as
text or inventing a hash-to-message mapping. Only recorded full Messages API
`system`/`messages`/`tools` parameters establish wire exposure. Compaction clears
candidate state until new evidence arrives. Pre-window components are marked
inherited; conflicting copied histories leave exposure unknown. Current effective
enablement remains separately unresolved. No installed/current file size is used
to establish historical exposure.

`report.exposure` publishes empirical calibration samples only for linked,
measured, single-part, complete text-only inputs with a matching request/response
model, grouped by harness, model and provider. Claude requests that record a
shape and only known text message blobs are complete. A blob holding an image,
document or redacted thinking is not text and blocks calibration; a thinking
block's opaque `signature` is dropped and every other field, tool input
included, counts. It uses the observed input tokens per Unicode character
of serialized system/messages/tools, including framing. Transfer to component
text is an estimate; sample extrema are sensitivity scenarios, not guaranteed
bounds. Recorded component token counts take precedence in the shared sizing
primitive. Without matching calibration, output keeps characters and unknown
tokens/dollars. Images, opaque blocks, deferred definitions, digest-only requests,
and multi-iteration totals cannot calibrate partial text.

`report.context` prices each piece of recurring context. Only Claude requests
with a recorded `api-request-shape` or full parameters qualify: tool
definitions, then the system prompt, lead the prompt in recorded order.
Prefix membership follows each request's own tool list, not where a component
was first seen. Attachments such as skill listings and instruction files ride in
messages at no recorded offset, so their history bounds them instead: a
revision the session's previous request carried lies between the end of the
system prompt and the end of that request's input, a new or changed revision was
appended after it, and with no usable previous request, or when the tools or
system prompt changed since it, it lies anywhere after the system prompt. An attachment is priced only when every offset in its bounds
falls in the same cache slot; otherwise it stays unpriced for that request. A
row with any unpriced piece in a request, such as an MCP server whose tool is
priced but whose instructions are not, is unpriced for that request. Each piece's calibrated
tokens are charged at the slot they fall in: cached reads, then 1h writes, then
5m writes, then ordinary input, at the request's model, provider and context
tier. A request whose calibrated pieces exceed its recorded input is left
unpriced rather than clipped. `costUsd` covers priced requests only;
`estimatedUsd` scales it to every request that carried the piece and is null
when none could be priced, which the report shows as unknown, never $0. A piece repeated within one request counts
once. Skill listings split into their member skills plus the listing framing;
MCP tools group by server. One-off attachments, skill bodies and tool results
are excluded because no setting removes them up front.

Each row names the setting that removes the piece: `enableArtifact` and
`enableWorkflows` for those tools, a bare-name `permissions.deny` rule for
other tools and `mcp__<server>` for a server, which
[remove the tool from context](https://code.claude.com/docs/en/permissions),
and `skillOverrides` set to `user-invocable-only` for a skill. Instruction files
have no switch. Core tools, the system prompt and the listing framing are always
sent. Calls count `tool_use` IDs in the window once, even when copied into several
transcript files. A copy cannot say which profile made a call, so it counts for
every row of its tool that shares a profile with it; skill uses count only the
model's Skill calls, since a hidden skill can still be typed. A piece still sent
today is recommended off when `estimatedUsd` divided by its uses, counting no
use as one, exceeds `maxCostPerUseUsd` (default $1, `--max-cost-per-use`);
otherwise it is kept. The ratio does not depend on the window's length. A piece
with no priced request gets no verdict. The cost is gross: removing a piece leaves later
cache writes, compaction timing and replacement work unmodeled, and rows are not
additive with the TTL or compaction checks.

Invocation evidence separates typed and mentioned skill requests, command
wrappers, unresolved references, Skill calls, identified SKILL.md reads, direct MCP calls, actual
`item_completed.McpToolCall` completions, resource/prompt calls, and weak script
references. Body/result text is sized transiently and discarded. A request for a
skill is not proof of successful execution. A textual mention or command wrapper
needs a known historical catalog entry to count as a skill request; otherwise it
remains a weak reference. Meta user-role attachments are excluded from mention
inference. Codex user aliases without a shared message identity are matched by
their corresponding equal-text occurrence, with that identity basis disclosed. Observed nested completions establish
positive evidence, never complete nested visibility. Corrupt pre-window exposure metadata invalidates subsequent candidate state and
adds a coverage issue, preserving the reader's existing billing-window semantics.
Calibration
estimates outside the request's measured input support stay unpriced.

No supported client/version
currently guarantees complete typed-skill or nested-call visibility, so automatic
unused findings remain blocked. Context verdicts rest on cost per recorded use,
not on a claim that a piece is unused, and change no local agent settings.

Record shapes were checked on 2026-11-07 using local structural metadata only;
no real transcript text was printed or copied into fixtures. Claude's existing
synthetic request tail was corrected to the verified digest-string shape. Full
wire parameters follow the public Messages API, and typed skill input follows
[Codex UserInput](https://github.com/openai/codex/blob/main/codex-rs/protocol/src/user_input.rs).
Current provider guidance distinguishes skill discovery from body loading:
[Codex skills](https://learn.chatgpt.com/docs/build-skills),
[Claude skills](https://code.claude.com/docs/en/skills), and
[Claude deferred tools](https://code.claude.com/docs/en/mcp#scale-with-mcp-tool-search).
[OpenAI tool search](https://developers.openai.com/api/docs/guides/tools-tool-search)
documents provided tool definitions. All guidance was checked on 2026-11-07.

## Speed controls and premiums

`checks` includes the `fast-mode` variant, using the same report, rendering,
evidence, action and exit policy as the other checks. Its `current` and
`recommended` values describe speed after known disabling controls;
`replacement` names the owning setting syntax. `observed.premium` is the selected
window's rate-only premium at present API rates, with known subtotal and unknown
total kept separate. `currentEstimate.premium` applies the current effective
speed preference to those tokens. Only the latter supplies this check's
`savingsUsd` when it recommends a change. It is a rate-only comparison, not net
of cache switching. A standard current preference keeps, regardless of old
premium usage. No records means unknown dollars, not zero.

Claude's default is off; per-session opt-in suppresses a saved true preference
at startup. Managed false, environment disable, unsupported selected models and
project/local overrides take precedence. Only active-profile shell environment
is applied. The tool observes local files, not live session toggles, CLI
`--settings`, server policy or availability checks. Codex gates default on in
current documented versions; fast/priority and ultrafast use independent gates.
Its local system/user files and requirements are inspected. Legacy
`profile`/`profiles` configuration is version-dependent (removed in 0.134.0);
new `NAME.config.toml` files require an unobserved `--profile` selection. Those
scopes and project trust/launch selection stay advisory when unresolved.

Requested controls have their own `Request.requestedSpeed` metadata and never
populate billed `BillingPart.tier`/`speed`. In particular the native
[Codex TokenUsageRecord schema](https://github.com/openai/codex/blob/main/codex-rs/protocol/src/protocol.rs)
contains no billed tier. Native Codex historical premiums therefore remain
unknown; current-setting estimates can still use the recorded counts. Claude
response `usage.speed` and `usage.service_tier` identify observed rates, including
standard fallback after a fast request. Missing speed is unknown for this check,
even where the general historical cost estimate labels a standard-speed
assumption. Each sampling iteration keeps its own model and billing metadata.

The [OpenAI API price table](https://developers.openai.com/api/docs/pricing),
checked 2026-11-07, gives supported GPT-6 and GPT-5.6 fast rates at 2x standard,
GPT-5.5 at 2.5x and GPT-5.4 at 2x below/equal 272K input. Verified ultrafast rates
for GPT-6 Astra and GPT-6.1 Sol are 6x across their context tiers. Older or
unsupported combinations remain unknown. These API rates are unrelated to
subscription/credit multipliers. The existing same-model pricing path preserves
reads, writes, write lifetimes, context tiers and Claude geography, with unchanged
server fees excluded from the difference. Unverified usage and missing prices
cannot supply a complete premium. Claude's fast and standard caches are separate;
recorded cache categories are held constant and cold-cache switching costs are
excluded. Monthly amounts extrapolate the selected window, independently of
other checks, with fixed work and no valuation of latency.

Current primary guidance: [Claude fast mode](https://code.claude.com/docs/en/fast-mode),
[Claude settings](https://code.claude.com/docs/en/settings-reference#fastmode),
[Codex speed](https://learn.chatgpt.com/docs/agent-configuration/speed),
[Codex configuration](https://learn.chatgpt.com/docs/config-file/config-reference),
[profile migration](https://learn.chatgpt.com/docs/config-file/config-advanced#profiles),
[API fast mode](https://developers.openai.com/api/docs/guides/fast-mode) and
[API ultrafast](https://developers.openai.com/api/docs/guides/ultrafast-mode),
checked 2026-11-07. Synthetic metadata follows these schemas; no real session
content is stored or tested.
