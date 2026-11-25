# [S] Advise on the subagent default effort in vibes

## Goal

When a Claude Code profile sets `CLAUDE_CODE_SUBAGENT_EFFORT` or a Codex
profile sets `agents.default_subagent_reasoning_effort`, `token-max vibes`
shows a `Subagent default` card with that effort and the subagent model it
pairs with. Past `high`, the card cites the same provider guidance the
`Main-session default` card gives past `high`, and offers to set the effort
to `high`, labeled as the maintainer's pick. At or below `high` it is a quiet
card. When the variable is unset, subagents follow the main session, which
the main-session card already covers, so there is no card. When the latest
Claude Code session ran a version that ignores the variable, the card says
it is ignored and recommends upgrading.

## Plan

Decided 2026-11-25 (maintainer, /quest-plan over five cost tips from
@TekonBurns's posts; the decisions on the other four are in the planning PR's
body). The tip: `CLAUDE_CODE_SUBAGENT_EFFORT` is now documented, from Claude
Code 2.1.315, beside `CLAUDE_CODE_SUBAGENT_MODEL`
([post](https://x.com/TekonBurns/status/2123586372615164286)). token-max
already resolves both variables and Codex's `agents.default_subagent_*` keys
(`claude_controls` in `src/subagents.rs`) as evidence, but advises nothing
about them.

Decision prompts, chosen options marked ✅:

- From the five tips: ✅ this card plus an OpenCode note in
  [kixel-ai/token-max#206](https://github.com/kixel-ai/token-max/pull/206)
  (rejected: the note only, keeping subagent effort evidence only as m0
  decided on 2026-11-11; drop everything).
- What the card offers: ✅ the edit to `high` past `high`, as the maintainer's
  pick, and a quiet card otherwise (rejected: the guidance with no edit; a
  pick of both subagent model and effort like the main-session card, which
  reaches into model choice).
- Version: ✅ recommend upgrading when the latest session's Claude Code
  ignores the variable (the maintainer's addition), as every version-gated
  line and card does (`docs/reference.md`'s Harness upgrades).

Recommendations:

- Reuse `past_high` and the main-session card's sources in `src/report.rs`
  rather than writing the guidance twice.
- Confirm the first version that reads `CLAUDE_CODE_SUBAGENT_EFFORT` in
  Claude Code's changelog; the tip says 2.1.315. Gate it with
  `Needs::upgrade` (`src/version.rs`) on the profile's latest request, as
  `doctor`'s `deniedMcpServers` gate does, and word the card with its
  `Upgrade::why` and `Upgrade::step`.
- Before writing a value the model can't take, check it: `xhigh` falls back
  to `high` on models without it (`src/effort.rs`), and `high` is accepted
  everywhere effort applies.
- The edit goes where the variable is set now, such as `settings.json`'s
  `env` or a T3 instance's environment, and it is offered over SSH too.
  Leave an agent file's `effort:` or an Agent call's effort alone: they
  override the default and are evidence on the subagent line.
- Add the card to `docs/reference.md#vibes`.
