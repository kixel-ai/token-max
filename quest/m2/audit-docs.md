# [M] Audit the docs

## Goal

Run the `docs` audit: `quest audit docs` prints what it checks, what each finding becomes, and how to plan its next run.

## Plan

- Baseline: the m1 run read `309b526`. Start from `git diff 309b526 -- README.md docs site src/cli.rs`, then read the pages no change touched.
- Check first: `docs/semantics.md` against `src/`, and the README and site figures against `docs/results.md`. Those had the most stale claims in the m1 run.
- `assets/demo/walk.tape` breaks whenever `doctor` adds a question. Run it and look at the frames, since nothing in CI runs it.
- Scope excludes `quest/` and the vendored `.agents/` and `.claude/` skills, which quest ships.
- `help.openai.com` answers 403 to scripted requests; check it in a browser.

## Audit

- docs

## Scheduled

- 2026-11-27 - so a fast milestone does not rerun the audit at once
