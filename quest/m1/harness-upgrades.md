# [M] Recommend upgrading a harness that a version-gated check needs

## Goal

When a `doctor` line or a `vibes` card is held back because the latest
session on a profile ran a Claude Code or Codex version too old for the
setting, it says which version unlocks it and recommends upgrading. Today
those checks quietly turn advisory or a step by hand. Each line keeps the
verdict and exit code it has now; the upgrade is the step it names.
token-max doesn't upgrade a harness itself.

## Plan

Decided 2026-11-25 (maintainer, /quest-plan over five cost tips from
@TekonBurns's posts). The maintainer asked "maybe detect the version and
recommend upgrading?" ✅ A quest for every version-gated check (rejected:
upgrade advice only in [the subagent card](/quest/m1/vibes/subagent-default.md)
and #206's OpenCode note; only in the subagent card). Most users
auto-update, so few will see it, but one owner keeps the wording and version
logic in one place.

Recommendations:

- The gates live at the `claude_at_least` and `codex_at_least` calls
  (`src/version.rs`), in `idle.rs`, `replay.rs`, `compaction.rs`,
  `turn_end.rs`, `subagents.rs` and `doctor.rs`'s `deniedMcpServers` gate,
  which already names the version in its step by hand. Route them through
  one helper that returns the version that unlocks the check, and word
  every line the same way.
- Judge from the profile's latest request, as the existing gates do. Say
  "an unrecorded version" when there is none, never guess.
- Name an upgrade command only when the install method is known, such as a
  native install's `claude update`. For a Nix, npm, Homebrew or T3-bundled
  install, name the version only. Check how T3 picks the Claude Code and
  Codex it runs before advising its instances.
- Cover a synthetic old-version transcript for each gate.

## Related

- [Release check](/quest/m1/releases.md) - each run that finds a new version
  gate adds it here, through the shared helper
