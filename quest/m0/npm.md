# [M] One-command npm distribution

## Goal

A publishable npm package ships the built JavaScript CLI and passes
packed-artifact tests under Node and Bun, with no global install, Rust
toolchain, or separate binary download step. The release path and first-use
documentation are ready for m0 to publish the complete 0.1.0 release after
all children merge. This quest prepares packaging; it does not publish an
incomplete first release.

## Plan

Decided 2026-11-06 (maintainer, /quest-plan): make first use easy and let the
agent choose distribution; the maintainer explicitly allowed TypeScript
instead of Rust. Choose a Node-compatible TypeScript CLI and npm, with Bun
for development. The repository only has a Rust stub, so a native binary
wrapper and per-platform packages add work without a current consumer.
This supersedes crates.io as m0's release channel. No native wrapper or
second implementation is planned. This joins m0 under the session's
first-release priority.

- Replay owns the TypeScript/tooling migration and runtime-compatible core.
  This quest owns the publishable package, release path, and installation
  documentation. Publish compiled JavaScript with a `bin` entry and Node
  shebang; consumers must not compile TypeScript or run install scripts.
  Keep Node compatibility in the shipped code so npm users do not need Bun.
- Target the scoped npm package `@kixelated/token-max`, retaining the
  `token-max` command. Document `npx --yes @kixelated/token-max@latest` and
  `bunx --bun @kixelated/token-max@latest`. `bunx` respects a Node shebang
  by default, so
  `bunx --bun` is the supported Bun-only command. Explain that launchers
  download/cache the package, rather than promising an entirely offline
  first run. Also show an exact version for repeatable runs and a global
  npm install for frequent use. Arguments, cwd, stdout/stderr, cancellation,
  and exit codes must match direct invocation.
- Declare the supported Node LTS baseline and Bun versions, and test the
  built package under both. Exercise packed artifacts from a clean cache
  outside the repository on supported Linux, macOS, and Windows runners,
  including help/version, a synthetic analysis, JSON, and recommendation
  versus error exits. Verify Bun-only operation without Node on PATH.
  Do not claim an OS/runtime combination the artifact has not exercised.
- The unscoped `token-max` registry lookup returned 404 on 2026-11-06,
  but that proves neither publishing eligibility nor ownership. `tokenmax`
  is an existing package in the same topic area, and npm blocks suspected
  typosquat publishing. Under the delegated packaging choice, prefer the
  scoped package rather than attempting an early public stub release or
  relying on an unverified unscoped name. Verify authority for the scope
  before finalizing public release automation; a 404 on a scoped package
  is not ownership evidence either. The m0 epic records the known publisher
  authentication prerequisite, which does not block local package tests.
- Keep a single version and reproducible build. Publish the same artifact
  that passed the package checks; include only runtime files and public
  metadata. Keep release credentials out of PR jobs, use protected release
  execution, and prefer npm trusted publishing where supported. The m0
  epic owns first publication after every child merges; this quest only
  prepares and tests that path. Never publish a placeholder to probe a name.
- Update first-use docs and the moq-dev/quest token-max handoff to check the
  scoped npm release instead of crates.io. The related handoff is being
  aligned by [moq-dev/quest#532](https://github.com/moq-dev/quest/pull/532).
  The analyzer still only fetches public prices and never uploads content;
  package-manager registry downloads are separate from analysis.

Sources checked 2026-11-06: [npm exec and npx](https://docs.npmjs.com/cli/v11/commands/npm-exec/),
[npm package executables](https://docs.npmjs.com/cli/v11/configuring-npm/package-json/#bin),
[Bun package execution and shebangs](https://bun.com/docs/pm/bunx),
[maintained Node releases](https://nodejs.org/en/about/previous-releases),
[npm naming protections](https://docs.npmjs.com/threats-and-mitigations), and
[existing tokenmax package](https://registry.npmjs.org/tokenmax/latest).

## Required

- [Replay and the cache TTL check](/quest/m0/replay.md) - TypeScript build, runtime compatibility, and an actual CLI to package
