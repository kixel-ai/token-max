# [L] Show what the settings already save against the defaults

## Goal

Every `token-max doctor` run, including a second run with nothing left to
change, shows how much the host's current settings save against the Claude
Code and Codex defaults: a labeled total under the headline, and on each row
whose setting differs from its default, the default and what the current
value saves against it.

## Plan

Decided 2026-11-15 (maintainer, /quest-plan): ✅ part of every doctor run,
"just to drive home how much token-max is saving even after a second run"
(rejected: a `--defaults` flag, the first proposal; the whole report as a
default user would see it). ✅ a headline line and a note on each row
(rejected: the headline only, which hides which setting earns it; a section
of its own at the end).

The headline reads like "Compared with defaults, your settings save about
$945/mo, 2% of this host's spend; overlapping changes may save less". Like
the walk's running total, it sums each setting's own saving against its
default and labels the sum as possibly overlapping; this extends m0's
2026-11-14 amendment of "no summed savings" to this total. Each row's note
reads like "default 1h, saves $410/mo".

Recommended: price each setting against its documented default with the
same replay the report already runs, such as the cache TTLs, compaction
windows, idle compaction, keepalive pings and the Bash timeout cap. A tool,
skill or MCP server already turned off sends nothing to the transcripts, so
estimate its cost from its definition's size where the tool knows it, label
the estimate, and call it unknown otherwise, never zero. Claim kinds stay:
an Opinion setting such as auto memory is labeled as one.
