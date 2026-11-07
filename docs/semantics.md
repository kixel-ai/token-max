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

Claude assistant entries sharing a message identity are usage snapshots. They are
merged, never summed. `api-request.id` links through assistant `requestRef` to
source and actual request-start time. Unlinked API requests remain explicitly
unpriced: they might have failed, still be running, or have hidden usage. Their
absence of assistant usage is not zero spend. Nested subagent filenames distinguish
separate conversations even when their parent `sessionId` is the same.

Claude's top-level usage overlaps message iterations and omits compaction and
advisor iterations. When iterations exist, pricing uses that list alone, with
each iteration's model. Advisor service tier is independent; an omitted tier stays
unknown. Unsupported iteration kinds stay unpriced. These semantics come from
[threshold compaction](https://platform.claude.com/docs/en/build-with-claude/compaction-threshold)
and [advisor billing](https://platform.claude.com/docs/en/agents-and-tools/tool-use/advisor-tool),
checked 2026-11-07. The metadata-only local sample had 4,180 usage records whose
top-level counters equalled their message-iteration counters. This sample does
not establish a complete host audit and is not a committed fixture.

Codex `token_usage_record.usage` is individual response usage, deduplicated by
`response_id`. `turn_token_usage` and `thread_token_usage` are cumulative.
Overlapping `event_msg.token_count` records are removed by matching cumulative
thread usage, not added again. Older records use `last_token_usage` where present;
otherwise only verified differences between successive cumulative counters are
used. Repeated totals add nothing. A first total or reset without a per-request
last usage stays unknown. Inherited response identities deduplicate across files.
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
unknown. Server web search adds its public per-search fee. Web fetch has no extra
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

Current TTL resolution follows force-5m, bucket environment, bucket setting,
enable-1h, then defaults. Shell overrides apply only to the active config root;
each profile's settings environment is read separately. Main subscription billing
within plan versus credits is not inferred from credential files, so its unset
default can be unknown. Agent frontmatter, project/CLI controls, cloud/gateway
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
proxy, with the preceding model/tier/provider assumption. Warm reads can cost less;
cold writes can reach 2x input, before unknown system/tool/instruction overhead.
The proxy is included once in each outside-main TTL scenario, separately from
subagent request totals. Unknown timing and missing write splits prevent inventing
a TTL-dependent summary split. Missing proxy sizes remain unknown.

The local metadata scan found auto/manual boundaries, and no verified idle notice
discriminator. `precomputed` remains an undocumented candidate. Boundary time and
duration cannot establish generation time; absence of a boundary in a final idle
period does not prove that no summary was generated or charged. Later checks must
keep these limits when interpreting eligibility and final periods. No observed
idle fixture or reproducible historical billing total is claimed.
