# Audit several hosts as one

## Goal

One `token-max doctor --ssh A --ssh B --include-local` run covers every
machine the maintainer works on, such as kixtop, desktop, nixos and fedora:
it runs on each without a manual install, reports one combined total, and
its walk applies accepted changes on whichever host owns them.

## Plan

Decided 2026-11-17 (maintainer, /quest-plan in token-max: "can you add
multiple --ssh endpoints? Like I want to collect metrics for my current
laptop + desktop + nixos + fedora"). Repeatable `--ssh` with
`--include-local` already audits several hosts in one run, each in its own
section; what was missing:

- ✅ a combined total, ✅ no manual install on each host, ✅ the walk edits
  remote hosts (rejected: nothing, only set the hosts up by hand).

Each is its own quest here, since each lands on its own.
