# [S] Advise upgrading OpenCode below 2.3.2 on gpt-6.2

## Goal

`token-max vibes` shows a card recommending an OpenCode upgrade when a
profile's latest OpenCode session ran a version older than 2.3.2 with a
gpt-6.2 model (gpt-6.2-sol or gpt-6.2-astra). The card cites
[openai/codex#62312](https://github.com/openai/codex/issues/62312) and prices
nothing.

## Plan

Decided 2026-11-27 (maintainer, /quest-run): ✅ replace the earlier plan to
price the uncached system prompt in `doctor` with a `vibes` card that
recommends upgrading and prices nothing (rejected: add the card in
[kixel-ai/token-max#283](https://github.com/kixel-ai/token-max/pull/283);
drop it entirely). There's no data to price it, and newer OpenCode fixes it.

Background, decided 2026-11-25 (maintainer, /quest-plan over
[this post](https://x.com/TekonBurns/status/2123525121168314876)): gpt-6.2-sol
and gpt-6.2-astra never cache a Responses request's top-level `instructions`,
but they cache the same text sent as a leading developer message
([openai/codex#62312](https://github.com/openai/codex/issues/62312), open).
The post says OpenCode v2.3.2 moved the prompt there. Codex shows no gap on
the maintainer's host, so Codex gets no card.

Recommendations:

- The OpenCode reader in `src/opencode.rs` already keeps the version that
  created each session (`opencode::Session::version`, from
  `session_v2.version` or `session.version`). Carry it onto each request, as
  Claude Code's requests carry theirs, so the card reads the latest session's
  version and model.
- Add the card in `src/vibes.rs`, worded like the upgrade advice in
  `docs/reference.md`'s Harness upgrades. OpenCode records no install method,
  so name the version alone, as for Codex. A version that isn't plain digits
  advises no upgrade, as those docs say.
- Cover it with a synthetic `opencode.db` fixture, since the maintainer's
  host has no OpenCode history on gpt-6.2.
- Add the card to the table in `docs/reference.md`'s Vibes section.
