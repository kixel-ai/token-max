# Release preparation

token-max ships as the `token-max` crate on crates.io; users run it with
`cargo install --locked token-max`. Nothing is published by a PR, tag, or
merge. m0 owns the 0.1.0 version change, full end-to-end validation,
publication after every child merges, and verification of the published
first-use commands. `Cargo.toml` is the only version source, `--version`
reports it, and the development version stays 0.0.0, which the release
preflight refuses.

The read-only `Check` workflow runs `just check` (formatting, clippy with
warnings as errors, the synthetic tests and `quest check`) on Linux, and the
tests on macOS and Windows. The required `Check` job aggregates them; failed,
cancelled, or skipped dependencies fail it, so a skipped draft run cannot
satisfy the gate. PR jobs have no publication credentials or OIDC permission.

## Release setup

crates.io creates a trusted publisher only for a crate that already exists,
so the first release publishes with a token:

1. Create the GitHub environment `crates-io`, restricted to **only the branch
   `main`**. Publication credentials live only in this environment, never in
   repository secrets, PR jobs, or checkout files.
2. For the first release, create a short-lived crates.io API token with the
   `publish-new` scope, store it as the environment secret
   `CARGO_BOOTSTRAP_TOKEN`, and dispatch with `bootstrap`. Revoke the token
   and delete the secret afterwards.
3. Then add a trusted publisher on the crate's crates.io settings for owner
   `kixel-ai`, repository `token-max`, workflow `release.yml` and
   environment `crates-io`. Later releases use the default `trusted` mode,
   where `rust-lang/crates-io-auth-action` exchanges the job's OIDC token
   for a temporary publish token and revokes it when the job ends.

An agent can drive Chrome through crates.io owner setup when requested; login
and crate ownership still belong to the owner. Do not send credentials in chat.

## Publish

After all m0 children merge, m0 sets the version in `Cargo.toml` to 0.1.0
through the protected, reviewed squash path, validates the complete tool, and
updates the README's pre-release notice in that change.

Dispatch **Release** on protected `main` with that exact version. Preflight
refuses 0.0.0 and a mismatched version, then the complete Check workflow
runs. Only after it passes does `cargo publish --locked` run from the
restricted environment. Inspect the package first with
`cargo publish --locked --dry-run` and `cargo package --list`.

After publication, run the README's install command from a clean
`CARGO_HOME`, and verify help, version and a synthetic priced analysis. Then
clear m0's publication wait and Quest's release handoff. A failed publish is a
release failure, never permission to publish an incomplete placeholder.

Sources checked 2026-11-08: [crates.io trusted publishing RFC 3691](https://rust-lang.github.io/rfcs/3691-trusted-publishing-cratesio.html),
[crates-io-auth-action](https://github.com/rust-lang/crates-io-auth-action), and
[GitHub environment protection](https://docs.github.com/en/actions/how-tos/deploy/configure-and-manage-deployments/manage-environments).
