# [M] Build the max.kixel.ai landing page

## Goal

[max.kixel.ai](https://max.kixel.ai) serves a landing page for token-max:
the logo, the pitch, a recording of the walk, the maintainer's real results,
the install lines, how it measures, and what stays on the machine, while
`install.sh`, `run.sh` and their PowerShell twins keep working.

## Plan

Decided 2026-11-15 (maintainer, /quest-plan): ✅ one hand-written HTML and
CSS page in `site/public`, with no build step, and a terminal recording of
the walk on synthetic transcripts (rejected: a static site generator, a
build step and dependencies for one page; a recording of the maintainer's
real run, which would need private names scrubbed from video).

Recommended: reuse the README's results snapshot and recording, and deploy
it as `docs/releasing.md` describes for `site/`.

## Required

- [Revamp the README](/quest/m0/launch/readme.md) - the results snapshot, recording and pitch it reuses
