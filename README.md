# token-max

Most advice about configuring coding agents is asserted, not measured.
token-max reads this machine's Claude Code and Codex transcripts and shows which
of your settings use up more of your Claude or ChatGPT plan's limits than they
return, with the evidence for each one. Then it offers to change them, one at a
time.

No provider publishes a plan's limits in tokens or dollars, so token-max
replays your sessions at public API prices, the closest public measure of what
a request uses of those limits. On an API key, the same dollars are close to
your bill.

Run it once without installing anything, or install it:

```sh
curl -LsSf https://max.kixel.ai/run.sh | sh
curl -LsSf https://max.kixel.ai/install.sh | sh
```

On Windows, in PowerShell:

```powershell
irm https://max.kixel.ai/run.ps1 | iex
irm https://max.kixel.ai/install.ps1 | iex
```

These work from the first release on; until then, see [Install](#install).

```text
$ token-max doctor
token-max
Dollars are public API prices, not an invoice: the closest public measure of what a request uses of a Claude or ChatGPT plan's limits, and what Claude's usage credits bill past them. Nothing was changed.

Read 10,800 requests from 2026-11-01 to 2026-11-15 (14 days), $466 at API prices:
  claude ~/.claude
  codex ~/.codex
3,600 Codex requests are priced at an assumed service tier, which their records leave unclear; --details shows each tier's cost.

82 changes recommended.
Savings are per 30 days at the window's rate, and costs are totals over the window; each percent is of this host's spend over the window, $466. A tool, skill or MCP server that was never used is recommended off, and one that cost more than $1 per use (--max-cost-per-use) is worth considering off.
Background estimates apply current files to history; historical settings and unrecorded launch overrides remain unknown.
Some feature costs are scaled from their priced requests; --details shows each line's coverage.

CHANGES  biggest saving first
change   Main-agent prompt cache TTL            unset -> 5m       saves up to  $146/mo  15%  Measured  [~/.claude]  "promptCacheTtl": "5m" in ~/.claude/settings.json
change   MCP server docs                        on -> off         saves         $17/mo   2%  Measured  [~/.claude]  "permissions": {"deny": ["mcp__docs"]} (added to the list) in ~/.claude/settings.json
change   EnterWorktree tool                     on -> off         saves       $3.99/mo  <1%  Measured  [~/.claude]  "permissions": {"deny": ["EnterWorktree"]} (added to the list) in ~/.claude/settings.json
change   NotebookEdit tool                      on -> off         saves       $3.99/mo  <1%  Measured  [~/.claude]  "permissions": {"deny": ["NotebookEdit"]} (added to the list) in ~/.claude/settings.json
...
change   Bash timeout cap                       55m -> 4m         saving      not measured       Measured  [~/.claude]  "env": {"BASH_MAX_TIMEOUT_MS": "240000"} in ~/.claude/settings.json

EVERYTHING ELSE  biggest cost first
info     Main-session effort                    medium            costs           $363  78%  Official  [~/.claude]  ~/.claude/settings.json:modelSettings.claude-opus-5-5.effortLevel (the model's default)
info     Main-session effort                    xhigh             costs           $103  22%  Official  [~/.codex]   ~/.codex/config.toml:model_reasoning_effort
keep     MCP server t3-code                     on                costs            $50  11%  Measured  [~/.claude]  ~/.claude/settings.json:permissions.deny
info     System prompt and core tools                             costs            $25   5%  Measured               no setting
keep     WebSearch tool                         on                costs          $1.91  <1%  Measured  [~/.claude]  ~/.claude/settings.json:permissions.deny
keep     WebFetch tool                          on                costs          $1.91  <1%  Measured  [~/.claude]  ~/.claude/settings.json:permissions.deny
...
keep     Skill skill-00                         on                costs          $0.03  <1%  Measured  [~/.claude]  ~/.claude/settings.json:skillOverrides.skill-00
...
keep     Fast mode                              off                                          Opinion   [~/.codex]   /etc/codex/config.toml:service_tier (documented default)
keep     Auto memory                            off                                          Opinion   [~/.claude]  ~/.claude/settings.json:autoMemoryEnabled
...
unknown  Idle compaction                        on                                           Measured  [~/.claude]  ~/.claude/settings.json:idleCompaction
...
unknown  Session recaps                         unknown                                      Measured  [~/.claude]  ~/.claude/settings.json:awaySummaryEnabled (default)
...
info     Fable weekly use                                                                    Official  [~/.claude]  /model, or model in settings.json
A feature's saving is what it cost, assuming nothing else changes when it is removed; cache writes and compaction can shift.

Prices: cached from test/fixtures/prices.json (2026-11-15).
Each line compares one setting with everything else unchanged; savings do not add up.
--details shows the evidence, assumptions and sources behind each line.

token-max doctor: 82 changes to review. Each is written only after a yes.

[1/82] Main-agent prompt cache TTL: unset -> 5m (Measured)
  In ~/.claude/settings.json:
    "promptCacheTtl": "5m"
Apply? [y]es, [n]o, [q]uit: y
...
```

That excerpt is from synthetic usage with synthetic prices, not a real host.

## Install

From the first release on, each GitHub release has prebuilt binaries for
Linux (x86_64 and arm64, statically linked), macOS (Apple silicon and Intel)
and Windows (x86_64). `install.sh` and `install.ps1` above are the release's
installers, which put `token-max` in `~/.local/bin` by default. `run.sh` and
`run.ps1` download the binary to a cache instead, check it against the
release's checksum, and run `token-max doctor`; the
[reference](https://github.com/kixel-ai/token-max/blob/main/docs/reference.md#running-it)
says how to pass it options.

`cargo binstall token-max` downloads the same binary, and
`cargo install --locked token-max` builds it. On Linux and Apple silicon,
Nix builds it from source:

```sh
nix run github:kixel-ai/token-max -- doctor
```

Until that release, install from GitHub with a Rust toolchain:

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
Piped or redirected, it prints the report, each change naming its edit, and
edits nothing.
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
| Main-session effort | What each model and effort cost, beside the provider's guidance; `max` recommended down to `xhigh`, `low` worth considering raising |
| Auto memory | What loading, reading and writing it cost, and how often it was used; worth considering off |
| Prompt suggestions, recaps, check-ins | What each background feature cost, where it can be estimated |
| Bash timeout cap | Whether long foreground waits let the cache expire |
| Keepalive pings | Whether pinging idle sessions would beat the cache misses |
| Subagent types | What each cost, which model and effort it ran on, and the same tokens on the cheapest current model, beside Anthropic's model guide |
| Fable weekly use | Each week's Fable spend beside Anthropic's rule that, on Max and Ultra plans, Fable can use up to half the weekly limit |

## Reading the report

Every setting and feature gets one line: its verdict, name and value, what it
saves or costs and that figure's share of your spend, the kind of claim it
rests on, and the setting that changes it. The changes come first, biggest
saving first; everything else follows, biggest cost first. `--details` adds
each line's evidence under it.

Each line names its kind of claim, so a preference never reads as a
measurement:

- **Measured**: replayed from your own sessions at public API prices. Its
  evidence says what the replay held constant and what it could not measure.
- **Official**: the provider's own guidance, such as Claude Code's suggestion
  to keep a CLAUDE.md under 200 lines, cited beside what the file cost.
- **Opinion**: the maintainer's preference, such as fast mode and auto memory
  off, beside whatever was measured.

On a terminal a colored dot gives the verdict: red for a recommended
change, yellow for one worth considering, green for a setting
already at its recommended value, and a dim hollow dot where there is no
verdict. Without color (`--plain`, `NO_COLOR` or redirected output), as above,
a word replaces the dot.

- Dollars are public API prices, not an invoice. No provider says how a
  plan's limits weigh tokens, so on a Claude or ChatGPT plan they are the
  closest public measure of what a request uses of them, and past a Claude
  plan's limits, usage credits bill these same prices. Where a provider
  documents that a plan counts something differently, the line says so under
  "Official:" in its evidence, as the
  [reference](https://github.com/kixel-ai/token-max/blob/main/docs/reference.md#plans-and-api-prices)
  lists. token-max never reads which plan you are on, so these notes say "on a
  Claude plan" rather than naming yours.
- Comparisons between setting values are per 30 days; what a feature cost,
  and how often it was used, are totals over the whole window. Each percent
  is of what every request on this host cost over the window.
- Turning a feature off **saves** what it cost, assuming nothing else
  changes.
- A **saves** figure comes from replaying your own sessions under the other
  value, assuming the sessions would otherwise have gone the same way. A
  smaller compaction window, for example, can also lose detail the replay
  cannot see, and its evidence says so.
- A tool, skill or MCP server that was never used is recommended off. One that
  cost more than $1 per use (`--max-cost-per-use`) is worth considering off.
  You know what a use was worth; the report only knows what it cost.
- A line without enough evidence for a verdict says why in its evidence. A
  cost that cannot be measured is shown as unknown, never as zero.
- Savings on different lines do not add up.

## More

- [Reference](https://github.com/kixel-ai/token-max/blob/main/docs/reference.md):
  every flag, report section and check, including `--ssh`, T3 Code
  and the `token-max sessions` export of each session's cost, branches and
  repositories, from this host or over SSH.
- [Replay semantics](https://github.com/kixel-ai/token-max/blob/main/docs/semantics.md):
  the accounting, assumptions, sources and limits.
- [First-release plan](https://github.com/kixel-ai/token-max/blob/main/quest/m0/README.md):
  the checks still to come.
- To work on it, run `nix develop`, then `just check`. See
  [CONTRIBUTING.md](https://github.com/kixel-ai/token-max/blob/main/CONTRIBUTING.md).
