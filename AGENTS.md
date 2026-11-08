# token-max

A Node-compatible TypeScript CLI that replays this host's Claude Code and Codex
transcripts against public API prices. It only reads transcripts and settings;
it never edits them and never prints prompt or reply text.

Its job is to put an estimated dollar amount on every setting it can measure
from transcripts, and to help the user decide what to enable or disable.

- Some settings don't change the experience, such as the cache TTL. Recommend
  the cheaper value outright.
- Others trade cost for capability, such as a tool, skill, MCP server or
  instruction file the user may not use. Report what each one cost over the
  window and how often it was used, then leave the call to the user. Never
  claim one is waste just because it was used rarely.
- Name the setting that turns each one off, and say plainly when a cost can't
  be measured rather than hiding the setting.

Development:

- The dev shell (`nix develop`, or direnv's `use flake`) provides Bun, Node,
  `just`, and `quest`, pinned by `flake.lock`.
- `just check` runs everything CI runs: formatting, TypeScript, a JavaScript
  build, tests under Bun and Node, and `quest check`.
- Tests use small synthetic transcripts checked into the repository, never
  real sessions.

Quests: before any work, run `quest context` and follow it.
