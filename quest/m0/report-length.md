# [L] Cut doctor's default report to one line each

## Goal

On a real host, `token-max doctor`'s default report fits on a few screens
and leads with what matters: every setting and feature on one line naming
its verdict, value, cost and the setting that changes it, the offered
changes first, with the evidence under `--details`. Its headline matches the
changes it offers, and every reason reads as a whole sentence.

## Plan

Decided 2026-11-14 (maintainer, /quest-plan after a local doctor run on the
maintainer's host): ✅ one line for each setting or feature, with the
evidence line and per-item breakdowns under `--details`, and rows with the
same verdict across profiles merged into one row naming them (rejected:
show only rows that offer a change plus counts, which breaks AGENTS.md's
"every setting and feature on its own line"). This carries out the
2026-11-08 decision of one plain verdict per setting.

Decided 2026-11-14 (maintainer): ✅ rank and page (rejected: a full-screen
TUI such as ratatui, a new dependency and an L/XL quest that would delay
0.0.1; ranking now plus an m1 TUI quest). Noise drowned out the important
suggestions, such as the cache TTL. Within the one-line-per-row default,
show the offered changes first, ordered by dollars saved, then every other
row ordered by its cost, cheapest last. On a terminal, long output goes
through `$PAGER` the way git does, while piped output stays plain. The y/n
walk offers changes in the same order.

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
