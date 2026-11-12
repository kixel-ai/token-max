# [M] Remove the JSON report

## Goal

`token-max` has no `--json` flag. The schema version 2 report, its
serialization code and `docs/json.md` are gone, and every test that read
the JSON asserts on the text report or on the analysis itself. The SSH
stream between hosts and the planned `sessions` export are untouched.

## Plan

Decided 2026-11-11 (maintainer, /quest-plan for `doctor`). Decision prompt,
chosen option marked ✅:

- `--json` once people run `doctor`: ✅ remove it (rejected: keep it as
  `doctor --json`; move it to its own sub-command). It has no consumer: SSH
  audits use their own stream (`--ssh-scan`, `src/wire.rs`), and
  moq-dev/quest reads only the session export, `token-max sessions`. The
  report is for people; each machine output is its own sub-command with a
  named consumer.

This is its own quest, ranked before `doctor`, so `doctor` never has to
define a JSON mode and the tests are rewritten once.

Agent's notes, for the implementer to revisit:

- `tests/cli.rs` and `tests/ssh.rs` pass `--json` 33 times and assert on
  its fields. Keep what each test proves; change only where it looks.
- Remove what only the JSON report used, such as `Serialize` derives the
  SSH stream does not need.
- Exit codes stay 0, 1 and 2 here; `doctor` changes them.
- Fix the README, `docs/reference.md`, `docs/semantics.md` and `--help`
  in the same PR.
