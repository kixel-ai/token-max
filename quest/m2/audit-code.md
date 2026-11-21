# [M] Audit the code

## Goal

Run the `code` audit: `quest audit code` prints what it checks, what each finding becomes, and how to plan its next run.

## Plan

Baseline: the m1 run (2026-11-20) covered the whole tree at about 82k lines
of Rust.

Check first:

- Dead code the compiler can see: `src/lib.rs` keeps every module private
  except what the binary and `tests/` use, so `cargo clippy` reports unused
  functions. It cannot see a `pub` field of a type `tests/` reaches, so
  search `src/model.rs` for fields written but read only in tests (the m1 run
  removed a dozen).
- Code only tests use. Build the library without tests (`cargo check --lib`)
  to find whole functions. It cannot see a branch only tests reach, such as a
  trace behind a flag production always passes as `false`: search for `bool`
  parameters and options and check every production call site's value.
- Helpers written again under a new name. The m1 run found copies of
  `median`, `harness_name`, `js_string`, `js_space`, hex encoding, the 30-day
  projection and plural helpers. Search for distinctive bodies such as
  `30.0 *`, `{b:02x}` and `== 1 { "" }`.
- Platform-only warnings: CI runs clippy on Linux only, and the macOS and
  Windows test job denies warnings.

Kept on purpose:

- `repository.rs` parses Git config by hand rather than running `git`, so
  `sessions` works without Git on PATH.
- `wire::Stream::header` and `request` name the order of the stream.
- `cli::JsFormatter::write_string_fragment` escapes as it streams instead of
  calling `safe::terminal_text`, so it allocates nothing.
- keepalive's `median` is an upper median, unlike idle's.
- `fast::difference` and `idle::difference` use different known-cost rules.
- The justfile's `run` recipe and the flake's `packages.quest` are for
  people and quest, not the tree.
- `report.rs`'s `summary` (about 1,350 lines) and `card` (about 730) are
  large, but neither holds a second mechanism. Split them only for a reason
  of their own.

## Audit

- code

## Scheduled

- 2026-11-27 - a week after the m1 run, so a fast m1 does not rerun it at once

## Required

- [m1](/quest/m1/README.md) - runs once per milestone
