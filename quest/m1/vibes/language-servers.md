# [M] Suggest code intelligence plugins in vibes

## Goal

When a Claude Code profile's recorded projects use a typed language with an
official code intelligence plugin, such as Rust or TypeScript, and that
plugin isn't installed, `token-max vibes` suggests installing it. The card
cites Anthropic's
[costs page](https://code.claude.com/docs/en/costs#install-code-intelligence-plugins-for-typed-languages):
symbol lookup replaces grep-and-read, and type errors surface after each
edit. It names the projects and the share of sessions in each language.

## Plan

Decided 2026-11-23 (maintainer, /quest-plan after a survey of Claude Code
2.1.322 and codex-cli 0.167.1 settings worth turning on): ✅ a code
intelligence card (rejected: `includeGitInstructions: false`, right only for
users with their own git workflow, which token-max can't detect; spend caps
such as `workflowSizeGuideline: small` and Codex's
`goals.max_goal_token_budget`, which matter only once they trigger; Codex code
mode, which the model catalog already turns on for every OpenAI model, T3's
included).

Recommendations:

- Detect a language from marker files at recorded working directories, such
  as `Cargo.toml` or `tsconfig.json`, and map it to the plugins
  [code-intelligence](https://code.claude.com/docs/en/plugins/code-intelligence)
  lists. Fire once per missing plugin.
- Offer the edit only if a documented settings key installs the plugin, such
  as `enabledPlugins` with its marketplace ID; otherwise give the install
  command by hand and ask nothing. Say that the language server binary must
  be on `PATH`, and check whether it is.
- Its effect becomes measurable once installed: fewer reads per session. That
  is a possible later `doctor` line, not this quest.
