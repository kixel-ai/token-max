# [M] Compaction window and idle compaction checks

## Goal

`token-max` recommends Claude Code's `autoCompactWindow` (per model, under
`modelSettings`) and Codex's `model_auto_compact_token_limit` from the spend
on requests above each candidate context size, net of the compaction
requests a lower window would add. For Claude Code it also compares
`idleCompaction` on and off, reports how often eligible sessions go idle
past the cache lifetime, and recommends either value when its estimated
net saving supports changing the current setting.

## Plan

Context on the maintainer's host, 2026-11-05: Claude sessions that peaked
above 400k (2.6% of sessions) were 35% of cost, and auto-compaction ran at a
median of 967k. Claude now compacts at 400k for Opus and Codex at 240k, half
its window.

Decided 2026-11-06 (maintainer): measure whether compacting just before an
idle session's cache expires would have saved money. The recommendation
must work both ways: suggest turning idle compaction off when its extra
summaries cost more than the later requests save.

Recommendations for the implementer:

- A compaction is one request that reads the whole context and writes a
  summary, after which the next request writes the summary as a new cache
  prefix; price both.
- Compaction also loses detail an agent may re-read later. Say so in the
  output beside the saving.

Idle compaction ([settings](https://code.claude.com/docs/en/settings-reference#idlecompaction),
[cache behavior](https://code.claude.com/docs/en/prompt-caching#compacting-the-conversation),
checked 2026-11-06):

- `idleCompaction` defaults to true. It needs auto-compaction enabled, a
  one-hour cache TTL, and a supported interactive client left open on an
  awake computer. It runs near cache expiry after about an hour since the
  last request; there is no documented 55-minute timer setting. Its default
  context threshold is 200,000 tokens, changed with
  `CLAUDE_CODE_IDLE_COMPACT_MIN_TOKENS`, with a floor of 100,000. The setting
  requires v2.1.290 or later; before v2.1.314 availability also depends on
  Anthropic's feature flags. Do not treat sub-agents, headless runs, tool
  waits, or an unknown client as eligible idle interactive sessions.
- Show the effective on/off setting and threshold, their owners, the
  eligible idle periods and sessions, the fraction that resume after
  expiry, and the fraction that never resume within the observed window.
  Distinguish known-ended sessions from sessions whose future is not yet
  observed. A request gap alone does not prove the client stayed open and
  awake; separate observed opportunities from conditional estimates.
- Replay on and off over the same window and TTL. On pays for the warm
  summarization input and summary output, then the shorter history's cache
  writes and reads after resumption. Off retains the full history, pricing
  cold resumes and any later manual or automatic compactions. Carry the
  changed context forward until the replay paths converge; do not count
  an observed compaction twice. Charge speculative summaries even when the
  session never resumes, and stop at known session closure. For uncertain
  final idle periods, show how open/awake and return assumptions affect
  the result instead of granting free savings or assuming every session
  stays open forever.
- Use observed compaction usage and summary sizes when available. Otherwise
  label estimates and show the assumptions and a savings range, including
  uncertainty about discarded detail being re-read. Do not claim to know
  the uncompacted future of an already-compacted transcript. Report net
  dollars over the window and per month at API prices, not subscription
  bill reductions; keep the current setting when the evidence cannot
  distinguish a winner. If another control prevents idle compaction, report
  the prerequisite instead of suggesting a toggle that has no effect.
- Synthetic fixtures cover a resumed long idle with a net saving, an
  unresumed session where a summary only adds cost, observed idle
  compactions without double counting, uncertain final idle periods, and
  ineligible TTLs, sizes, clients, or disabled auto-compaction. Verify both
  recommendation directions and no recommendation when uncertainty changes
  the winner.

## Required

- [Replay and the cache TTL check](/quest/m0/replay.md) - the reader, prices, and report it adds to
