# [M] Remove production code that exists only for tests

## Goal

Production code carries no structure that only tests read: tests assert on
what the report uses.

## Plan

The m1 code audit found two such seams:

- **Idle's `detail` trace.** `idle_checks` and `t3_idle_checks` always pass
  `detail = false`; only tests pass `true`. Production still builds
  `IdlePeriod` and `PeriodCalibration`, `Period::detail`, the
  `[Vec<IdlePeriod>; 2]` return tuples, two `detailed_*` wrappers, and the
  `detail.get_mut(i).filter(|_| point)` blocks through `Runner::run` and
  `t3_replay`. That is about 150 lines. Rewrite those tests to assert on the
  `IdleCheck` aggregates, or on a cfg(test) trace if an aggregate can't pin
  the behavior. `Calibration::events` in `src/compaction.rs` serves only
  `PeriodCalibration`, so drop it with the trace.
- **`Serialize` on the `Scan` tree.** Nothing in production serializes `Scan`
  or the model types under it (except the `Usage` dedup key in
  `src/reader.rs`). What crosses the SSH stream and `sessions` is
  `wire::Outcome` and `sessions::Session`. The derives, about 127 `#[serde]`
  attributes and six hand-written impls (`Id`, `Few`, `Settings`,
  `ServerTools`, `Ids`, `Exposure`) serve privacy tests of the form
  `!serde_json::to_string(&scan).contains("PRIVATE")` and one JSON-shape
  assertion. Run those tests on `format!("{scan:?}")` instead. That is stricter,
  since `Debug` includes `#[serde(skip)]` fields. Then drop the serde surface,
  keeping it on the types `wire` and `sessions` carry. Also fix `model.rs`'s
  header, which claims the SSH stream carries these types.

Neither changes output. Check compile time before and after for the serde
change, and report it.
