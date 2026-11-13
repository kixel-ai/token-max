# [M] Serve the install and run scripts from max.kixel.ai

## Goal

`curl -LsSf https://max.kixel.ai/install.sh | sh` installs the latest
`token-max` onto PATH, and `curl -LsSf https://max.kixel.ai/run.sh | sh`
runs `token-max doctor` once without installing anything. On Windows,
`irm https://max.kixel.ai/install.ps1 | iex` and
`irm https://max.kixel.ai/run.ps1 | iex` do the same. A stand-alone
Cloudflare Worker in this repository serves all four. The README leads with
these lines, and the SSH hint for a missing or mismatched destination
names the install line.

## Plan

Decided 2026-11-11 (maintainer, /quest-plan for the follow-ups of
[kixelated/token-max#62](https://github.com/kixelated/token-max/pull/62)).
Decision prompts, chosen options marked ✅:

- URL: ✅ `https://max.kixel.ai/install.sh` and `run.sh` (the maintainer:
  "A subdomain means we can easily make a stand-alone worker"; rejected:
  the long GitHub Releases URL in the README; a path on kixel.ai).
- `run.sh`: ✅ run once with no install: download the latest binary to the
  cache directory, verify its checksum, and run `token-max doctor` attached
  to the terminal, like `npx` (rejected: install, then run).
- Windows run-once: ✅ `run.ps1` beside `run.sh` (decided 2026-11-12,
  maintainer, /quest-plan from
  [moq-dev/quest#669](https://github.com/moq-dev/quest/pull/669);
  rejected: cargo-dist's npm installer for `npx @kixel/token-max`, which
  adds a release channel and a GitHub-hosted publish job; both).
- Where the worker lives: ✅ a small worker directory in this repository,
  deployed by hand with wrangler as kixel.ai is (rejected: its own
  repository under kixel-ai).

Agent's choices, for the implementer to revisit:

- `install.sh` is cargo-dist's shell installer from the latest release,
  redirected or proxied, not a second script to maintain. Serve
  `install.ps1` the same way for Windows. Each release carries
  `token-max-installer.sh`, `token-max-installer.ps1`, and per target
  `token-max-<target>.tar.xz` (a `token-max-<target>/` directory) or, on
  Windows, a flat `token-max-<target>.zip`, each with a `.sha256`, plus
  `sha256.sum`; [releasing](/docs/releasing.md) lists the targets.
- `run.ps1` is ours too, mirroring `run.sh`: download the latest Windows
  binary to the cache directory, verify its checksum, and run `doctor` in
  the console. Test it on Windows CI against the same fixture release.
- `run.sh` is ours. A script piped into `sh` has the pipe as stdin, so it
  must give `doctor` the terminal, such as from `/dev/tty`, or `doctor`
  sees no terminal and never prompts. It reuses a cached binary that is
  still the latest.
- Keep the worker free of dependencies and build steps, so the Rust
  repository gains no JavaScript toolchain beyond wrangler.
- Test `run.sh` against a local server and a fixture release, since no
  release exists before 0.1.0.
- Deploying needs a wrangler login for the kixel.ai zone on the host. If
  `wrangler whoami` fails, hand back with a `Maintainer:` wait naming the
  login, rather than asking for a token in chat.
