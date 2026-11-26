# [S] Add Token audit skill card to token-max vibes

## Goal

`token-max vibes` includes a `Token audit skill` card that checks whether the
`token-audit` skill is installed for the audited host's Claude Code and Codex
profiles; when missing, it explains what `/token-audit` diagnoses beyond
settings and names `token-max skill install` as a step by hand, and when already
installed, it stays quiet so `doctor` does not offer `vibes` solely for it.

## Plan

Implements the `vibes` discovery surface decided in the
[token-audit epic](/quest/m1/token-audit/README.md).

### Card behavior

- Follow the existing step-by-hand card pattern in `src/vibes.rs` and
  `src/report.rs` (`how: None`, matching `Code intelligence` and `Explore
  subagent`): never writes `SKILL.md` files from `doctor`/`vibes`.
- Detection (local and over `--ssh`, reusing `src/discovery.rs`):
  - Check whether `skills/token-audit/SKILL.md` exists under each discovered
    Claude profile dir (`<claude-dir>/skills/token-audit/SKILL.md`), Codex home
    (`<codex-dir>/skills/token-audit/SKILL.md`), or the shared
    `~/.agents/skills/token-audit/SKILL.md` (following symlinks safely, never
    reading unrelated files).
  - If all discovered harness profiles on the host have `token-audit` available,
    emit the card with `quiet: true` (visible when running `token-max vibes`
    directly, not counted by `doctor`'s end-of-walk offer).
  - If missing for any profile on the host, emit an active card (`quiet: false`)
    stating concisely what `/token-audit` finds beyond settings (cache-busting
    commands/actions, bloated/repetitive tool calls, useless and missing
    context), citing the maintainer's pick as source, and naming
    `token-max skill install` (with `npx skills add kixel-ai/token-max -g` as an
    alternative) as the step by hand. Include `--claude-dir`/`--codex-dir` when
    non-default profiles are missing it, matching how `Code intelligence` scopes
    its install command.
- Keep `token-max doctor` strictly measured: no new line in `doctor`, and no
  change to `doctor`'s exit code.
- Check whether `assets/demo/walk.tape` and `assets/demo/site.py` are affected
  by the new `vibes` card (per `AGENTS.md`, rerun if `doctor`/`vibes` output in
  the demo changes).

### Verification

- Integration tests in `tests/` covering: missing skill (active card, `doctor`
  offers `vibes`), installed skill in `~/.claude/skills` / `~/.codex/skills` /
  `~/.agents/skills` (quiet card, `doctor` does not offer `vibes` solely for
  it), partial install (one harness missing), and `--ssh` per-host detection.
- Document the card in `docs/reference.md#vibes`.

## Required

- [token-audit skill and installer](/quest/m1/token-audit/skill.md) - ships the `token-audit` skill and `token-max skill install` command the card checks for and names
