# [M] Resolve Claude sessions by T3 turn items

## Goal

A Claude session with no T3 provider thread is owned by the T3 thread whose
turn items record its requests, or by the native profiles when T3's
retained turn items cover the request time and none match. The client
label no longer decides it, so sessions continued after `/clear` in T3 and
`claude -p` runs launched from a T3 agent's shell stop keeping the native
profiles advisory.

## Plan

Decided 2026-11-11 (maintainer, /quest-complete planning the follow-up of
[kixelated/token-max#57](https://github.com/kixelated/token-max/pull/57)):
✅ plan it in m0 (rejected: m1; drop).

Research on the maintainer's host, 2026-11-11 (origin/main `82ab865`,
`--since 2026-11-04`): all 229 requests under "no T3 provider thread owns
the native session, but its client label is one T3 writes" (`src/t3.rs`)
were Claude, and none was a thread T3 dropped, so the premise in
`docs/t3.md` ("A T3 provider session lacks a thread only if T3 dropped it")
is wrong.

- `/clear` inside a T3 Claude thread (173 requests): the session gets a new
  id while T3 keeps the old `nativeThreadRef.nativeId`, yet T3 keeps
  writing turn items for the new transcript under the same provider thread.
  The transcript's first `system` record has `commandRun.command = "clear"`.
- `claude -p` from a T3 agent's shell (56 requests, 7 of them subagents):
  nested runs inherit `CLAUDE_CODE_ENTRYPOINT=t3-code` (or `sdk-ts`), but T3
  records no turn item for them. They belong to the native profiles.

Approach:

- For a Claude session with no provider thread, look up T3 turn items by
  primary key, `turn-item:provider:claudeAgent:native-item:<id>`, for each
  selected request's message and tool-use ids. `PROVIDER_THREADS` matches by
  `nativeId` first and never reaches these items today.
- Found: the session belongs to that thread's provider instance; subagents
  already inherit their root's owner. Several threads: unresolved.
- Not found, with T3's state read and its oldest retained turn item before
  the request: native. Before the oldest retained item: unresolved with
  that reason, since T3 compacts turn items (the oldest found was
  2026-10-27, newer than the oldest thread).
- Drop the label rule for Claude and correct `docs/t3.md`. Codex keeps its
  originator rule; no Codex case was observed.
- The lookup cost scales with selected requests through the primary-key
  index, never a scan of the 20 GB state database.

Open, for the implementer to establish: whether T3 writes a turn item for
every Claude turn, such as one ending in an API error. If not, a session
whose only requests failed may need the error record's own ids.
