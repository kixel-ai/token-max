# [L] Audit the code

## Goal

Run the `code` audit: `quest audit code` prints what it checks, what each finding becomes, and how to plan its next run.

## Plan

Baseline: the m2 run (2026-11-27) read `5cb059f`, about 121k lines of Rust
in `src/` and `build.rs` (129k with `tests/`), up from 82k at the m1 run.
Start from `git diff 5cb059f -- src build.rs tests`, split by area across
parallel readers: rendering (`report.rs`, `tally.rs`, `rows.rs`,
`totals.rs`), commands and UI (`doctor.rs`, `tui.rs`, `cli.rs`, `vibes.rs`),
ingestion (`discovery.rs`, `reader.rs`, `t3.rs`, `opencode.rs`,
`antigravity.rs`, `ssh.rs`, `wire.rs`), replays (`model.rs`, `replay.rs`,
`prices.rs`, `compaction.rs`, `idle.rs`, `turn_end.rs`) and settings checks.

Check first:

- Dead code the compiler can't see. `src/lib.rs` keeps every module private
  except what the binary and `tests/` use, so clippy reports unused
  functions. It can't see a `pub` field of a type `tests/` reaches, so search
  `src/model.rs` for fields written but read only in tests. The m2 run
  removed six, including idle's whole per-context-size cost replay.
- Code only tests use. Build the library without tests (`cargo check --lib`)
  to find whole functions. That won't find a branch only tests reach, such
  as an `Option` parameter every production caller passes as `Some`, so
  check every production call site's value.
- Helpers written again under a new name. The m2 run found copies of
  `report::count`, `plural`, `list` and `by_saving`, `prices::floored`,
  `controls::highest`, and the 30-day projection `report::monthly`. Search
  for distinctive bodies such as `30.0 *`, `== 1 { "" }`, `total_cmp`,
  `split_last` and `.rev().find_map`.
- Platform-only warnings: CI runs clippy on Linux only. The Windows gate job
  and the macOS run after each merge deny warnings.

Kept on purpose:

- `repository.rs` parses Git config by hand rather than running `git`, so
  `sessions` works without Git on PATH.
- `wire::Stream::header` and `request` name the order of the stream.
- `ssh::Profiles`, `cli::Printed` and `Job::Audit`'s fields mirror their
  `wire` messages and convert in one line at the SSH boundary. Folding them
  into `wire` would change `wire::IDENTITY` and help no reader.
- `cli::JsFormatter::write_string_fragment` escapes as it streams instead of
  calling `safe::terminal_text`, so it allocates nothing.
- keepalive's `median` is an upper median, unlike idle's.
- `fast::difference` and `idle::difference` use different known-cost rules.
- `lsp::expand` returns a `String` built with `discovery::join`, and
  `t3::expand` a `PathBuf`. Merging them would change separators on Windows.
- `skills::absolute` accepts Windows paths on any host; `safe::is_absolute`
  follows the host.
- `build.rs` repeats `safe::hex` because a build script can't use crate code.
- `lsp::installs` reads `installed_plugins.json` itself so a bad file is a
  message on the card, not a failed scan.
- `window.rs` has a streaming walk and a parsed-`Value` fallback for lines
  the streaming walk rejects.
- `tests/support/mod.rs`'s `allow(dead_code, unused_imports)`: each test
  binary uses part of it.
- `model::CheckResult`'s `expect(clippy::large_enum_variant)`: a report
  holds a few dozen results.
- The justfile's `run` recipe and the flake's `packages.quest` are for
  people and quest, not the tree.
- `report.rs`'s `summary` and `card` are large, but neither holds a second
  mechanism. Split them only for a reason of their own.

## Audit

- code

## Required

- [m2](/quest/m2/README.md) - runs once per milestone

## Scheduled

- 2026-12-04 - a week after the m2 run, so a fast milestone does not rerun it at once
