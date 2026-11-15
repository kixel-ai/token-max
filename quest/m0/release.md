# [M] Publish token-max 0.0.1

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

- Validate end to end: rerun `token-max doctor` on the maintainer's host and
  confirm the report and the walk read well.
- Change the version to 0.0.1 and publish the crate through the protected
  release path.
- Verify the README's install commands against the published crate and
  release, including both curl lines.
- Clear Quest's release handoff.

No crate has been published; `token-max` was unclaimed on crates.io on
2026-11-08, which does not prove it can be published.

## Required

- [Launch token-max](/quest/m0/launch/README.md) - the logo, site, README and CLI polish the maintainer wants before release

## Waiting

- Maintainer: publish the first crates.io release with a short-lived token, since crates.io creates trusted publishers only for an existing crate, then add the trusted publisher for later releases. Follow [release preparation](../../docs/releasing.md); the `crates-io` GitHub environment restricted to `main` exists (created 2026-11-10 by /quest-unblock). The maintainer offered browser control, so an agent drives Chrome through the token and trusted-publisher steps at release time. An agent can automate the protected workflow and drive Chrome through crates.io setup, but cannot sign in as the owner. No release was attempted.
