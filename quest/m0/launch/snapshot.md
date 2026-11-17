# [M] Retake the results snapshot across every host

## Goal

The README's top, the max.kixel.ai page and `docs/results.md` show the
maintainer's real usage over the last 30 days summed across all four hosts,
kixtop, desktop, nixos and fedora, instead of desktop alone, with the
figures the extrapolation and billing quests produce. Like T3 Code's usage
view, it opens with three totals: the API cost, the session count, and the
potential savings.

## Plan

Decided 2026-11-17 (maintainer, /quest-run: "please update the example
savings with my actual usage from all hosts. t3code says over the last 30
days: $60,391.31, 23,285 sessions · API estimate"):

- ✅ take it after the extrapolation and billing quests land, in one pass
  with their math (rejected: now, and again later).
- ✅ the last 30 days on every host (rejected: all history, whose span
  differs per host).
- ✅ lead with the totals, as T3 Code's usage view does: the total API cost,
  the number of sessions, and the potential savings across all four hosts
  ("please do like T3Code and list out the total API cost, sessions, and
  potential savings").
- Sum the hosts as they are, as the hosts epic in
  [kixel-ai/token-max#151](https://github.com/kixel-ai/token-max/pull/151)
  decided for its total; until that total exists, add the four reports by
  hand and say so in `docs/results.md`.

Recommended: doctor's header line already gives requests and API cost but
no session count, so add sessions there (one per Claude Code session and
Codex rollout, subagents counted with their parent), and read all three
totals from doctor rather than counting by hand. Then run the same main commit on each host with `nix run`, as the
desktop snapshot did, with `--since` set 30 days back; check the summed
spend against T3 Code's $60,391.31 over the same 30 days and explain any gap
in `docs/results.md`; keep private skill, MCP server and path names out, and
redeploy the site from `site/` once it merges (wrangler is logged in on
fedora).

## Required

- [Extrapolate over unpriced requests](/quest/m0/launch/extrapolate.md) - the `~` figures and margins the page leads with
- [Ask whether a Claude profile is on a plan or API billing](/quest/m0/launch/ask-billing.md) - the main-agent TTL priced against how each profile pays
