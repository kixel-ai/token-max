# [M] Offer to remove unused MCP servers

## Goal

When the walk turns off a never-used MCP server, it also offers to remove
the server's configuration, and it offers to remove servers whose tools are
all denied already. Only the file that defines a server is edited, after the
confirm shows the diff; a server from a claude.ai connector or a plugin is
named with where to remove it by hand.

## Plan

Decided 2026-11-16 (maintainer, /quest-plan: "maybe offer to uninstall any
completely disabled/unused mcps"). Today doctor turns a never-used server
off with `permissions.deny`, which leaves it configured and launching.

- ✅ offer both: deny stays the change that saves the money, and the walk
  also asks whether to remove the configuration too (rejected: removal
  instead of deny; removal offered only for already-denied servers).
  Recommended: ask once for each group of never-used servers the walk
  offers (a `doctor::Group` of kind `McpServer` and reason `Unused`, whose
  card names every server), right after its yes, rather than once per
  server; a server saving $5/mo or more, or one taken one by one with `o`,
  is asked on its own.
- ✅ also offer to remove servers whose tools are all denied already, after
  the paid changes, as cleanup that saves no dollars (rejected: leave them).

Recommended: a Claude Code server lives in `~/.claude.json` (user and local
scope) or a project's `.mcp.json`, and a Codex server under `[mcp_servers.<id>]`
in `~/.codex/config.toml`. Claude Code rewrites `~/.claude.json` while it
runs, so check how an edit there survives a running session, or remove
through `claude mcp remove`, before choosing. Never edit a managed settings
file or T3's settings.
