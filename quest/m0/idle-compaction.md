# [M] Idle compaction check

## Goal

`token-max` reports how often Claude Code sessions have idle-compaction
opportunities, compares `idleCompaction` on and off at public API prices,
and recommends whichever has the lower estimated total net cost over the
analysis window. It reports the assumptions and uncertainty beside the
recommendation and never edits settings or prints transcript text.

## Plan

Decided 2026-11-06 (maintainer): measure whether compacting before the cache
expires would have saved money, including suggesting off when summaries
cost more than they save. This is part of m0's compaction analysis. The
on/off recommendation uses the effective token threshold; threshold tuning
is outside this quest. Report costs by context size so that the aggregate
does not conceal differences between small and large sessions.

Clarified 2026-11-06 (maintainer): recommend the setting that is net better
or worse overall. Aggregate the estimated savings and extra costs across
all periods; do not require a win on each period or across every possible
scenario. Uncertainty explains the estimate rather than imposing that
stronger recommendation policy.

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

Use the replay check's verified compaction signals and estimated summary
charges. If the format cannot distinguish idle summaries, report that
limit and use the explicit activation estimate below. For returned gaps with known
settings and versions, use observed applied/not-applied outcomes and the
first request's actual usage as the baseline. Do not charge on for a
summary merely because a returned gap was long enough. Report activation
rates by version and client where known, and compare them to the
break-even rate; do not transfer those rates to a different version or
feature-flag state. An unlogged discarded summary cannot be established
from a missing applied boundary; expose that limit separately.

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
applies to hypothetical on scenarios whose activation is unobserved, not
to replacing known returned outcomes with assumed activation.

For the deciding estimate, use recorded on outcomes where available. For
unobserved eligible periods, use the applied-summary fraction from
comparable eligible known-on periods with the same version, client, and flag state,
when available; otherwise assume the documented trigger fires once with
the client left open and awake. Apply that activation estimate to both
hypothetical resumed periods and final periods without an observed
activation, charging a summary in the latter without a return benefit.
The empirical fraction includes nonactivation from client lifetime; do not
multiply it by a second unknown stayed-open probability. State that final
periods are assumed to activate at the same rate as the comparable
returned periods. Label the empirical basis or documented-trigger fallback
and show the closed/open alternatives as sensitivity. Unknown activation
and client lifetime therefore have stated deciding estimates, rather than
being reasons to withhold the net recommendation.

Compare on and off over the same window, first at current settings and
then after the TTL and compaction-window recommendations. Identify the
baseline for each saving; independent savings are not additive. The latter
configuration decides the recommended toggle; the current-settings result
is reported for context. With a recommended main-conversation 5m TTL or a
window below the idle threshold, report that idle compaction is
inapplicable. Keepalive is a later alternative to this policy.

Price on's warm summarization input, summary output, and subsequent cache
reads and writes against off's retained history, cold resumes, and later
manual or automatic compactions. A compaction request shares the
outside-main TTL bucket with sub-agents, so use both TTL recommendations.
Prefer actual resumed-request cache reads/writes over inferring cache
survival from the gap: other same-directory sessions may keep a prefix
warm. Preserve any unchanged cached system/project prefix. Without observed
generation timing, estimate summary-to-resume delay from the documented
near-expiry trigger and label it uncertain; boundary time and `durationMs`
are not generation timing. Returns near expiry need a timing range, not an
asserted before/after classification. Use the replay check's usage or
estimated summary charges. For carry-forward, calibrate full post-compaction
context from observed resumed requests, including system/project context
and new input. `postTokens` alone is a summary-size proxy, not the full
request; a summary-only lower bound cannot support a savings
recommendation. Carry changes forward until the paths converge, counting
observed compactions once. Do not claim the uncompacted future is observed
in an already-compacted transcript.

Show the net over the window and per month, the assumptions, and a range
for unknown client lifetime and unobserved summary sizes. Price summaries
in unresumed sessions as well as resumed ones. Report the break-even
fraction of uncertain periods that must have stayed open to change the
winner. Also report the extra re-read cost per resumed summary that would
erase on's saving as a caveat to the fixed-transcript assumption, rather
than an unbounded input to the recommendation. Define delta as on
cost minus off cost, summed over the whole analysis window, including
summaries in unresumed sessions. Use observed costs where possible and
state the assumptions used to estimate activation, client lifetime,
summary size, and timing where they are unobserved. Recommend off when
the aggregate estimated delta is positive, on when it is negative, and
keep the current value when it is zero. Emit a change and a nonzero exit
only when the recommended value differs from the effective setting and
the control applies. A zero-cost period, an unprofitable individual idle,
or a sensitivity range crossing zero does not override the aggregate
estimate's winner. Show those sensitivity results and break-evens beside
the recommendation. If required inputs cannot be estimated, report the
missing evidence and conditional costs instead of inventing a net value
or emitting a change. These are modeled API-price savings, not an observed
comparison of both policies or reductions to a subscription bill.

Synthetic tests cover both recommendation directions, conditional results,
final periods with unknown client lifetime, and ineligible clients, TTLs,
sizes, or controls. Include a resumed idle with savings, an unresumed
summary that only adds cost, a return before original expiry, an expired
prefix on a post-compaction request, and compactions without double counting.
Verify mixed periods whose aggregate winner differs from the winner on
most periods, a sensitivity range crossing zero with an estimable net
winner, and a net-zero tie that keeps the current setting. Verify the
same-cohort activation estimate and documented-trigger fallback, including
their charges on final periods without double-counting client lifetime.

## Required

- [Compaction window check](/quest/m0/compaction.md) - context and compaction costs, and recommended windows
