# [XS] Add the crates.io trusted publisher

## Goal

The `token-max` crate on crates.io has a trusted publisher, so the next
release dispatches **Release** in the default `trusted` mode without a
token.

## Plan

0.0.1 was published on 2026-11-18 by
[run 44407550222](https://github.com/kixel-ai/token-max/actions/runs/44407550222)
in `bootstrap` mode. With the maintainer away from their PC, it used their
existing `cargo login` token from desktop instead of a fresh short-lived
`publish-new` token (maintainer, /quest-run). The token went straight from
the file into the `crates-io` environment secret, which was deleted after
the run. The token itself stays valid, since it is the maintainer's everyday
cargo login.

What remains: on the crate's crates.io settings, add a trusted publisher with
owner `kixel-ai`, repository `token-max`, workflow `release.yml` and
environment `crates-io`, as [release preparation](../../docs/releasing.md)
describes.

## Waiting

- Maintainer: sign in to crates.io (through GitHub) in the agent Chrome on desktop, so an agent can add the trusted publisher. The agent Chrome had no GitHub session on 2026-11-18, and an agent can't sign in as the owner. To check it's done, the crate's settings on crates.io list the publisher.
