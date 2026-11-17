# [M] Apply the walk's changes on SSH hosts

## Goal

When `doctor` runs on a terminal with `--ssh`, the walk offers each remote
host's changes beside the local ones, shows each file's diff at the confirm,
and writes the accepted ones on that host, with the same lock and
`.token-max.bak` backup as a local write. Today it only lists them.

## Plan

Decided 2026-11-17 (maintainer, /quest-plan): ✅ the remote host's own
token-max applies the edits it is sent, reusing its locks, backups and
git-tracking checks (rejected: reading and writing the remote files over
SSH from this host, which duplicates that logic for remote paths).

Recommended:

- Send the accepted changes, not file contents; the remote reads its file,
  shows nothing on its own, and returns the diff the confirm shows here, so
  the confirm still shows every edit before any write.
- A host that fails stops the run before any later host is written, and the
  closing line names what each host wrote.

## Required

- [Run on an SSH host without installing token-max there](/quest/m1/hosts/remote-run.md) - a token-max of this version on each host to send the edits to
