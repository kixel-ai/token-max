# token-max

A Rust CLI that replays this host's Claude Code and Codex transcripts against
public API prices and recommends cheaper agent settings. It only reads
transcripts and settings; it never edits them and never prints prompt or reply
text.

- The dev shell (`nix develop`, or direnv's `use flake`) provides the pinned
  Rust toolchain, `just`, and `quest`.
- `just check` runs everything CI runs: rustfmt, clippy with warnings denied,
  tests, and `quest check`.
- Tests use small synthetic transcripts checked into the repository, never
  real sessions.

Quests: before any work, run `quest context` and follow it.
