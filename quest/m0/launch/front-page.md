# [M] Revamp the README

## Goal

The README sells token-max to someone deciding whether to run it: the logo,
a short pitch, a recording of the walk, the maintainer's real results
against the defaults, and the install lines, with per-check behavior left to
`docs/`.

## Plan

Decided 2026-11-15 (maintainer, /quest-plan): ✅ show the maintainer's real
results, as [the epic](/quest/m0/launch/README.md) describes, which this
quest takes as a dated snapshot that [the site](/quest/m0/launch/site.md)
reuses.

Recommended: replace the long text sample report with a short VHS recording
of the walk on synthetic transcripts, checked in with its tape so it can be
regenerated, and lead with the results' headline figure. Keep the claim
kinds (Measured, Official, Opinion) and privacy (nothing leaves the machine)
in a few lines each.

The logo at the top is `assets/logo/logo-on-light.svg` and
`assets/logo/logo-on-dark.svg`, switched with a `<picture>` element and
`prefers-color-scheme`; the mark alone is the root `favicon.svg`.
