# [M] Share the OpenCode, Antigravity and T3 readers' plumbing

## Goal

OpenCode and Antigravity read their stores through the same helpers as T3 and
the other harnesses, with no copied mechanism. A harness added next writes
only what differs about it. `doctor`'s output is unchanged, except where the
Plan says otherwise.

## Plan

The m2 code audit found these copies, each landed with its harness's support
([kixel-ai/token-max#283](https://github.com/kixel-ai/token-max/pull/283),
[kixel-ai/token-max#294](https://github.com/kixel-ai/token-max/pull/294)):

- A read-only SQLite snapshot is opened three times: `opencode::read`,
  `antigravity::summaries` and `t3::enrich`. Each does the read-only,
  no-mutex open, `busy_timeout`, an optional `SQLITE_LIMIT_LENGTH`, the
  `query_only` and `trusted_schema` pragmas, then a transaction it rolls back.
  Only the busy timeout and the value bound differ on purpose. Write one
  `snapshot(path, busy, bound, read)` helper and one locked-or-unreadable
  error mapper that takes the product's name.
- `opencode.rs` and `antigravity.rs` each write out the same ~65-line
  `Profile` literal: the `unset` TTL closure, the "compaction is not
  replayed" controls, and about 35 identical empty fields. Write one
  constructor, so each module sets only what differs. The OpenCode replays
  quest changes what OpenCode's literal sets, so this waits for it.
- `t3.rs`'s `opencode_instance` and `antigravity_instance` repeat the
  native-root `realpath` handling and the `T3Instance` literal, and their
  caller arms repeat each other. `Excluded.missing` and `.unsupported` are
  never both true, so they can be one enum.
- "An environment variable if non-empty, else a path under home" is written
  three times: `antigravity::gemini_home`, `opencode::data_home` and
  `opencode::config_dir`. The two single-caller wrappers can be inlined.
- `opencode::modified` is a byte-for-byte copy of `subagents::modified`.
- `prices::ordinary_charge` is `prices::charge` with a fixed label, since
  every call writes into a fresh zero `Cost`.
- `discovery.rs` has OpenCode and Antigravity arms that no production path
  reaches: `policy_off`, `pretend_on`, `pretend_t3_projects_on` and
  `settings` are only called for Claude and Codex profiles. OpenCode's
  `--defaults` handles only the global file (`opencode::profile`), so a
  switch an OpenCode project file turns off still shows off under
  `--defaults`, unlike Claude Code's and Codex's. Recommended: route OpenCode
  profiles through `pretend_t3_projects_on` and the per-directory loop, so
  the arms run, and make the Antigravity arms `unreachable!`, as
  `skill.rs` does.
- `antigravity::read_transcript` reimplements `reader::read_lines`. The
  differences look deliberate: it accepts an unterminated last line that
  parses, and doesn't count blank lines as skipped. Keep it unless
  `read_lines` can take both rules without a flag only Antigravity sets.

## Required

- [OpenCode replays](/quest/m1/harnesses/opencode-replays.md) - edits the OpenCode profile and pricing this refactor shares
