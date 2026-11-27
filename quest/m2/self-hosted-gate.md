# [M] Run the merge gate on self-hosted runners

## Goal

Every job a pull request or the merge queue runs in `Check` uses a kixel-ai
self-hosted runner, as the maintainer's standard for public repositories
asks, or a listed exception says why it cannot, so the gate's latency no
longer depends on the org's shared GitHub-hosted runners.

## Plan

Decided 2026-11-27 (maintainer, /quest-run): ✅ add a self-hosted Windows
runner too, so the Windows tests stay on the gate (rejected: keep
`windows-2025` as a listed exception; move Windows off the gate; drop the
quest).

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
- The Windows tests move to a self-hosted Windows host in the same
  `token-max-queue` group, labelled `windows` so `runs-on` picks it apart
  from the Linux runners. Each job gets a fresh worker, such as a VM
  restored from a clean snapshot with Rust and the MSVC build tools
  preinstalled, under the same isolation rule as Linux below. Until that
  host is registered, the Windows job stays on `windows-2025`.
- Pull requests from forks would run their code on that host. Set the
  repository's fork-run approval to "Require approval for all outside
  collaborators", since GitHub's default asks only first-time contributors,
  and run every job that executes pull request or merge-group code, which
  holds a fork's code once its PR is queued, on a fresh, ephemeral worker, so
  no job leaves state that a later `Check` reads. Give the runner no
  credentials beyond the job's read-only token. Until the host can give each
  job a fresh worker, those jobs stay on GitHub's runners.
- `Check` stays the single required context, so the ruleset needs no change.

## Waiting

- Maintainer (PC): provision a Windows host for token-max's Windows tests, a machine or VM that can restore a clean snapshot per job, with Rust and the MSVC build tools installed; an agent cannot, since it needs the maintainer's hardware or a paid cloud account and a Windows license. Check that the host is reachable and `cargo --version` runs on it.
- Maintainer (action): create a `token-max-queue` runner group in the kixel-ai org allowing kixel-ai/token-max; an agent cannot, since creating a runner group takes an org admin's token, which no agent holds. Check with `gh api orgs/kixel-ai/actions/runner-groups --jq '.runner_groups[].name'`.
- Maintainer (action): register runners in `token-max-queue` on a NixOS CI host and on the Windows host; an agent cannot, since registration takes an org admin's token and changes the hosts' configuration. Check with `gh api orgs/kixel-ai/actions/runners --jq '.runners[] | [.name, .status, (.labels | map(.name) | join(","))] | @tsv'`.
