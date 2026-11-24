# [S] Leave crabx's binaries to crabx in the upgrade offer

## Goal

When token-max runs from crabx's cache, its upgrade offer names crabx as the
binary's manager and never replaces the file, so a `crabx token-max` or
`npx @kixel/crabx token-max` run can't overwrite the entry crabx verified for
that version.

## Plan

Split 2026-11-24 from [the crabx tabs](/quest/m1/crabx.md) by its review:
0.1.0 is the first release with the upgrade offer (0.0.1 has no
`src/upgrade.rs`), so 0.1.0 shipping this guard keeps every release a pinned
`crabx token-max@<version>` can run safe. The tabs alone would leave 0.1.0
unguarded.

Recommendations:

- In `upgrade::Paths::install`, treat a binary under crabx's cache (`crabx/v1/`
  under the OS user cache directory, which crabx 0.1.0's `cache.rs` takes
  from `dirs::cache_dir` with no override) as `Install::Manager`. An unpinned run's newest eligible
  version comes from crabx, so say crabx picks it up, without promising when:
  crabx waits until a release is a day old, and a pinned run never moves.
- Cover it with a `Paths` unit test, as the run-script and Nix cases have.
- Exercise it on the built binary before 0.1.0 ships: copy it into a
  crabx-shaped cache path (`<cache dir>/crabx/v1/.../bin/token-max`), run
  `doctor` against a fixture release newer than it, as `tests/run_script.rs`
  serves one, and check that the offer names crabx and the copy's SHA-256 is
  unchanged afterwards.
