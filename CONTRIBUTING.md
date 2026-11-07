# Landing

PRs into `main` land through GitHub's squash merge queue, one at a time.
`Check` gives feedback on ready PRs. The required `Queue` job is skipped on
PRs for admission, then runs `nix develop --command just check` against the
merge group. GitHub squashes the tree that passed that check.

Run local checks and the independent review before requesting landing.
With maintainer approval, use `quest merge <pr> --squash --land <full-head-sha>`
to enqueue the reviewed head or arm auto-merge while checks are pending.
A push needs a new review and landing request. Never bypass the queue.

Workflows use read-only credentials and GitHub-hosted runners. Publication
credentials belong only to the separate protected release path.
