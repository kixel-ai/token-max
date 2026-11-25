# [M] Show settings-file edits once, at the end

## Goal

No change row in the report carries its settings-file edit. A report
printed without a terminal, `--defaults` included, ends with one section of
edits per file, one line per key, a list's members on one wrapped line, such
as `permissions.deny += ScheduleWakeup, ReportFindings, … (8)`; `--details`
shows the exact JSON or TOML. The walk's cards and confirm diff stay as
they are.

## Plan

Settled in the [epic](/quest/m1/short/README.md): the maintainer called the
per-row edits "noise" until the very end.

Recommendations:

- Steps by hand, such as uninstalling a plugin, join the closing section
  under their file or profile rather than staying on rows.
- After a walk, the closing section prints after the confirm and lists
  every edit not written, declined at its card or at the confirm, so it can
  still be made by hand.
- Name files as `host:path` under `--ssh`, as the confirm does.
- Update `docs/reference.md` ("each change's line ends with its edit") and
  rerun `assets/demo/walk.tape` if the post-walk report changes.
