# [S] Simplify the SSH wire and doctor presentation types

## Goal

The SSH stream and `doctor` describe each thing once: one error type, one
presentation setting, and a reply that carries only what a destination sends.

## Plan

The m1 code audit found:

- `wire::Outcome::Error { message, location }` repeats `ScanError`'s two
  fields. It is converted one way in `ssh::failure` and back in two identical
  blocks in `src/ssh.rs`. Derive serde on `ScanError`, carry
  `Outcome::Error(ScanError)`, and add `Session` helpers for:
  - the remote error;
  - the "invalid SSH audit stream" error, built twice;
  - "token-max sent a different result than requested", written four times.
- `wire::Format`, `report::ReportOptions` and `doctor::Style` hold the same
  width, color, unicode and host settings:
  - `src/cli.rs` copies `Format` into `ReportOptions` field by field, and
    builds both `Format` and `Style` from the same values.
  - `ssh::serve` rebuilds a `Style` from a `Format`.

  Make `Style` hold a `Format`, or build it with one `Format::style(home)`.
- `Rendered` has `fixes`, `listing` and `turned_off`, which a destination's
  report never fills, and both local readers `.expect` the listing. Give the
  remote replies their own `{ text, change, totals }`, and make `listing`
  non-optional for local reports.

The wire's header pins the exact build (`wire::build` hashes these sources),
so a wire change needs no compatibility handling. Output must not change.
