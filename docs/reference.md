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
release, and a newer release replaces it. `run.sh` hands `doctor` the
terminal from `/dev/tty`, since its stdin is the piped script, so the changes
are still offered. Options go after `sh -s --`, as in
`curl -LsSf https://max.kixel.ai/run.sh | sh -s -- --since 2026-11-01`, or
in PowerShell,
`& ([scriptblock]::Create((irm https://max.kixel.ai/run.ps1))) --since 2026-11-01`.
`install.sh` and `install.ps1` redirect to the release's own installers.

## Applying changes

After the report, `doctor` offers each recommended change it can make, then
each setting the report suggests considering turning off, in report order. A
line no setting changes, such as an instruction file's size, has nothing to
offer. Each prompt shows
the line's name and values and the exact edit, such as `"promptCacheTtl":
"1h"` in `~/.claude/settings.json`, and takes `y`, `n` or `q`; anything else
asks again, and end of input is a `q`. A `q` leaves the rest unapplied.
Nothing is written until the last answer: each file is then written once,
with every edit accepted for it. A no or an unanswered change is listed after
the walk with its edit, to make by hand, which also suits a file another tool
rewrites, such as a dotfiles manager.

Only a profile's own `settings.json` or Codex `config.toml` is edited,
including Claude's `env` block. An edit sets the setting that decides the
current value, as its line's `Set in` names it, so a change is applied only
when that owner is the profile's file, or the default no file sets. A shell
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
the change a step by hand naming the resolved path. Ctrl-C at a prompt exits 2
with no settings changed.

Without a terminal, `doctor` changes nothing: the report ends with every
change as its edit and file, or its step by hand. A destination of `--ssh`
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

## Prices

The first online run fetches only `https://models.dev/api.json`, caching validated
prices at `~/.cache/token-max/models.json` (or under `XDG_CACHE_HOME`).
Use `--offline` for that cache, `--price-cache FILE` to choose its location, or
`--prices-file FILE` for a local models.dev catalog with no network access.
Network failures fail the run; offline use is explicit. Cached prices include
their file timestamp, so an old catalog remains visible.

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

## Reading the report

The default report says what was read, then gives every setting and feature
two lines in one of three sections, by the kind of claim its recommendation
rests on:

- **Measured**: replayed from your own transcripts at public API prices: the
  cache TTL, compaction window, idle compaction (native and T3), the Bash cap,
  keepalive pings, background generation, tool output caps, and every tool,
  skill and MCP server, with subagent types and context no setting turns off.
  The evidence line says what the replay held constant, such as the same
  sessions, and what it could not measure, such as the detail a smaller
  compaction window or idle summary loses, or what checking on a backgrounded
  Bash command costs.
- **Official**: the provider's own guidance beside what the transcripts show:
  CLAUDE.md length and the AGENTS.md byte budget, and MCP tool loading. The
  section ends with its sources.
- **Opinion**: the maintainer's preference, beside whatever was measured: fast
  mode off and auto memory off.

The first line names the setting or feature, its current value (and the
recommended one after an arrow for a change), what it saves per 30 days or
costs over the window, and the profile when several were read. The second line
is the evidence, ending with the setting that changes it. A dot's color carries
the verdict: red for a recommended change (exit 1), yellow for one worth
considering (exit 0), green for a value already at its recommendation, and a
dim hollow dot for no verdict, either for lack of evidence, which the line
explains, or because nothing can be set. Without color a word replaces the dot:
`change`, `consider`, `keep`, `unknown` or `info`. Within a section, lines are
ordered by that verdict, red first.

Comparisons between setting values are per 30 days at the window's rate, so
checks with different windows compare; what features, context and long Bash
waits cost, and how often they were used, are totals over the whole window.
Settings that don't change the experience, such as the cache TTL, name the
current and suggested values, the savings, and the setting that owns them. A
line without enough evidence says why, such as two options being too close to
call. Unresolved current ownership stays there and does not cause exit 1.

Features sent with every request, such as a tool, MCP server or skill, show what
they cost and how often they were used. One never used is recommended off,
whatever it cost. One that cost more than `--max-cost-per-use` (default $1) per
use is worth considering off, an advisory that exits 0; the rest are kept. An
unused one stays an advisory when the window cannot rule out a use: it was first
sent after the window began, an unreadable transcript line could hold a call,
or, for a Codex skill, its uses are only partly recorded. A Claude skill's uses count only the model's own calls, because hiding it
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
it, keeps an info line with its cost, which its server's line already includes.
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

Use `--details` for evidence cards: every check's owner, baseline, assumptions,
uncertainty, cost components, break-evens, sensitivity scenarios, cache witnesses
and sources. Repeated idle periods, cache witnesses and attempt estimates are
grouped by their recorded outcomes and reasons. Token ranges and combined costs
summarize modeled compactions; geometry sources are listed once. Identifiers in
grouped cache and attempt evidence are examples. No evidence is hidden behind
the walk's prompts or a top-N limit. The layout wraps long paths and
names on narrow terminals, where the name column narrows first and a line too
long for the width wraps under its name. Redirected output keeps each field on
one line so paths remain copyable. On terminals, the default report's colored
dot carries each line's verdict, with a legend above the sections; `NO_COLOR`
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

IDs in the report are opaque hashes. Shared histories are read once and historical costs stay
in a shared bucket when profile provenance is ambiguous. A replay for an alias
names that profile and is conditional, not another copy of historical spend.
Independent check savings must not be summed.

## Session export

`token-max sessions` prints each session's usage at public API prices, one
JSON object per line, for tools that tie sessions to their own records, such
as moq-dev/quest's `quest usage`. It takes `--since` and the profile and price
options; `token-max sessions --help` lists every field.

```sh
token-max sessions --since 2026-11-01
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
Native session IDs appear only in this export: the report hashes them, and an
SSH destination sends only its report.

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
Missing lifecycle or compatible full-prefix calibration stays unpriced, and
the report names the first missing input. Current lifecycle guards do not
reconstruct prior guard states. See [T3 evidence and sources](t3.md) for
supported schema and limits.

## Other hosts over SSH

`--ssh HOST` (repeatable) audits a machine you reach with OpenSSH, using your
SSH configuration, keys and known hosts without prompting. Add
`--include-local` to audit this host in the same run. Each destination needs
the same version of token-max installed; token-max installs nothing there
itself. A missing or different version fails with the
command to run. Linux and macOS destinations are supported.

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
section to make there; `doctor` offers only this host's.

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
They hold work constant, do not value latency, and make no subscription-bill,
credit-multiplier or combined savings claim. Fast mode compares with the current
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

## Subagent models

The Measured section gives every Claude Code subagent
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
See [subagent models](semantics.md#subagent-models).
