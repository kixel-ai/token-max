# [XS] Deploy the max.kixel.ai landing page

## Goal

[max.kixel.ai](https://max.kixel.ai) serves the landing page in
`site/public`: a short page with one run line, the maintainer's two cache
TTL findings, how token-max reads and replays transcripts, and the recording
of the walk, while `install.sh`, `run.sh` and their PowerShell twins keep
working.

## Plan

Decided 2026-11-15 (maintainer, /quest-plan): ✅ one hand-written HTML and
CSS page in `site/public`, with no build step, and a terminal recording of
the walk on synthetic transcripts (rejected: a static site generator, a
build step and dependencies for one page; a recording of the maintainer's
real run, which would need private names scrubbed from video).

Decided 2026-11-17 (maintainer, /quest-run, after seeing the first draft in
[kixel-ai/token-max#140](https://github.com/kixel-ai/token-max/pull/140)):
"That site is awful. It needs to market this, not a wall of text." ✅ a
dramatically shorter page: a snappy line such as "The default settings are
costing you money", a single "run this" box, and how token-max parses the
transcripts (rejected: the long page). ✅ lead with the subagent prompt cache
TTL (~15% less at 1h than the default 5m, $6,105 over desktop's 56 days on
the 95.1% of requests that could be priced) and show the main-agent TTL for
API-key and usage-credits users (~36%, $7,383), marking figures with unpriced
requests with `~` (rejected: the main-agent TTL alone; both side by side as
one headline). ✅ the README's top gets the same treatment, with detail moved
to `docs/` (rejected: the site only).

The page landed in this quest's first PR, checked locally with
`wrangler dev`; what remains is the deploy, which `docs/releasing.md`
describes for `site/`. Until it runs, `/` still redirects to GitHub.

## Waiting

- Maintainer: run `nix shell nixpkgs#wrangler -c wrangler deploy` from `site/` with a wrangler login for the kixel.ai account, then check that `curl -sI https://max.kixel.ai/` answers 200 `text/html` with the `content-security-policy` header and `curl -sI https://max.kixel.ai/install.sh` still answers 302 to the latest release's installer. An agent cannot deploy without that login; it can drive the maintainer's signed-in Chrome through `wrangler login` on request, and could deploy from CI with a Cloudflare API token in a `main`-only environment, which was not set up because one hand deploy per page change has not earned a secret.
