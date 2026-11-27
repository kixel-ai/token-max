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
  improvements": a gap the pipeline has becomes a crabx quest, not a workaround here. crabx
  0.2.0 and later (0.2.1 on 2026-11-26) refuse token-max@0.0.1, whose attestation signs from
  `refs/heads/main`.
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
  (kixel-ai/crabx's `quest/m1/installer-crates.md`, finished in its commit 6f8d275)
  (rejected: token-max's own installers; the pipeline publishing per-crate installers). A
  redirect can't pass the crate name, so recommended: serve small scripts at those paths that
  run crabx's installer for `token-max`, so shared links keep working. Those installers shipped
  (`https://crabx.kixel.me/install.sh` reads `CRABX_INSTALL`, checked 2026-11-26).

Recommendations:

- Add the caller with `crabx init` (the configless path every consumer takes), pinned to
  `pipeline-v1.1.0`. `release.yml` keeps its protected crates.io publish (environment
  `crates-io`, trusted publishing). After the publish, it pushes the `v<version>` tag on Cargo's
  recorded commit, so the pipeline reuses it instead of creating `token-max-v<version>`, which
  `run.sh`, `ssh.rs` and `upgrade.rs` don't expect. Then it dispatches the pipeline instead of
  waiting for the daily schedule, and once the archives are up it marks the release latest.
  Before the publish, it runs the pipeline's consumer dry run on its own commit. If `crabx init`'s workflow can't be run this way cleanly, plan
  that gap in crabx rather than working around it.
- The pipeline's assets are `token-max-<target>-v<version>.tgz` on every platform, Windows
  included, with the binary under `token-max-<target>-v<version>/`. Move `run.sh`, `run.ps1`,
  `src/ssh.rs`'s `DOWNLOAD`, `src/upgrade.rs`, `tests/run_script.rs` and the ssh tests to those
  names. Windows has `tar.exe` from 1803 on.
- Delete `dist-workspace.toml`, cargo-dist's steps and staging in `release.yml`, and
  `[package.metadata.binstall]` in `Cargo.toml`, since the pipeline uses binstall's defaults.
  Rewrite `docs/releasing.md`'s Binaries section, and the README's and `docs/reference.md`'s
  installer lines.
- Outside `docs/`, two texts name cargo-dist's release: the `deps` audit stub's scope (cargo-dist's
  version), which becomes the pipeline pin, and kixel-ai/quest's `SETUP.md`, which says the run
  scripts check "the release's checksums". Edit the stub here and open a kixel-ai/quest PR
  dropping that clause once this lands.
- The page deploys by hand. The deployed run scripts request cargo-dist's names from the latest
  release, and the new ones and `/install.sh` need a release with pipeline assets. So this PR
  changes `site/` and leaves the deploys to 0.1.0's release: before publishing, deploy a site whose
  run scripts and install redirects are pinned to `v0.0.1`, so the latest release flipping
  breaks nothing, then deploy this PR's `site/` once the pipeline has uploaded 0.1.0's archives.
- Validate the branch in two halves, since no pipeline run on it leaves archives to download: a
  consumer's dispatch builds only versions already on crates.io, and the consumer dry run never
  uploads (kixel-ai/crabx's `quest/m1/consumer-dry-run.md`, finished in its commit 0ea2324). Run the pipeline's consumer dry run on this
  branch's head to show every target builds. Check the names and layout end to end on archives
  the tests and a manual run build locally in the pipeline's shape (binstall's default
  `token-max-<target>-v<version>/` directory in a `.tgz`), served through `TOKEN_MAX_RELEASES`
  to `run.sh`, `run.ps1`, `doctor --ssh` and the upgrade offer. The pipeline's real archives
  are first checked by [0.1.0](/quest/m1/release-0.1.md)'s post-publication checks, while the
  deployed site is still pinned to `v0.0.1`.

## Waiting

- kixel-ai/crabx: the `pipeline-v1.1.0` tag, whose pipeline builds musl archives and offers the
  consumer dry run that 0.1.0's Release gates on. kixel-ai/crabx's `quest/m1/pipeline-v1.1.md`
  cuts it once its musl builds (`quest/m1/pipeline-musl.md`) land. Check with
  `git ls-remote --tags https://github.com/kixel-ai/crabx pipeline-v1.1.0`.
