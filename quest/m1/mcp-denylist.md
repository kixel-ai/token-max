# [M] Block MCP servers token-max can't remove with deniedMcpServers

## Goal

After `token-max doctor` turns off a never-used Claude Code MCP server, its
cleanup question offers a confirmed edit that adds the server to
`deniedMcpServers` in the same `settings.json`. It does this for every server
whose definition token-max can't delete: a claude.ai connector, a plugin's
server, a project's `.mcp.json` server, and one from `managed-mcp.json` or
`--mcp-config`. These are by-hand steps today: "disconnect it at claude.ai",
which also removes the connector on the web, and "uninstall the plugin",
which also drops its skills. A server whose name can't be matched reliably
keeps its by-hand step. Codex and the removable `~/.claude.json` entries
don't change.

## Plan

Decided 2026-11-23 (maintainer, /quest-plan over a scan of eight
cost tips from other people's posts). The tips were:

- Haiku's 100k context cliff
- effort-off and Luna-max benchmarks
- Gemini's 15-minute cache
- Vertex cache breaks
- `/model` and `opusplan` switches
- claude.ai connectors busting the cache when `ENABLE_TOOL_SEARCH=false`
- plan-mode compaction billing on Opus since Claude Code 2.1.326

Only the connector tip pointed at a gap in token-max, and that gap was this
by-hand step, not the cache bust.

Decision prompts, chosen options marked ✅:

- From the scan: ✅ this cleanup edit, plus a note in the Antigravity quest
  in [kixel-ai/token-max#206](https://github.com/kixel-ai/token-max/pull/206)
  about Gemini's cache lifetime (rejected: also an `opusplan` cost line; drop
  everything). The decisions on the other six tips are in that PR's body.
- Scope: ✅ all four by-hand kinds (rejected: connectors only).

Recommendations:

- This stays cleanup that saves nothing, like the removal question in
  `docs/reference.md` "Removing MCP servers". The `permissions.deny`
  turn-off already keeps the tools out of context. The denylist also stops
  Claude Code from connecting the server at all.
- Write `{"serverName": ...}` entries, appending to any existing array.
  `deniedMcpServers` is honored from any settings file
  ([settings reference](https://code.claude.com/docs/en/settings-reference#deniedmcpservers),
  checked 2026-11-23).
- The tool prefix isn't always the name the denylist matches. A connector's
  tools are `mcp__claude_ai_<Name>`, but its entry is its display name, such
  as `"claude.ai Slack"`, and `_` in the prefix may stand for a space. Take
  the name from a transcript or config record that spells it out, and
  verify each source's form against a real Claude Code before relying on
  it. Where only the prefix is known and the mapping is ambiguous, keep the
  by-hand step.
- Gate on the Claude Code version. Since 2.1.330 a name entry hides only
  the named connector. Before that, it also hid other connectors at the same
  URL, so on older versions keep the by-hand step for connectors. Find the
  version where name entries began to match plugin servers too.
- `disableClaudeAiConnectors: true` turns off every connector, including
  ones added later. That is a different choice from cleaning up servers that
  were never used, so leave it out.
- Update `src/mcp.rs`'s module comment and the "Removing MCP servers"
  section of `docs/reference.md`.
