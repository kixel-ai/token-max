# [M] Sum costs in any order

## Goal

Every cost, saving and context total that adds up charges from more than one
session comes out bit-identical whatever order its parts are added in, so
[the per-session fold](/quest/m0/memory-fold.md) can merge sessions as they
finish and still print the same report on every run.

## Plan

Today `add_costs`, `Sum`, `paired_savings` and the context costs
(`cost_usd +=`) add f64 values in one global (time, id) order, about 60 call
sites. A fold merges per-session partial sums in another order, which changes
the low bits, and a tie or a threshold such as "cannot flip the sign" can then
turn on them.

If the maintainer picks order-independent sums, round each term once to a
fixed grid, such as an `i128` count of 2^-64 dollars, and add integers;
convert to f64 only to print or compare. Integer addition is associative, so
any merge order gives the same total. Keep `Cost`'s known and unknown parts
and its first-seen reasons as they are; only the arithmetic changes.

## Waiting

- Maintainer: how the fold keeps totals identical. An agent can't pick this,
  since each option changes what "identical" in the fold's Goal means.
  Options:
  1. Order-independent sums (recommended): every total changes once, in its
     last bits, when this lands, and printed dollars change only where a
     total sat on a rounding boundary. After that every run and every merge
     order prints the same report.
  2. Ordered merges: keep each request's contribution to every scenario and
     add them in global order at the end. Bit-identical to today, but the
     per-request record grows with the number of scenarios, dozens of values
     for compaction windows and the idle bisection, past the fold's budget.
  3. Accept order-dependent low bits: fold in completion order. Runs of the
     same history can differ in the last bits and, rarely, in a tie-break.

  Option 1 keeps this quest; 2 or 3 deletes it and records the choice in
  the fold's Plan.
