# [S] Release 0.1.1, the first release crabx runs

## Goal

`token-max` 0.1.1 is on crates.io and is the latest GitHub Release, with archives built and
attested by crabx's pipeline, so `npx @kixel/crabx token-max@0.1.1 --version` works on Linux,
macOS and Windows, and max.kixel.me serves the `/install.sh` and `/install.ps1` that install
through crabx.

## Plan

Decided 2026-11-25 (maintainer, /quest-plan): ✅ 0.1.0 ships with cargo-dist, and 0.1.1 is the
first pipeline release (rejected: holding 0.1.0 for the pipeline). 0.1.0's attestations sign from
a branch, so crabx refuses it.

Follow `docs/releasing.md` as [the switch](/quest/m1/crabx-pipeline.md) rewrites it. Then:

- check that `gh attestation verify` on each archive names crabx's pipeline workflow as signer;
- run `npx @kixel/crabx token-max@0.1.1 --version` on x86_64 glibc Linux, on macOS, and on
  Windows (CI legs are fine), and `run.sh` on Alpine, which picks the musl archive;
- run `doctor --ssh` to a host of another platform, so it downloads the new archives;
- deploy `site/` and check that `curl -LsSf https://max.kixel.me/install.sh | sh` installs crabx
  and token-max.

## Required

- [crabx's pipeline](/quest/m1/crabx-pipeline.md) - builds and attests this release
