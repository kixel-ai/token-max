# [L] Section the report by kind of claim

## Goal

The report's sections are **Measured**, **Official** and **Opinion**, in
that order, and every setting and feature line sits under the kind of claim
its recommendation rests on. The line's color carries the verdict: red for
a recommended change, yellow for an advisory, green for a setting to keep.
Each line has a second line of evidence. A Measured line says what the
replay held constant and what it could not measure, so a smaller
compaction window, idle compaction and the Bash cap say what they give up.
An Official line cites the provider's guidance. An Opinion line states the
maintainer's preference beside whatever was measured.

## Plan

Decided 2026-11-11 (maintainer, /quest-plan for the follow-ups of
[kixelated/token-max#62](https://github.com/kixelated/token-max/pull/62),
whose `AGENTS.md` requires the three kinds). The pitch is evidence against
unproven advice, so a preference must not look like a measurement.
Decision prompts, chosen options marked ✅:

- Goal and milestone: ✅ its own quest in m0, absorbing the follow-up about
  checks that change behavior (rejected: m1; folding it into the verdict
  colors, which landed in
  [kixelated/token-max#64](https://github.com/kixelated/token-max/pull/64)).
- Layout: ✅ the tiers are the top-level sections and color carries the
  verdict (the maintainer, after seeing three mockups with Change, Keep
  and Not enough evidence on top: "flip the sections"; rejected: a tier
  word on each line's evidence, a tier tag column, tier sub-headings).
- Density: ✅ two lines per setting (rejected: one line with a few words of
  evidence, about 92 columns wide).
- Name: ✅ Official for the provider's guidance (rejected: Documented).
- Severity of an opinion: ✅ an Opinion line can be a red change, set case
  by case, so fast mode on stays red as decided 2026-11-06 and auto memory
  stays yellow (rejected: Opinion and Official always advisory).

The chosen layout, with invented names and numbers:

```text
MEASURED  replayed from your own sessions
● Prompt cache TTL       5m → 1h        saves $150   red
  Replayed at both lifetimes, assuming the same sessions.
● Auto-compact window    400k → 200k    saves  $60   red
  Net of 14 extra summaries. Lost detail is not measured.
● Agent tool             on             costs  $40   green
  Used 310 times, $0.13 each.
○ Idle compaction        on                          dim
  Not enough evidence: no idle gaps to compare.

OFFICIAL  the provider's own guidance
● ~/.claude/CLAUDE.md    412 lines      costs   $5   yellow
  Claude Code suggests under 200 lines.

OPINION  the maintainer's preference
● Fast mode              on → off       costs  $40   red
  Standard speed is enough. The premium is measured.
● Auto memory            on             costs  $12   yellow
  Prefer reviewed, checked-in instructions.
```

Agent's choices, for the implementer to revisit:

- Which tier each check lands in. Measured: cache TTL, compaction window,
  idle compaction, Bash cap, keepalive, background generation, and tools,
  skills and MCP servers under `--max-cost-per-use`. Official: instruction
  budgets and other cited provider guidance. Opinion: fast mode and auto
  memory.
- Lines without enough evidence sit dim at the end of Measured.
- Without color (`--plain`, `NO_COLOR`, redirected output) a word carries
  the verdict in place of the colored dot.
- Within a section, red lines come first, then yellow, then green.

The maintainer asked that the output look good; show a real run's report
in the PR. The same PR updates the README's sample and
`docs/reference.md`.

## Related

- [Report auto memory's total cost and advise against it](/quest/m0/auto-memory.md) - its yellow line is the first Opinion advisory
- [Run the report as an interactive doctor](/quest/m0/doctor.md) - offers the red lines first, in this order
