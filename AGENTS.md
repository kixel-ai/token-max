# token-max

A Rust CLI that replays this host's Claude Code and Codex
transcripts against public API prices. It only reads transcripts and settings;
it never edits them and never prints prompt or reply text.

Its job is to put an estimated dollar amount on every setting it can measure
from transcripts, and to help the user decide what to enable or disable.

- Some settings don't change the experience, such as the cache TTL. Recommend
  the cheaper value outright.
- Others trade cost for capability, such as a tool, skill or MCP server.
  Report what each one cost and how often it was used. Recommend turning it
  off when it was never used (red, exit 1). When it cost more than
  `--max-cost-per-use` (default $1) per recorded use, suggest turning it off
  as an advisory (yellow, exit 0); otherwise recommend keeping it (green).
- Show every setting and feature on its own line with its cost. Name the
  setting that turns each one off, and say plainly when a cost can't be
  measured rather than hiding the setting.

Development:

- The dev shell (`nix develop`, or direnv's `use flake`) provides the Rust
  toolchain, `just`, and `quest`, pinned by `flake.lock`.
- `just check` runs everything CI runs on Linux: formatting, clippy with
  warnings as errors, tests, and `quest check`.
- Tests use small synthetic transcripts checked into the repository, never
  real sessions.

Quests: before any work, run `quest context` and follow it.
