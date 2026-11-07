# [M] Keepalive check

## Goal

`token-max` compares leaving sessions idle, compacting them while idle,
and pinging just under the cache TTL for each cap on how long to keep
pinging. It recommends a keepalive only when it beats no keepalive and,
where supported, idle compaction, after their costs and unknown client
lifetimes are accounted for.

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

Codex on gpt-6.1-sol has at least 30 minutes of cache coverage, refreshed
by reads. The maintainer's cached input above 95% for gaps up to 30 minutes
does not establish the cost of longer gaps. Include those gaps in the cap
comparison, pinging below the model's documented refresh interval and
pricing its actual read/write rates and ping overhead
([OpenAI prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching),
checked 2026-11-06). Compare against no keepalive with the same client
lifetime assumptions; omit idle compaction where the harness has no such
policy. Longer observed cache retention remains evidence, not a guarantee.

Decided 2026-11-06 (maintainer, review checkpoint): ✅ continue the net-cost
design and include Codex gaps beyond 30 minutes (rejected: scope m0
keepalive to Claude). A short-gap hit rate is not grounds to exclude longer
idle periods.

Recommendations for the implementer:

- Price a ping as a read of the session's last context plus a short prompt
  and reply, including estimated pings in final periods with no observed
  return under the same client-lifetime assumptions as the other policies.
- Report the best cap and its net, not only whether any cap wins.
- Compare no keepalive, each cap, and idle compaction where supported. For
  Claude, use the recommended TTL/window and the idle check's lifetime
  bounds. For Codex, use the model's documented cache policy, the
  recommended window if applicable, and the lifetime estimate below; there
  is no recommended TTL setting or idle-compaction policy to apply. Do not
  assume every final period stays open. For Claude, pings reset the
  last-request clock and may prevent idle compaction; replay that timing rather than charging both
  policies
  independently. Recommend the lower estimated aggregate net cost and
  show uncertainty beside that recommendation; do not require every period
  or sensitivity scenario to favor it. Do not add the two policies'
  savings for the same idle period.
- Define a deciding lifetime estimate for every cap, not just closed/open
  bounds. Use recorded client liveness/shutdown events when present. Without
  them, model the documented open-and-awake condition through the earlier
  of return and cap; final periods incur the full cap. Label that fallback
  assumption and show closure/sleep sensitivity and the break-even open
  duration. Do not extend the idle check's one-hour activation fraction to
  multi-hour ping lifetimes. Choose the aggregate estimate's winner, not a
  requirement to win across every lifetime bound.

## Required

- [Replay and the cache TTL check](/quest/m0/replay.md) - the reader, prices, and report it adds to
- [Idle compaction check](/quest/m0/idle-compaction.md) - the compaction alternative and client lifetime bounds used in the comparison
