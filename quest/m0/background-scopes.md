# [M] Project-scoped background controls

## Goal

Claude prompt suggestions, recaps and goal check-ins use the effective control
for each recorded project. Reports name its owner and give applicable
recommendations without treating unresolved project state as a profile default.

## Plan

Recommended during /quest-complete on 2026-11-08 after
[kixelated/token-max#23](https://github.com/kixelated/token-max/pull/23).
The maintainer authorized "do anything you recommend". ✅ Plan this in m0
(rejected: keep user/managed-only discovery; defer beyond first release),
because project and local settings can override the control the report advises
changing. The PR introduced background recommendations without resolving project/local overrides; its implementation PR should list that cause.

- Reuse the existing project/local ownership discovery and background control
  resolver instead of adding a parallel scope implementation. Verify current
  Claude scope, environment, trust and embedding-host precedence, including
  subdirectories and worktrees, before extending the shared resolver.
- Resolve controls at recorded working directories; preserve distinct owners
  for histories spanning projects and attribute only their applicable requests.
  Retain user, managed and active-profile environment ownership. Current files
  do not prove historical state. Missing directories or unresolved launch state
  stay explicit and advisory rather than inventing a current value.
- Carry ownership and coverage through existing background estimates, text and
  JSON reports, settings snippets and the SSH metadata route. Never edit the
  discovered files or expose their contents beyond allowlisted control fields.
- Cover overriding and conflicting synthetic scopes, multiple projects, absent
  working directories, managed/environment precedence and SSH reporting.

Source checked 2026-11-08:
[Claude settings scope and precedence](https://code.claude.com/docs/en/settings).
