# [M] Report as a default user would see it

## Goal

`token-max doctor --defaults` prints the report a user with no settings
files would get on this host's own transcripts: every setting at its
documented Claude Code or Codex default, every tool, skill and MCP server on,
and the headline, rows and savings priced from there. It is read-only: it
offers no changes and writes nothing. A normal `doctor` run is unchanged,
including its "Compared with defaults" line.

## Plan

Decided 2026-11-16 (maintainer, /quest-plan in token-max: "--default ignores
the current settings files and pretends they are set to the default values"):

- ✅ add the flag beside the always-on comparison line of
  [defaults.md](/quest/m0/launch/defaults.md) (rejected: replacing that line
  with the flag, which reverses that quest's pick of a line on every run).
  That quest rejected a flag as the way to show the comparison; this one is
  a different view, the whole report as a default user sees it.
- ✅ read-only (rejected: a walk that writes its picks into the real files;
  rejected: resetting the files to the defaults plus the picks). The
  maintainer first allowed writes and then settled on read-only.
- ✅ pretend away the user, project and local files of both CLIs and T3
  Code's per-session state, and keep managed or system policy
  (`managed-settings.json`, `/etc/codex/config.toml`) (rejected: every file,
  which would recommend changes a user can't make; user-level files only,
  which lets project files skew the view).
- ✅ named `--defaults` (rejected: `--default`, ambiguous beside
  `--max-cost-per-use`'s "(default $1)").
- ✅ the launch's README and site snapshot keeps the defaults quest's figure,
  what the maintainer's settings save against the defaults (rejected: this
  flag's figure; both side by side). So this quest blocks nothing in the
  launch, and joins m1.

Recommended:

- Reuse the defaults quest's per-setting replay against each documented
  default as the baseline, rather than a second pricing path.
- A feature the real settings turn off is on by default but absent from the
  transcripts while it was off: price it as the defaults quest does, from
  what it cost per request while it was still sent, and call it unknown
  otherwise, never zero.
- Under the flag, drop the "Compared with defaults" line, which would only
  restate that everything matches, and say in the header that the report
  pretends the settings files away.
- Exit as a normal non-interactive run does, so a script can still tell
  whether a default user would have anything to change.

## Required

- [Show what the settings already save against the defaults](/quest/m0/launch/defaults.md) - the per-setting pricing against each default that this reuses
