# Landing

PRs into `main` land by squash through GitHub's merge queue, which runs the
`Check` workflow on each entry against the current `main` (`just check` in the
Nix dev shell, plus the Windows tests, or only `quest check` when nothing
outside `quest/` and the agent docs changed). `Check` is no longer a required
check, so the queue lands an entry without waiting on it until Kixel Quest's
merge queue takes token-max; a failed `Check` there means fixing `main`. The
macOS tests run after pushes to `main` and file an issue when they fail; a
newer push replaces a pending run, so an intermediate merge may go untested. A
branch only behind `main` needs no merge; merge `main` only when the PR
conflicts or the queue refuses its head.

CI runs no checks on pull requests, so run `just check` before pushing, and
the independent review before requesting landing.
Then use `quest merge <pr> --squash --land <full-head-sha>`, which enqueues
the reviewed head.
Never bypass the queue or use `--direct`.

Workflows use read-only credentials and GitHub-hosted runners; the one
exception is the `macOS` workflow's report job, which runs no repository code
after a push to `main` and may write issues. Publication credentials belong
only to the separate protected release path.
