# [XS] Deploy the retaken results to max.kixel.ai

## Goal

[max.kixel.ai](https://max.kixel.ai) shows the four-host, 30-day results that
[kixel-ai/token-max#155](https://github.com/kixel-ai/token-max/pull/155) put in
`site/public/index.html`, instead of the desktop-only snapshot.

## Plan

Deploy `site/` from main as `docs/releasing.md`
describes (`nix shell nixpkgs#wrangler -c wrangler deploy` from `site/`;
wrangler is logged in on fedora), then load the page and check that its
totals read ~$69,222, 11,797 sessions and ~$721/mo. `og.png` covers only the
page above the results, so it needs no retake.
