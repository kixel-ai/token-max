# [S] Try and promote max.kixel.me

## Goal

The maintainer has tried [max.kixel.me](https://max.kixel.me) on a desktop
and a phone and shared it where they choose to announce token-max.

## Plan

Kept 2026-11-17 (maintainer, /quest-run, at the review of
[kixel-ai/token-max#140](https://github.com/kixel-ai/token-max/pull/140)): ✅
keep this follow-up (rejected: drop it).

Decided 2026-11-22 (maintainer, /quest-run): moved from m1 so the maintainer's
promote step no longer holds back the m2 audits. ✅ move to m2 (rejected: keep
it gating m2)

The page is `site/public/index.html`; `docs/releasing.md` says how to preview
and deploy it. Its link preview is `og.png`, so a shared link shows the logo,
the headline and the run line. The redesign from
[kixel-ai/token-max#191](https://github.com/kixel-ai/token-max/pull/191), with
`og.png` retaken, was deployed on 2026-11-22 by
[kixel-ai/token-max#194](https://github.com/kixel-ai/token-max/pull/194).

Tried 2026-11-22 (maintainer, /quest-run): the page "looks nice". The
requested tweaks, including the move to max.kixel.me, landed with the
personal-site quest's PR; promote the page once max.kixel.ai redirects to it.

Decided 2026-11-24 (maintainer, /quest-run): promote only after 0.1.0. The
redirect is live, but the deployed page still demos 0.0.1's flags, and main's
page demos flags only an unreleased binary has, so the release quest deploys
the page that matches what the installers serve.

## Waiting

- Maintainer: once 0.1.0 is released and the page deployed, open https://max.kixel.me, try it on a desktop and a phone, and promote it; report anything to change as a new quest. Only the maintainer can judge the page and post as themselves.

## Required

- [Release 0.1.0](/quest/m1/release-0.1.md) - publishes the binary the page's demo shows and deploys the page
