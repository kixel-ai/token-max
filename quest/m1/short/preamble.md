# [M] Shorten the report's preamble

## Goal

A report opens with its usage title, the headline counting changes and what
they save, and one dim line counting what was left out or assumed, such as
unsupported T3 instances, unattributed or removed T3 requests, requests at
an assumed service tier or with no usage, and unreadable lines, pointing to
`--details`. The `--defaults` explanation, profiles, T3 state, pricing and
percent caveats and the legend print only under `--details`.

## Plan

Settled in the [epic](/quest/m1/short/README.md): keep one left-out line
rather than drop the caveats, since the project says plainly what it could
not measure (rejected: title and headline only; trim the wording).

Recommendations:

- A legend is needed only without color, where words replace the dots; with
  color, drop it.
- Keep the left-out line to one row: counts, not explanations.
- The `--defaults` run says so in the title, such as "as a default user",
  rather than a paragraph.
- Update `docs/reference.md` and rerun `assets/demo/walk.tape`.
