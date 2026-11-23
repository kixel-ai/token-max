# [L] Offer to upgrade when a newer release is out

## Goal

When `token-max doctor` runs a build older than the latest GitHub release, it
says so before the analysis and offers to upgrade and rerun, since Claude Code
and Codex change fast enough that an old build's recommendations may be stale.
Not a background updater, and `--offline` checks nothing.

## Plan

Decided 2026-11-23 (/quest-plan):

- Signal: ✅ only a newer token-max release (rejected: also warn when
  transcripts come from a Claude Code or Codex version newer than the build).
- Upgrade: ✅ ask "upgrade to vX and rerun?", and on yes put the release in
  place of the running binary, checksum-verified and atomic, then re-exec with
  the same arguments (rejected: print the install method's command only; run
  the newer release from a cache for this run only, as `run.sh` does). A
  binary a package manager owns, such as one under `~/.cargo/bin` or
  `/nix/store`, gets that manager's command printed instead. `run.sh` already
  runs the latest release, so it never sees the offer.
- Unreachable releases: ✅ say on one line that the check could not run and
  continue (rejected: fail the run like a failed price fetch).
- When: ✅ every `doctor` run, before reading transcripts (rejected: at most
  daily from a cached result). Only `doctor`; machine-readable commands never
  check. Without a terminal, print the warning to stderr and continue on the
  current build.
- Declined: ✅ ask again every run, with no remembered answer and no new flag;
  `--offline` is the only skip (rejected: remember the declined version; a
  `--no-update-check` flag).

Recommendations:

- Find the latest tag the way `run.sh` does, from the `/releases/latest`
  redirect, with `ureq`, which doctor already uses for prices. Honor
  `TOKEN_MAX_RELEASES` so tests serve releases from a directory, as
  `run.sh`'s tests and `src/ssh.rs`'s already do; no test reaches GitHub.
- Release archives are `.tar.xz` (`.zip` on Windows) and token-max has no
  extractor. Running that version's own cargo-dist installer (`install.sh` /
  `install.ps1`) into the running binary's directory avoids new dependencies
  and handles Windows, where a running `.exe` can't be overwritten; weigh it
  against adding extraction crates, and check whether the installer verifies
  checksums before relying on it.
- A declined or failed upgrade is advisory: exit code unchanged.
- The offer is a new doctor question, so rerun `assets/demo/walk.tape` and
  `assets/demo/site.py`, and keep the offer out of them (they run a current
  build). Document the behavior in `docs/reference.md`.
- [kixel-ai/token-max#199](https://github.com/kixel-ai/token-max/pull/199)
  reworks how doctor opens; put the offer before its splash.

