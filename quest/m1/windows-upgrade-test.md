# [M] Drive doctor's upgrade offer on Windows in CI

## Goal

CI's `Tests (windows-2025)` job answers doctor's upgrade offer yes on a real
console and checks what only Windows does: Windows' own `tar.exe` unpacks the
release zip, the running binary moves aside to `token-max.exe.old`, the rerun
is spawned and waited on, and its exit code becomes the run's. A later run
deletes the `.old` file. Today only the Unix pty tests in `tests/cli.rs` drive
the offer, so a change that breaks the Windows path still passes CI.

## Plan

- Attach the child to a pseudo console: `CreatePseudoConsole` with
  `STARTUPINFOEXW` through `windows-sys`, already a Windows dev-dependency.
  `std::process::Command` cannot attach one. Match the output by substring,
  since conhost re-renders it.
- The Windows fixture in `tests/support/mod.rs` packs the real binary, whose
  version is this build's, so no release it publishes is newer. Make a
  `harness = false` test binary the stand-in, the way `ssh_windows` doubles as
  the fake `ssh.exe`. It prints the version the test sets in an environment
  variable, which doctor passes on to it.
- Develop it off Windows under Wine 11 with a MinGW build of libarchive's
  `bsdtar` installed as `tar.exe`; that setup ran the whole path end to end for
  [kixel-ai/token-max#208](https://github.com/kixel-ai/token-max/pull/208).
  Wine's sockets and file locks differ from Windows, though: it passed tests
  that failed on windows-2025 for want of `SystemRoot`. Only the CI job shows
  that the test works.
