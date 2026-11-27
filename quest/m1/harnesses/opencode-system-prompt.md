# [M] Price OpenCode's uncached gpt-6.2 system prompt before 2.3.2

## Goal

When the latest OpenCode session on a profile ran a version older than 2.3.2
with gpt-6.2-sol or gpt-6.2-astra, `token-max doctor` prices the system
prompt's uncached input over the window as a `~` estimate with its margin of
error, and recommends upgrading OpenCode through the version-gated upgrade
advice in `docs/reference.md`'s Harness upgrades.

## Plan

Decided 2026-11-25 (maintainer, /quest-plan over
[this post](https://x.com/TekonBurns/status/2123525121168314876)):
gpt-6.2-sol and gpt-6.2-astra never cache a Responses request's top-level
`instructions`, but they cache the same text sent as a leading developer
message ([openai/codex#62312](https://github.com/openai/codex/issues/62312),
open). The post says OpenCode v2.3.2 moved the prompt there. Codex itself
shows no gap on the maintainer's host (since 2026-11-18, gpt-6.2-sol cached
90.2% of input over 1,281 requests, and gpt-6.1-sol 88.9% over 7,960), so
Codex gets no line.

Recommendations:

- The OpenCode reader in `src/opencode.rs` already keeps the version that
  created each session (`opencode::Session::version`, from
  `session_v2.version` or `session.version`); carry it onto each request, as
  Claude Code's requests carry theirs, so the gate reads the latest session's
  version.
- OpenCode's stored token counts don't split out the system prompt. Size it
  from OpenCode's prompt for the model family at the recorded version, at the
  quarter-token-per-character estimate the other OpenCode pieces use, and
  price it as ordinary input on every request that read nothing from cache
  beyond it. Mark the result `~` with its margin, and say which part of the
  prompt it could not size, such as the environment block.
- Reuse the upgrade line `doctor` gained in
  [kixel-ai/token-max#247](https://github.com/kixel-ai/token-max/pull/247)
  for the recommendation; OpenCode records no install method, so name the
  version alone, as for Codex.
- The maintainer's host has no OpenCode history on gpt-6.2, so cover it with
  a synthetic `opencode.db` fixture and confirm on real data only if one
  appears.
- Document the estimate in `docs/reference.md` and `docs/semantics.md`'s
  OpenCode sections.
