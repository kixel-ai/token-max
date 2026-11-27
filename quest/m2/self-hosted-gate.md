# [M] Run the Linux merge gate on Kixel Quest's CI hosts

## Goal

token-max's Linux gate jobs (`Plan`, `Source`, `Check`) run on Kixel Quest's
native CI hosts instead of GitHub's shared runners, so the gate's latency no
longer depends on the org's GitHub-hosted runner pool. The Windows tests stay
on GitHub's `windows-2025` runner, and fork pull requests from outside
contributors stay on read-only GitHub Actions.

## Plan

Decided 2026-11-27 (maintainer, /quest-run): ✅ wait on Kixel Quest's native
CI and adopt it, with Windows on GitHub's runner and no runners registered by
hand (rejected: token-max registers its own Linux and Windows runners in a
`token-max-queue` group now; delete the quest). This supersedes the same
day's earlier pick of a self-hosted Windows runner. Two decisions from
2026-11-26 had already chosen this path:
[kixel-ai/dotfiles#378](https://github.com/kixel-ai/dotfiles/pull/378)
dropped per-repository GitHub runners for public merge gates in favor of
Kixel Quest's CI, and that CI epic (kixel-ai/kixel.quest
`quest/m1/ci/README.md`) keeps Windows and macOS on thin GitHub Actions legs,
rejecting native Windows and macOS hosts for now. dotfiles' `repo-settings`
stays unchecked for public repositories (dotfiles#369).

Why: in the week the m2 `cost` audit measured, waits for the org's shared
macOS runners held gate runs for up to 84 minutes and two merge-queue entries
timed out. That audit moved macOS off the gate; the Linux jobs still share
GitHub's pool.

Recommended, for the implementer to confirm against the CI epic as it lands:

- Declare token-max's gate in a root `quest.toml`, following the CI epic's
  contract (`quest/m1/ci/contract.md` there), rather than adding runner
  labels to `.github/workflows/check.yml`. `Source` already runs in the Nix
  dev shell.
- Keep `Check` as the single required context, or move the ruleset to
  whatever context the contract names, in the same PR.
- The Windows job and macOS's post-merge run stay as they are.

## Waiting

- kixel-ai/kixel.quest: native Linux CI on quest hosts (`quest/m1/ci/runner.md`) and the `quest.toml` contract it reads (`quest/m1/ci/contract.md`) ship for GitHub-mirrored repositories. Check that both quests are gone from kixel.quest's `main` and that its CI epic names a pilot repository that landed through it.
