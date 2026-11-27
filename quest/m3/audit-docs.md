# [M] Audit the docs

## Goal

Run the `docs` audit: `quest audit docs` prints what it checks, what each finding becomes, and how to plan its next run.

## Plan

- Baseline: the m2 run read `104ec46`. Start from `git diff 104ec46 -- README.md docs site src/cli.rs src/tui.rs`, then read the pages no change touched.
- Check first: `docs/reference.md`'s `Applying changes` and `The checklist` against `src/cli.rs` and `src/tui.rs`, and `docs/semantics.md` against `src/`. Those had the most stale claims in the m2 run, after a feature PR reshaped the walk.
- The README and `docs/results.md` are one dated snapshot; max.kixel.me is a later run by `assets/demo/site.py` and need not match them. Check the README's figures against `docs/results.md`, and the site's only against themselves.
- `assets/demo/walk.tape` breaks whenever `doctor` adds a question. Run it and look at the frames, since nothing in CI runs it.
- Scope excludes `quest/` and the vendored `.agents/` and `.claude/` skills, which quest ships.
- `#![warn(missing_docs)]` with clippy's `-D warnings` already gates undocumented exports.
- GitHub blob links answer a GET sweep with 429; confirm those through the contents API.

## Audit

- docs

## Required

- [m2](/quest/m2/README.md) - runs once per milestone

## Scheduled

- 2026-12-04 - so a fast milestone does not rerun the audit at once
