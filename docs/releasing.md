# Release preparation

Packaging is ready for a complete first release; nothing is published by a PR,
tag, or merge. m0 owns the 0.1.0 version change, full end-to-end validation,
publication after every child merges, and verification of the published first-use
commands. The development version stays 0.0.0 with `private: true` until that work is complete;
this also blocks accidental publication outside the workflow.
Synthetic artifact tests do not validate costs from real host histories.

`package.json` is the only version source. The build inlines its version in
`dist/cli.js`; `--version` reports it. Bun bundles locked dependencies for Node,
and ships a self-contained `dist/scan-helper.js` for SSH. The CLI verifies the
helper's exact bytes and shared build fingerprint. Dependency notices cover the
union of both bundles' actual module graphs.
The npm allowlist contains the JavaScript CLI and helper, notices, README, license, billing
semantics, and package metadata. No source histories, tests, source maps, install
hooks, or consumer dependencies are shipped.

Run `nix develop --command quest gate just check` to create
`.tmp/package/token-max.tgz` and its SHA-512 integrity in `artifact.json`.
The tests install that tarball with scripts disabled, then execute a standalone
copy and the real scoped npx/bunx commands from a clean consumer directory with
spaces. A local fixture registry serves only that tarball, including a `latest`
tag, so testing does not require a placeholder publication. Both caches start
empty. Synthetic prices and transcripts exercise help/version, text and JSON,
priced recommendations, unchanged settings, exits 0/1/2, malformed input,
privacy, and read-only histories/settings. The Bun-only PATH contains no Node.
The actual packed helper also runs from stdin under Node and Bun without sibling
modules or network access. On POSIX, every launcher audits a synthetic SSH process
using the checked helper, including Node-free bunx. Native SSH transport tests
exercise both remote runtimes, quoting, corruption, failures and reaping.

The read-only `Check` workflow builds once, uploads that tarball, and tests the
same bytes across Linux/macOS/Windows with Node 22/24/26 LTS and Bun 1.4.4.
The required `Check` job aggregates source validation and the entire matrix;
failed, cancelled, or skipped dependencies fail it. Draft validation jobs stay
skipped, but their aggregate fails so an old skipped result cannot satisfy the
required gate while the ready run is pending. PR jobs have no publication
credentials or OIDC permission. Local Linux results do not establish macOS or
Windows success; the matrix must pass before landing and before publishing.

## Release setup

Read-only npm checks on 2026-11-07 found `npm whoami` returning E401.
Authentication and scope authority remain release prerequisites. Agents cannot
sign in as the owner or grant themselves scope rights. The branch-only GitHub
`npm` environment is configured and verified through the authenticated API;
that reversible repository setup needs no owner-only step. No npm token or
expiring trusted publisher was configured during preparation.

1. Restore npm login with `npm login --registry=https://registry.npmjs.org` and
   check `npm whoami`. Confirm that identity owns the personal `@kixelated`
   scope, or has the organization's package-creation rights if it is an org.
   Registry 404s do not prove ownership. For an existing package, also check
   `npm access list collaborators @kixelated/token-max` for write authority.
2. Keep the GitHub environment `npm` restricted to **only the branch `main`**
   through a selected branch policy, without tags or wildcard branches. `main`
   must remain protected; the release preflight checks its protected-ref status.
   Agents can create and verify this environment with the authenticated GitHub
   API. There is no required reviewer or per-release environment approval.
   Keep publication credentials exclusively in this environment, never repository
   secrets, PR jobs, or checkout files. Preflight fails closed if the environment
   is absent or admits other refs.
3. Immediately before the first complete release, use authenticated bootstrap
   if npm still requires an existing package for trusted-publisher setup. The
   current [official OIDC setup](https://docs.npmjs.com/trusted-publishers)
   starts from an existing package's settings; it does not document creating a
   new package through OIDC. This workflow therefore does not depend on that
   capability. Create a narrowly scoped, short-lived granular publishing token
   with read/write publish permission for `@kixelated` and publishing 2FA bypass
   for this noninteractive job, and store it only as the
   environment secret `NPM_BOOTSTRAP_TOKEN`. Scope it for creating this package;
   a token limited to an unrelated existing package cannot bootstrap it.
   Do this near the authorized release, not during packaging preparation.
4. Prefer trusted publishing for subsequent releases. Configure npm for owner
   `kixelated`, repository `token-max`, workflow `release.yml`, environment
   `npm`, and explicitly allow `npm publish`. npm requires npm >=11.5.1 and
   Node >=22.14.0 on a supported hosted runner; the release uses Node 24's npm.
   A new trusted-publisher configuration must publish successfully within
   **2 days**, or it expires and must be recreated. Configure it near the next
   actual version release; an already published version cannot be republished
   to validate it. After bootstrap, remove/revoke the bootstrap token. Once OIDC
   works, restrict traditional token publication in npm settings.

An agent can drive Chrome through npm owner setup when requested; login and
scope authority still belong to the owner. Do not send credentials in chat.

## Publish the checked artifact

After all m0 children merge, m0 updates the version in `package.json` to 0.1.0 and removes its private guard through the
protected, reviewed squash path. Validate the complete tool and update README's
pre-release notice in that change. Recheck the current official authentication
guidance and scope authority before releasing.

Dispatch **Release** on protected `main` with the exact existing package version.
Select `bootstrap` only for authenticated first publication; otherwise use the
default `trusted` mode. Preflight refuses version 0.0.0, a private manifest, mismatched versions,
unprotected refs, remaining m0 children, and missing environment protection.
It then runs the complete Check workflow, including every package matrix cell.
Only after successful validation does publication proceed automatically from the
restricted environment. No additional manual approval is required.

The publish job downloads `npm-package` from that same run, verifies its SHA-512
and version, and publishes `package/token-max.tgz --access public --ignore-scripts`.
It neither rebuilds nor repacks after validation. `publishConfig.access` also
requires public scoped access. Only this environment job can request an npm
OIDC token or read the bootstrap secret. No publisher is available to PR code.

After m0 removes the private guard for the complete release, inspect the checked
artifact without publishing:

```sh
npm publish .tmp/package/token-max.tgz --access public --ignore-scripts --dry-run
```

After successful publication, verify registry integrity against `artifact.json`
with `npm view @kixelated/token-max@0.1.0 dist.integrity`, run the README's exact
npx and Bun-only commands against the real registry from fresh caches, and
verify help/version and a synthetic priced analysis. Confirm the `latest` tag,
remove bootstrap credentials, and clear m0's auth/publication wait and Quest's
release handoff. A first failed publish is a release failure, never permission
to publish an incomplete placeholder.

Sources checked 2026-11-07: [npm package metadata and bin](https://docs.npmjs.com/cli/v11/configuring-npm/package-json/),
[npm pack](https://docs.npmjs.com/cli/v11/commands/npm-pack/),
[npm publish tarballs and dry runs](https://docs.npmjs.com/cli/v11/commands/npm-publish/),
[public scoped packages](https://docs.npmjs.com/creating-and-publishing-scoped-public-packages/),
[npm trusted publishing and two-day expiry](https://docs.npmjs.com/trusted-publishers/),
[Bun shebangs and caching](https://bun.com/docs/pm/bunx),
[Bun bundling for Node](https://bun.com/docs/bundler),
[maintained Node releases](https://nodejs.org/en/about/previous-releases), and
[GitHub environment protection](https://docs.github.com/en/actions/how-tos/deploy/configure-and-manage-deployments/manage-environments).
