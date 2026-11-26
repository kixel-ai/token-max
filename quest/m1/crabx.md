# [M] Offer crabx as the first way to run and install token-max

## Goal

max.kixel.me opens on a `node` tab showing `npx @kixel/crabx token-max doctor`,
followed by a `crabx` tab and then today's `macOS / Linux` and `Windows` run
scripts, which stay. The README and `docs/reference.md` list the same lines.
Run through crabx, `doctor` is interactive on Linux, macOS and Windows.
Non-goals: publishing token-max to npm itself, and holding back 0.1.0 for this.

## Plan

Decided 2026-11-24 (maintainer, /quest-plan):

- ✅ crabx in addition to `run.sh` and `run.ps1` (rejected: replacing them;
  drop). crabx checks token-max's GitHub attestation and works the same on
  every OS, but it needs Node for `npx`, and the run scripts need nothing.
- ✅ Tabs in this order: `node`, `crabx`, `macOS / Linux`, `Windows`. The
  `node` tab runs `npx @kixel/crabx token-max doctor` and installs with
  `npx @kixel/crabx install token-max`; the `crabx` tab runs
  `crabx token-max doctor` and installs with `crabx install token-max`. Each
  install line is followed by `token-max doctor --ssh devbox`, as today.
- ✅ The page always opens on `node` (rejected: opening on the run script for
  the detected OS). That line is the same on every OS, so `site.js` no longer
  detects Windows.
- ✅ m1, gating [the promotion](/quest/m2/promote-site.md), so the page the
  maintainer announces shows these tabs (rejected: m2 without gating
  anything). The 0.1.0 release doesn't wait for it.

Decided 2026-11-25 (maintainer, /quest-plan): ✅ token-max moves to crabx's pipeline
([the switch](/quest/m1/crabx-pipeline.md)), and its first release crabx runs is 0.1.1. The
crabx release this quest waited on (musl fallback and `crabx install`) shipped as crabx 0.2.0,
which also refuses an author attestation signed from a branch, as 0.0.1's and cargo-dist's 0.1.0
are. The run-script tabs stay, but their "Or install it" lines become max.kixel.me's
`/install.sh` and `/install.ps1`, which now install through crabx's installers, so their
directory and `PATH` handling are crabx's, not cargo-dist's. Validate on 0.1.1 instead of 0.1.0.

Found while planning: on 2026-11-24, `npx @kixel/crabx token-max --version`
on x86_64 glibc Linux failed with `no usable prebuilt release for
token-max@0.0.1 on x86_64-unknown-linux-gnu`. crabx 0.1.0 only tries the
host's exact target, and token-max ships static musl builds for Linux. The
crabx release this quest waits on falls back to musl.

Recommendations:

- The README's "Run this:" leads with the npx line and gives the curl and irm
  lines as the way to run it without Node. Its Install section adds
  `npx @kixel/crabx install token-max`.
- The page's "Or install it" line says, for the selected tab, where its
  installer puts token-max and whether it adds that directory to `PATH`, so
  `token-max doctor --ssh devbox` doesn't surprise anyone with
  `command not found`. Read each installer for its directory: `crabx install`
  uses crabx's install directory and handles `PATH` as crabx's installers
  do, and the run-script tabs' installers are cargo-dist's.
- Linux hosts without glibc, such as Alpine, can't run crabx's npm launcher,
  so the README and `docs/reference.md` name the run script as their way in;
  it picks the static musl build.
- The page is deployed by hand (`docs/releasing.md`), so this quest's PR
  deploys it once the crabx release is out, and checks that
  https://max.kixel.me opens on the `node` tab.
- Retake `og.png` and update its `og:image:alt` so a shared link shows the
  npx line.
- crabx runs an unpinned crate's newest version only once it is 24 hours
  old (kixel-ai/crabx's release-age delay), so `npx @kixel/crabx token-max`
  reaches 0.1.0 a day after it is published. Validate after that, or pin it.
- Validate on the published crabx: run `npx @kixel/crabx token-max doctor`
  and answer a question on Linux, macOS and Windows; run
  `npx @kixel/crabx install token-max`, then `token-max --version` from a new
  shell; and run `crabx token-max@0.1.0 --version` to check that pinned runs
  work. Check the upgrade guard against a real crabx cache: with
  `TOKEN_MAX_RELEASES` pointed at a local server whose `/latest` redirects to
  a higher tag, `crabx token-max@0.1.0 doctor` names crabx instead of
  offering the upgrade, and the cache entry's SHA-256 is unchanged. Check too
  that the entry still holds `record.json` beside `bin/` and that its `bins`
  list names `token-max` (`token-max.exe` on Windows), the layout the guard
  reads, so a crabx change to it shows up here; run the
  installed `token-max doctor` the same way. If `crabx install` links into
  crabx's cache, that installed binary gets the same crabx line, which suits a
  `crabx token-max` run but not a plain `token-max` one, so word the line for
  both.

## Required

- [Release 0.1.1](/quest/m1/release-0.1.1.md) - the first release crabx runs, since crabx 0.2.0 refuses the branch-signed 0.0.1 and 0.1.0
