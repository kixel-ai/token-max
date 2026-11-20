# [M] Test SSH audits run from Windows

## Goal

`token-max doctor --ssh HOST` run on Windows, a platform the release ships, is
either exercised by tests on the Windows CI runner or refused there with an
error that names what to run instead. Today it compiles and ships with no test
on any platform it runs on.

## Plan

`tests/ssh.rs` is `cfg(unix)`: its fake `ssh` and destinations are `sh`
scripts, and it signals children with `libc` and drives a pty. `src/ssh.rs`'s unit
tests are Unix-only too.

To test it: the `windows-2025` runner has Git for Windows' `sh`, so a fake
`ssh.cmd` can run a destination's command with it. A destination must look
like Linux or macOS (a faked `uname`), so the coordinator takes the
other-platform path and runs an installed `token-max` whose `--version`
matches; a shim running this build with `--ssh-scan` can stand in for it.
Cover a report, a failed destination, and a cancel.

Cancelling differs on Windows: `ssh.rs` puts each `ssh` in its own process
group only on Unix, so a Ctrl-C reaches the children directly, not only
token-max. The tests decide whether that still stops every destination and
reports `cancelled`.

## Hosts

- windows - the coordinator under test runs there

## Waiting

- Maintainer: test Windows coordinators, or refuse `--ssh` on Windows? (a)
  Port the harness as the Plan describes (this quest's size). (b) Refuse
  `--ssh` on Windows with an error suggesting WSL, documented in
  `docs/reference.md`, which is an XS change. Recommended: (a), since the
  release ships Windows binaries and `run.ps1` as first-class, and (b) would
  drop a feature that may already work. An agent cannot pick: it decides
  which platforms the feature supports.
