# [M] Collapse EVERYTHING ELSE to a line per section

## Goal

The report's EVERYTHING ELSE section shows one line per section, such as
"Skills  31 kept, 4 unknown  cost ~$120", instead of a row for every setting
and feature; `--details` shows every row. AGENTS.md's "Show every setting and
feature on its own line" says every setting and feature has its line in
`--details`.

## Plan

Settled in the [epic](/quest/m1/short/README.md): one line per section
(rejected: only under `--details`; keep it). On the maintainer's hosts this
section ran 233 to 394 lines.

Recommendations:

- Use the sections' kinds (`Section` in `src/doctor.rs`), plus one line for
  each checked setting with no kind, ranked by cost as now.
- Each line names the setting that changes its kind, as the rule requires.
- Keep the site's cut of this section in `assets/demo/site.py` while any
  line outside `--details` names a project file, which can belong to a
  private repository; the page's shown run never passes `--details`.
