# [M] Project-scoped background controls

## Goal

Claude prompt suggestions, recaps and goal check-ins, and Codex automatic
recaps, use the effective control for each recorded project. Reports name its
owner and give applicable recommendations without treating unresolved project
state as a profile default.

## Plan

Recommended during /quest-complete on 2026-11-08 after
[kixelated/token-max#23](https://github.com/kixelated/token-max/pull/23).
The maintainer authorized "do anything you recommend". ✅ Plan this in m0
(rejected: keep user/managed-only discovery; defer beyond first release),
because project and local settings can override the control the report advises
changing. The implementation PR for this quest should list the
[background-generation PR](https://github.com/kixelated/token-max/pull/23)
under `Caused by`: it introduced recommendations without resolving
project/local overrides for either harness.

- Reuse the existing project/local ownership discovery and background control
  resolver instead of adding a parallel scope implementation. Verify current
  Claude and Codex scope, trust and launch precedence, including subdirectories
  and worktrees, before extending their shared resolvers. Codex's
  `tui.auto_recap` follows supported trusted project config layers; unknown
  trust or session selection must not become an assumed effective value.
- Resolve controls at recorded working directories; preserve distinct owners
  for histories spanning projects and attribute only their applicable requests.
  Include Claude project/local `env` blocks as control owners alongside user,
  managed and active-profile environment sources. Current files do not prove
  historical state. Missing directories or unresolved launch state stay
  explicit and advisory rather than inventing a current value.
- Carry ownership and coverage through existing background estimates, text and
  JSON reports, settings snippets and the SSH metadata route. Never edit the
  discovered files or expose their contents beyond allowlisted control fields.
- Cover overriding and conflicting synthetic scopes, multiple projects, absent
  working directories, project/local environment overrides, Codex recap
  overrides and trust, managed/environment precedence and SSH reporting.

Sources checked 2026-11-08:
[Claude settings scope and precedence](https://code.claude.com/docs/en/settings),
[Codex config layers](https://learn.chatgpt.com/docs/config-file/config-basic),
and [Codex recap setting](https://learn.chatgpt.com/docs/config-file/config-reference).
