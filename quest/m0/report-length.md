# [L] Cut doctor's default report to one line each

## Goal

On a real host, `token-max doctor`'s default report fits on a few screens
and leads with what matters: every setting and feature on one line naming
its verdict, value, cost in dollars and percent, and the setting that
changes it, the offered changes first, with the evidence under `--details`. Its headline matches the
changes it offers, and every reason reads as a whole sentence.

## Plan

Decided 2026-11-14 (maintainer, /quest-plan after a local doctor run on the
maintainer's host): ✅ one line for each setting or feature, with the
evidence line and per-item breakdowns under `--details`, and rows with the
same verdict across profiles merged into one row naming them (rejected:
show only rows that offer a change plus counts, which breaks AGENTS.md's
"every setting and feature on its own line"). This carries out the
2026-11-08 decision of one plain verdict per setting.

Decided 2026-11-14 (maintainer, after mockup videos of a ranked report in
a pager, a full-screen TUI and a ranked y/n walk): ✅ the walk to decide, then the ranked list
to confirm, in [Walk the changes biggest saving first](/quest/m0/walk.md)
(rejected: rank and page through `$PAGER` alone, which this replaces; a
full-screen TUI, a new dependency that would delay 0.0.1). The walk
explains why each change makes sense, so it leads on a terminal. ✅ the rows the walk does not offer are
listed after it, one line each (rejected: behind `--all`, which breaks
AGENTS.md's "every setting and feature on its own line"). This quest owns
that list and the report printed without a terminal: the offered changes
first, ordered by dollars saved, with those whose saving was not measured
after the ones that were, then every other row by its cost, cheapest last
and unknown costs after. ✅ every dollar figure comes with a percent of its
profile's spend over the window at API prices, such as "$8,830/mo, 12% of
T3 claudeAgent" (rejected: of the whole host's spend, which makes a big
profile's changes look small; of the setting's own cost). A row merged
across profiles, or a change to a file several profiles share, takes the
combined spend of the profiles it names as its base.

That run printed 558 lines, 121 KB, by default:

- A Codex automatic recaps row listing every worktree, 22,060 characters on
  one line, and another of 13,732.
- About 135 `MCP discovery <tool> (included in server total)` info rows,
  one per t3-code and ChatGPT app tool, already counted in their server's
  row.
- About 45 skill rows with full `SKILL.md` paths, most from one worktree's
  `.claude/skills`, plus a second evidence line under every row.
- Subagent rows in both the Measured and Official sections.
- Columns breaking on long labels, such as model lists and skill paths.
- "No setting changes recommended." above 14 "Changes to make", each a
  "worth considering" advisory. Count both in the headline.
- Cache TTL rows saying "5m and 1h differ by only $1177 over 4 days … too
  close to call". Give the difference relative to the cost, since the
  absolute figure reads as large.
- A Bash timeout cap reason missing its verb: "Not enough evidence: the
  cache lifetime the latest main-agent request wrote, so the cap has nothing
  to compare with."

Rerun on a large synthetic history and give the default line count and
widest line in the PR's Validation. Update the README's sample report.

## Related

- [Give each profile one row per tool](/quest/m0/profile-rows.md) - also changes these rows
- [Walk the changes biggest saving first](/quest/m0/walk.md) - the terminal walk printed before this list
