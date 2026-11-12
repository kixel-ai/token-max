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

The maintainer picked order-independent sums (rejected: ordered merges, and
accepting order-dependent low bits). Round each term once to a fixed grid,
such as an `i128` count of 2^-64 dollars, and add integers; convert to f64
only to print or compare. Integer addition is associative, so any merge order
gives the same total. Every total changes once, in its last bits, when this
lands. Keep `Cost`'s known and unknown parts and its first-seen reasons as
they are; only the arithmetic changes.
