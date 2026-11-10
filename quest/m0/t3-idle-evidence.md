# [M] Measure T3 idle compaction from recorded evidence

## Goal

On a host running T3, each T3 Claude instance's idle-compaction line reports a
priced comparison of its modes, or names the one piece of evidence it could not
find, instead of the generic "T3 lifecycle, observed expiry or compatible
summary evidence is unavailable". Fast mode stays out of scope: T3 does not
store its current thread or browser selection.

## Plan

After [kixelated/token-max#47](https://github.com/kixelated/token-max/pull/47)
resolved ownership, a full run on the maintainer's host (2026-11-10, T3 0.1.7,
1,020,619 requests over 49 days) prints `onReturn` for the `claudeAgent`,
`claude_local` and `claude_api_credits` instances, with every line unpriced by
`CheckResult::T3Idle` (`src/report.rs`). Ownership no longer blocks it; only 6
T3-labelled requests stay unresolved, which the maintainer judged not worth a
quest.

`docs/t3.md` "Idle comparison" describes what pricing needs: explicit idle run
identity, or verified current guards and observed `promptCacheExpiresAt`, plus
compatible summary calibration. Find out what this host actually records, then
make the line price from it or name precisely which input is missing:

- Which T3 state tables or provider-turn records carry idle runs, compaction
  runs or cache expiry (read through T3's indexes, as #41 does), and whether
  T3's transcripts mark idle summaries.
- Whether `onReturn` needs proactive evidence at all: it charges no final
  summary, so its baseline may be priceable from the window/cache simulation
  alone once ownership and the window resolve.
- Whether `launchArgs` being present (`docs/t3.md` keeps them advisory) is what
  leaves the window unresolved for these instances.

Planned 2026-11-10 by /quest-plan from #47's follow-ups: ✅ m0 (rejected: m1,
drop). The sibling follow-up, usage-less requests marking a T3 profile
unresolved, was dropped because the run shows ownership no longer blocks the
T3 lines.
