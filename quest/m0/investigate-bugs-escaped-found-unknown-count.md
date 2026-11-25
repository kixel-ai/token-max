# [M] Investigate bugs/escaped/found:unknown (count), cost/by-model/model:claude-opus-5-5 (tokens), and 2 more

## Goal

Kixel Quest measured what the Plan lists and filed it here on its own. For each finding:

1. Diagnose its cause, starting from the culprit's diff and sessions where it has them.
2. Have a second session, one that did not write the diagnosis, try to refute it, and diagnose again when it does.
3. Fix it, accept it with the reason recorded in this Plan, or improve on it further. A fix carries the measurement that shows it.

Then look past the finding: make the related improvements the diagnosis turns up where they fit here, and add the rest as follow-up quests.

## Plan

Each finding keeps the comment Kixel Quest finds it by, so a later run that sees it again adds its sighting here instead of filing it anew.

### `bugs/escaped/found:unknown (count)`: bottleneck

<!-- kixel.quest finding {"series":"bugs/escaped/found:unknown (count)","culprit":null} -->

- Series: [`bugs/escaped/found:unknown (count)`](https://kixel.quest/factory#bottlenecks-1447578188).
- 2026-11-25: 0 against a baseline of 0, +0. The largest share of escaped bugs, 75% of its measure over the 30 days to 2026-11-25; its last 7 days ran no different than the 30 before, median 0 over 7 days against 0 over 30, one-sided Mann-Whitney U, p = 0.1892.

### `cost/by-model/model:claude-opus-5-5 (tokens)`: bottleneck

<!-- kixel.quest finding {"series":"cost/by\u002dmodel/model:claude\u002dopus\u002d5\u002d5 (tokens)","culprit":null} -->

- Series: [`cost/by-model/model:claude-opus-5-5 (tokens)`](https://kixel.quest/factory#bottlenecks-1447578188), in tokens.
- 2026-11-25: 13770000 tokens against a baseline of 13770000 tokens, +0 tokens (+0%). The largest share of cost, 82.75% of its measure over the 30 days to 2026-11-25; too few measurements for a trend.

### `host/gate-wait/command:cargo test (h)`: bottleneck

<!-- kixel.quest finding {"series":"host/gate\u002dwait/command:cargo test (h)","culprit":null} -->

- Series: [`host/gate-wait/command:cargo test (h)`](https://kixel.quest/factory#bottlenecks-1447578188), in h.
- 2026-11-25: 0.03359 h against a baseline of 0.03359 h, +0 h (+0%). The largest share of host time, 51.67% of its measure over the 30 days to 2026-11-25; too few measurements for a trend.

### `ship/pick (h)`: bottleneck

<!-- kixel.quest finding {"series":"ship/pick (h)","culprit":null} -->

- Series: [`ship/pick (h)`](https://kixel.quest/factory#bottlenecks-1447578188), in h.
- 2026-11-25: 0.1756 h against a baseline of 0.1756 h, +0 h (+0%). The largest share of time to ship, 29.45% of its phases over the 30 days to 2026-11-25; too few measurements for a trend.
