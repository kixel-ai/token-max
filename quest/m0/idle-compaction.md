# [M] Idle compaction check

## Goal

`token-max` reports how often Claude Code sessions have idle-compaction
opportunities, compares `idleCompaction` on and off at public API prices,
and recommends either value when supported by the evidence. When logs
cannot establish a winner, it reports conditional savings and the
break-even instead of claiming measured savings or silently keeping the
current value. It never edits settings or prints transcript text.

## Plan

Decided 2026-11-06 (maintainer): measure whether compacting before the cache
expires would have saved money, including suggesting off when summaries
cost more than they save. This is part of m0's compaction analysis. The
on/off recommendation uses the effective token threshold; threshold tuning
is outside this quest. Report costs by context size so that the aggregate
does not conceal differences between small and large sessions.

The [settings reference](https://code.claude.com/docs/en/settings-reference#idlecompaction)
and [cache behavior](https://code.claude.com/docs/en/prompt-caching#compacting-the-conversation),
checked 2026-11-06, define the available controls:

- `idleCompaction` defaults to true and needs auto-compaction enabled and a
  one-hour cache TTL. It runs before cache expiry after about an hour since
  the last request, in an open session on an awake computer. No documented
  setting selects a 55-minute timer.
- Supported clients are the interactive terminal, VS Code's chat panel,
  and Desktop's Code tab running locally or over SSH. Validate how client
  entrypoints identify these; a desktop label alone does not establish
  local execution. Unknown clients, SDK/headless runs, sub-agents, and
  active tool waits are not confirmed interactive idle opportunities.
- The default context threshold is 200,000 tokens.
  `CLAUDE_CODE_IDLE_COMPACT_MIN_TOKENS` changes it, with a floor of 100,000.
  The auto-compact window can remove the eligible size band entirely.
- The toggle requires v2.1.290 or later; older versions may compact while
  idle but ignore the key. Before v2.1.314, availability also depends on
  feature flags. Report unsupported controls or blocked prerequisites
  instead of recommending a toggle that cannot take effect.

Start by establishing which events the logs expose. Compare structural
metadata from real resumed and unresumed idle summaries before defining
an identification rule, retaining no text. Current `compact_boundary`
records label triggers `auto` or `manual`, not `idle`; `precomputed: true`
is an undocumented candidate signal, not a guaranteed idle label. A
boundary may appear only on resume. A missing boundary therefore does not
prove that no idle summary was generated or paid for. Use synthetic parser
fixtures matching verified metadata, not invented idle events. If the
format cannot distinguish idle summaries, report that limit and model
opportunities conditionally.

The check reads effective settings, their owners, and recorded versions.
Use historical settings only when recorded. If the last change is unknown,
use `--since` when supplied, otherwise the available transcript window,
and label it as a replay under today's settings, not measured historical
on/off performance. Unknown versions stay unknown; do not invent a date
when the feature became available on this host.

Report request-gap opportunities, confirmed eligible periods where the
metadata permits them, resumed periods both before and after cache expiry,
and final periods without an observed return. Recent final periods that
have not reached the idle trigger are censored, not abandoned sessions.
No session-end marker is assumed. For older final periods with unknown
client lifetime, bound cost between closure before the trigger (no summary)
and staying open and awake (one charged summary). The same uncertainty
applies to intervening gaps when the client could have closed and resumed.

Compare on and off over the same window, first at current settings and
then after the TTL and compaction-window recommendations. Identify the
baseline for each saving; independent savings are not additive. With a
recommended 5m TTL or a window below the idle threshold, report that idle
compaction is inapplicable. Keepalive is a later alternative to this policy.

Price on's warm summarization input, summary output, and subsequent cache
reads and writes against off's retained history, cold resumes, and later
manual or automatic compactions. A compaction request has its own TTL
bucket; use the summary-to-resume delay and preserve any unchanged cached
system/project prefix. Account for returns after summarization but before
the original cache expiry too. Use observed usage if available; otherwise
estimate from `preTokens` at the read price and `postTokens` as a summary
size proxy, stating which overhead and cache layers cannot be separated.
Carry context changes forward until the paths converge, counting observed
compactions once. Do not claim the uncompacted future is observed in an
already-compacted transcript.

Show the net over the window and per month, the assumptions, and a range
for unknown client lifetime and unobserved summary sizes. Price summaries
in unresumed sessions as well as resumed ones. Report the break-even
fraction of uncertain periods that must have stayed open to change the
winner and the extra re-read cost per resumed summary that would erase
on's saving. This exposes unobserved detail recovery without demanding an
unbounded guarantee about future behavior. Recommend on or off under the
stated fixed-transcript replay when observed evidence selects a winner;
otherwise report which side of the break-even favors each setting as a
conditional result, without an actionable change or nonzero exit. These
are API-price estimates, not reductions to a subscription bill.

Synthetic tests cover both recommendation directions, conditional results,
final periods with unknown client lifetime, and ineligible clients, TTLs,
sizes, or controls. Include a resumed idle with savings, an unresumed
summary that only adds cost, a return before original expiry, a summary
whose own cache has expired, and compaction events without double counting.

## Required

- [Replay and the cache TTL check](/quest/m0/replay.md) - request metadata, prices, report, and recommended TTL
- [Compaction window check](/quest/m0/compaction.md) - context and compaction costs, and recommended windows
