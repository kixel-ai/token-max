# [M] Draw the token-max logo

## Goal

The repository holds an SVG token-max logo the maintainer picked from
several concepts, in the forms the README, site and favicon use: the mark
alone, the mark with the wordmark, light and dark backgrounds, and a favicon
that reads at 16 px.

## Plan

Decided 2026-11-15 (maintainer, /quest-plan): ✅ SVG drawn by hand in code
(rejected: concepts from an image model redrawn as SVG; a logo the
maintainer supplies). ✅ several variants.

Recommended: open the PR with 4-6 distinct concepts rendered side by side
on light and dark, so the maintainer picks one at the merge pick; then
finish the picked one in every form above and delete the rest. Keep each
file small and free of embedded raster images or fonts (convert the wordmark
text to paths), so it renders the same on GitHub and the site.

Drawn 2026-11-15 ([#128](https://github.com/kixel-ai/token-max/pull/128)):
six concepts in `assets/logo/concepts/`, each a lockup on light and dark
plus a favicon tile, compared in `concepts.svg` and drawn by `gen.py`, with
a monoline wordmark stroked as paths (no font). At a true 16 px, Stairs,
Ceiling and Slice read; Pulse passes; Gauge and Receipt blur.

Added 2026-11-15 (maintainer, on #128: "try a few more. Maybe lean more
into agentic coding and slop and anything that might be funny"): four more
concepts, Piggy, Drip, Cinch and Slots, beside the six. At 16 px Piggy,
Drip and Slots read; Cinch reads as a hamburger. Remaining:
finish the picked concept as the mark alone, the lockup, light and dark,
and a 16 px favicon that also replaces the root `favicon.svg` (today's
Stairs icon for T3 Code), then delete the concepts and `gen.py`.

## Waiting

- Maintainer: pick a concept from `assets/logo/concepts/concepts.svg`
  (A Stairs, B Gauge, C Pulse, D Ceiling, E Slice, F Receipt, G Piggy,
  H Drip, I Cinch, J Slots). Recommended: G Piggy, a piggy bank (savings)
  that is also a pig (slop), the clearest silhouette at 16 px; H Drip if
  the mark should say "agentic coding" outright; E Slice if it should be
  serious.
