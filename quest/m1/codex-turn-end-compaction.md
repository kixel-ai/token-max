# [L] Measure Codex turn-end compaction

## Goal

`doctor` gives Codex's `model_post_turn_compact_threshold_percent` its own
measured line: it replays turn-end compaction at candidate thresholds against
the current setting (off by default) and recommends the cheapest at API
prices, offering the `config.toml` edit as other measured lines do. Codex
compacts after a final response once active context reaches that percentage
of the usable window; the usual auto-compaction limit still applies
([config reference](https://developers.openai.com/codex/config-reference)).

## Plan

Decided 2026-11-23 (maintainer, /quest-plan after a survey of codex-cli
0.167.1 settings worth turning on): ✅ plan it as a measured `doctor` line
(rejected: leave it unmeasured). It is the one setting the survey found worth
turning on that the transcripts can price.

Recommendations:

- Reuse the compaction window replay: same summary and resumed-prefix
  calibration, same extrapolation with `~` and a margin. The difference is
  timing: a turn-end compaction runs while the cache is warm, before the user
  returns, so it trades a summary now for a smaller prefix on every later
  request, and a final turn's summary has no return benefit, as idle
  compaction treats it.
- Replay after the window line's recommendation, as idle compaction does, and
  label the baseline.
- `src/discovery.rs` already keeps the key among its structural settings;
  nothing reads it yet.
- Candidates come from the window and price-tier boundaries, not a fixed
  list. A tie or unpriced result gives no verdict.
