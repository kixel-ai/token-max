# Landing

PRs into `main` land by squash through GitHub's merge queue, which tests
each entry against the current `main` with the required `Check` job
(`just check` in the Nix dev shell, plus the Windows tests, or only
`quest check` when nothing outside `quest/` and the agent docs changed). The
macOS tests run after pushes to `main` and file an issue when they fail; a
newer push replaces a pending run, so an intermediate merge may go untested. A
branch only behind `main` needs no merge; merge `main` only when the PR
conflicts or the queue refuses its head.

CI runs no checks on pull requests, so run `just check` before pushing, and
the independent review before requesting landing.
Then use `quest merge <pr> --squash --land <full-head-sha>`, which enqueues
the reviewed head, or arms auto-merge bound to it while checks are pending.
Never bypass the required checks or use `--direct`.

Workflows use read-only credentials and GitHub-hosted runners; the one
exception is the `macOS` workflow's report job, which runs no repository code
after a push to `main` and may write issues. Publication credentials belong
only to the separate protected release path.
