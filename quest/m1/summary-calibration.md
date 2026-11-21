# [M] Calibrate compaction summaries once

## Goal

Idle, compaction and T3 idle replays size a compaction summary from one
calibration, so a fix to how summaries are grouped or measured reaches all
three.

## Plan

Today the grouping and measuring is written three times:

- `CompactionSamples::new` in `src/compaction.rs`, keyed by `SampleCohort`.
- `ReplayIndex::new` in `src/idle.rs`, keyed by the tuple `SampleKey`.
- The T3 replay in `src/idle.rs`, which scans the samples linearly for each
  gap and checks uniformity field by field.

Each one groups samples by provider, model, tier, speed and geo, and takes
the median and maximum of output and prefix.

They already disagree when event ids repeat: `ReplayIndex` keeps the last
event with an id, while the T3 path keeps the first. Recommendation: keep the
last, since a later record of the same event carries its final state. Add a
test with a repeated id.

Build one cohort key, `SampleKey` as `SampleCohort` plus version and client,
and one geometry builder that the T3 path reads from `ReplayIndex` instead of
recomputing. Report output must not change while ids are unique.
