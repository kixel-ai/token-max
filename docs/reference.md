# Reference

Every flag, report section and check. [Replay semantics](semantics.md) covers
the accounting underneath.

## Running it

`token-max doctor` prints the report, then, when stdin and stdout are both a
terminal, walks through its changes. Bare `token-max` prints the commands and
exits 2; options follow a command. Relative paths resolve from the working
directory. Reports go to stdout; diagnostics go to stderr; exit codes are as
described below. Ctrl-C cancels the running analysis: SIGINT or SIGTERM exits 2
with no success report.

`curl -LsSf https://max.kixel.ai/run.sh | sh` and, on Windows,
`irm https://max.kixel.ai/run.ps1 | iex` run `token-max doctor` from the
latest release without installing it. Each downloads the release archive for
this platform, refuses it unless it matches the release's `.sha256` file, and
keeps the binary in `token-max/run` under `$XDG_CACHE_HOME` or `~/.cache`,
beside the price cache. Later runs reuse it while it is still the latest
release; a newer one is downloaded beside it, and deleting the directory
reclaims the space. `run.sh` hands `doctor` the
terminal from `/dev/tty`, since its stdin is the piped script, so the changes
are still offered. Options go after `sh -s --`, as in
`curl -LsSf https://max.kixel.ai/run.sh | sh -s -- --since 2026-11-01`, or
in PowerShell,
`& ([scriptblock]::Create((irm https://max.kixel.ai/run.ps1))) --since 2026-11-01`.
`install.sh` and `install.ps1` redirect to the release's own installers.

## Applying changes

