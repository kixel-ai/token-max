---
name: token-audit
description: Audit Claude Code and Codex transcripts for wasted tokens and offer fixes. Use when the user asks where a repository's or host's tokens go, why sessions cost so much, or to audit cache busts, bloated or repeated tool calls, unused always-loaded context, or context sessions keep rediscovering. Not for settings such as cache TTL, models, effort or MCP servers; token-max doctor covers those.
compatibility: Requires token-max and jq.
allowed-tools: Bash(token-max skill audit) Bash(token-max audit *)
---

Run `token-max skill audit` and follow what it prints. If `token-max` is not installed, or is too old to have `token-max skill audit`, run `npx @kixel/crabx token-max skill audit` instead and prefix every `token-max` command it names the same way, or point the user to https://max.kixel.me to install it.
