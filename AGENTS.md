# token-max

A Rust CLI that reads this host's Claude Code and Codex transcripts and
settings and tells a person which settings to change. Most advice about agent
configuration is asserted, not measured; this tool shows its evidence.

Every line is measured: replayed from the user's own transcripts at public API
prices, naming what the replay holds constant and what it could not measure.
The maintainer's preferences never become a verdict. A provider's guidance
that the transcripts can't price, such as Claude Code's 200-line CLAUDE.md
advice, is a cited warning apart from the lines, with no verdict and no effect
on the exit code. Say plainly when a cost can't be measured
rather than hiding the setting or guessing a number. A cost measured on only some
requests is extrapolated to the rest, marked with `~` and its margin of error,
and states its assumption that the unpriced requests resemble the priced ones.

- A setting that changes only the bill, such as the cache TTL, gets the
  cheaper value recommended outright.
- One that trades cost for capability, such as a tool, skill or MCP server,
  shows what it cost and how often it was used. Recommend turning it off when
  it was never used (red, exit 1). When it cost more than
  `--max-cost-per-use` (default $1) per recorded use, suggest turning it off
  as an advisory (yellow, exit 0); otherwise recommend keeping it (green).
- Show every setting and feature on its own line, and name the setting that
  changes it.

`token-max doctor` is the command people run. It is interactive: it walks
through the recommendations and offers to apply each one, editing a settings
file only after showing the change and getting a yes. Machine-readable output
for other tools, such as quest's session export, lives in other commands and
never shapes `doctor`. A change to `doctor`'s questions reruns
`assets/demo/walk.tape` and `assets/demo/site.py`, since no check runs them.

- Never edit a transcript, and never print or keep prompt or reply text.
- The README is for someone deciding whether to run the tool. Per-check
  behavior and caveats go in `docs/`.
- Tests use small synthetic transcripts checked into the repository, never
  real sessions.
- `just check` runs everything CI runs on Linux.

Quests: before any work, run `quest context` and follow it, unless this session already holds its rules.
