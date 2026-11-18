# [S] Publish token-max 0.0.1

## Goal

`token-max` 0.0.1 is published on crates.io and as a GitHub release through
the [protected release path](../../docs/releasing.md), and the README's
install commands, `cargo install` and both curl lines, work against it.
Quest's release handoff is cleared: moq-dev/quest's `SETUP.md` points to it.

## Plan

This is m0's own remaining work, split out of [m0](/quest/m0/README.md) once
its last report quest merged, since a release that unblocks work is its own
quest. m0's Plan holds the decisions behind it, such as 0.0.1 rather than
0.1.0 and publishing only after a rerun on the maintainer's host reads well
(2026-11-14).

[kixel-ai/token-max#169](https://github.com/kixel-ai/token-max/pull/169)
validated `token-max doctor` on fedora, set the version to 0.0.1, dropped the
README's and the site's pre-release notices, and dry-ran the Release
workflow from its branch. What remains, in order, once it merges:

- Bootstrap crates.io: create a short-lived `publish-new` token, store it as
  the `crates-io` environment secret `CARGO_BOOTSTRAP_TOKEN`, and dispatch
  **Release** on `main` with version `0.0.1` and `bootstrap`.
- Revoke the token, delete the secret, and add the crate's trusted publisher
  (owner `kixel-ai`, repository `token-max`, workflow `release.yml`,
  environment `crates-io`), so later releases use `trusted`.
- Redeploy max.kixel.ai from `site/`, which drops its pre-release notice.
- Verify both installers and both run scripts from a clean home without a
  Rust toolchain, and `cargo binstall token-max` and `cargo install --locked
  token-max` from a clean `CARGO_HOME`: help, version and a synthetic priced
  analysis for each.
- Clear Quest's release handoff in moq-dev/quest's `SETUP.md`.

## Waiting

- Maintainer: confirm publishing 0.0.1, which cannot be undone, then sign in to crates.io and Cloudflare in the agent Chrome so an agent can drive the token, secret, trusted-publisher and deploy steps above. An agent can dispatch the workflow and drive Chrome, but cannot sign in as the owner or decide to publish; [release preparation](../../docs/releasing.md) has the details. The `crates-io` and `github-releases` environments, each restricted to `main`, exist.