On a terminal, `doctor` first walks through each change it can make, after a
headline counting them, across every settings file, recommended and worth
considering alike, biggest monthly saving first. Each one shows its current
and offered value, whether it is recommended or worth considering, its kind of
claim, its saving per 30 days in dollars and as a percent of this host's spend
over the window (the base the report's percents use), what was measured, such
as the requests a replay covered or a tool's uses and what each cost, why the
change makes sense, and the exact edit, such as `"promptCacheTtl": "1h"` in
`~/.claude/settings.json`. A change whose saving was not measured, such as
raising an effort, comes after those saving $5/mo or more, one question each,
and says so. Changes saving under $5/mo each come last as one question, which
lists them with their savings and takes `y` for all, `n` for none or `o` to go
through them one at a time; a single such change is asked on its own.

Each question takes `y`, `n` or `q`; anything else asks again. An answer only
selects: after each yes, a running total sums the accepted changes' own
savings, in dollars and as a percent of this host's spend, and says that
overlapping changes may save less, since each saving holds every other setting
constant. An accepted change whose saving was not measured is counted beside
the total, not in it, and so is one that saves only when an unset default would
otherwise be the other value, such as an unset main-agent cache TTL, as an "up
to" figure. A saving priced from only part of its requests counts what was
priced, and the total says so. A `q`, or end of input, ends the walk and leaves the rest
unaccepted.

The report follows, its list grouped: the accepted changes first with the
total, then the ones not accepted, then the steps to take by hand, such as a
CLAUDE.md over Claude Code's size guidance or a change something else would
override, each saying why, then every other line, each group ranked as the
report ranks it. Then one question asks whether to write the accepted changes,
naming each file; nothing is written before that yes, end of input is a no,
and a no says nothing was written. Each file is then written once, with every
edit accepted for it. A change not accepted keeps its edit in the list, to
make by hand, which also suits a file another tool rewrites, such as a dotfiles
manager. Either answer exits 0.

Only a profile's own `settings.json` or Codex `config.toml` is edited,
including Claude's `env` block. An edit sets the setting that decides the
current value, as its line's `Set in` names it, so a change is applied only
when that owner is the profile's file, or the default no file sets. A line
that covers several profiles, such as native Claude Code and a T3 instance
sharing `~/.claude`, is one edit to the file they share. A tool, skill or MCP
line whose requests were copied into the histories of profiles with different
files, so any of them could have made them, is a step by hand naming each
file. A shell
variable, a project, local or managed settings file, or T3 that owns it makes
the change a step by hand naming that owner, and so does a Codex skill whose
`SKILL.md` path the catalog does not resolve. A tool, skill or MCP switch is
left by hand when a file that outranks the profile's for some recorded session,
such as a project's settings, a managed file or a named Codex profile, sets the
same switch; a Claude `permissions.deny` rule adds to every file's rules, so it
is always applied. Auto memory is left by hand when a named profile or a project
whose trust is unrecorded may set it, naming those files. Two lines that want
one setting at different values, in one file or two names a symlink makes one
file, leave it by hand too.

The edit keeps the file's other settings and their order. `settings.json` is
rewritten as indented JSON with the file's indentation, the form Claude Code
writes it in; `config.toml` keeps its comments and formatting. A list gains the
value once, and a Codex `[[skills.config]]` entry with the same `path` is
updated in place. Each file is read when the walk starts and again before
writing; if it changed in between, nothing is written and the run fails. A
file is replaced atomically with its owner, group and permission bits kept, and
its replacement is never readable by more people than the file was; a new file
is readable only by you. Other access controls, such as an ACL, are not copied.
The file is checked once more just before it is replaced. A symlink is written through to its target, which the summary names, and
profiles whose files resolve to one file share one write. A read-only target, or one
that cannot take the edit, such as a `permissions` that is not an object, makes
the change a step by hand naming the resolved path. Ctrl-C at any question,
the last included, exits 2 with no settings changed.

Without a terminal, `doctor` changes nothing: each change's line ends with its
edit and file, or its step by hand. A destination of `--ssh`
lists its changes the same way, to make on that host, and a run of `--ssh`
destinations alone only lists them.

## Profiles

Without directory flags, discovery includes `~/.claude`, `~/.codex`,
`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, and recognizable `~/.claude*` and `~/.codex*`
profiles. Codex includes archived sessions; Claude includes nested subagents.
T3 instances are included from `~/.t3/userdata`, an existing `~/.t3/dev`,
and `$T3CODE_HOME/userdata`. Repeatable `--claude-dir`, `--codex-dir` and
`--t3-dir` flags select only those roots across both harnesses. `--t3-dir`
takes a state directory containing `settings.json` and optionally `statev2.sqlite`. `--details` shows every discovered root's selection. A missing or
inaccessible explicitly selected root fails the run. Missing default roots do not.

The report gives each setting, tool, skill or MCP server one line per settings
file that owns it, combining every profile that file controls. Native Claude
Code and a T3 instance that read `~/.claude/settings.json` share its lines:
their requests, costs and uses are added together, each source's history
replayed on its own, and the line recommends the value with the lower total.
Claude's and Codex's files, each other home, a T3 instance's own settings, and
each `--ssh` host's files stay separate lines. Ownership is per setting: a T3
instance whose own setting decides fast mode, effort or the compaction window
gets its own line for that setting while sharing the native file's other
lines. A setting a shell variable, a project, local or managed file decides,
and every setting of a disabled T3 instance or one whose settings could not
be resolved, keeps a line per profile naming it. Two paths that resolve to one
file, such as a symlinked `settings.json`, are one file.

A request is counted once on each line: one copied into several profiles'
histories, or one T3 could not settle between instances, counts once on the
line those profiles share. When those profiles have different settings files,
each file's line for a setting counts it, and
a piece of context gets one line naming every file. A tool, skill or MCP
server's use counts on every line that shares a settings file with the
profiles that recorded it, so no line offers to turn off what one of its
files used.

## Prices

The first online run fetches only `https://models.dev/api.json`, caching validated
prices at `~/.cache/token-max/models.json` (or under `XDG_CACHE_HOME`).
Use `--offline` for that cache, `--price-cache FILE` to choose its location, or
`--prices-file FILE` for a local models.dev catalog with no network access.
Network failures fail the run; offline use is explicit. Cached prices include
their file timestamp, so an old catalog remains visible.

## Plans and API prices

token-max is written for people on a Claude or ChatGPT plan. No provider
publishes a plan's limits in tokens or dollars, so it measures with public API
prices, the closest public measure of what a request uses of those limits, and
the report's first line says so. It never reads which plan a profile is signed
in to, so a fact that differs by plan is stated conditionally, as in "on a
Claude plan".

What the providers document, checked 2026-11-14:

- Claude Code's [costs page](https://code.claude.com/docs/en/costs#why-usage-climbs-in-a-long-session)
  says a cached re-read draws plan usage at the cached token rate, and that
  cache misses, compaction, subagents and background requests all draw on it.
  OpenAI's [Codex pricing](https://learn.chatgpt.com/docs/pricing) says model,
  context, reasoning, tool use and caching all affect usage, and prices its
  credits per token with the API's split of input, cached input and output.
  Neither says how much of a limit a token is.
- Past a Claude plan's limits, [usage credits](https://support.claude.com/en/articles/11145838-use-claude-code-with-your-pro-or-max-plan)
  are billed at standard API rates, so there the dollars are the charge.
- Claude Code's `/usage` shows a Claude plan's own usage bars and a breakdown
  of what counted against them on this machine. token-max adds what other
  settings would have cost, replayed over the same sessions.

Where a provider documents a rule that API prices alone don't show, the line's
evidence under `--details` adds an `Official:` sentence before the setting that
changes it, and the list ends with the source:

| Line | On a plan |
| --- | --- |
| Main-agent prompt cache TTL, while unset | Claude Code [requests 1h](https://code.claude.com/docs/en/prompt-caching#which-ttl-each-request-gets) for the main conversation on a Claude plan within its included usage, and 5m once usage credits or an API key pay for it. Since token-max can't tell which applies, the line shows `unset` and names both. When the replay picks a winner, setting it is the cheaper choice either way, so the line recommends `unset -> 1h` (or `5m`) and `doctor` offers to set it; it shows the replay's gap as "saves up to", saved only while the other default would apply. |
| Claude fast mode | A Claude plan [pays fast mode only from usage credits](https://code.claude.com/docs/en/fast-mode#requirements), at its API rates, even with plan usage left. |
| Codex fast mode | A ChatGPT plan [counts Fast at 2.5x and Ultrafast at 8x](https://learn.chatgpt.com/docs/pricing) standard against included usage, and 2x and 6x on credits. The API premium priced is 2x for Fast on GPT-6 and GPT-5.6 and 6x for Ultrafast, so included usage can draw faster than it shows. |
| Prompt suggestions | Claude Code [skips them](https://code.claude.com/docs/en/costs#background-token-usage) while a plan is near its usage limit, which the estimate, run on every eligible turn, cannot see. |
| Fable weekly use | Anthropic [caps Fable models](https://support.claude.com/en/articles/15424964-claude-fable-models-on-your-plan) at up to half the weekly usage limit on Max, Ultra and Team and seat-based Enterprise premium seats, drawing it faster than other models, runs them on usage credits on Pro and Team and seat-based Enterprise standard seats, and bills them at API rates on usage-based Enterprise. The line shows weekly API-price spend and no share of the cap, as [Fable weekly use](#fable-weekly-use) explains. |
| Codex compaction window and keepalive pings | ChatGPT credits [charge cache writes as ordinary input](https://learn.chatgpt.com/docs/pricing), while API prices from GPT-5.6 on charge 1.25x for a 30-minute write, so a replay that trades writes for reads can come out differently on credits. |

Every other line's dollars are API prices with no documented plan rule beside
them.

## Window

`--since YYYY-MM-DD` means midnight UTC; ISO instants must include a timezone.
Without it, each Claude bucket starts at its latest observed write-TTL transition,
or the available history when no transition is recorded. A transition can also be
a billing or agent-policy change. Reports label today's settings applied to
history and do not invent historical settings. Recommendations name the owning
setting or environment override.

## Exit codes and unreadable lines

When `doctor` walks through this host's changes on a terminal, it exits `0`, or
`2` for a failure or cancellation. Otherwise exit codes are `0` for no applicable
change, `1` for a recommended setting or feature change, and `2`
for a failed scan, price fetch, invalid argument, or cancellation. A failed run emits no
success report or partial recommendations. Invalid selected usage fails with only
a file and line. Complete lines that are not JSON objects, which harnesses leave
after crashes and concurrent writes, are skipped and counted in the report;
their usage counts as missing, and each one counts as an unpriced request in
that profile's comparisons. Unterminated final lines are deferred, and
`--details` counts the files that had one.

An unreadable line does not by itself keep an unused tool, skill or MCP server
from being recommended off. It holds the recommendation back, as an advisory,
in two cases. Its bytes name the feature as a call records it, such as
`mcp__server__tool` or a skill's name (any case), or a Codex completion's
`"server"`, after decoding JSON escapes like `\u0041gent`; a mention in prose
counts too. Or its transcript has a tool
result whose id matches no parsed call, in the parsed records or in the
unreadable lines: a crash can cut a record off, leaving no name to search, and
every call that ran leaves a result, so such a result shows a call is missing.
That second case cannot say which feature the call used, so it holds back every
never-used feature that transcript's profile carries. One miss is accepted: a call cut
off inside its name whose client died before writing its result counts as
unused, though it may have run; the default report's skipped-lines line says so,
and `--details` repeats it on each red row beside unreadable lines. A deferred line always holds back the
recommendation. Only hashes of the names and ids are kept in memory, never the
text.

## Reading the report

The default report says what was read and what it cost at API prices, then
gives every setting and feature one line. Each line names its verdict, the
setting or feature, its current value (and the value to change to, for a
change), what it saves per 30 days or costs over the window, that figure's
share of this host's spend, the kind of claim it rests on, the profile when
several were read, and the setting that changes it. For a change `doctor` offers,
that last column is the exact edit and file, or the step to take by hand.

The kinds of claim are:

- **Measured**: replayed from your own transcripts at public API prices: the
  cache TTL, compaction window, idle compaction (native and T3), the Bash cap,
  keepalive pings, background generation, tool output caps, and every tool,
  skill and MCP server, with subagent types and context no setting turns off.
- **Official**: the provider's own guidance beside what the transcripts show:
  CLAUDE.md length and the AGENTS.md byte budget, MCP tool loading, and
  Fable's weekly use beside the plan rule. A Claude
  subagent type's evidence also cites Anthropic's model guide.
- **Opinion**: the maintainer's preference, beside whatever was measured: fast
  mode off and auto memory off.

The lines come in two groups. **Changes** come first: every line recommended
(red, exit 1) or worth considering (yellow, exit 0), ranked together by what
they save per 30 days, biggest first, with those whose saving was not
measured after the rest. A feature turned off saves what it cost, assuming
nothing else changes. **Everything else** follows, ranked by what it costs,
biggest first, with lines whose cost is unknown or that have no figure after.
When several profiles were read, each line also names the settings file that
owns it, or the profile when no file does (see [Profiles](#profiles)).
The headline counts both kinds of change. A dot's color carries the verdict:
red, yellow, green for a value already at its recommendation, and a dim hollow
dot for no verdict, either for lack of evidence or because nothing can be set.
Without color a word replaces the dot: `change`, `consider`, `keep`,
`unknown` or `info`.

Every percent is of this host's spend over the window at API prices: every
request it recorded, shared histories counted once. A cost's percent divides
its total by the host's, since a profile's history lies within the host's
window; a saving's compares its monthly rate with the host's. When some
requests could not be priced, the base is the priced part, so a share can read
high, and the line explaining the percents says so.
Comparisons between setting values are per 30 days at the window's rate; what
features, context and long Bash waits cost, and how often they were used, are
totals over the whole window.

`--details` puts each line's evidence under it: what the replay held constant,
such as the same sessions, and what it could not measure, such as the detail a
smaller compaction window or idle summary loses, or what checking on a
backgrounded Bash command costs, then the setting that changes it. A line
without enough evidence says why there, such as two options too close to call
for the requests that could not be priced. Unresolved current ownership gives
no verdict and does not cause exit 1. The list then cites its sources, and the
evidence cards follow.

Features sent with every request, such as a tool, MCP server or skill, show what
they cost and how often they were used. One never used is recommended off,
whatever it cost. One that cost more than `--max-cost-per-use` (default $1) per
use is worth considering off, an advisory that exits 0; the rest are kept. An
unused one stays an advisory when the window cannot rule out a use: it was first
sent after the window began, an unreadable transcript line names it or may be
a cut-off call (see [Exit codes and unreadable lines](#exit-codes-and-unreadable-lines)),
a transcript was still being written, or, for a Codex skill, its uses are only
partly recorded. A Claude skill's uses count only the model's own calls, because hiding it
with `skillOverrides` set to `user-invocable-only` still lets you type `/name`.
Skills synced from claude.ai share one line, since only `syncClaudeAiSkills`
turns them off in settings, and that also removes typed uses, so those count.
Codex skills have separate listing-cost lines by absolute `SKILL.md` path,
with requested uses and confirmed file loads distinguished. Disable one with
`[[skills.config]]`, its `path`, and `enabled = false` in `config.toml`; this
also removes explicit invocation. Same-named copies are flagged even without
prices, with one copy retained. Repeated discovery of the same path needs the
redundant source removed. Zero observations do not prove nonuse, and listing
cost is not promised savings: Codex can backfill the freed catalog space.
Auto memory, Claude Code's `MEMORY.md` and system prompt section or Codex's
memory summary, is worth considering off while it is on, or may be on because
a Codex setting such as a `--profile` selection is not recorded, on the maintainer's
opinion rather than a measurement: notes the agent writes for itself skip
review and encourage slop, so durable guidance belongs in a reviewed,
checked-in CLAUDE.md or AGENTS.md. Its line shows the total cost of the loaded
memory, the memory files read back and the writes, and how often it was read or
written, but `--max-cost-per-use` does not apply and it does not cause exit 1. A
profile with memory off shows it as kept.
Claude plugin skills, which `skillOverrides` does not reach, and the system
prompt, core tools such as Bash and Read, and the skill and subagent lists have
no switch, so their Measured lines give no verdict.
A switch is an edit such as `"enableArtifact": false` or a bare tool name added
to `permissions.deny`, which `doctor` offers to make (see
[Applying changes](#applying-changes)). `--details` lists every piece,
including those no longer sent.

A run with no histories or no requests in the selected window gives a next step;
no applicable changes does not imply complete visibility or optimal settings.

## MCP servers and instruction budgets

MCP servers include separate tool-schema and discovery-context costs. Those
details are part of the server total. Codex tool recommendations use
`mcp_servers.<id>.disabled_tools` only when the declaring server id is unambiguous and native completions prove
the raw tool name in every owning profile;
`enabled_tools` is the corresponding allow list. The report explains Claude's
`ENABLE_TOOL_SEARCH` and per-server `alwaysLoad`, and Codex's `omit_tools_from`
surfaces. These exposure controls do not establish net savings: search turns,
returned definitions, cache changes and repeated loading are not completely
attributable. A small frequently used server can be cheaper to load upfront.
An MCP tool with no switch of its own, or whose server's off already covers
it, has no line: its server's line includes its cost, and `--details` lists it.
App controls require the actual app id, which these transcripts do not map to
the exposed app namespaces; the report names the supported controls without
inventing per-app costs or a settings patch.

Instruction files are Official lines. A CLAUDE.md is worth considering
shorter at 200 recorded lines or more, Claude Code's documented guidance, and
kept below that. A Codex AGENTS.md is compared with the selected profiles'
`project_doc_max_bytes` from the system and user configuration (32 KiB by
default): the recorded rendered bytes include headers and wrappers, so under
the budget it is kept, and at or above it Codex may have truncated it, which is
worth considering. The evidence names the file that sets the budget, or says
it is the default. A project `.codex/config.toml` at a recorded directory or a named
profile that sets it gives no verdict, since the transcripts do not record
which applied; neither do profiles with different or unreadable budgets, or
records with no rendered text. Command-line overrides and original file sizes
are unavailable.
Length alone is not a quality score. Other instruction files Claude loads have
no size guidance and get a Measured line with their cost. Output-cap lines
still recommend no value because truncation-related read-back cannot be priced
from these transcripts.

An auto-compact window without enough evidence also shows how large that
model's requests got (median and 90th percentile), to judge whether a smaller
window would matter.

## How context is priced

Each piece is sized from the request shapes Claude Code records (tools, then the
system prompt, then messages) and converted to tokens with a characters-to-tokens
ratio measured from the same transcripts. It is priced at the cache position it
occupied in that request: cached read, 1h write, 5m write, then ordinary input.
Attachments have no recorded position, so one is priced only when every
position its history allows pays the same rate. Requests that could not be
priced are estimated from those that could, and a piece with none is shown as
unknown.
Removing a piece can also shift later cache writes and compaction, which this
does not model. Claude requests without a recorded shape cannot price their
context pieces. Codex prices rendered chronological context with a quarter-token
per-character estimate; metadata-only pieces remain unknown.

Comparisons use the requests priced under both options. Requests with missing
usage are left out of both sides, and a recommendation is made only when they
could not flip it, assuming none differs by more than the largest measured
difference. The verdict then says what share of requests was measured.

## Details and terminal layout

Use `--details` for each line's evidence under it, the sources, then evidence
cards: every check's profiles, owner, baseline, assumptions, uncertainty, cost
components, break-evens, sensitivity scenarios, cache witnesses and sources,
and every piece of context, including MCP tools counted in their server's line
and pieces no longer sent. A piece of context several sources share is broken
down by the profiles that recorded its requests, with each one's requests and
cost. A background feature's evidence names three of the directories it ran in
and counts the rest. Repeated idle periods, cache witnesses and attempt estimates are
grouped by their recorded outcomes and reasons. Token ranges and combined costs
summarize modeled compactions; geometry sources are listed once. Identifiers in
grouped cache and attempt evidence are examples. No evidence is hidden behind
the walk's prompts or a top-N limit. The layout wraps long paths and
names on narrow terminals, where the name column narrows first and a line too
long for the width wraps under its name. A name or profile too long for its
column pushes the rest of its own line along rather than widening every line.
Redirected output keeps each line whole so paths and edits remain copyable. On
terminals, the report's colored dot carries each line's verdict, with a legend
above the lines; `NO_COLOR`
disables color, and `--plain` also disables decorative Unicode, and either
puts the verdict word back in place of the dot. Evidence cards keep their
verdict words and only add color, and each names its claim. Redirected output
is plain, and progress appears only on interactive stderr. SIGINT or SIGTERM ends
with exit 2 and no success report.
Unicode decorations require a UTF-8 locale. Progress has phases, never an invented percent.

## Machine-readable output

The report is for people and has no machine-readable form; `--json` was
removed. Each machine output is its own sub-command with a named consumer, such
as the session export below.

IDs in the report are opaque hashes. Shared histories are read once, and a
request whose profile is ambiguous counts once on the line its profiles share
(see [Profiles](#profiles)). Independent check savings must not be summed; the
walk's running total is the one sum, and it says that overlapping changes may
save less.

## Session export

`token-max sessions` prints each session's usage at public API prices, one
JSON object per line, for tools that tie sessions to their own records, such
as moq-dev/quest's `quest usage`. It takes `--since`, the profile and price
options, and `--ssh` with `--include-local` as `doctor` does;
`token-max sessions --help` lists every field.

```sh
token-max sessions --since 2026-11-01
token-max sessions --ssh devbox --include-local
```

A Claude subagent, and a Codex review or spawned thread, is a session of its
own that names the session it came from. Each session lists its native ID,
working directory, every Git branch its records name with the first and last
time, and its tokens and cost per model and reasoning effort. Branches come
from records in the `--since` window; when none names one, as in a Codex
session begun before it (Codex names its branch only when a session starts),
the last branch named before the window is listed. A request whose parts ran on
several models, such as an advisor call, counts once in each model's entry. A request copied
into another transcript, as a fork does, counts once, with its original
session. A cost is a lower bound, marked `"complete": false`, when a request
could not be priced or a transcript had unreadable or unfinished lines.
Compaction summaries the transcripts never recorded are not estimated here.

Each line names its `host`: the `--ssh` destination as given, or null for this
host. Lines from every host are ordered together by first request; a session
whose history was copied between hosts is printed once per host, each with its
own `host`, and the consumer decides which to count.

Each line's `repositories` is resolved on the host that ran the session, so a
consumer never reads another host's path on its own. It is the repository
Codex recorded when the session started (`"repositoriesSource": "recorded"`),
or else the remote URLs of the Git checkout now containing `cwd`, a
subdirectory, linked worktree or symlinked directory included (`"checkout"`). A
checkout is read when the export runs and may have been replaced since the
session, so treat `checkout` as unverified. It is null when the directory no
longer exists or this user cannot read it (another user's, or a dropped network
mount), is in no checkout, or the checkout has no remote or an unreadable
config. URLs are read from the
repository's own config file, in the order it lists them, without following
includes or applying `insteadOf` rewrites, and are printed without userinfo, so
a credential such as `https://user:token@host` never leaves the host. A config
Git would reject fails the export, naming the file and line.

Every string prints with terminal controls and direction overrides escaped as
`\uXXXX`, which decodes to the same text, so a path, branch or URL, from this
host or a destination, cannot drive the terminal that shows the output.

Native session IDs never reach the report, which hashes them. Over SSH, they
cross only in a destination's session lines.

## Cache TTL

Unlinked API attempts stay in coverage. A TTL comparison may estimate input from
matching adjacent measured requests, including growing-context retry geometry
within one five-minute cache lifetime, with their IDs and assumptions reported;
original usage and historical billing stay unknown. Unsupported inputs leave their
own parts unpriced. Sensitivity scenarios accompany the aggregate
winner, and unknown fixed charges can cancel without pretending they cost zero.

Cache replay preserves known, estimated and unknown predecessor states, including
unsupported attempts before the selected window. Missing cohort metadata spans
compatible histories; `--details` groups
their states and reasons. A comparison
stays unpriced when those histories imply different cache charges, while invariant
charges can still be compared.

## Compaction window

Compaction-window comparisons run after the recommended TTLs and use the current
window as their baseline. Claude controls resolve environment overrides,
managed and user settings, including `modelSettings.<model>.autoCompactWindow`;
Codex uses `model_auto_compact_token_limit`. An unset Claude window uses each
model's latest observed automatic-compaction trigger, else Claude Code's
documented default. A window larger than the model's context window replays
at that context window, as Claude Code caps it. Unresolved defaults, disabled
compaction, unsupported versions/scopes, and ambiguous named Codex profiles stay
advisory. No settings are edited.

Candidates come from current controls, price-tier boundaries, provider limits,
and measured resumed-prefix geometry. Added compactions use observed summary and resumed-prefix sizes in matching
cohorts; unsupported affected candidates stay unpriced. Observed compactions remain
charged once, including when earlier hypothetical passes might replace them.
Summary generation and cold resumed prefixes are included in net savings. Larger
observed summaries/prefixes, cold-summary costs, and the extra re-read spend that
would erase a saving accompany the aggregate winner. Lost detail and changed work
remain unmeasured risks, not zero-cost observations.

## Idle compaction

Idle compaction compares on and off over the same window, first with current
controls and then after applicable TTL and window changes. The latter aggregate
estimate decides: positive on-minus-off cost favors off, negative favors on, and
a zero tie, such as when no gap was long enough to trigger, is not enough evidence. Final periods incur expected summary cost
without an invented return benefit. A sensitivity crossing zero does not veto the
point estimate. A recommended 5m main TTL or a window at/below the idle threshold
makes idle compaction inapplicable.

The native toggle defaults to true, requires Claude Code v2.1.290+, enabled
auto-compaction and a 1h main TTL; versions before v2.1.314 also need confirmed
feature availability. The threshold defaults to 200,000 tokens, with a 100,000
floor for `CLAUDE_CODE_IDLE_COMPACT_MIN_TOKENS`. This check does not tune it.
Current user, project/local, managed and active-profile environment owners are
read separately from historical metadata. Missing directories, unknown execution
mode/version/flags or unresolved controls remain advisory and unpriced where they
affect the comparison. Native CLI interactivity needs REPL provenance; Desktop
labels alone do not establish local/SSH execution. T3's proactive policies are
outside this native check.

Only verified native informational notices attribute an applied idle summary.
`precomputed` alone does not. Where comparable known-on outcomes exist, their
same-version/client/flag applied fraction estimates unobserved activation,
including final periods. Otherwise the deciding assumption leaves the client
open and awake for one documented near-expiry trigger. Closed/open alternatives,
summary-size and timing sensitivity, the activation break-even and extra re-read
cost per resumed summary accompany the recommendation. Missing boundaries never
prove that discarded or unlogged summaries did not run. Without linked billed
summary output and full resumed-prefix calibration, affected hypothetical costs
stay unknown; `postTokens` alone cannot establish the full request.

## T3 Code

T3's Claude Code and Codex instances reuse their native histories, including
retained histories of disabled instances. Shared homes, Codex shadow-home
symlinks and request identities are deduplicated. T3 Usage aggregates are never
added to native billed requests. Unsupported providers are explicit coverage
gaps. Each T3 state gets a line naming the model its
`textGenerationModelSelection` resolves to for titles, branch names, PR text and
other generated text, with the narrower selections that replace it; their cost
is unmeasured because T3 runs them in ephemeral sessions. Native terminal
suggestions, recaps, goal check-ins and keepalive guidance do not control them.

The report names T3's `providerInstances.<id>.config.autoCompactWindow` when
it overrides a native Claude window. Thread speed selection belongs to
`modelSelection.options.fastMode` or `.serviceTier`, with Codex's native fallback
when omitted. Current browser/new-thread selections are unavailable and remain
advisory. A uniquely joined historical requested tier never fills a billed tier.
Disabled instances and unresolved environment, launch or machine-policy overrides
keep historical cost but receive advisory current guidance.

An instance that reads a native home's settings shares that file's lines with
native Claude Code or Codex, such as the cache TTL, the Bash cap, auto memory
and every tool, skill and MCP server (see [Profiles](#profiles)). The settings
T3 itself decides, such as thread fast mode and effort and an instance's
window override, are the instance's own lines. Instances whose idle compaction
inherits the global `idleCompaction` share one idle line naming T3's
`settings.json`. A line shared with an instance whose ownership T3 could not
settle stays advisory unless the requests it could not settle all lie within
that line.

Optional SQLite metadata uses the bundled SQLite library; no system SQLite
executable is needed. One read-only transaction supplies a consistent WAL view,
then closes before analysis. Queries return allowlisted native identities, run
state, token counts, timestamps, requested speed and lifecycle predicates only.
No message text, credentials or connection catalog is exported. Schema, size,
query-budget and read-only WAL failures leave native usage intact with an
explicit metadata gap. The native file and database snapshot bounds are reported
separately; later or ambiguous evidence cannot supply a historical join.

`t3-idle-compaction` compares `off`, `onReturn` and `beforeCacheExpires`
under current and recommended TTL/window controls using the shared cache,
compaction calibration and API pricing. Each mode's cost is what it changes
from the recorded history, so charges every mode shares cancel. The proactive
timer starts two minutes before the observed cache expiry, for eligible
completed main threads with at least 200,000 tokens. Five-minute and one-hour
caches both apply. In-memory timers disappear on server restart, so generation
remains conditional on server liveness. `onReturn` assumes the SDK offers a
resume dialog when a new run resumes a cold session of at least 200,000 tokens
and the user accepts it; the adapter does not infer an unrecorded acceptance.
It needs no T3 lifecycle guard. `off` also suppresses that dialog. Summaries
pay warm or cold input plus output; later requests pay their full changed
prefixes, and final proactive summaries have no invented return benefit.
The lifecycle guards for each gap are rebuilt from T3's event log at every
completed-run event and when the timer would fire. T3 keeps about one day of
that log, so the check's window starts where the log is complete, and gaps
that open earlier stay as recorded in every mode. Unknown lifecycle or missing
compatible full-prefix calibration stays unpriced, and the report names the
first missing input. See [T3 evidence and sources](t3.md) for supported schema
and limits.

## Other hosts over SSH

`--ssh HOST` (repeatable) audits a machine you reach with OpenSSH, using your
SSH configuration, keys and known hosts without prompting. Add
`--include-local` to audit this host in the same run. Each destination needs
the same version of token-max installed; token-max installs nothing there
itself. A missing or different version fails with the
command to run. Linux and macOS destinations are supported. `token-max
sessions` takes the same flags, and each destination sends only its priced
session lines.

```sh
token-max doctor --ssh devbox --ssh user@laptop --include-local
```

The destination runs `token-max doctor --ssh-scan`, which discovers that host's own
profiles from its home and environment, reads its transcripts and settings,
and runs every check there, priced from the one catalog this host loads and
sends it. Only its finished report comes back, so its transcripts and scan
never leave it, and this host never holds more than its own scan. It looks for
`token-max` on the non-interactive `PATH`, then in the release installers'
directory (the first of `$XDG_BIN_HOME`, `$XDG_DATA_HOME/../bin` and
`~/.local/bin`), then in `~/.cargo/bin`.
`--claude-dir`, `--codex-dir` and `--t3-dir` select local profiles only, so with
`--ssh` they need `--include-local`.

Each host is analyzed on its own and gets its own section, labeled with the
destination, or `this host`. A history copied between hosts
counts in each host's report, so don't add amounts across hosts. Any failed
host, connection or stream fails the whole run with exit 2 and no report;
Ctrl-C stops every destination. A destination's changes are listed in its
section to make there; `doctor` offers only this host's. Each host's percents
are of that host's own spend.

## Keepalive pings

The `keepalive` check asks whether pinging an idle session just under its cache
lifetime would cost less than the cache misses it avoids. It compares leaving
sessions idle, Claude idle compaction where that check recommends it, and
pinging for each cap on how long to keep going. Claude uses the recommended
main TTL; Codex uses the documented 30-minute lifetime of models that bill cache
writes. Each cap replays the entire session with the same recommended compaction
window as the idle alternative, carrying ping context and cache warmth forward. A ping is the median recorded request, and a session that never came
back pings for the whole cap, since transcripts do not record when a client
closed. The details show every cap, the same caps if those sessions closed at
once, and how soon they would need to close for pinging to pay.

Neither harness has a capped keepalive setting: a `/loop` task pings on a fixed
schedule until cancelled or for seven days, and Codex cannot schedule pings. A
priced result is therefore advisory and never changes the exit code. Current
ping state stays unknown: recorded turns, including scheduled-task turns, are
activity, and their metadata does not establish whether they are keepalive pings.

## Fast mode

The maintainer's cost preference is fast mode off for both harnesses. The
`fast-mode` check names the current effective local preference, its owner and
the standard-speed replacement: Claude `fastMode: false`, or Codex
`service_tier = "default"`. Use `/fast off` for an active session. No settings
are edited. A justified current premium preference causes exit 1 even without
historical usage or prices. Standard current settings never receive a change
because of historical premium sessions.

Claude resolution includes `fastModePerSessionOptIn`, managed `fastMode: false`,
project/local files at recorded working directories, model support and the active
profile's `CLAUDE_CODE_DISABLE_FAST_MODE`. Codex resolves local system/user
settings, independent fast/ultrafast feature gates and local requirements.
Unresolved named/legacy profile selection, project trust/overrides,
unsupported providers and unsupported models stay advisory. Unrecorded CLI/session overrides
and remote account policy are unknown, not inferred from old sessions.

Each card separates observed response-speed premiums from a current-setting
estimate applied to recorded usage. Native Codex usage records do not record a
billed tier; requested turn/thread settings are retained separately and never
count as a billed premium. Claude response usage can record billed speed. Missing
speed, usage or exact pricing stays unknown. Dollar comparisons use the same
model and standard API rates, preserving recorded cache reads, write lifetimes,
geography and context tiers. These are **rate-only API-price equivalents**:
Claude's separate fast/standard caches mean cache-switch effects are excluded.
They hold work constant, do not value latency, and make no combined savings
claim. A plan counts fast mode its own way; see
[Plans and API prices](#plans-and-api-prices). Fast mode compares with the current
profile, independently of the TTL/window/idle recommendations.

Measured Codex usage without a billed tier receives a **conditional API equivalent**
in the shared pricing layer. It uses a supported historical requested tier, or
standard rates when the request omits a tier or says `auto`. Omitted and `auto`
can inherit a nonstandard OpenAI project default; requests can also be served at
a different tier. Today's config never establishes a historical tier. Unsupported
requested tiers, conflicting evidence, missing usage and unavailable prices stay
unknown. Recorded effective tiers always take precedence.

Reports count conditional requests separately from recorded tier coverage.
`--details` shows standard, fast and ultrafast served-tier scenarios for
conditionally priced parts, preserving recorded tiers and all context/cache
categories. Unsupported scenarios, including Flex and Scale without verified
rates, remain unknown. These are sensitivity examples, not bounds on actual
charges. Context keep/off, compaction and keepalive winners that depend on missing tiers stay
advisory because tier sensitivity could reverse them. The explicit preference to
disable premium speed still applies. SSH reports use the same pricing and evidence.

## Background generation

The `background-generation` check prints one line per feature: Claude prompt
suggestions, session recaps and goal check-ins, and Codex automatic recaps. Each
names its current value and the setting that turns it off:
`promptSuggestionEnabled`, `awaySummaryEnabled`,
`CLAUDE_CODE_GOAL_CHECKIN_MINUTES=0` or `tui.auto_recap = false`.

Separate lines retain distinct owners across recorded projects and charge only
their requests. Claude project/local settings and their `env` blocks participate;
Codex uses trusted project config layers, closest to the working directory first.
Missing directories, unknown trust and unrecorded profile selection stay advisory.
Current files are applied to history as an estimate, not proof of past settings.

Neither client logs these requests, so suggestion and recap costs are estimated
at each recorded interactive-terminal turn where they would have run. Their uses
are not recorded either, so one never counts as unused: a whole cost above
`--max-cost-per-use` is worth considering off, an advisory, and one below it is
kept. Goal check-ins and Codex recaps
cannot be told apart in transcripts, so their cost is shown as unknown.

## Bash timeout cap

Claude Code waits on a foreground Bash call for at most
`max(BASH_MAX_TIMEOUT_MS, BASH_DEFAULT_TIMEOUT_MS)`, 600000 ms by default,
then moves the command to the background. A wait at least as long as the cache
TTL lets the cache expire before the model replies, so the next request
rewrites the whole prefix. The `bash-timeout-cap` check compares the cap with
the main-agent TTL: the one this report recommends, else the current setting,
else the lifetime the latest main-agent request wrote. At or above it, the
check recommends 240000 ms for a 5m cache or 3300000 ms for 1h, which leaves
room for the model's reply on either side of the wait, and exits 1. Commands
longer than the cap keep running in the background, where checking on them
reads the cache instead.

The line also counts foreground Bash waits that outlasted the cache the
starting request wrote, how many hit their timeout or slept in a polling loop,
and what their cache misses cost: the prefix the next request wrote instead of
read, at the write rate minus the read rate. Background commands and the user's
own processes are unaffected by the cap. Codex's
`background_terminal_max_timeout` bounds one poll of a background terminal,
and each poll reads the cache, so it is not checked.

## Main-session effort

Each settings file that decides it gets one `Main-session effort` line, and a
T3 instance its own: the reasoning effort its main
session defaults to, the level it runs at, what each recorded model and effort
pair cost over the window and how many requests it ran, and the setting that
changes the default. A different effort changes the output tokens, so no cost
is repriced and no saving is claimed.

- `max` is a recommended change to `xhigh`, or to `high` on a model without
  `xhigh`, and exits 1. For Claude it is Official, citing Anthropic's
  [model guide](https://claude.dev/blog/which-claude-model-should-you-build-with/)
  (past `high`, test the next model up at a lower effort) and the
  [model configuration docs](https://code.claude.com/docs/en/model-config#choose-an-effort-level)
  (`max` is prone to overthinking; test it before adopting it broadly). For
  Codex the same advice is Opinion, beside OpenAI's
  [reasoning guide](https://developers.openai.com/api/docs/guides/reasoning).
- `low` is worth considering raising to `medium`, as Opinion.
- `xhigh` cites the same guidance under Official, with no verdict; so do
  `medium` and `high`, with the guidance for those levels.
- A default token-max cannot resolve, such as a T3 thread's selection, is
  unknown and never a change.

On a terminal, `doctor` offers the change in the profile's own file: for Claude
the `env.CLAUDE_CODE_EFFORT_LEVEL` that sets it, or else the model's
`modelSettings.<model>.effortLevel`, or the `maxEffortLevel` that holds a `low`
level down; for Codex `model_reasoning_effort`. The target is the next level
the model accepts, by Claude Code's effort table or Codex's model catalog. A
shell variable, a project or managed file, or a named Codex profile that sets
it is listed to change by hand. See
[default effort](semantics.md#main-session-effort) for how the default
resolves.

## Subagent models

The report gives every Claude Code subagent
type (Explore, Plan, general-purpose, forks and each custom agent) and every
Codex agent role its own line: what it cost over how many runs, the models and
reasoning efforts it ran on, where its model came from, and what the same
recorded tokens would cost on the provider's cheapest current model (the
newest Haiku, or the cheapest current GPT). Like feature costs, these are
totals over the whole window. Each line names the settings that change the
model and the effort, such as `model:` and `effort:` in
`~/.claude/agents/Explore.md`, or a Codex role file's `model` and `[agents]
default_subagent_model`. A file that replaces a built-in such as Explore must
restate its tool restriction and system prompt.

The lines are evidence, never recommendations: their dot is dim, with no
verdict, and they do not change the exit code or offer a change. A cheaper model trades
capability, and the main agent's extra work after a weaker subagent cannot be
attributed. The reprice assumes the same turns, and a lower effort's cost
cannot be measured. The source is unknown when today's settings and agent
files do not explain the recorded model, for example after a file changed.

Each Claude subagent type's evidence also gives an `Official:` sentence citing
Anthropic's
[model guide](https://claude.dev/blog/which-claude-model-should-you-build-with/):
subagents that search files, read logs or summarize can start on Haiku or
Sonnet at the model's default effort, and code edits and judgment calls stay
on the main model. Explore's line adds the guide's tip, a user agent named
`Explore` with `model: haiku`, unless every request already ran on Haiku. The
transcripts don't record whether a type searched or edited, so the sentence cannot
say which half applies. The guide also measured an Opus 5.5 lead over Sonnet 5.5
workers at about a third less cost than Opus 5.5 alone at its default effort,
scoring about the same, on one internal benchmark; the report doesn't quote it,
since that pattern splits work too big for one context window rather than
side tasks. Codex roles get no such sentence, nor do forks, which always run on
the main model.
See [subagent models](semantics.md#subagent-models).

## Fable weekly use

The report gives every Claude settings file whose profiles sign in to
Anthropic its own Official Fable line, shared by native Claude Code and the T3
instances that use it: what its Fable requests cost at API prices in each 7-day span
of the window, counted back from the report, beside the plan rule Anthropic
documents on its [pricing](https://claude.com/pricing) page and in
[Fable models on your plan](https://support.claude.com/en/articles/15424964-claude-fable-models-on-your-plan),
given as an `Official:` sentence before the model setting, as
[Plans and API prices](#plans-and-api-prices) describes: on a Claude Max or
Ultra plan or a Team or seat-based Enterprise premium seat, Fable models can
use up to half the weekly usage limit and use it faster than other models; on
Pro or a Team or seat-based Enterprise standard seat they run on usage credits;
on usage-based Enterprise they bill at API rates. Weeks before the profile's first Fable request
are left out.

No plan limit is published in tokens or dollars, so the line shows no share of
the limit and gives no verdict: its dot is dim, and it never changes the exit
code or offers a change. The figures are partial evidence. The cap counts Fable
use on every Claude surface, such as claude.ai and Claude Desktop, while the
line covers only its profiles' recorded sessions, and it may include requests
paid with usage credits after the cap was reached. The plan's weekly reset time
is not recorded, so spans end at the report rather than at a reset. When
a Claude profile with another settings file that signs in to Anthropic reads
the same transcripts,
the line says how many Fable requests it shares, since whose they were is
unknown; one on a cloud provider has no plan limit and is not counted. A profile
with `CLAUDE_CODE_USE_BEDROCK`, `CLAUDE_CODE_USE_VERTEX` or
`CLAUDE_CODE_USE_FOUNDRY` is billed by its cloud provider and gets no line.
Change the model with `/model` or the `model` setting.
See [Fable weekly use](semantics.md#fable-weekly-use).
