# [M] Release through crabx's pipeline instead of cargo-dist

## Goal

token-max's release binaries are built and attested by kixel-ai/crabx's pipeline, so crabx runs
and installs them, and cargo-dist is gone from the repository. `run.sh`, `run.ps1`,
`doctor --ssh` and the upgrade offer work from the pipeline's assets, and max.kixel.me's
`/install.sh` and `/install.ps1` install through crabx's installers. Non-goal: cutting the
release itself, which [0.1.0](/quest/m1/release-0.1.md) does.

## Plan

Decided 2026-11-25 (maintainer, /quest-plan in kixel-ai/crabx and here):

- ✅ crabx's pipeline (rejected: keep cargo-dist and release from a tag, which crabx would also
  accept; drop). "We want to dog food. If the crabx pipeline sucks then quest-plan
  improvements": a gap the pipeline has becomes a crabx quest, not a workaround here. Today
  crabx 0.2.0 refuses token-max@0.0.1, whose attestation signs from `refs/heads/main`.
- ✅ 0.1.0 waits for this switch and is the first pipeline release (rejected: 0.1.0 with
  cargo-dist and the pipeline from 0.1.1; teaching 0.1.0's upgrade the pipeline's names; every
  pipeline release also publishing cargo-dist-named copies). This reverses the same day's first
  pick, after this PR's review found that installed 0.1.0 binaries would upgrade by downloading
  cargo-dist's names from the latest release (`stage` in `src/upgrade.rs`), so 0.1.1 would 404 for
  them. 0.0.1 has no upgrade offer, so no installed binary needs cargo-dist's names.
- ✅ Static musl Linux builds come from the pipeline, beside its glibc ones
  ([kixel-ai/crabx `quest/m1/pipeline-musl.md`](https://github.com/kixel-ai/crabx/blob/main/quest/m1/pipeline-musl.md))
  (rejected: shipping glibc only, which breaks `doctor --ssh` on Alpine and old-glibc hosts).
  `run.sh`, `doctor --ssh` and the upgrade offer keep using the musl archives on Linux.
- ✅ Drop the `.sha256` checks in `run.sh`, `run.ps1` and `doctor --ssh`'s download (rejected:
  the pipeline publishing checksum files; reading GitHub's asset digest). A checksum from the
  same release only shows that the archive arrived whole, which `tar` and the existing
  `--version` check already catch. crabx's attestation check is the authenticity check.
- ✅ `/install.sh` and `/install.ps1` install through crabx's installers taking a crate name
  ([kixel-ai/crabx `quest/m1/installer-crates.md`](https://github.com/kixel-ai/crabx/blob/main/quest/m1/installer-crates.md))
  (rejected: token-max's own installers; the pipeline publishing per-crate installers). A
  redirect can't pass the crate name, so recommended: serve small scripts at those paths that
  run crabx's installer for `token-max`, so shared links keep working.

Recommendations:

- Add the caller with `crabx init` (the configless path every consumer takes), pinned to
  `pipeline-v1.1.0`. `release.yml` keeps its protected crates.io publish (environment
  `crates-io`, trusted publishing). After the publish, it pushes the `v<version>` tag on Cargo's
  recorded commit, so the pipeline reuses it instead of creating `token-max-v<version>`, which
  `run.sh`, `ssh.rs` and `upgrade.rs` don't expect. Then it dispatches the pipeline instead of
  waiting for the daily schedule. If `crabx init`'s workflow can't be run this way cleanly, plan
  that gap in crabx rather than working around it.
- The pipeline's assets are `token-max-<target>-v<version>.tgz` on every platform, Windows
  included, with the binary under `token-max-<target>-v<version>/`. Move `run.sh`, `run.ps1`,
  `src/ssh.rs`'s `DOWNLOAD`, `src/upgrade.rs`, `tests/run_script.rs` and the ssh tests to those
  names. Windows has `tar.exe` from 1803 on.
- Delete `dist-workspace.toml`, cargo-dist's steps and staging in `release.yml`, and
  `[package.metadata.binstall]` in `Cargo.toml`, since the pipeline uses binstall's defaults.
  Rewrite `docs/releasing.md`'s Binaries section, and the README's and `docs/reference.md`'s
  installer lines.
- The page deploys by hand. The deployed run scripts request cargo-dist's names from the latest
  release, and the new ones and `/install.sh` need a release with pipeline assets. So this PR
  changes `site/` and leaves the deploy to 0.1.0's release, right after the pipeline uploads its
  archives. Between the pipeline creating the release and that deploy, the old `run.sh` gets a
  404, a window of minutes.
- Validate with the pipeline's manual dispatch on this branch's head. It builds and attests
  every target without creating tags or releases. Point `run.sh` and the ssh download at a local
  copy of those archives (`TOKEN_MAX_RELEASES`) to check them end to end.

## Waiting

- kixel-ai/crabx: the `pipeline-v1.1.0` tag, whose pipeline builds musl archives. Check with
  `git ls-remote --tags https://github.com/kixel-ai/crabx pipeline-v1.1.0`.
- kixel-ai/crabx: crabx's installers taking crate names. Check that
  `curl -fsSL https://crabx.kixel.me/install.sh` mentions `CRABX_INSTALL`.
