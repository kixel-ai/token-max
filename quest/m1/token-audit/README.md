# [S] token-audit: agent-run transcript efficiency audit

## Goal

Ship an agent-run `token-audit` workflow—combining a deterministic `token-max audit`
forensic subcommand with a `token-audit` agent skill—so a user can audit their
Claude Code and Codex transcripts (including T3 Code instances and `--ssh` hosts)
for actionable, non-setting token inefficiencies (cache-busting commands/actions,
excessive or bloated tool usage, useless always-loaded or carried context, and
recurring discoveries that belong in repo/user context) and interactively apply
approved fixes. Non-goals: duplicating `token-max doctor`'s settings checks,
printing prompt/reply/command text from the `token-max` binary, or applying edits
without user approval.

## Plan

### Settled decisions (2026-11-25, maintainer, `/quest-plan`)

- Goal and shape: ✅ an agent-run `token-audit` skill backed by `token-max`
  forensics (rejected: a `quest` built-in audit in `kixel-ai/quest`, which would
  reach only `quest` repos; a CLI-only forensic report without an agent skill,
  which cannot judge useless vs. missing context; drop). Invoking Claude/Codex
  directly from `token-max` was explicitly rejected in favor of a skill.
- CLI backing: ✅ new `token-max audit` forensic subcommand + `token-audit`
  skill (rejected: skill-only on `token-max sessions` + `doctor --details`;
  standalone skill with no `token-max` dependency).
- Default audit scope: ✅ current Git repository by default, with an option for
  a whole-host (`--all`) audit (rejected: whole-host by default; single recent
  session by default).
- Distribution: ✅ standard `skills/token-audit/SKILL.md` in the repository (so
  `npx skills add kixel-ai/token-max -g` works out of the box) plus a CLI
  installer `token-max skill install` (rejected: repo `skills/` only;
  binary-embedded installer only). The maintainer considered relying solely on
  an external `npx` skill installer, then confirmed keeping `token-max skill
  install` because `token-audit` requires `token-max` installed locally anyway.
- Output and action model: ✅ ranked, dollar-attributed findings report plus an
  interactive offer to apply approved fixes (rejected: read-only report;
  auto-applying edits on a branch/PR).
- CLI privacy boundary: ✅ strict metadata and `(host, file, line)` pointers
  only—zero prompt, reply, or raw command/argument text from `token-max`
  (rejected: opt-in command strings or truncated snippets from the CLI).
- Inefficiency buckets in v1: ✅ all four buckets—(1) cache invalidations and
  prefix busts, (2) tool-output bloat and carried-context tax, (3) repetitive or
  failing tool loops and session-shape outliers, and (4) skill-side semantic
  inspection of flagged slices and sampled early exploration turns for useless
  and missing context (rejected: cache busts + tool-output bloat only).
- `SKILL.md` structure: ✅ thin `SKILL.md` stub that runs `token-max skill
  audit` to print the live audit prompt, matching `quest`'s pattern (rejected:
  self-contained `SKILL.md` with version-drift checks).
- Surface in `doctor`/`vibes`: ✅ a `Token audit skill` card in `token-max
  vibes` (shown when `token-audit` is not installed, quiet when installed) naming
  `token-max skill install` as a step by hand (rejected: keeping `doctor`/`vibes`
  untouched; extending `vibes` to write `SKILL.md` files on accept; adding a
  separate closing prompt in `doctor`).
- Remote hosts (`--ssh`): ✅ support `--ssh` in `token-max audit` from v1, with
  the skill reading flagged remote slices directly over `ssh <host>` from the
  `(host, file, line)` pointers so `token-max` stays strictly metadata-only
  (rejected: local-only in v1; stats-only for `--ssh` hosts; adding a
  text-reading slice command to `token-max`).
- `token-max skill install` defaults: ✅ install into global user skill
  directories for detected harnesses (`~/.claude/skills/token-audit/SKILL.md`,
  `~/.codex/skills/token-audit/SKILL.md`, and
  `~/.agents/skills/token-audit/SKILL.md` when `~/.agents` exists; honoring
  `--claude-dir`/`--codex-dir`), with `--project` for repo-local `.agents/skills`
  and `.claude/skills` (rejected: `~/.agents/skills` only; project-local by
  default).
- Codex install path (2026-11-26, taken while the maintainer was away, in
  [the skill PR](https://github.com/kixel-ai/token-max/pull/275)): ✅
  `token-max skill install` writes Codex's copy to `~/.agents/skills`, which
  Codex reads for every home, and takes no `--codex-dir`; this replaces the
  `~/.codex/skills` and `--codex-dir` parts of the decision above (rejected:
  also writing `$CODEX_HOME/skills`, which Codex keeps only as a deprecated
  location and which would list the skill twice).

### Overlap and boundaries

- `token-max doctor` and `vibes` own settings-level recommendations (and `m0`'s
  2026-11-17 decision keeps habit lines out of `doctor`). `token-audit` owns
  non-setting workflow, command, tool, and context inefficiencies.
- `quest`'s `instructions` audit checks instruction-file compliance against
  official harness docs and review escapes; `token-audit` checks measured token
  spend, cache busts, tool bloat, and context ROI for any repository or host.
- This epic does not gate [Release 0.1.0](/quest/m1/release-0.1.md).

### Epic closing work

Once all child quests merge:

- Update `README.md`, `docs/reference.md`, and `AGENTS.md` (if needed) so
  `token-max audit`, `token-max skill audit`, `token-max skill install`, and the
  `vibes` card are documented together cleanly.
- Exercise the full loop end to end on real local transcripts (without committing
  any transcript content): `token-max skill install`, `token-max skill audit`,
  `token-max audit` (repo-scoped and `--all`), and `token-max vibes`.
- Add a follow-up quest (in `m2`, with a `Maintainer:` item) for the maintainer
  to try `/token-audit` on a real repo and decide whether and where to promote it
  (including `max.kixel.me`).
