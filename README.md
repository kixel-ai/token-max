# token-max

Most advice about configuring coding agents is asserted, not measured.
token-max reads this machine's Claude Code and Codex transcripts, replays them
at public API prices, and shows which of your settings cost more than they
return, with the evidence for each one.

```text
token-max
Estimated with public API prices, not your subscription bill. Nothing was changed.

Read 2,000 requests from 2026-11-01 to 2026-12-01 (30 days):
  claude ~/.claude

2 setting changes recommended.
Setting comparisons are per 30 days at this window's rate; what features and context cost, and how often they were used, are totals over the whole window. A tool, skill or MCP server that was never used is recommended off, and one that cost more than $1 per use (--max-cost-per-use) is worth considering off.

Change
  Main-agent prompt cache TTL: 5m -> 1h. Saves $150. Set in ~/.claude/settings.json:promptCacheTtl.
  Artifact tool: on -> off. Costs $90; never used.

Consider turning off
  Skill example: on. Costs $12; the model used it 2 times ($6 each). Hide it from the model: "skillOverrides": {"example": "user-invocable-only"}.

Keep
  Fast mode: off.
  Agent tool: on. Costs $40; used 310 times ($0.13 each).

Not enough evidence
  Idle compaction: on. No idle gaps could be compared with it on and off.

No setting turns these off
  ~/.claude/CLAUDE.md: costs $5. Shorten it, or move parts into path-scoped rules or skills.
  System prompt, core tools such as Bash and Read, and the skill and subagent lists: $210.

To turn these off, merge this into ~/.claude/settings.json, adding to any lists already there:
  {
    "enableArtifact": false
  }
```

That excerpt is from synthetic usage with synthetic prices, not a real host.

## Install

It needs a Rust toolchain. Until the first crates.io release, install from
GitHub:

```sh
cargo install --locked --git https://github.com/kixelated/token-max
```

## Run

```sh
token-max                     # every Claude Code and Codex profile on this host
token-max --since 2026-11-01  # only sessions from that date on
token-max --details           # the evidence behind every line
token-max --help
```

The first run downloads public prices from `https://models.dev/api.json` and
caches them; `--offline` reuses the cache. That is the only network request,
unless you name other machines with `--ssh`.

Transcripts and settings are only read. Nothing is edited: the report ends
with a settings snippet to merge by hand. No prompt, reply, tool argument or
tool output text is kept or printed.

## What it checks

| Setting or feature | What the report says |
| --- | --- |
| Prompt cache TTL (5m or 1h) | Which lifetime would have cost less on your sessions |
| Auto-compact window | Whether a different window would have cost less, summaries included |
| Idle compaction | Whether compacting sessions you walked away from paid for itself |
| Tools, skills and MCP servers | What each cost to send with every request, and how often it was used |
| Instruction files | What each CLAUDE.md or AGENTS.md cost, beside the provider's size guidance |
| Fast mode | What the premium cost; recommended off |
| Auto memory | What loading, reading and writing it cost, and how often it was used; worth considering off |
| Prompt suggestions, recaps, check-ins | What each background feature cost, where it can be estimated |
| Bash timeout cap | Whether long foreground waits let the cache expire |
| Keepalive pings | Whether pinging idle sessions would beat the cache misses |

## Reading the report

Every setting and feature gets one line under **Change**, **Consider turning
off**, **Keep** or **Not enough evidence**. On a terminal, color repeats the
verdict: red for a recommended change (exit 1), yellow for an advisory or
missing evidence (exit 0), green for a setting already at its recommended value.

- Dollars are at public API prices, not your subscription bill. Comparisons
  between setting values are per 30 days; what a feature cost, and how often
  it was used, are totals over the whole window.
- A **Saves** figure comes from replaying your own sessions under the other
  value, assuming the sessions would otherwise have gone the same way. A
  smaller compaction window, for example, can also lose detail the replay
  cannot see.
- A tool, skill or MCP server that was never used is recommended off. One that
  cost more than $1 per use (`--max-cost-per-use`) is worth considering off.
  You know what a use was worth; the report only knows what it cost.
- Fast mode off and auto memory off are the maintainer's preferences, not
  measurements.
- **Not enough evidence** says why in one sentence. A cost that cannot be
  measured is shown as unknown, never as zero.
- Savings on different lines do not add up.

## More

- [Reference](https://github.com/kixelated/token-max/blob/main/docs/reference.md):
  every flag, report section and check, including `--json`, `--ssh`, T3 Code
  and the `token-max sessions` export.
- [Replay semantics](https://github.com/kixelated/token-max/blob/main/docs/semantics.md):
  the accounting, assumptions, sources and limits.
- [First-release plan](https://github.com/kixelated/token-max/blob/main/quest/m0/README.md):
  the checks still to come.
- To work on it, run `nix develop`, then `just check`. See
  [CONTRIBUTING.md](https://github.com/kixelated/token-max/blob/main/CONTRIBUTING.md).
