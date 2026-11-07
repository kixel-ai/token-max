# [M] One-command npm distribution

## Goal

A user can run the released CLI with `npx --yes token-max@latest` or
`bunx --bun token-max@latest`, with no global install, Rust toolchain, or
separate binary download step. A single npm package ships built JavaScript
and works on Node and Bun. Its README explains first use and version
pinning, and the first-release process publishes the tested artifact.

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
- Document both launchers. `bunx` respects a Node shebang by default, so
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
- The npm name `token-max` returned 404 on 2026-11-06; that is availability,
  not ownership. Verify publisher rights before the first release. If the
  name becomes unavailable, settle the public package name before
  documenting or publishing a substitute.
- Keep a single version and reproducible build. Publish the same artifact
  that passed the package checks; include only runtime files and public
  metadata. Keep release credentials out of PR jobs, use protected release
  execution, and prefer npm trusted publishing where supported. First
  publication/authentication may require maintainer setup; record the
  concrete requirement when known rather than inventing credentials.
- Update first-use docs and the moq-dev/quest token-max handoff to check the
  npm release instead of crates.io. That related quest currently waits for
  `cargo search token-max`; align it when the release-channel plan lands.
  The analyzer still only fetches public prices and never uploads content;
  package-manager registry downloads are separate from analysis.

Sources checked 2026-11-06: [npm exec and npx](https://docs.npmjs.com/cli/v11/commands/npm-exec/),
[npm package executables](https://docs.npmjs.com/cli/v11/configuring-npm/package-json/#bin),
[Bun package execution and shebangs](https://bun.com/docs/pm/bunx), and
[maintained Node releases](https://nodejs.org/en/about/previous-releases).

## Required

- [Replay and the cache TTL check](/quest/m0/replay.md) - TypeScript build, runtime compatibility, and an actual CLI to package
