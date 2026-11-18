# [S] Retake the results snapshot once T3 shares its controls

## Goal

The README's top, max.kixel.ai and `docs/results.md` show the four hosts'
last 30 days with the plan profiles' cache TTL findings counted in the
potential savings, rather than left out as "not enough evidence".

## Plan

Decided 2026-11-17 (maintainer, /quest-run on
[kixel-ai/token-max#155](https://github.com/kixel-ai/token-max/pull/155)):
✅ take the `--defaults` snapshot now and again once the shared-controls fix
lands (rejected: holding #155 for the fix).

Recommended: rerun #155's procedure (the same four hosts, a fresh 30-day
window, `--defaults`, billing answered as recorded in #155, every change
declined), and redeploy the site once it merges.

## Required

- [Read shared settings for controls T3 leaves alone](/quest/m0/launch/t3-shared-controls.md) - the TTL lines that count toward the savings
