# Reference

Every flag, report section and check. [Replay semantics](semantics.md) covers
the accounting underneath, and [JSON reports](json.md) the `--json` schema.

## Running it

Relative paths resolve from the working directory. Reports go to stdout;
diagnostics go to stderr; exit codes are 0/1/2 as described below. Ctrl-C
cancels the running analysis: SIGINT or SIGTERM exits 2 with no success
report.

## Profiles

Without directory flags, discovery includes `~/.claude`, `~/.codex`,
`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, and recognizable `~/.claude*` and `~/.codex*`
profiles. Codex includes archived sessions; Claude includes nested subagents.
T3 instances are included from `~/.t3/userdata`, an existing `~/.t3/dev`,
and `$T3CODE_HOME/userdata`. Repeatable `--claude-dir`, `--codex-dir` and
`--t3-dir` flags select only those roots across both harnesses. `--t3-dir`
takes a state directory containing `settings.json` and optionally `statev2.sqlite`. `--details` and JSON show every discovered root's selection. A missing or
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
setting or environment override and never apply it.

## Exit codes and unreadable lines

Exit codes are `0` for no applicable change, `1` for a recommended setting or
feature change, and `2`
for a failed scan, price fetch, invalid argument, or cancellation. JSON errors go to stderr; a failed run emits no
success report or partial recommendations. Invalid selected usage fails with only
a file and line. Complete lines that are not JSON objects, which harnesses leave
after crashes and concurrent writes, are skipped and counted in the report and in
JSON `coverage.skippedLines`; their usage counts as missing, and each one counts
as an unpriced request in that profile's comparisons. Unterminated final
lines are deferred, with frozen file lengths and deferred byte counts in JSON.

## Reading the report

The default report says what was read, then puts every setting and feature on
one line under **Change**, **Keep**, or **Not enough evidence**, with dollar
amounts per 30 days at the window's rate. Settings that don't change the
experience, such as the cache TTL, name the current and suggested values, the
savings, and the file that owns the setting. Not enough evidence says why in one
sentence, such as two options being too close to call. Unresolved current
ownership stays there and does not cause exit 1.

Features sent with every request, such as a tool, MCP server or skill, show what
they cost and how often they were used. One is recommended off when it cost more
than `--max-cost-per-use` (default $1) per use, counting no use as one; the rest
are kept. A Claude skill's uses count only the model's own calls, because hiding it
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
Instruction files, Claude plugin skills, which `skillOverrides` does not reach, and the
system prompt, core tools such as Bash and Read, and the skill and subagent
lists have no switch and are listed after the verdicts.
The report ends with one `settings.json` or `config.toml` snippet per settings file holding every
recommended change, such as `"enableArtifact": false` or a bare tool name in
`permissions.deny`; merge it by hand. `--details` lists every piece, including
those no longer sent.

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
App controls require the actual app id, which these transcripts do not map to
the exposed app namespaces; the report names the supported controls without
inventing per-app costs or a settings patch.

Instruction-budget notes compare recorded CLAUDE.md line counts with Claude's
under-200-line guidance, and show the selected profile's `project_doc_max_bytes`
(32 KiB by default) beside recorded rendered AGENTS.md bytes. Codex records can
include headers and wrappers, so these bytes cannot diagnose source-chain truncation. Effective
project/CLI overrides and original file sizes may be unavailable; recorded
text can already be truncated. Length alone is not a quality score. Output-cap
lines still recommend no value because truncation-related read-back cannot be
priced from these transcripts.

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
grouped cache and attempt evidence are examples. Nothing is hidden behind an interactive prompt
or a top-N limit. The layout wraps long paths and
names on narrow terminals. Redirected output keeps each field on one line so
paths remain copyable. Color supports the labels on terminals; `NO_COLOR`
disables color, and `--plain` also disables decorative Unicode. Redirected output
is plain, and progress appears only on interactive stderr. SIGINT or SIGTERM ends
with exit 2 and no success report.
Unicode decorations require a UTF-8 locale. Progress has phases, never an invented percent.

## JSON

Text and `--json` share the same analysis. `--json` emits schema version 2. It retains every check's costs, coverage, current and recommended
values, owning controls, assumptions, uncertainty and sensitivity, plus every
per-piece cost and use count in `context`. Large evidence arrays become counts
and groups of distinct conditions; unknown totals remain null with their known
subtotal and every unknown-cost reason. `--json-full` and its per-request schema
version 1 are removed; see [JSON migration](json.md).

Each check has a `check` discriminator: `claude-cache-ttl`
retains its TTL-specific fields, while `compaction-window` includes candidate
windows, modeled summaries, prefix charges and sensitivity. `idle-compaction` adds
current-settings and post-recommendation on/off costs, grouped periods,
activation cohorts, context-size costs and lifetime/summary/timing sensitivities.
`background-generation` adds the feature, the setting that turns it off, its
estimated request count and cost, and the cache rewrites it avoided.
`keepalive` adds the cache lifetime, ping interval and size, and one scenario
per cap. Consumers
should branch on `check` before reading setting-specific fields.
IDs are opaque hashes. Shared histories are read once and historical costs stay
in a shared bucket when profile provenance is ambiguous. A replay for an alias
names that profile and is conditional, not another copy of historical spend.
Independent check savings must not be summed.

## Cache TTL

Unlinked API attempts stay in coverage. A TTL comparison may estimate input from
matching adjacent measured requests, including growing-context retry geometry
within one five-minute cache lifetime, with their IDs and assumptions reported;
original usage and historical billing stay unknown. Unsupported inputs leave their
own parts unpriced. Sensitivity scenarios accompany the aggregate
winner, and unknown fixed charges can cancel without pretending they cost zero.

Cache replay preserves known, estimated and unknown predecessor states, including
unsupported attempts before the selected window. Missing cohort metadata spans
compatible histories; JSON and text group
their states and reasons. A comparison
stays unpriced when those histories imply different cache charges, while invariant
charges can still be compared.

## Compaction window

Compaction-window comparisons run after the recommended TTLs and use the current
window as their baseline. Claude controls resolve environment overrides,
managed and user settings, including `modelSettings.<model>.autoCompactWindow`;
Codex uses `model_auto_compact_token_limit`. An unset Claude window uses each
model's latest observed automatic-compaction trigger, else Claude Code's
documented default. Unresolved defaults, disabled
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
the same version of token-max installed with `cargo install`; token-max
installs nothing there itself. A missing or different version fails with the
command to run. Linux and macOS destinations are supported.

```sh
token-max --ssh devbox --ssh user@laptop --include-local
```

The destination runs `token-max --ssh-scan`, which discovers that host's own
profiles from its home and environment, reads its transcripts and settings,
and streams only the metadata the checks read, never prompt, reply or
component text. It looks for `token-max` on the non-interactive `PATH` and in
`~/.cargo/bin`. Prices and every check run on this host, with one price
catalog. `--claude-dir`, `--codex-dir` and `--t3-dir` select local profiles only, so with
`--ssh` they need `--include-local`.

Each host is analyzed on its own and gets its own section, labeled with the
destination, or `this host`. With `--ssh`, JSON is
`{"schemaVersion": 2, "hosts": [{"ssh": "devbox", "report": {...}}]}`, one
report per host and `ssh` null for this host. A history copied between hosts
counts in each host's report, so don't add amounts across hosts. Any failed
host, connection or stream fails the whole run with exit 2 and no report;
Ctrl-C stops every destination.

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

Reports count conditional requests separately from recorded tier coverage. Details
and JSON show standard, fast and ultrafast served-tier scenarios for
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
at each recorded interactive-terminal turn where they would have run, and their
uses count as one against `--max-cost-per-use`. Goal check-ins and Codex recaps
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
