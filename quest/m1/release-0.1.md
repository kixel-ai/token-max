# [M] Release 0.1.0 and deploy max.kixel.me

## Goal

`token-max` 0.1.0 is on crates.io and is the latest GitHub Release, and
max.kixel.me serves the page from `main` whose demo matches that binary, so
the maintainer can promote the site.

## Plan

Decided 2026-11-24 (maintainer, /quest-run): ✅ release 0.1.0 once the open
m1 feature PRs below land (rejected: 0.1.0 from main now, leaving the vibes
cards for 0.1.1; 0.0.2 from main now). The only release, 0.0.1 from
2026-11-18, predates `vibes`, the upgrade offer, the splash and the per-host
payment questions, and main's `site/public/index.html` already demos flags
only the unreleased binary has, so main's page cannot be deployed before a
release.

Decided 2026-11-25 (maintainer, /quest-plan): ✅ 0.1.0 waits for
[the move to crabx's pipeline](/quest/m1/crabx-pipeline.md) and is the first pipeline release
(rejected: 0.1.0 with cargo-dist, then the pipeline from 0.1.1). A cargo-dist 0.1.0 would sign
from a branch, which crabx refuses, and its upgrade offer would look for cargo-dist's asset names
in every later release.

Follow `docs/releasing.md`'s Publish section, as the switch rewrites it: the version PR after an end to
end `doctor` run on a real host, a rerun of `assets/demo/site.py` on `nixos`
so the page's run shows 0.1.0 (read it for private names before committing),
a `walk.gif` rerun if the cards changed doctor's questions, dispatching
**Release** on `main`, deploying `site/`, and the post-publication installer
checks. Also:

- before the crates.io publish, the Release run dry-runs the pipeline on its own commit, the one
  the crate will name, and publishes only if every target builds. The pipeline builds only
  versions crates.io has, so it can't stage artifacts to promote later, but its run after the
  publish builds the same commit, and a flaky job reruns alone, reusing the other targets'
  archives (`pipeline/README.md` in kixel-ai/crabx);
- before the publish, deploy the site pinned to `v0.0.1` that the switch describes, and after
  the pipeline uploads 0.1.0's archives, deploy main's `site/`;
- check that `gh attestation verify` on each archive names crabx's pipeline workflow as signer;
- run `npx @kixel/crabx token-max@0.1.0 --version` on x86_64 glibc Linux, on macOS and on Windows
  (CI legs are fine), and `run.sh` on Alpine, which picks the musl archive;
- run `doctor --ssh` to a host of another platform, so it downloads the new archives;
- after that deploy, check that `curl -LsSf https://max.kixel.me/install.sh | sh` installs
  crabx and token-max.

Decided 2026-11-24 (maintainer, /quest-run): ✅ release without the language
servers card (rejected: hold 0.1.0 for it). Its PR,
[kixel-ai/token-max#218](https://github.com/kixel-ai/token-max/pull/218), still
had a malformed plugin registry aborting every command, so it ships in a later
release, where the upgrade offer reaches 0.1.0's users.

Decided 2026-11-25 (maintainer, /quest-run): ✅ 0.1.0 waits for
`advisor-after-default.md` (rejected: ship 0.1.0 without it). With the
`Main-session default` card landed, accepting it (Sonnet to Opus) and the
advisor card (`opus`) in one walk would write an Opus advisor under an Opus
main.

## Required

- [crabx's pipeline](/quest/m1/crabx-pipeline.md) - builds and attests this release

- [Short doctor output](/quest/m1/short/README.md) - the short report and walk the page and first release show
