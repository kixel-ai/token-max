# [S] Back up each settings file before doctor writes it

## Goal

Before `token-max doctor` writes a settings file, it copies the file as it
was to `<file>.token-max.bak` beside it, so the last accepted change can be
undone by copying the backup back. The confirm names each backup it will
write, and the closing line names each one written. A file git tracks with
no uncommitted changes gets no backup; the confirm says git keeps its
previous version.

## Plan

Decided 2026-11-16 (maintainer, /quest-run, asking "should we make backups
of config files before writing?"). Today doctor writes atomically after a
shown diff and a yes, but keeps no copy, and
[#132](https://github.com/kixel-ai/token-max/pull/132) adds deleting MCP
server entries from `~/.claude.json` and Codex's `config.toml`.

- ✅ its own quest in m0, covering every write before 0.0.1 (rejected:
  folding it into #132; no backups).
- ✅ beside each file (rejected: one folder per run in token-max's state
  directory; beside user-level files only).
- ✅ one rolling `<file>.token-max.bak`, overwritten on each write, holding
  the file as it was just before the latest change (rejected: timestamped
  backups keeping five; writing it once before token-max first touches the
  file).
- ✅ skip a file git tracks with no uncommitted changes, since a `.bak`
  beside a project's `.claude/settings.json` would show up to be committed
  (rejected: backing it up and adding it to `.git/info/exclude`; backing it
  up anyway). A tracked file with uncommitted changes is backed up, since
  git doesn't hold those.

Recommended:

- Give the backup the original's permissions, since these files can hold
  secrets such as an MCP server's `env`.
- Back up the content the write replaces: for `~/.claude.json`, which #132
  edits under Claude Code's lock after re-reading it, back up what that
  re-read returned, not what the walk read at the start.
- Follow a symlinked settings file to its target, as the write already does,
  and put the backup beside the target.
- Order: take `~/.claude.json`'s lock and re-read it first, then stage every
  file's backup as a temporary sibling before writing any settings file, and
  hold that lock until its write. A failed staging writes nothing, leaves
  every existing `.bak` as it was, and fails the run with the reason.
- Move each staged backup over its `.bak` only once its settings write
  commits, so a run that stops partway never replaces a `.bak` whose file it
  didn't change.
