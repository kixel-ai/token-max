# [M] Export priced sessions as JSON

## Goal

`token-max` prints each transcript file it replays, main or sub-agent, as
one JSON record, priced at public API prices: harness, session ID, the
parent's session ID for a sub-agent, working directory, each Git branch the
file names with its first and last time, effort, whether Codex marked it a
review, and per-model input, cache-write, cache-read, and output tokens
with their cost. It keeps no prompt or reply text, like every other report.
moq-dev/quest's `quest usage` consumes it to tie sessions to PRs, in place
of ccusage and its own transcript readers.

## Plan

Decided 2026-11-11 (maintainer, /quest-plan in moq-dev/quest,
[moq-dev/quest#637](https://github.com/moq-dev/quest/pull/637)):

- Where does transcript pricing live?
  - ✅ token-max prices and quest attributes, so one transcript reader
    serves both tools
  - Rejected: moving all of `quest usage` into token-max, which would teach
    it quest's PR records; keeping ccusage and quest's own reader
- Milestone: ✅ m0 (rejected: m1)

Why quest needs it: ccusage folds a Claude Code sub-agent's records into
its parent's session ID. quest reads only the parent's log, so a
`/quest-spawn` sub-agent's work on a quest branch counted as its
coordinator's. On 2026-11-11, 72% of the host's spend since 2026-11-01
could not be tied to any PR.

Agent's choices, for the implementer to revisit:

- Shape it to the CLI's existing options, such as a flag beside `--json`,
  with `--since` selecting the files.
- A sub-agent file is its own record, since its branch differs from its
  coordinator's. `src/t3.rs` already maps a sub-agent file to its parent.
- Claude Code sometimes writes an interleaved, malformed line in the middle
  of a transcript, such as line 1385 of one moq session on 2026-11-04. The
  export counts such lines per file and marks that file's cost as a lower
  bound, rather than failing every report on the host. Settle this with the
  reader's existing handling, if it already has one.
- quest pins token-max as a flake input, so this ships without waiting on
  a crates.io release.
