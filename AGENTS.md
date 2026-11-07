# token-max

A Node-compatible TypeScript CLI that replays this host's Claude Code and Codex transcripts against
public API prices and recommends cheaper agent settings. It only reads
transcripts and settings; it never edits them and never prints prompt or reply
text.

- The dev shell (`nix develop`, or direnv's `use flake`) provides Bun, Node,
  `just`, and `quest`, pinned by `flake.lock`.
- `just check` runs everything CI runs: formatting, TypeScript, a JavaScript
  build, tests under Bun and Node, and `quest check`.
- Tests use small synthetic transcripts checked into the repository, never
  real sessions.

Quests: before any work, run `quest context` and follow it.
