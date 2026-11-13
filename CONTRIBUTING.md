# Landing

PRs into `main` land by squash through GitHub's merge queue, which tests
each entry against the current `main` with the required `Check` job
(`nix develop --command just check`, plus the macOS and Windows tests). A
branch only behind `main` needs no merge; merge `main` only when the PR
conflicts or the queue refuses its head.

Run local checks and the independent review before requesting landing.
Then use `quest merge <pr> --squash --land <full-head-sha>`, which enqueues
the reviewed head, or arms auto-merge bound to it while checks are pending.
Never bypass the required checks or use `--direct`.

Workflows use read-only credentials and GitHub-hosted runners. Publication
credentials belong only to the separate protected release path.
