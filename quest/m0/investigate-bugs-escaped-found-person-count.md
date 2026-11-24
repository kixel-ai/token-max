# [M] Investigate bugs/escaped/found:person (count), conflicts/events (count), and 4 more

## Goal

Kixel Quest measured what the Plan lists and filed it here on its own. For each finding:

1. Diagnose its cause, starting from the culprit's diff and sessions where it has them.
2. Have a second session, one that did not write the diagnosis, try to refute it, and diagnose again when it does.
3. Fix it, accept it with the reason recorded in this Plan, or improve on it further. A fix carries the measurement that shows it.

Then look past the finding: make the related improvements the diagnosis turns up where they fit here, and add the rest as follow-up quests.

## Plan

Each finding keeps the comment Kixel Quest finds it by, so a later run that sees it again adds its sighting here instead of filing it anew.

### `bugs/escaped/found:person (count)`: bottleneck

<!-- kixel.quest finding {"series":"bugs/escaped/found:person (count)","culprit":null} -->

- Series: [`bugs/escaped/found:person (count)`](https://kixel.quest/factory#bottlenecks-1447578188).
- 2026-11-24: 0 against a baseline of 0, +0. The largest share of escaped bugs, 100% of its measure over the 30 days to 2026-11-24; its last 7 days ran no different than the 30 before, median 0 over 7 days against 0 over 30, one-sided Mann-Whitney U, p = 0.1892.

### `conflicts/events (count)`: bottleneck

<!-- kixel.quest finding {"series":"conflicts/events (count)","culprit":null} -->

- Series: [`conflicts/events (count)`](https://kixel.quest/factory#bottlenecks-1447578188).
- 2026-11-24: 0 against a baseline of 0, +0. The largest share of conflicts, 33.33% of PRs over the 30 days to 2026-11-24; too few measurements for a trend.

### `cost/by-model/model:claude-opus-5-5 (USD)`: bottleneck

<!-- kixel.quest finding {"series":"cost/by\u002dmodel/model:claude\u002dopus\u002d5\u002d5 (USD)","culprit":null} -->

- Series: [`cost/by-model/model:claude-opus-5-5 (USD)`](https://kixel.quest/factory#bottlenecks-1447578188), in USD.
- 2026-11-24: 6.736 USD against a baseline of 6.736 USD, +0 USD (+0%). The largest share of cost, 100% of its measure over the 30 days to 2026-11-24; too few measurements for a trend.

### `friction/split/category:waiting (h)`: bottleneck

<!-- kixel.quest finding {"series":"friction/split/category:waiting (h)","culprit":null} -->

- Series: [`friction/split/category:waiting (h)`](https://kixel.quest/factory#bottlenecks-1447578188), in h.
- 2026-11-24: 0.005197 h against a baseline of 0.005197 h, +0 h (+0%). The largest share of friction, 5.159% of its measure over the 30 days to 2026-11-24; too few measurements for a trend.

### `host/gate-wait/command:just check (h)`: bottleneck

<!-- kixel.quest finding {"series":"host/gate\u002dwait/command:just check (h)","culprit":null} -->

- Series: [`host/gate-wait/command:just check (h)`](https://kixel.quest/factory#bottlenecks-1447578188), in h.
- 2026-11-24: 0.00003194 h against a baseline of 0.00003194 h, +0 h (+0%). The largest share of host time, 54.55% of its measure over the 30 days to 2026-11-24; too few measurements for a trend.

### `ship/review (h)`: bottleneck

<!-- kixel.quest finding {"series":"ship/review (h)","culprit":null} -->

- Series: [`ship/review (h)`](https://kixel.quest/factory#bottlenecks-1447578188), in h.
- 2026-11-24: 0.1919 h against a baseline of 0.1919 h, +0 h (+0%). The largest share of time to ship, 44.83% of its phases over the 30 days to 2026-11-24; too few measurements for a trend.
