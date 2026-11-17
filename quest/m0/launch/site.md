# [XS] Deploy the max.kixel.ai landing page

## Goal

[max.kixel.ai](https://max.kixel.ai) serves the landing page in
`site/public`: the logo, the pitch, a recording of the walk, the
maintainer's real results, the install lines, how it measures, and what
stays on the machine, while `install.sh`, `run.sh` and their PowerShell
twins keep working.

## Plan

Decided 2026-11-15 (maintainer, /quest-plan): ✅ one hand-written HTML and
CSS page in `site/public`, with no build step, and a terminal recording of
the walk on synthetic transcripts (rejected: a static site generator, a
build step and dependencies for one page; a recording of the maintainer's
real run, which would need private names scrubbed from video).

The page landed in this quest's first PR, checked locally with
`wrangler dev`; what remains is the deploy, which `docs/releasing.md`
describes for `site/`. Until it runs, `/` still redirects to GitHub.

## Waiting

- Maintainer: run `nix shell nixpkgs#wrangler -c wrangler deploy` from `site/` with a wrangler login for the kixel.ai account, then check that `curl -sI https://max.kixel.ai/` answers 200 `text/html` with the `content-security-policy` header and `curl -sI https://max.kixel.ai/install.sh` still answers 302 to the latest release's installer. An agent cannot deploy without that login; it can drive the maintainer's signed-in Chrome through `wrangler login` on request, and could deploy from CI with a Cloudflare API token in a `main`-only environment, which was not set up because one hand deploy per page change has not earned a secret.
