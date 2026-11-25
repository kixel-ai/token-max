# [L] Price the advisor's recorded calls on a doctor line

## Goal

`doctor` shows an `Advisor model` line (info, no verdict) for each group of
Claude profiles that sets an advisor or recorded an advisor call: how many
calls ran on how many requests, on which models, and what the
`advisor_message` iterations alone cost at API prices, with `--details`
giving the unknowns and assumptions. Every recorded call is counted and
priced once, on the right line, and the Fable line accounts for advisor
calls it can't place.

[#217](https://github.com/kixel-ai/token-max/pull/217) landed only the
`vibes` card and the discovery fix; this line and its pricing came out of it,
with the review findings still open on them:

- A scan with only `--t3-dir`, or a T3 instance whose home no native profile
  reads, drops its advisor calls: lines exist only for native profiles.
- A request naming profiles on two advisor lines (two config roots reading a
  symlinked `projects/`) is counted and priced in full on both, where the
  Fable line counts such requests as shared and says whose they were is
  unknown.
- An advisor iteration with no recorded model is left out of the Fable line,
  so a Fable executor's subtotal reads as complete and an Opus executor's
  request is never counted as possible Fable use.
- A `compact_boundary` that links to a request holding `message` and
  `advisor_message` iterations, with no `compaction` iteration, reclassifies
  both as compaction, so the advisor call disappears from the line and its
  usage is billed as summary work.
- When the newer of two streamed snapshots of a request is read first, the
  older one's advisor model, cache counters and tier are lost.

## Plan

Decided 2026-11-25 (maintainer, /quest-run): ✅ land #217 with the card and
the discovery fix only, and move `doctor`'s advisor line and its pricing
here (rejected: keep them in #217, reverting its line split and filling the
cache counters; merge #217 as is). Decided 2026-11-25 (maintainer,
/quest-run): ✅ keep this quest (rejected: drop it).

What moved, to restore from #217's last head before the split,
[0f29d68](https://github.com/kixel-ai/token-max/pull/217/commits/0f29d68039a6167a10361c632e36bc93a9c6fa2b),
and fix rather than rewrite:

- The line itself, grouped as the `vibes` card groups profiles, with
  `--details`' cost breakdown, the README table row and the reference docs.
  Partly priced requests never extrapolate below their known charge.
- The card's sentence naming the window's calls, which shows a blocked
  card's advisor still loaded somewhere.
- Pricing: an advisor iteration records no service tier, so every request
  with one prices as unknown today. Price it at standard when every executor
  iteration on its request is standard (Priority Tier is committed per
  model), stating the assumption.
- Reading: an advisor iteration that records no model stays unpriced
  instead of taking the executor's, and a streamed snapshot keeps an advisor
  model the other recorded, in either read order.

Recommendations for the open findings:

- Give a T3 instance with no native line its own advisor line, with no card,
  rather than dropping its calls.
- Count a request shared across advisor lines once, in a shared entry of
  its own rather than in full on each line (`src/fable.rs` marks such a
  request shared but still adds it to every line), and say so on the lines
  it touches.
- Treat an advisor iteration with no model as possibly Fable in the Fable
  line's coverage note.
- Keep `advisor_message` iterations out of compaction reclassification.
- Merge both snapshots' usage counters and tier, not only the model, in
  either read order.
