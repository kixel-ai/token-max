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
setting and let the user decide (rejected: recommend disabling rarely used
pieces). Settings that don't change the experience, like the cache TTL, get a
recommendation; tools, MCP servers, skills and instruction files get their
cost, use count and off switch, with no waste verdict. Rarely used is not
unused. Recorded in AGENTS.md.

Progress 2026-11-07: per-piece costs from recorded Claude request shapes,
priced at their cache positions, with call counts and controls, are in the
default report. Codex pieces, tool-result context and output-budget checks
remain.

- Attribute actual exposed server instructions, discovery names,
  descriptions, schemas, and loaded CLAUDE.md/AGENTS.md/rule or memory
  content separately. Separate recurring discovery cost, on-demand schema
  loading, and tool-result context. Installed/enabled does not imply fully
  loaded on every request, and a schema hash cannot establish token size.
- Replay component exposure using recorded snapshots/deltas and request
  identities. Price marginal before/after tokens at the model's applicable
  cache and context tiers, stating tokenizer/allocation assumptions. Show
  attributed overhead separately from estimated net savings; account for
  changed discovery calls, replacement reads, catalog backfill, or cache
  invalidation when supported. Missing schemas or loaded-file content
  yield a coverage warning, not an invented full-schema charge.
- For each server/tool, show the window, exposed sessions/requests, observed
  calls, last use, and usage visibility. Include identifiable nested calls
  through execution wrappers; code references alone do not prove execution
  or success. With incomplete call visibility, say "no observed use", not
  "unused". Incomplete visibility and rare use only produce advisory
  rankings; an automatic unused finding requires complete exposure/call
  visibility across the window, positive recurring overhead, and a finding
  that still applies to the current effective configuration. Unused
  findings for components introduced during the window stay advisory, per
  the milestone rules.
- Recommend scoped disabling of unused MCP servers/tools and restoring
  deferred loading where appropriate. Claude exposes `ENABLE_TOOL_SEARCH`
  and per-server `alwaysLoad`; check supported models/provider conditions
  before warning. Codex exposes server/tool enablement; do not recommend
  retired tool-search switches. A small frequently used toolset can be
  cheaper upfront once discovery overhead is included.
  Claude server disabling uses `/mcp`. A bare-name deny rule
  (`"Artifact"`, `"mcp__server"`) removes the tool from context, while a
  scoped rule such as `Bash(rm *)` only blocks matching calls
  ([permissions](https://code.claude.com/docs/en/permissions), checked
  2026-11-07). Codex has
  server/tool enablement lists and `mcp_servers.<id>.omit_tools_from` for
  `direct`, `deferred`, or `code_mode` surfaces; the equivalent app setting
  combines with server omissions. Omission changes exposure, while
  `disabled_tools` blocks calls. Name the supported control for each finding.
- Use applicable provider guidance to warn about oversized always-loaded
  instructions, duplicate content, or catalogs over documented budgets.
  Suggest concise descriptions, project/path scoping, and moving detailed
  workflows to on-demand skills. Cite the guidance and name the affected
  files/settings, without judging instruction quality from length alone or
  recommending arbitrary truncation that discards required instructions.
  Current file sizes can support a current warning but not a claim about
  historical exposure or dollars.
  Include agent listings, hooks, and plugin/app/managed instructions in the
  category breakdown; link shared plugin costs without charging them to
  both skills and MCPs.
- Check inline tool-output budgets against observed output sizes and
  repeated reads: Claude exposes `bashOutputMaxChars` and
  `MAX_MCP_OUTPUT_TOKENS`; Codex exposes `tool_output_token_limit`. Compare
  recurring context against extra reads caused by smaller limits, rather
  than always recommending the lowest cap. Where replay cannot establish
  the tradeoff, flag an applicable provider-budget or overflow warning and
  give unknown dollars.

Sources checked 2026-11-06: [Claude MCP overhead
guidance](https://code.claude.com/docs/en/costs#reduce-mcp-server-overhead),
[MCP tool-search controls](https://code.claude.com/docs/en/mcp),
[Claude instruction guidance](https://code.claude.com/docs/en/memory),
[Codex instruction loading](https://learn.chatgpt.com/docs/agent-configuration/agents-md),
[Codex context guidance](https://learn.chatgpt.com/blog/rethinking-skills-and-prompts-for-gpt-6-astra),
and [Codex MCP settings](https://learn.chatgpt.com/docs/config-file/config-reference).

## Related

- [Skills catalog and usage check](/quest/m0/skills-catalog.md) - owns skill listing and usage recommendations
