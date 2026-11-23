# [L] Add a vibes command and move the Bash cap and Warnings into it

## Goal

`token-max vibes` walks the advice token-max can't measure, using `doctor`'s
cards and y/n/q confirm. It offers an edit only where one setting applies, and
shows every other item as a cited card with no question. It exits 0, or 2 on
a failed scan. Off a terminal it prints the cards.

Two kinds of item move out of `doctor` into `vibes`. The first is today's
Warnings: main-session effort above `high`, instruction files past their
budget, Explore off Haiku and Fable weekly use. The second is the Bash timeout
cap, the only line whose verdict rests on a guess. After this, `doctor` never
recommends or offers a change it didn't measure.

When `doctor`'s walk ends on a terminal and `vibes` has items, `doctor` asks
whether to show them. It runs `vibes` from the same scan, without rereading
transcripts. Off a terminal, `doctor` prints a one-line hint naming
`token-max vibes`.

## Plan

Decision prompts, chosen options marked ✅ (maintainer, 2026-11-23):

- `vibes` itself: ✅ `doctor`'s walk, offering an edit where one setting
  applies, exit 0 (rejected: exit 1 when it has an offer, which puts a verdict
  back on a guess; print only, which drops the Bash cap edit that motivated
  the command).
- Hand-off: ✅ ask only when `vibes` has items, from the same scan, and print
  a hint off a terminal (rejected: always hint and never ask, which makes the
  user rerun and rescan).
- Bash cap home: ✅ all in `vibes`. Its card shows the measured long waits and
  their cache-miss cost, and says the saving isn't measured (rejected: an
  "at most $X" bound on doctor's line with no verdict; the same evidence in
  both commands; a verdict from the bound, which needs a guessed threshold).
  The maintainer: "some commands legit need longer to run".
- Bash target: ✅ suggest `safe_cap` (4m under a 5m cache, 55m under 1h)
  whenever the cap differs from it, raising a low cap as well as lowering a
  high one (rejected: only when far off, which adds a second guessed
  threshold). This also fixes today's gap: a 4m30s cap under a 5m TTL is
  kept, while 5m gets 4m offered.

Recommendations:

- The effort, instruction-file, subagent and Fable lines are already measured
  evidence in `doctor` and stay there. Only their warnings move. The Bash cap
  line leaves `doctor` entirely, so AGENTS.md's "show every setting on its own
  line" holds across the two commands. Rewrite AGENTS.md's paragraph on
  provider guidance and its `doctor` paragraph to say so.
- Reuse `doctor`'s walk, profiles, `--ssh` hosts and T3 instances rather than
  build a second one. `vibes` covers what `doctor` covers.
- A raised cap trades foreground waits for fewer background check-ins. A
  lowered cap doesn't kill a command: it keeps running in the background. Say
  so on the card.
- `doctor`'s questions change, so rerun `assets/demo/walk.tape` and
  `assets/demo/site.py`. The demo walks a 2h Bash cap today, which now belongs
  to `vibes`.
- [kixel-ai/token-max#199](https://github.com/kixel-ai/token-max/pull/199)
  reworks how `doctor` opens. Expect to merge with it.
