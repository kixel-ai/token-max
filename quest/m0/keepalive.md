# [M] Keepalive check

## Goal

`token-max` compares leaving sessions idle, compacting them while idle,
and pinging just under the cache TTL for each cap on how long to keep
pinging. It recommends a keepalive only when it beats both alternatives
after their costs and unknown client lifetimes are accounted for.

## Plan

The maintainer asked on 2026-11-05 whether a command that wakes every 4.5
or 59.5 minutes could keep the cache from going cold. Neither harness has a
keepalive: Claude Code's cache refreshes only on a real request, and the
closest building blocks are `/loop` at a fixed interval (`/loop 5m` fires
every 4m45s; a 59.5m interval rounds to 60m, past a 1h TTL) and an
`asyncRewake` hook exiting 2. Each ping is a full turn that reads the
context and may think.

Replayed on the maintainer's main Claude sessions, 10-30 to 11-05, pinging
every 55 minutes under the 1h TTL at Opus 5.5's prices: keeping a session
warm for up to 1 extra hour nets about $258 a month; up to 2 hours, $123;
up to 4 hours, -$171; up to 24 hours, -$4,285. Pings on sessions that never
resume cost more than the misses they save.

Codex needs no ping on gpt-6.1-sol: its cache lives at least 30 minutes and
every read renews it, and on the maintainer's sessions cached input stays
above 95% across gaps up to 30 minutes.

Recommendations for the implementer:

- Price a ping as a read of the session's last context plus a short prompt
  and reply, and count the pings on a session's final idle period, which
  never resumes.
- Report the best cap and its net, not only whether any cap wins.
- Compare no keepalive, idle compaction, and each keepalive cap at the same
  recommended TTL and compaction window, using the idle check's client
  lifetime bounds for final periods rather than assuming every final
  period stays open. Pings reset the last-request clock and may prevent
  idle compaction; replay that timing rather than charging both policies
  independently. Recommend the better supported net saving and report
  conditional results when uncertainty changes the winner. Do not add the
  two policies' savings for the same idle period.

## Required

- [Replay and the cache TTL check](/quest/m0/replay.md) - the reader, prices, and report it adds to
- [Idle compaction check](/quest/m0/idle-compaction.md) - the compaction alternative and client lifetime bounds used in the comparison
