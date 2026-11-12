# [M] Grade verdicts green, yellow and red

## Goal

Every setting and feature line in the report is colored by verdict: red for
a recommended change (exit 1), yellow for an advisory (exit 0), and green
for a setting already at its recommended value. Every feature line shows
its total cost and use count over the window, while recommendations stay
based on cost per use.

## Plan

Decided 2026-11-11 (maintainer, /quest-plan for auto memory). Decision
prompts, chosen options marked ✅:

- Colors: ✅ green optimal, yellow not recommended, red probably bad
  (rejected: keep today's cyan changes and uncolored settings that need no
  change). Today `src/report.rs` styles "Recommended changes" cyan (36) and
  "Advisory findings" yellow (33).
- Display versus verdict: ✅ show total cost and uses on every line, and keep
  `--max-cost-per-use` as the rule for recommending a tool, skill, MCP server
  or background generation off (rejected: a total-cost or monthly cutoff;
  the maintainer: "total is terrible for recommendations").
- Unused features: ✅ a recurring cost with no use across a fully visible
  window stays red, whatever the amount.

Color stays an addition to text labels: `NO_COLOR` and `--plain` still
render the same words, and the JSON verdicts are unchanged. Update the
README's color note and sample report in the same PR.

## Related

- [Report auto memory's total cost](/quest/m0/auto-memory.md) - the first yellow-by-policy line
- [Remove the JSON report](/quest/m0/remove-json.md) - removes the JSON verdicts this quest leaves unchanged
- [Run the report as an interactive doctor](/quest/m0/doctor.md) - on a terminal, exit 1 no longer marks a red line
- [Section the report by kind of claim](/quest/m0/claim-tiers.md) - makes these colors the only carrier of the verdict
