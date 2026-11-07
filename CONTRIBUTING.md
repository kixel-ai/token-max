# Landing

PRs into `main` land by squash through protected auto-merge. The required
`Check` job runs `nix develop --command just check`, and the branch must be
up to date with `main` before merging. GitHub's native merge queue is not
available for personal repositories.

Run local checks and the independent review before requesting landing.
After local checks and independent review pass, use `quest merge <pr> --squash --land <full-head-sha>`
to land the reviewed head or arm auto-merge while checks are pending.
When main advances, merge it into the PR branch and rerun the required
checks. Never bypass the required checks or use `--direct`.

Workflows use read-only credentials and GitHub-hosted runners. Publication
credentials belong only to the separate protected release path.
