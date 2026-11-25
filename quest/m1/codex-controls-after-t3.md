# [S] Resolve Codex's latest-session controls after T3 attribution

## Goal

A native Codex profile's memory and main-session default are read at its own
latest main-session request's directory, never at one a T3 Codex thread
sharing `~/.codex` ran in, nor one T3 could not settle. Claude's controls
already work this way (`discover_latest_claude_controls`, from
[#217](https://github.com/kixel-ai/token-max/pull/217)); Codex's
`discover_codex_controls` still runs before `t3::enrich` and takes every
request naming the profile.

## Plan

Decided 2026-11-25 (maintainer, /quest-run): ✅ keep this quest (rejected:
drop it, keeping only `advisor-accounting.md`).

Recommendation: move Codex's latest-directory pass after `t3::enrich` with
the same `owned_by` filter, sharing one helper that returns the settled
latest directory and its scopes for both harnesses and `discover_inbound`.
When it leaves a profile no settled request, say so where the main-session
default is unknown, rather than that no request recorded a model.
