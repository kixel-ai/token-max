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

token-max doctor: 7 changes to review, biggest saving first. Nothing is written until you confirm the list at the end.

[1/4] WebSearch tool: on -> off
  Recommended (Measured): saves $206/mo, 39% of this host's spend.
  Never used; it cost $99 over the window.
  In ~/.claude/settings.json:
    "permissions": {"deny": ["WebSearch"]} (added to the list)
Accept? [y]es, [n]o, [q]uit: y
  Accepted so far: about $206/mo, 39% of this host's spend.

[2/4] WebFetch tool: on -> off
  Recommended (Measured): saves $52/mo, 10% of this host's spend.
  Never used; it cost $25 over the window.
  In ~/.claude/settings.json:
    "permissions": {"deny": ["WebFetch"]} (added to the list)
Accept? [y]es, [n]o, [q]uit: n

[3/4] Bash timeout cap: 10m -> 4m
  Recommended (Measured): its saving was not measured.
  Measured on 504 requests over 14 days. A wait that long outlasts the 5m cache. No foreground Bash wait outlasted the cache. Commands past the cap keep running in the background; what checking on them costs is not measured.
  In ~/.claude/settings.json:
    "env": {"BASH_MAX_TIMEOUT_MS": "240000"}
Accept? [y]es, [n]o, [q]uit: y
  Accepted so far: about $206/mo, 39% of this host's spend, plus 1 change whose saving was not measured.

[4/4] 4 smaller changes, each saving under $5/mo, together: about $9.41/mo, 2% of this host's spend; overlapping changes may save less.
  NotebookEdit tool: on -> off, saves $3.57/mo, 1% of this host's spend (recommended, Measured)
  EnterWorktree tool: on -> off, saves $3.17/mo, 1% of this host's spend (recommended, Measured)
  CronCreate tool: on -> off, saves $2.66/mo, <1% of this host's spend (recommended, Measured)
  Auto memory: on -> off, saves $0/mo, 0% of this host's spend (worth considering, Opinion)
Accept all 4? [y]es, [n]o, [o]ne by one, [q]uit: y
  Accepted so far: about $215/mo, 40% of this host's spend, plus 1 change whose saving was not measured; overlapping changes may save less.

token-max
Dollars are public API prices, not an invoice: the closest public measure of what a request uses of a Claude or ChatGPT plan's limits, and what Claude's usage credits bill past them. Nothing was changed.

Read 504 requests from 2026-11-01 to 2026-11-15 (14 days), at least $256 at API prices:
  claude ~/.claude

6 changes recommended and 1 worth considering.
...

ACCEPTED  biggest saving first
change   WebSearch tool                         on -> off       saves   $206/mo   39%  Measured  "permissions": {"deny": ["WebSearch"]} (added to the list) in ~/.claude/settings.json
change   NotebookEdit tool                      on -> off       saves  $3.57/mo    1%  Measured  "permissions": {"deny": ["NotebookEdit"]} (added to the list) in ~/.claude/settings.json
change   EnterWorktree tool                     on -> off       saves  $3.17/mo    1%  Measured  "permissions": {"deny": ["EnterWorktree"]} (added to the list) in ~/.claude/settings.json
change   CronCreate tool                        on -> off       saves  $2.66/mo   <1%  Measured  "permissions": {"deny": ["CronCreate"]} (added to the list) in ~/.claude/settings.json
consider Auto memory                            on -> off       saves     $0/mo    0%  Opinion   "autoMemoryEnabled": false in ~/.claude/settings.json
change   Bash timeout cap                       10m -> 4m       saving not measured        Measured  "env": {"BASH_MAX_TIMEOUT_MS": "240000"} in ~/.claude/settings.json
         Accepted: about $215/mo, 40% of this host's spend, plus 1 change whose saving was not measured; overlapping changes may save less.

NOT ACCEPTED
change   WebFetch tool                          on -> off       saves    $52/mo   10%  Measured  "permissions": {"deny": ["WebFetch"]} (added to the list) in ~/.claude/settings.json

EVERYTHING ELSE  biggest cost first
info     Main-session effort                    medium          costs  at least $256  100%  Official  ~/.claude/settings.json:modelSettings.claude-opus-5-5.effortLevel (the model's default)
info     System prompt and core tools                           costs     $6.89    3%  Measured  no setting
keep     Main-agent prompt cache TTL            5m              saves   $139/mo   26%  Measured  ~/.claude/settings.json:promptCacheTtl
...
info     Fable weekly use                                                              Official  /model, or model in settings.json
...

Write the 6 accepted changes to ~/.claude/settings.json? [y]es, [n]o: y

Wrote 6 edits to ~/.claude/settings.json.
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

Transcripts are only read. On a terminal, `doctor` walks through each change it
can make, biggest saving first, with why it makes sense and the exact edit to
your own `settings.json` or `config.toml`. Your answers only select: it then
lists the accepted changes and writes them only after one more yes. A change
something else would override, such as a shell variable or a project's
settings, is listed to make by hand.
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
- Savings on different lines do not add up, except in the walk's running
  total of the changes you accept and the line under the headline that sums what
  your settings save against the defaults, each of which says that overlapping
  settings may save less.

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
