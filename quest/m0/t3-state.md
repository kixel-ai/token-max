# [S] Resolve T3 control ownership of unowned shared sessions

## Goal

On a host where T3 instances share a native Claude or Codex home with other
clients, a session with no T3 provider thread no longer keeps the T3 instance
profiles unresolved, so the T3 idle-compaction and fast-mode lines carry
evidence instead of "unknown". Sessions whose owner stays unproven keep their
named cause.

## Plan

[kixelated/token-max#44](https://github.com/kixelated/token-max/pull/44)
gave subagents their root session's owner, joined Claude tool-only and
thinking-only responses by item id, scoped ownership to the window, and named
each unresolved request's cause. A `--since 2026-11-08` run on the
maintainer's host (2026-11-10, T3 0.1.7) leaves 482 unresolved requests, all
in sessions no T3 provider thread owns:

- 438 `claude -p` (`sdk-cli`) requests. T3 text generation runs
  `claude -p --json-schema` in the shared home, as do the maintainer's own
  `claude -p` tools such as `quest review`.
- 41 requests from other clients without native CLI provenance, mostly Codex
  `vscode` sessions (Codex Desktop or the VS Code extension) and their
  subagents.
- 3 Codex subagent requests whose root session left no transcript, such as an
  ephemeral `codex exec`.

Each keeps every T3 instance of its home as a candidate owner, which marks
them unresolved. A session T3 ran as a provider instance has a provider
thread, unless T3's projection maintenance dropped it, which is why the
current rule does not exclude T3 on a missing thread alone.

Fast mode stays unknown by design: T3's current thread and browser selection
is not read, only historical requested speed.

Decided 2026-11-10 (maintainer, /quest-unblock): when T3's state was read
and a session has no T3 provider thread, exclude T3's instances as its owner
unless the session's client label is one T3's provider adapters write (Claude
`sdk-ts` or T3's own label, Codex originator `T3 Code`); such a request stays
unresolved only among the native profiles. ✅ exclude unless a T3 label
(rejected: exclude for every session without a provider thread, since T3's
projection maintenance may drop a thread and that was not ruled out; keep
today's rule, which leaves the T3 lines unknown on the maintainer's host).

Settled: T3 metadata stays optional, and a failed read stays a named
evidence gap that keeps native usage, never a failed run (2026-11-07
decision in the m0 README).
