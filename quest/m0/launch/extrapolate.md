# [M] Extrapolate over unpriced requests instead of calling a result unknown

## Goal

A replay with unpriced requests always gives a figure and a verdict when at
least one request was priced: it extrapolates from the priced requests to all of them, prefixes the cost with
`~`, and shows the margin of error beside it, such as "~$3,270/mo ± $410".
No line reads "Not enough evidence" because some requests could not be
priced; a figure with no unpriced requests keeps no `~`, and one with no
priced requests at all stays unmeasured.

## Plan

Decided 2026-11-17 (maintainer, /quest-run, after doctor on desktop called
both cache TTLs unknown: "1h cost 14.8% less than 5m ... on the 95.1% of
requests that could be priced, too little to rule out the 28,082 that could
not". The maintainer: "Wtf why is there not enough evidence? Just
extrapolate or something who cares about 5%."):

- ✅ always extrapolate, put a `~` in front of the cost whenever any request
  is unpriced, and include the margin of error (rejected: extrapolate only
  above a priced share such as 90%; keep the worst-case rule).
- This reverses m0's 2026-11-08 rule that a recommendation stands only when
  the unpriced requests, each bounded by the largest measured difference,
  cannot flip it; [m0's README](/quest/m0/README.md) now says so.

Recommended:

- Extrapolate by dividing the priced requests' figure by the priced share,
  so $950 measured on 95% of requests reads ~$1,000.
- The margin of error is a statistical estimate, not a bound: no hard bound
  exists for a request with no price. Under the extrapolation's own
  assumption that unpriced requests resemble priced ones, give each figure,
  each scenario's cost and each signed saving, its own interval around the
  extrapolated estimate from the spread of that figure's per-request values
  among the priced requests, such as a 95% interval, and say so under
  `--details`.
- Apply it to every replay that leaves requests unpriced, not only the cache
  TTLs, and to the walk's and headline's totals, which then carry the `~`
  when any part does.
- Keep naming the share measured under `--details`.
- AGENTS.md's "Say plainly when a cost can't be measured rather than hiding
  the setting or guessing a number" still holds for a cost with no priced
  requests at all; reword it to say a partial measurement is extrapolated
  and marked, once the maintainer agrees to the wording in this quest's PR.
