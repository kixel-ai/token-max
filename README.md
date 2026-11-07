# token-max

Replay this host's Claude Code and Codex transcripts at public API prices and
recommend cheaper cache TTL settings. Transcripts and settings stay read-only.
No prompt, reply, summary, tool argument, or tool output text is kept in the result
or printed. This is an API-price equivalent, not a subscription bill.

The working core uses Node-compatible TypeScript and Bun development tooling.
The planned npm package is `@kixelated/token-max`, with a `token-max` executable.
Publishing, npm artifact preparation, and the polished report have separate quests;
this repository does not publish a package yet.

```sh
nix develop
quest gate just check
just run --since 2026-11-01
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
both harnesses. Every discovered root's selection is reported. A missing or
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
for a failed scan or price fetch. JSON errors go to stderr; a failed run emits no
success report or partial recommendations. Complete malformed selected records
fail with only a file and line. Unterminated final lines are deferred, with frozen
file lengths and deferred byte counts in JSON. There is no skip-malformed mode.

The minimal report and `--json` share the same analysis. JSON schema version 1
includes profile aliases, snapshots, deduplicated requests, structural events,
usage coverage, partially known costs, summary estimates, and check results.
IDs are opaque hashes. Shared histories are read once and historical costs stay
in a shared bucket when profile provenance is ambiguous. A replay for an alias
names that profile and is conditional, not another copy of historical spend.
Independent check savings must not be summed.

See [billing and replay semantics](docs/semantics.md) for supported accounting,
assumptions, sources, and limits. Component exposure, other setting checks, terminal
presentation, and npm releases remain in [the first-release plan](quest/m0/README.md).

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
