# [S] Compare Claude Code versions with one function

## Goal

One function decides whether a Claude Code version is at least a given
release, used by every check that gates on a version.

## Plan

Four parsers behave differently today:

- `idle::supported` accepts only digits.
- `compaction::version_supported` parses each part as a JavaScript Number,
  so it accepts `2.01.0x200` and `2.1.-0`.
- `replay::version_supported` parses the same way but finite only, with 242
  hard-coded.
- `subagents::version` takes leading digits only.

Tests pin each one's quirks. The quirks date from the Rust rewrite
([kixel-ai/token-max#22](https://github.com/kixel-ai/token-max/pull/22)) and
look like leftovers of the JavaScript port, not mirrors of Claude Code.

Recommendation: write `claude_at_least(version, minor, patch)` accepting plain
digit parts only, and treat anything else as unsupported. Real versions are
plain digits, so only malformed version strings change outcome. Drop the
tests that pin the quirks.
