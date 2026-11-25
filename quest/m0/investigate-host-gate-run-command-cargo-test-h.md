# [M] Investigate host/gate-run/command:cargo test (h)

## Goal

Kixel Quest measured what the Plan lists and filed it here on its own. For each finding:

1. Diagnose its cause, starting from the culprit's diff and sessions where it has them.
2. Have a second session, one that did not write the diagnosis, try to refute it, and diagnose again when it does.
3. Fix it, accept it with the reason recorded in this Plan, or improve on it further. A fix carries the measurement that shows it.

Then look past the finding: make the related improvements the diagnosis turns up where they fit here, and add the rest as follow-up quests.

## Plan

Each finding keeps the comment Kixel Quest finds it by, so a later run that sees it again adds its sighting here instead of filing it anew.

### `host/gate-run/command:cargo test (h)`: bottleneck

<!-- kixel.quest finding {"series":"host/gate\u002drun/command:cargo test (h)","culprit":null} -->

- Series: [`host/gate-run/command:cargo test (h)`](https://kixel.quest/factory#bottlenecks-1447578188), in h.
- 2026-11-25: 0.09018 h against a baseline of 0.09018 h, +0 h (+0%). The largest share of host time, 46.28% of its measure over the 30 days to 2026-11-25; too few measurements for a trend.
