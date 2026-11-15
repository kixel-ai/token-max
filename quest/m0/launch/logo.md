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
