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

Follow `docs/releasing.md`'s Publish section: the version PR after an end to
end `doctor` run on a real host, a rerun of `assets/demo/site.py` on `nixos`
so the page's run shows 0.1.0 (read it for private names before committing),
a `walk.gif` rerun if the cards changed doctor's questions, dispatching
**Release** on `main`, deploying `site/`, and the post-publication installer
checks.

## Required

- [Codex unmeasured lines](/quest/m1/codex-unmeasured-lines.md) - shows the Codex compaction settings a host set when no request is selected
- [Advisor card](/quest/m1/vibes/advisor.md) - a vibes card shipped in 0.1.0
- [Language servers card](/quest/m1/vibes/language-servers.md) - a vibes card shipped in 0.1.0
- [Memory card](/quest/m1/vibes/memory.md) - a vibes card shipped in 0.1.0
- [Retention card](/quest/m1/vibes/retention.md) - a vibes card shipped in 0.1.0
- [Window card](/quest/m1/vibes/window.md) - a vibes card shipped in 0.1.0
