# [M] Count every recorded advisor call once, on the right line

## Goal

`doctor`'s `Advisor model` line counts and prices every advisor call the
window recorded, once, and the Fable line accounts for advisor calls it can't
place. Today, from the review of
[#217](https://github.com/kixel-ai/token-max/pull/217):

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

## Plan

Recommendations:

- Give a T3 instance with no native line its own advisor line, with no card,
  rather than dropping its calls.
- Count a request shared across advisor lines the way `src/fable.rs` does,
  and say so on the line.
- Treat an advisor iteration with no model as possibly Fable in the Fable
  line's coverage note.
- Keep `advisor_message` iterations out of compaction reclassification.
