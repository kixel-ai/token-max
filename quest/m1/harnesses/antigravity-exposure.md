# [M] Show Antigravity's MCP servers, skills and rules with their recorded uses

## Goal

`token-max doctor` gives each Antigravity MCP server (global
`config/mcp_config.json`, a plugin's, and T3's injected `t3-code`), skill and
rule file (`AGENTS.md`, `GEMINI.md`) its own line with the uses Antigravity's
transcripts record, such as `mcp_<server>_<tool>` calls, and either a measured
cost or a plain statement that its cost is unmeasured. A switch is offered only
where the cost is measured.

## Plan

Antigravity's transcripts record no prompt: not the system prompt, the tool and
MCP definitions, the skill listing or the rules it sent. Antigravity's reader
([kixel-ai/token-max#294](https://github.com/kixel-ai/token-max/pull/294))
therefore prices no Antigravity context and shows none of these lines; the
report's assumptions say their cost is unmeasured. AGENTS.md asks for every
feature on its own line, so this quest adds them.

What exists to work from:

- Recorded uses: each `PLANNER_RESPONSE` step's `tool_calls[].name`, where an
  MCP tool is `mcp_<server>_<tool>` (T3's server shows as `mcp_t3-code_*`).
- Antigravity caches each connected server's tool definitions as
  `<root>/mcp/<server>/<tool>.json`, the current definitions only.
- MCP servers in `<GEMINI_HOME>/config/mcp_config.json` (`mcpServers`), which
  Antigravity's docs give no disable flag, so the off switch removes the entry;
  plugins switch off with `plugins.<dir>.enabled = false` in
  `<GEMINI_HOME>/config/config.json`, which also holds credentials.
- Skills in `<GEMINI_HOME>/config/skills`, `antigravity-cli/skills`, the
  root's `builtin/skills` and a project's `.agents/skills`; rules in
  `AGENTS.md` and `GEMINI.md` from the repository root down.
- T3 rewrites an instance's `antigravity-acp/settings.json` on every launch,
  so no edit may target it.

## Waiting

- Maintainer: How should doctor treat the cost of Antigravity's MCP servers, skills and rules, which its transcripts never record?
  - Lines with uses, cost unmeasured, no switch (Recommended) - each feature shows on its own line with its recorded uses and says its cost is unmeasured, without guessing what Antigravity sent
  - Price from current files - size each server from its cached tool definitions and each skill and rule from its file at 4 characters per token, take every request as having carried them, and offer removing a never-used server from mcp_config.json; prices what the files hold now, not what was sent
  - Drop - keep the single assumption line the report already prints

## Related

- [OpenCode and Antigravity support](/quest/m1/harnesses/README.md) - the epic
  this finishes
