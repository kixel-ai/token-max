# [XS] Restore Pullfrog reviews under kixel-ai

## Goal

Pullfrog reviews PRs in kixel-ai/token-max again: its GitHub App is
installed on the kixel-ai organization with access to token-max, and on a
PR push the app dispatches `.github/workflows/pullfrog.yml` on `main`, which
succeeds and posts its review.

## Plan

Found 2026-11-12 while moving the repository
([kixel-ai/token-max#83](https://github.com/kixel-ai/token-max/pull/83)):
the last Pullfrog run, on [#80](https://github.com/kixel-ai/token-max/pull/80)
at 23:19 UTC, still checked out `kixelated/token-max`, and after the
transfer `gh api orgs/kixel-ai/installations` lists no apps. An app
installed on a personal account does not follow a repository to an
organization. The `pullfrog` environment survived, limited to `main`.

Once the app is installed, confirm the Pullfrog console still grants model
access to the repository under its new owner, then check that the next PR
push produces a successful Pullfrog run on `main`.

## Waiting

- Maintainer: install the Pullfrog GitHub App on the kixel-ai organization for token-max, and confirm model access in the Pullfrog console. An agent cannot grant a third-party app access to an organization on its own; it can drive the agent Chrome through the install when asked. Check with `gh api orgs/kixel-ai/installations --jq '[.installations[].app_slug]'`, which should list `pullfrog`.
