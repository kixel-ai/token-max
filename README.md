# token-max

Most advice about configuring coding agents is asserted, not measured.
token-max reads this machine's Claude Code and Codex transcripts, replays them
at public API prices, and shows which of your settings cost more than they
return, with the evidence for each one. Then it offers to change them, one at a
time.

```text
$ token-max doctor
token-max
Estimated with public API prices, not your subscription bill. Nothing was changed.

Read 2,000 requests from 2026-11-01 to 2026-12-01 (30 days):
  claude ~/.claude

2 setting changes recommended.
Savings are per 30 days at this window's rate; costs and uses are totals over the whole window. A tool, skill or MCP server that was never used is recommended off, and one that cost more than $1 per use (--max-cost-per-use) is worth considering off.

MEASURED  replayed from your own sessions
change   Main-agent prompt cache TTL   5m -> 1h   saves $150
         Replayed at 5m and 1h, assuming the same sessions; 1h costs less. Set in ~/.claude/settings.json:promptCacheTtl.
change   Artifact tool                 on -> off  costs  $90
         Never used. Turn off: "enableArtifact": false.
consider Skill example                 on         costs  $12
         The model used it 2 times, $6 each. Hide it from the model: "skillOverrides": {"example": "user-invocable-only"}.
keep     Agent tool                    on         costs  $40
         Used 310 times, $0.13 each. Turn off: "permissions": {"deny": ["Agent"]}.
unknown  Idle compaction               on
         Not enough evidence: no idle gaps could be compared with it on and off. Set in ~/.claude/settings.json:idleCompaction.
info     System prompt and core tools             costs $210
         Includes core tools such as Bash and Read, and the skill and subagent lists. No setting turns these off.

OFFICIAL  the provider's own guidance
consider ~/.claude/CLAUDE.md  412 lines  costs $5
         Claude Code suggests under 200 lines per CLAUDE.md; length alone is not a quality score. Shorten it, or move parts into path-scoped rules or skills. Imports still load together.
Sources: https://code.claude.com/docs/en/memory#write-effective-instructions

OPINION  the maintainer's preference
consider Auto memory  on   costs $12
         Opinion: notes the agent writes for itself skip review and encourage slop, so keep durable guidance in reviewed, checked-in CLAUDE.md or AGENTS.md. Measured: used 3 times (1 read, 2 writes). Set "autoMemoryEnabled": false in ~/.claude/settings.json.
keep     Fast mode    off
         Opinion: standard speed is enough, and fast mode bills a premium for the same work. Set in ~/.claude/settings.json:fastMode.

token-max doctor: 4 changes to review. Each is written only after a yes.

[1/4] Main-agent prompt cache TTL: 5m -> 1h (Measured)
  In ~/.claude/settings.json:
    "promptCacheTtl": "1h"
Apply? [y]es, [n]o, [q]uit: y
...
```

That excerpt is from synthetic usage with synthetic prices, not a real host.

## Install

It needs a Rust toolchain. Until the first crates.io release, install from
GitHub:

```sh
cargo install --locked --git https://github.com/kixel-ai/token-max
```

## Run

```sh
token-max doctor                     # every Claude Code and Codex profile on this host
token-max doctor --since 2026-11-01  # only sessions from that date on
token-max doctor --details           # the evidence behind every line
token-max doctor --help
```

The first run downloads public prices from `https://models.dev/api.json` and
caches them; `--offline` reuses the cache. That is the only network request,
unless you name other machines with `--ssh`.

Transcripts are only read. On a terminal, `doctor` shows each change it can
make as the exact edit to your own `settings.json` or `config.toml`, and writes
the file only after you answer yes. A change something else would override,
such as a shell variable or a project's settings, is listed to make by hand.
Piped or redirected, it prints the report and the changes and edits nothing.
No prompt, reply, tool argument or tool output text is kept or printed.

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
| Subagent types | What each cost, which model and effort it ran on, and the same tokens on the cheapest current model |

## Reading the report

The report has one section for each kind of claim, so a preference never
reads as a measurement:

- **Measured**: replayed from your own sessions at public API prices. Each
  line says what the replay held constant and what it could not measure.
- **Official**: the provider's own guidance, such as Claude Code's suggestion
  to keep a CLAUDE.md under 200 lines, cited beside what the file cost.
- **Opinion**: the maintainer's preference, such as fast mode and auto memory
  off, beside whatever was measured.

Every setting and feature gets two lines: its name, value and cost, then its
evidence. On a terminal a colored dot gives the verdict: red for a recommended
change, yellow for one worth considering, green for a setting
already at its recommended value, and a dim hollow dot where there is no
verdict. Without color (`--plain`, `NO_COLOR` or redirected output), as above,
a word replaces the dot. Red lines come first in each section.

- Dollars are at public API prices, not your subscription bill. Comparisons
  between setting values are per 30 days; what a feature cost, and how often
  it was used, are totals over the whole window.
- A **saves** figure comes from replaying your own sessions under the other
  value, assuming the sessions would otherwise have gone the same way. A
  smaller compaction window, for example, can also lose detail the replay
  cannot see, and its line says so.
- A tool, skill or MCP server that was never used is recommended off. One that
  cost more than $1 per use (`--max-cost-per-use`) is worth considering off.
  You know what a use was worth; the report only knows what it cost.
- A line without enough evidence for a verdict says why. A cost that cannot
  be measured is shown as unknown, never as zero.
- Savings on different lines do not add up.

## More

- [Reference](https://github.com/kixel-ai/token-max/blob/main/docs/reference.md):
  every flag, report section and check, including `--ssh`, T3 Code
  and the `token-max sessions` export.
- [Replay semantics](https://github.com/kixel-ai/token-max/blob/main/docs/semantics.md):
  the accounting, assumptions, sources and limits.
- [First-release plan](https://github.com/kixel-ai/token-max/blob/main/quest/m0/README.md):
  the checks still to come.
- To work on it, run `nix develop`, then `just check`. See
  [CONTRIBUTING.md](https://github.com/kixel-ai/token-max/blob/main/CONTRIBUTING.md).
