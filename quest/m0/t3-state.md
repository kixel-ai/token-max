# [M] Resolve T3 control ownership on shared native homes

## Goal

On a host where T3 instances share a native Claude or Codex home with the
terminal CLI, every request in that home resolves to the T3 instance or
native profile whose controls ran it, so the T3 idle-compaction and fast-mode
lines carry evidence instead of "unknown". Requests that stay ambiguous are
named with their cause.

## Plan

[kixelated/token-max#41](https://github.com/kixelated/token-max/pull/41)
fixed the read itself. The 10,000 x 1,000 VM-step cap was the cause: one
Claude request's lookup cost 5.9M steps and about 50 s of I/O on the
maintainer's 20 GB `statev2.sqlite`, because it scanned every provider turn
and every turn item's payload. Reads now follow T3's indexes from the selected
sessions' provider threads, and a failure names schema, resource bound, or
access. A `--since 2026-11-08` run joins 18,992 requests in a 16.5 s
snapshot, none ambiguous.

The T3 lines still say "unknown" on that host (measured 2026-11-09, T3 0.1.7):

- Subagent requests never inherit their parent session's T3 owner: Claude
  subagent sessions are keyed per agent file, and Codex subagent threads by
  their own id (`session_meta.session_id` and `parent_thread_id` name the
  root). About 11,500 requests in two days, marking the shared profiles
  unresolved, which also clears their cache TTL.
- About 430 `sdk-cli` Claude requests (`claude -p`-style sessions) have no T3
  provider thread. They may be T3's ephemeral helper generation, which
  [Report T3's text-generation model](/quest/m0/t3-text-generation.md)
  covers, so they stay ambiguous and keep the shared `~/.claude` unresolved.
- Fast mode stays unknown by design: T3's current thread and browser
  selection is not read, only historical requested speed.
- About 14,000 Claude main requests join only at the session level. Their
  responses carry tool calls or thinking but no text, and T3 keys those items
  by tool-use id or `<message id>:thinking:<n>`, which token-max does not
  collect.

Settled: T3 metadata stays optional, and a failed read stays a named
evidence gap that keeps native usage, never a failed run (2026-11-07
decision in the m0 README).

## Related

- [Report T3's text-generation model](/quest/m0/t3-text-generation.md) - T3 helper sessions with no provider thread
