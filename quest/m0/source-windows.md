# [M] Decide whether each source keeps its own cache TTL window

## Goal

A cache TTL line that combines several transcript sources, such as native
Claude Code and T3's claudeAgent sharing `~/.claude/settings.json`, measures
each source over the window the maintainer picks, and the compaction window
check that starts after the TTL windows agrees with it.

## Plan

Found while landing one row per settings file
([#114](https://github.com/kixel-ai/token-max/pull/114)). That quest's Plan
said a line "sums each candidate value's cost across [its sources], each
source keeping its own window". Every check but the cache TTL uses `--since`
or the whole available history, so each source already contributes all of
its own requests in the window. The TTL check is different: without
`--since`, its window starts at the latest observed write-TTL transition,
found per cohort (client source, agent kind, part kind and model) and
taken as the latest across cohorts. A line now has the cohorts of all its
sources, so a T3 instance that picked up a changed `promptCacheTtl` later
than native Claude Code, because its server kept old processes running,
moves the start of native Claude Code's window too. The compaction check
starts after the latest TTL window, so it follows.

Recommendation: keep one window per line. The sources share one settings
file, so the setting changed for both at one moment; a source that shows
it later ran a stale process, and the latest transition is when the line's
current setting is known to hold for all of them. One window also keeps
one `since`, one per-30-day rate and one compaction window per line. It
only matters for a run without `--since`.

The alternative gives each source its own transition: `ReplayOptions.since`
in `src/scenarios.rs` and compaction's replay options in
`src/compaction.rs` take a start per source instead of one, the TTL check
lists each source's start under `--details`, and its per-30-day figure sums
each source's savings over its own span, which needs each source's paired
savings from the replay. The compaction check takes each source's TTL
window the same way.

## Waiting

- Maintainer: keep one TTL window per line, starting at the latest
  transition among its sources (recommended; delete this quest), or give
  each source its own transition (implement the alternative above).
