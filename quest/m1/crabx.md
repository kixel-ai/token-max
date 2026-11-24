# [M] Offer crabx as the first way to run and install token-max

## Goal

max.kixel.me opens on a `node` tab showing `npx @kixel/crabx token-max doctor`,
followed by a `crabx` tab and then today's `macOS / Linux` and `Windows` run
scripts, which stay. The README and `docs/reference.md` list the same lines.
Run through crabx, `doctor` is interactive on Linux, macOS and Windows, and
token-max's upgrade offer never touches a binary crabx manages. Non-goals:
publishing token-max to npm itself, and holding back 0.1.0 for this.

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

Found while planning: on 2026-11-24, `npx @kixel/crabx token-max --version`
on x86_64 glibc Linux failed with `no usable prebuilt release for
token-max@0.0.1 on x86_64-unknown-linux-gnu`. crabx 0.1.0 only tries the
host's exact target, and token-max ships static musl builds for Linux. The
crabx release this quest waits on falls back to musl.

Recommendations:

- `upgrade.rs`: a binary under crabx's cache (`crabx/v1/` in the OS user
  cache directory) is managed by crabx. Its upgrade offer replaces the file in
  place, which would corrupt crabx's verified cache entry. An unpinned crabx
  run has already started a background fetch of the newest release, so name
  crabx as the manager and say the next run picks the new version up.
- The README's "Run this:" leads with the npx line and gives the curl and irm
  lines as the way to run it without Node. Its Install section adds
  `npx @kixel/crabx install token-max`.
- Retake `og.png` and update its `og:image:alt` so a shared link shows the
  npx line.
- crabx runs an unpinned crate's newest version only once it is 24 hours
  old (kixel-ai/crabx's release-age delay), so `npx @kixel/crabx token-max`
  reaches 0.1.0 a day after it is published. Validate after that, or pin it.
- Validate on the published crabx: run `npx @kixel/crabx token-max doctor`
  and answer a question on Linux, macOS and Windows; run
  `npx @kixel/crabx install token-max`, then `token-max --version` from a new
  shell; and run `crabx token-max@0.1.0 --version` to check that pinned runs
  work.

## Waiting

- kixel-ai/crabx: a crabx release that falls back to musl Linux builds and
  has `crabx install`, planned in that repository's quest tree. Check with
  `npx @kixel/crabx@latest token-max --version` on glibc Linux and
  `npx @kixel/crabx@latest install --help`.

## Required

- [Release 0.1.0](/quest/m1/release-0.1.md) - the page this edits is the one 0.1.0 deploys, and crabx runs the newest crates.io version
