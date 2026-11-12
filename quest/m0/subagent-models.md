# [L] Report each subagent type's model and cost

## Goal

The report gives every Claude Code subagent type (Explore, Plan,
general-purpose, each custom agent) and every Codex agent role its own
line: what it cost, how many times it ran, the model and reasoning effort it
ran on, where that model came from, and what the same recorded tokens would
cost on the provider's cheapest current model. Each line names the setting
that changes the model and the effort. The lines are evidence only: they
never recommend a model, never exit nonzero, and add nothing to the settings
snippet.

Non-goal: quest's own model routing tables, which moq-dev/quest's
[model check](https://github.com/moq-dev/quest/blob/main/quest/m1/model-check.md)
owns.

## Plan

Decided 2026-11-11 (maintainer, /quest-plan, after asking whether a post
claiming "Explore now burns your Fable usage" was true):

- Harnesses? ✅ Claude and Codex (rejected: Claude only). Codex records
  `agent_role` on spawned threads; this host had 10 spawns, all on the
  default role, at gpt-6.1-sol with xhigh effort.
- Milestone? ✅ m0, with m0's model-selection exclusion amended to carve
  out subagent model settings (rejected: m1). That exclusion kept token-max
  off quest's routing tables, which stay out.
- Verdict? ✅ Report only, as advisory evidence (rejected: recommend a
  cheaper model above `--max-cost-per-use` per run; recommend a cheaper
  Explore model outright). A cheaper model trades capability, and the main
  agent's extra work after a weaker search lands in the parent's requests,
  where transcripts cannot attribute it, so savings can't be claimed.
- Repricing target? ✅ The provider's cheapest current model in the price
  catalog: the newest Haiku for Anthropic, the cheapest current GPT for
  OpenAI (rejected: each cheaper family; no estimate). Skip it for a type
  already on that model. Label it a reprice of the recorded tokens that
  assumes the same turns.
- Effort? ✅ Show the recorded effort and the setting that changes it, and
  say its cost effect can't be measured (rejected: model only). A lower
  effort changes the output tokens, so it can't be repriced.

Evidence, Claude Code 2.1.321 (binary read 2026-11-11) and the
[changelog](https://github.com/anthropics/claude-code/blob/main/CHANGELOG.md):

- 2.1.198 made Explore inherit the main model, "capped at opus", instead of
  running on Haiku, together with subagents inheriting the thinking
  configuration "improving output quality on delegated tasks". This was a
  deliberate quality trade, not an oversight. Before 2.1.198 Explore ran
  on Haiku; read each request's recorded `version`.
- The cap applies only to the built-in Explore, only on the first-party
  API, and only when the main model's family ranks above Opus (Fable,
  Mythos). `CLAUDE_CODE_DISABLE_EXPLORE_INHERIT_CAP` turns it off. 2.1.284
  made Explore inherit model ids Claude Code doesn't recognize instead of
  switching to Opus.
- Model precedence: the Agent call's `model`, then the agent definition's
  `model:` (including `inherit` and the Explore cap), then
  `CLAUDE_CODE_SUBAGENT_MODEL`, then the parent model. So the env var
  reaches only agents without a `model:`, and never the built-in Explore.
  `CLAUDE_CODE_SUBAGENT_MODEL_FORCE` overrides the definition. Fork
  subagents always inherit.
- A user or project agent with the same `name:` replaces a built-in one,
  and the cap no longer applies. The report names
  `.claude/agents/<name>.md` with `model:` and `effort:` for each type. For
  a built-in, it also says the replacement must restate the read-only tool
  restriction and system prompt, and set `omitClaudeMd: true` to keep
  skipping CLAUDE.md as Explore and Plan do.
- Each Claude subagent transcript has an `agent-*.meta.json` beside it with
  `agentType` and the requested `model` (the per-call value, such as
  `haiku`). Attribute the source from that, the profile's agent files and
  env, the recorded version and the main model, and say "unknown" when
  they can't settle it, for example when an agent file changed during the
  window.
- On this host, 389 Explore runs since 2026-09-01: 375 on Opus 5.5, with
  1.31B cache-read, 43M cache-write and 2.3M output tokens.

Codex: confirm from Codex 0.167's source how a spawned thread's model and
effort are chosen (role config, spawn arguments, inheritance) and which
`config.toml` keys change them. Name those keys on each role line. Read the
role from `source.subagent.thread_spawn.agent_role`, null meaning the
default role.

Update the README's feature list and `docs/semantics.md` in the same PR.

## Related

- [Session export](https://github.com/kixelated/token-max/pull/54) - prices each subagent transcript file as its own record; share the attribution
