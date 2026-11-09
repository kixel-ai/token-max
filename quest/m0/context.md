# [M] MCP and loaded-context check

## Goal

`token-max` shows which MCP servers/tools, instruction files, and tool
results consume recurring context, what they cost where measurable, and how
often exposed MCPs are observed being used. It recommends trimming unused
or rarely used MCPs and excessive loaded context, with provider guidance
when costs cannot be established. It never edits settings or prints context
content.

## Plan

Decided 2026-11-06 (maintainer, /quest-plan): ✅ include in m0 (rejected:
m1). The maintainer wants transcript-based cost evidence where possible,
and guidance warnings for context/MCP overhead that cannot be priced.
The skills catalog quest owns skill-specific findings; this check links
those findings instead of adding their costs again.

Decided 2026-11-07 (maintainer): ✅ put an estimated dollar amount on every
setting, one line each. Later the same day: ✅ recommend turning a tool, MCP
server or skill off when it cost more than $1 per recorded use, counting no
use as one, with `--max-cost-per-use` to change it (rejected: report cost and
leave every call to the user; a rarely used count threshold). The report ends
with one settings snippet for the recommended changes. Recorded in AGENTS.md.

Progress 2026-11-08: Claude and Codex pieces, tool output held until
compaction, and output-cap lines are in the default report. Claude content is
bounded by its cached reads plus writes, since Claude Code caches through the
last message. What remains:

- Deferred loading: Claude exposes `ENABLE_TOOL_SEARCH` and per-server
  `alwaysLoad`; check supported models/provider conditions, then price a
  server's always-loaded schemas against the discovery overhead of deferring
  them. A small frequently used toolset can be cheaper upfront. Codex exposes
  `mcp_servers.<id>.omit_tools_from` for `direct`, `deferred`, or `code_mode`
  surfaces; do not recommend retired tool-search switches.
- Per-tool Codex controls: `mcp_servers.<id>.enabled_tools`/`disabled_tools`
  and `apps.<id>.enabled`, once the app id format is documented. Omission
  changes exposure, while `disabled_tools` blocks calls.
- Instruction budgets: warn when AGENTS.md exceeds Codex's
  `project_doc_max_bytes` or CLAUDE.md exceeds Claude's documented guidance,
  citing it and naming the file, without judging quality from length alone.
- Output caps: the report lists each cap with the output it held and its
  largest result, but recommends no value, since replay cannot price the
  read-back a smaller cap causes. Revisit if transcripts link a truncated
  result to the reads that followed it.

Sources checked 2026-11-06: [Claude MCP overhead
guidance](https://code.claude.com/docs/en/costs#reduce-mcp-server-overhead),
[MCP tool-search controls](https://code.claude.com/docs/en/mcp),
[Claude instruction guidance](https://code.claude.com/docs/en/memory),
[Codex instruction loading](https://learn.chatgpt.com/docs/agent-configuration/agents-md),
[Codex context guidance](https://learn.chatgpt.com/blog/rethinking-skills-and-prompts-for-gpt-6-astra),
and [Codex MCP settings](https://learn.chatgpt.com/docs/config-file/config-reference).

## Related

- [Codex skills catalog](/quest/m0/skills-catalog.md) - owns per-skill Codex lines and duplicate listings, reusing the existing context pricing
