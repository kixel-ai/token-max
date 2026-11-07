# token-max

Replay this host's Claude Code and Codex transcripts at public API prices and
recommend cheaper cache TTL, compaction-window, native Claude idle-compaction and standard-speed settings. Transcripts and settings stay read-only.
No prompt, reply, summary, tool argument, or tool output text is kept in the result
or printed. This is an API-price equivalent, not a subscription bill.

The working core uses Node-compatible TypeScript and Bun development tooling.
The planned npm package is `@kixelated/token-max`, with a `token-max` executable.
Publishing and npm artifact preparation have a separate quest; this repository
does not publish a package yet.

```sh
nix develop
quest gate just check
just run --since 2026-11-01
just run --claude-dir ~/.claude-work --details
just run --offline --plain
just run --json --claude-dir ~/.claude-work --codex-dir ~/.codex
just build
node dist/src/cli.js --help
```

`just check` installs the locked Bun dependencies, checks formatting and types,
builds JavaScript, runs the synthetic tests under Bun and Node, and checks quests.
Shipped code uses Node APIs and targets maintained Node LTS releases starting at
Node 22. The Nix shell pins Bun and Node 24. Tests use no real sessions or network.

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

Exit codes are `0` for no applicable change, `1` for a recommended change, and `2`
for a failed scan, price fetch, invalid argument, or cancellation. JSON errors go to stderr; a failed run emits no
success report or partial recommendations. Complete malformed selected records
fail with only a file and line. Unterminated final lines are deferred, with frozen
file lengths and deferred byte counts in JSON. There is no skip-malformed mode.

The default report starts with the selected window, profiles, usage coverage and
price catalog, then puts actionable changes before advisory findings and settings
to keep. Each card names the current and suggested values, owner, baseline,
estimated window/monthly net savings, and evidence. Unknown dollars stay unknown;
a known subtotal is not the total. Measured request usage is distinct from
estimated replay and unpriced provider guidance. Low-use rankings and unresolved
current ownership remain advisory; they do not cause exit 1.

Use `--details` for every check's assumptions, uncertainty, cost components,
break-evens, sensitivity scenarios, cache witnesses and sources. Nothing is hidden
behind an interactive prompt or a top-N limit. The layout wraps long paths and
names on narrow terminals. Redirected output keeps each field on one line so
paths remain copyable. Color supports the labels on terminals; `NO_COLOR`
disables color, and `--plain` also disables decorative Unicode. Redirected output
is plain, and progress appears only on interactive stderr. SIGINT or SIGTERM ends
with exit 2 and no success report; a repeated signal forces exit if I/O stalls.
Unicode decorations require a UTF-8 locale. Progress has phases, never an invented percent.

Illustrative excerpt from **synthetic** usage, with a synthetic owner and prices
(not a measurement of this host). The comparison spans 30 days, so the window
and 30-day figures agree:

```text
1 recommended change
Each check has its own baseline. Do not sum independent savings.

Recommended changes

CHANGE  claude-cache-ttl / main  [estimated]
  Profile: claude "/example/.claude"
  Owner: "/example/.claude/settings.json:promptCacheTtl"
  Current: 5m
  Suggested: 1h
  Reason: Lowest estimated aggregate cache cost under the stated replay
  assumptions.
  Baseline: current main TTL for /example/.claude
  Window: 2026-11-01T00:00:00.000Z -> 2026-12-01T00:00:00.000Z; 2 requests;
  available history; no recorded setting change, current settings applied to
  history
  Estimated net saving vs baseline: $1.50 over window; $1.50 / 30 days
  Replay cost: 5m $10.50; 1h $9.00
  Cache comparison: 5m $10.50; 1h $9.00
```

An advisory card instead says `ADVISORY` with its evidence label. Missing values
say `unknown`, including net savings when dollars cannot be established. A run
with no histories or no requests in the selected window gives a next step; no
applicable changes does not imply complete visibility or optimal settings.

Text and `--json` share the same analysis. JSON schema version 1 is unchanged and
includes profile aliases, snapshots, deduplicated requests, structural events,
usage coverage, pre-window cache context, partially known costs, summary estimates,
component measurements with exposure/invocation coverage, calibration provenance,
and check results. Each check has a `check` discriminator: `claude-cache-ttl`
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
assumptions, sources, and limits. Component measurements are shared infrastructure;
skills/context recommendations, other setting checks, and npm releases remain in [the first-release plan](quest/m0/README.md).

Unlinked API attempts stay in coverage. A TTL comparison may estimate input from
matching adjacent measured requests, including growing-context retry geometry
within one five-minute cache lifetime, with their IDs and assumptions reported;
original usage and historical billing stay unknown. Unsupported inputs leave the
affected comparison unpriced. Sensitivity scenarios accompany the aggregate
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
Unresolved named/legacy profile selection, project trust/overrides, custom
providers and unsupported models stay advisory. Unrecorded CLI/session overrides
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
