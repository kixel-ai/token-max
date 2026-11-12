# [L] Run the report as an interactive doctor

## Goal

A person runs `token-max doctor` in a terminal, reads today's report, and is
then walked through each recommended change it can make: the exact edit is
shown, and a user settings file is written only after a yes. A change it
cannot make is listed as a step to do by hand, with the reason. Bare
`token-max` prints help. Without a terminal, `doctor` prints the report,
changes nothing, and keeps today's exit codes.

Out of scope: applying changes on `--ssh` hosts, which still get a report
with their changes as steps to run there; project, managed-policy and T3
settings files; new checks.

## Plan

Decided 2026-11-11 (maintainer, /quest-plan after an audit of the premise;
[kixelated/token-max#62](https://github.com/kixelated/token-max/pull/62)
states the direction in `AGENTS.md`). The maintainer: "A user runs it and it
spits out recommendations, offering to update their configuration. It
should focus on being interactive with humans." This supersedes the
2026-11-05 choice of recommend only. Decision prompts, chosen options
marked ✅:

- Scope: ✅ one quest renames the entry point and adds the apply flow
  (rejected: rename only, with the apply flow later; also applying over
  SSH).
- Command shape: ✅ `doctor` is required and bare `token-max` prints help
  (rejected: bare `token-max` as an alias). One way to run it, and the
  flags parse one way.
- What it applies: ✅ every recommendation that can be expressed in a
  user-level Claude `settings.json` or Codex `config.toml`, its `env` block
  included; the maintainer: "try to automate as much as possible"
  (rejected: only today's tool, skill and MCP off-switches; project and T3
  files, whose edits land in someone's git diff). When a higher-precedence
  owner would still win, such as a shell environment variable or managed
  policy, the change is a manual step that names that owner.
- Files it may not own: ✅ write through a symlink when its target is
  writable, and say where the edit landed; when the target is read-only,
  print the edit and the resolved path (rejected: never write through a
  symlink; a flag or config entry that marks a file as managed). A no at
  the prompt always prints the edit, which covers a file a dotfiles job
  rewrites: on the maintainer's host `dotfiles-update.timer` rewrites a
  regular, writable `~/.claude/settings.json` hourly, so nothing on disk
  marks it as managed.
- Consent: ✅ one prompt per change, answered yes, no or quit, and each
  file written once with the accepted edits (rejected: one prompt per file;
  a checklist, which needs a terminal UI dependency).
- Exit codes: ✅ on a terminal, 0 unless the run failed; without one, 0, 1
  and 2 as today (the maintainer: "0, 1, 2 only if run without a term";
  rejected: 0, 1 and 2 everywhere).

Today `src/report.rs` prints a snippet only for tool, skill and MCP
off-switches (`settings_patch`, `config_patch`); every other check names
its owner in the line and has no edit to show.

Agent's choices, for the implementer to revisit:

- A terminal means stdin and stdout both are one.
- An edit keeps the file's other content, key order and comments, which
  for `config.toml` needs a format-preserving editor.
- Re-read each file just before writing and stop if it changed since the
  report read it. Write atomically, without replacing a symlink.
- `--plain` and `NO_COLOR` still apply to the prompts.
- `token-max sessions` parses only a leading `sessions` word
  (`src/cli.rs`); generalize it into sub-command parsing for `doctor`.

The same PR updates `--help`, the README and `docs/reference.md`, whose
"Nothing is hidden behind an interactive prompt" no longer holds.

## Required

- [Remove the JSON report](/quest/m0/remove-json.md) - `doctor` has no JSON mode to carry over
