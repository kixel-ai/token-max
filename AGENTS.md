# token-max

A Rust CLI that reads this host's Claude Code and Codex transcripts and
settings and tells a person which settings to change. Most advice about agent
configuration is asserted, not measured; this tool shows its evidence.

Every recommendation is one of three kinds, and its line says which:

- **Measured**: replayed from the user's own transcripts at public API prices.
  Name what the replay holds constant and what it could not measure.
- **Official**: the provider's own guidance, cited, beside whatever the
  transcripts show.
- **Opinion**: the maintainer's preference, such as avoiding auto memory in
  repositories that have an AGENTS.md. Label it as one, and show the
  measurement that makes it apply to this user.

Never present one kind as another. Say plainly when a cost can't be measured
rather than hiding the setting or guessing a number.

- A setting that changes only the bill, such as the cache TTL, gets the
  cheaper value recommended outright.
- One that trades cost for capability, such as a tool, skill or MCP server,
  shows what it cost and how often it was used. Recommend turning it off when
  it cost more than `--max-cost-per-use` (default $1) per recorded use,
  counting no use as one; otherwise recommend keeping it.
- Show every setting and feature on its own line, and name the setting that
  changes it.

`token-max doctor` is the command people run. It is interactive: it walks
through the recommendations and offers to apply each one, editing a settings
file only after showing the change and getting a yes. Machine-readable output
for other tools, such as quest's session export, lives in other commands and
never shapes `doctor`.

- Never edit a transcript, and never print or keep prompt or reply text.
- The README is for someone deciding whether to run the tool. Per-check
  behavior and caveats go in `docs/`.
- Tests use small synthetic transcripts checked into the repository, never
  real sessions.
- `just check` runs everything CI runs on Linux.

Quests: before any work, run `quest context` and follow it.
