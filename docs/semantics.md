# Replay semantics

The reader freezes every selected file's length during discovery before parsing
any transcript. It streams one newline-terminated JSON record at a time. Raw
objects live only through that line's normalization; retained data contains
counts, opaque identities, times, and whitelisted structural metadata. A file
that changes identity or shrinks fails. Appended bytes are outside the snapshot.
The JSON result records the discovery interval, each byte boundary, and unfinished
final-line sizes. Metadata storage grows with requests/events, not transcript text.

Valid records whose timestamp proves they precede `--since` are excluded before
usage validation. Context records still establish the subsequent model/client.
File dates and modification times cannot prove that all records precede the
window. A broken JSON record cannot safely prove its top-level timestamp, so it
fails even when the file began earlier. Diagnostics never quote JSON parser errors.
Unexpected normalization exceptions remain internal errors, distinct from malformed
JSON/UTF-8 and explicit invalid metadata. The CLI sanitizes internal failures.

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
Custom endpoints and unsupported providers/tiers are not assigned standard rates.
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
keep observed reads. Model/speed/provider/conversation changes and compaction
boundaries break inferred reuse. The lower aggregate estimate decides the winner,
even if most periods prefer the other TTL. A cold-prefix sensitivity and numeric
reuse break-even accompany the estimate and do not veto it.

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
shortest documented cache lifetime, five minutes, and the next measured read
contains at least the preceding cached prefix. That preceding prefix supplies
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
metadata, even before `--since`. Stable boundary identities deduplicate copied
boundaries across changed session metadata; their observed conversation/location
aliases remain available for attribution and cache invalidation. Warm reads can cost less;
cold writes can reach 2x input, before unknown system/tool/instruction overhead.
The proxy is included once in each outside-main TTL scenario, separately from
subagent request totals. Unknown timing and missing write splits prevent inventing
a TTL-dependent summary split. Missing proxy sizes remain unknown.

The local metadata scan found auto/manual boundaries, and no verified idle notice
discriminator. `precomputed` remains an undocumented candidate. Boundary time and
duration cannot establish generation time; absence of a boundary in a final idle
period does not prove that no summary was generated or charged. Later checks must
keep these limits when interpreting eligibility and final periods. No observed
idle fixture or reproducible historical billing total is claimed. The host has
multiple malformed complete records across several histories; the first safe
failure location is not a claim that repairing one file would enable a full audit.
