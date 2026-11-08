# token-max

Replay this host's Claude Code and Codex transcripts at public API prices and
recommend cheaper cache TTL, compaction-window, native Claude idle-compaction and standard-speed settings. Transcripts and settings stay read-only.
No prompt, reply, summary, tool argument, or tool output text is kept in the result
or printed. This is an API-price equivalent, not a subscription bill.

The first npm release is being prepared; the commands below become available
when the complete 0.1.0 release is published. The development package retains
`private: true` until m0 prepares that release. See the
[first-release plan](https://github.com/kixelated/token-max/blob/main/quest/m0/README.md).

With Node.js 22, 24, or 26 LTS and npm:

```sh
npx --yes @kixelated/token-max@latest --help
npx --yes @kixelated/token-max@latest --since 2026-11-01
npx --yes @kixelated/token-max@latest --json --claude-dir ~/.claude-work
```

With Bun 1.4.4 (no Node installation needed):

```sh
bunx --bun @kixelated/token-max@latest --help
bunx --bun @kixelated/token-max@latest --offline --plain
```

Put `--bun` before the package name: Bun otherwise respects the executable's
Node shebang and tries to start Node. Both launchers download the npm package
and cache it; first use requires registry access. `@latest` can check the
registry again. This is separate from the analyzer's public-price fetch, and
`--offline` controls prices only, not package-manager downloads. See
[npx caching](https://docs.npmjs.com/cli/v11/commands/npm-exec/#a-note-on-caching)
and [bunx execution](https://bun.com/docs/pm/bunx).

Pin a published version for repeatable execution (0.1.0 is the planned first
release, not yet published):

```sh
npx --yes @kixelated/token-max@0.1.0 --details
bunx --bun @kixelated/token-max@0.1.0 --details
```

For frequent use, an optional global install provides the same command:

```sh
npm install --global @kixelated/token-max@0.1.0
token-max --version
token-max --offline --plain
```

The package contains built JavaScript with its dependencies bundled and their
licenses included. It requires no consumer compilation, install scripts, native
binary downloads, or Rust toolchain. The CLI receives the same arguments and
working directory whether launched directly, with npx, or with bunx. Relative
paths resolve from that directory. Reports go to stdout; diagnostics go to
stderr; exit codes remain 0/1/2 as described below. Launchers may also print
their own download diagnostics to stderr. Ctrl-C cancels the running analysis;
the CLI handles SIGINT/SIGTERM with exit 2 and no success report. The launcher
may report its own termination status when it is also interrupted.

The package CI exercises the same tarball on Linux, macOS, and Windows under
Node 22/24/26 and Bun 1.4.4, including a PATH with Bun alone. Until those matrix
jobs pass, OS/runtime coverage is pending, not a verified support claim.

For development:

```sh
nix develop
quest gate just check
just run --since 2026-11-01
just build
node dist/cli.js --help
```

`just check` installs locked dependencies, checks formatting and types, builds
JavaScript, runs synthetic tests under Bun and Node, packs the allowlisted
artifact, exercises clean consumers, and checks quests. The Nix shell pins Bun
and Node 24. Analysis tests use synthetic histories and local prices only;
launcher tests use a local fixture registry without publishing. See
[release preparation](https://github.com/kixelated/token-max/blob/main/docs/releasing.md)
for protected publication and remaining owner setup.

Without directory flags, discovery includes `~/.claude`, `~/.codex`,
`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, and recognizable `~/.claude*` and `~/.codex*`
profiles. Codex includes archived sessions; Claude includes nested subagents.
Repeatable `--claude-dir` and `--codex-dir` flags select only those roots across
both harnesses. `--details` and JSON show every discovered root's selection. A missing or
inaccessible explicitly selected root fails the run. Missing default roots do not.

The first online run fetches only `https://models.dev/api.json`, caching validated
prices at `~/.cache/token-max/models.json` (or under `XDG_CACHE_HOME`).
Use `--offline` for that cache, `--price-cache FILE` to choose its location, or
`--prices-file FILE` for a local models.dev catalog with no network access.
Network failures fail the run; offline use is explicit. Cached prices include
their file timestamp, so an old catalog remains visible.

`--since YYYY-MM-DD` means midnight UTC; ISO instants must include a timezone.
Without it, each Claude bucket starts at its latest observed write-TTL transition,
or the available history when no transition is recorded. A transition can also be
a billing or agent-policy change. Reports label today's settings applied to
history and do not invent historical settings. Recommendations name the owning
setting or environment override and never apply it.

Exit codes are `0` for no applicable change, `1` for a recommended setting or
feature change, and `2`
for a failed scan, price fetch, invalid argument, or cancellation. JSON errors go to stderr; a failed run emits no
success report or partial recommendations. Invalid selected usage fails with only
a file and line. Complete lines that are not JSON objects, which harnesses leave
after crashes and concurrent writes, are skipped and counted in the report and in
JSON `coverage.skippedLines`; their usage counts as missing. Unterminated final
lines are deferred, with frozen file lengths and deferred byte counts in JSON.

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
are kept. A skill's uses count only the model's own calls, because hiding it
with `skillOverrides` set to `user-invocable-only` still lets you type `/name`.
Instruction files and the system prompt, core tools such as Bash and Read, and
the skill and subagent lists have no switch and are listed after the verdicts.
The report ends with one `settings.json` snippet per settings file holding every
recommended change, such as `"enableArtifact": false` or a bare tool name in
`permissions.deny`; merge it by hand. `--details` lists every piece, including
those no longer sent.

An auto-compact window without enough evidence also shows how large that
model's requests got (median and 90th percentile), to judge whether a smaller
window would matter.

Each piece is sized from the request shapes Claude Code records (tools, then the
system prompt, then messages) and converted to tokens with a characters-to-tokens
ratio measured from the same transcripts. It is priced at the cache position it
occupied in that request: cached read, 1h write, 5m write, then ordinary input.
Attachments have no recorded position, so each is placed as late as its history
allows. Requests that could not be priced are estimated from those that could,
and a piece with none is shown as unknown.
Removing a piece can also shift later cache writes and compaction, which this
does not model. Requests without a recorded shape, and Codex requests, are not
priced per piece.

Comparisons use the requests priced under both options. Requests with missing
usage are left out of both sides, and a recommendation is made only when they
could not flip it, assuming none differs by more than the largest measured
difference. The verdict then says what share of requests was measured.

Use `--details` for the full evidence cards: every check's owner, baseline,
assumptions, uncertainty, cost components,
break-evens, sensitivity scenarios, cache witnesses and sources. Nothing is hidden
behind an interactive prompt or a top-N limit. The layout wraps long paths and
names on narrow terminals. Redirected output keeps each field on one line so
paths remain copyable. Color supports the labels on terminals; `NO_COLOR`
disables color, and `--plain` also disables decorative Unicode. Redirected output
is plain, and progress appears only on interactive stderr. SIGINT or SIGTERM ends
with exit 2 and no success report; a repeated signal forces exit if I/O stalls.
Unicode decorations require a UTF-8 locale. Progress has phases, never an invented percent.

Illustrative excerpt from **synthetic** usage, with a synthetic owner and prices
(not a measurement of this host):

```text
token-max
Estimated with public API prices, not your subscription bill. Nothing was changed.

Read 2,000 requests from 2026-11-01 to 2026-12-01 (30 days):
  claude ~/.claude

3 setting changes recommended.
Dollar amounts are per 30 days at this window's rate. A tool, skill or MCP server is worth turning off when it cost more than $1 per use (--max-cost-per-use).

Change
  Main-agent prompt cache TTL: 5m -> 1h. Saves $150. Set in ~/.claude/settings.json:promptCacheTtl.
  Artifact tool: on -> off. Costs $90; never used.
  Skill example: on -> only when you type /example. Costs $12; the model used it 2 times ($6 each).

Keep
  Fast mode: off.
  Agent tool: on. Costs $40; used 310 times ($0.13 each).

Not enough evidence
  Idle compaction: on. No idle gaps could be compared with it on and off.

No setting turns these off
  ~/.claude/CLAUDE.md: costs $5. Shorten it, or move parts into path-scoped rules or skills.
  System prompt, core tools such as Bash and Read, and the skill and subagent lists: cost $210.

To turn these off, merge this into ~/.claude/settings.json, adding to any lists already there:
  {
    "enableArtifact": false,
    "skillOverrides": {
      "example": "user-invocable-only"
    }
  }
```

A run with no histories or no requests in the selected window gives a next step;
no applicable changes does not imply complete visibility or optimal settings.

Text and `--json` share the same analysis. JSON schema version 1 includes profile aliases, snapshots, deduplicated requests, structural events,
usage coverage, pre-window cache context, partially known costs, summary estimates,
component measurements with exposure/invocation coverage, calibration provenance,
per-piece context costs in `context`, and check results. Each check has a `check` discriminator: `claude-cache-ttl`
retains its TTL-specific fields, while `compaction-window` includes candidate
windows, modeled summaries, prefix charges and sensitivity. `idle-compaction` adds
current-settings and post-recommendation on/off costs, request/final periods,
activation cohorts, context-size costs and lifetime/summary/timing sensitivities. TypeScript consumers
should narrow `CheckResult` by `check` before reading setting-specific fields.
IDs are opaque hashes. Shared histories are read once and historical costs stay
in a shared bucket when profile provenance is ambiguous. A replay for an alias
names that profile and is conditional, not another copy of historical spend.
Independent check savings must not be summed.

See [billing and replay semantics](docs/semantics.md) for supported accounting,
assumptions, sources, and limits. Remaining setting checks are in the
[first-release plan](https://github.com/kixelated/token-max/blob/main/quest/m0/README.md).

Unlinked API attempts stay in coverage. A TTL comparison may estimate input from
matching adjacent measured requests, including growing-context retry geometry
within one five-minute cache lifetime, with their IDs and assumptions reported;
original usage and historical billing stay unknown. Unsupported inputs leave their
own parts unpriced. Sensitivity scenarios accompany the aggregate
winner, and unknown fixed charges can cancel without pretending they cost zero.

Cache replay preserves known, estimated and unknown predecessor states, including
unsupported attempts before the selected window. Missing cohort metadata spans
compatible histories; JSON and text retain predecessor witnesses. A comparison
stays unpriced when those histories imply different cache charges, while invariant
charges can still be compared.

Compaction-window comparisons run after the recommended TTLs and use the current
explicit window as their baseline. Claude controls resolve environment overrides,
managed and user settings, including `modelSettings.<model>.autoCompactWindow`;
Codex uses `model_auto_compact_token_limit`. Unresolved tuned defaults, disabled
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

Idle compaction compares on and off over the same window, first with current
controls and then after applicable TTL and window changes. The latter aggregate
estimate decides: positive on-minus-off cost favors off, negative favors on, and
a zero tie retains the current toggle. Final periods incur expected summary cost
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
