# [M] Investigate bugs/escaped/found:reviewer (count) and friction/iterates/iterate:idle (count)

## Goal

Kixel Quest measured what the Plan lists and filed it here on its own. For each finding:

1. Diagnose its cause, starting from the culprit's diff and sessions where it has them.
2. Have a second session, one that did not write the diagnosis, try to refute it, and diagnose again when it does.
3. Fix it, accept it with the reason recorded in this Plan, or improve on it further. A fix carries the measurement that shows it.

Then look past the finding: make the related improvements the diagnosis turns up where they fit here, and add the rest as follow-up quests.

## Plan

Each finding keeps the comment Kixel Quest finds it by, so a later run that sees it again adds its sighting here instead of filing it anew.

### `bugs/escaped/found:reviewer (count)`: bottleneck

<!-- kixel.quest finding {"series":"bugs/escaped/found:reviewer (count)","culprit":null} -->

- Series: [`bugs/escaped/found:reviewer (count)`](https://kixel.quest/factory#bottlenecks-1447578188).
- 2026-11-27: 0 against a baseline of 0, +0. The largest share of escaped bugs, 36.36% of its measure over the 30 days to 2026-11-27; its last 7 days ran no different than the 30 before, median 0 over 7 days against 0 over 30, one-sided Mann-Whitney U, p = 0.004505.

### `friction/iterates/iterate:idle (count)`: bottleneck

<!-- kixel.quest finding {"series":"friction/iterates/iterate:idle (count)","culprit":null} -->

- Series: [`friction/iterates/iterate:idle (count)`](https://kixel.quest/factory#bottlenecks-1447578188).
- 2026-11-27: 0 against a baseline of 0, +0. The largest share of friction, 21.74% of its measure over the 30 days to 2026-11-27; too few measurements for a trend.
