# [M] Run the merge gate on self-hosted runners

## Goal

Every job a pull request or the merge queue runs in `Check` uses a kixel-ai
self-hosted runner, as the maintainer's standard for public repositories
asks, or a listed exception says why it cannot, so the gate's latency no
longer depends on the org's shared GitHub-hosted runners.

## Plan

Today `Plan`, `Source` and `Check` run on `ubuntu-24.04` and the Windows tests
on `windows-2025`; the m2 `cost` audit moved the macOS tests off the gate to a
run after each push to `main`. In the week it measured, waits for the org's
shared macOS runners held gate runs for up to 84 minutes, and two merge-queue
entries timed out, which is what this standard prevents. Its other kixel-ai
repositories each run their gate on a runner group of their own, such as
`quest-queue` on `nixos-quest-ci`.

- Linux jobs move to a `token-max` runner group on a NixOS CI host, labelled
  like the others (`token-max-queue`). `Source` already runs in the Nix dev
  shell, so the host needs only Nix.
- Pull requests from forks would run their code on that host. Keep fork runs
  behind GitHub's approval for outside contributors, and give the runner no
  credentials beyond the job's read-only token, as the other queue runners do.
- `Check` stays the single required context, so the ruleset needs no change.

## Waiting

- Maintainer (action): create a `token-max-queue` runner group allowing kixel-ai/token-max and register a runner on a NixOS CI host in it; an agent cannot, since registering takes an org admin's token and the hosts' configuration. Check with `gh api orgs/kixel-ai/actions/runner-groups --jq '.runner_groups[].name'`.
- Maintainer: Where should the Windows tests run, given no self-hosted Windows runner exists and they failed where Linux passed in 13 of about 220 pull request runs in a week, each time in a test new to that branch?
  - Keep `windows-2025` as a listed exception (Recommended) - its runner waits were p90 33 s, and the signal is worth keeping on the gate
  - A self-hosted Windows host - meets the standard, at the cost of a Windows machine to maintain
  - Move Windows off the gate to a run after each push to `main` - fastest gate, but Windows bugs would land and be fixed after the fact
