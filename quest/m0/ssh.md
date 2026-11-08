# [L] Read-only audits over SSH

## Goal

`token-max` audits explicitly selected SSH hosts with the same analyzer and
produces one report with host/profile ownership, coverage and cost evidence.
Conversation, instruction and schema text is processed on its owning host;
only sanitized derived metadata crosses the SSH connection. No remote
installation, daemon, setting change or transcript copy is required.

## Plan

Requested 2026-11-07 through /quest-plan and delegated by the maintainer's
"afk spawn and merge whatever you want" instruction. ✅ Add explicit SSH
selection to m0 (rejected: requiring automatic T3 connection discovery or
decrypting its saved catalog). Known feasibility targets are nixos, nixos2
and kixtop; earlier read-only probes found native histories there, but were
not complete billing audits or remote runtime checks.

- Support repeatable `--ssh HOST`, using existing OpenSSH configuration,
  authentication and host-key handling. With no host flags, preserve the
  current local default. Explicit SSH hosts select those remote audits;
  `--include-local` adds a local audit to the same report. Do not connect
  to every configured alias or turn discovery into connection approval.
  Make directory/profile overrides' destination scope unambiguous; local
  paths must not silently acquire a different remote interpretation.
- Preflight each selected destination's platform and the same-version
  token-max it will run (see Waiting), and report an unsupported platform
  or version clearly, without a silent execution fallback. POSIX SSH
  destinations cover the initial Linux/macOS use case; identify other
  remote platforms as unsupported. Require no runtime download, persistent
  agent or registry access on the destination.
- Run discovery, frozen native snapshots, any supported T3 metadata
  transaction and content-derived exposure extraction on the owning host.
  The coordinator performs all pricing and settings comparisons on the
  sanitized normalized metadata with one selected price catalog. Remote
  hosts do not fetch prices. Reuse the same readers/adapters and privacy
  rules; never expose auth/environment secrets or raw transcript/component
  text through the transport or diagnostics.
- Use a versioned, validated stream of derived scan/control metadata with
  backpressure. Do not buffer an entire SSH stdout stream. Delimit protocol
  errors from remote stderr and SSH/auth/runtime failures. Quote remote
  commands correctly and reject host/option injection. Cancellation stops
  and reaps all audit processes, with no success report after failure.
- Preserve host identity, per-host snapshot boundaries, selection/exclusion
  coverage and current-setting ownership in text/details/JSON. Namespace
  local cache histories by execution context; a shared path on two machines
  does not prove one cache or history. Deduplicate proven copied request
  identities across selected hosts, preserving all origins and ambiguous
  profile attribution. Conflicting copies do not silently become two bills.
  Keep per-setting savings non-additive and API-price framing explicit.
- A failed selected host or invalid selected usage fails the requested
  report atomically with exit 2 and safe host/location diagnostics.
  Unreadable lines are skipped and counted, as locally. Missing
  usage remains unknown under the existing policy. Do not silently drop an
  unreachable destination or emit a partially successful whole-host report.
- Test the actual remote mode and transport with synthetic remote subprocesses:
  platform selection, version mismatch, names/paths with spaces, duplicate
  copied histories, price consistency, host failures, protocol corruption,
  stderr noise, cancellation and absence of content/secrets. Where reachable, perform read-only smoke
  runs on the named hosts and state coverage; never claim a full cost audit
  from a failed or partial scan.

T3's desktop catalog is encrypted and contains credentials when decrypted.
Its `t3 environment list --json` instead lists server peers, not SSH aliases.
Neither is an automatic SSH target source for this quest. A supported,
secret-free T3 host export would be separate future work, not a first-release
prerequisite. Sources checked 2026-11-07:
[desktop catalog store](https://github.com/pingdotgg/t3code/blob/9c6429d74b59c7a06fb7357526457d192a744a9f/apps/desktop/src/app/DesktopConnectionCatalogStore.ts),
[environment CLI](https://github.com/pingdotgg/t3code/blob/9c6429d74b59c7a06fb7357526457d192a744a9f/apps/server/src/cli/environment.ts),
and [OpenSSH command handling](https://man.openbsd.org/ssh.1).

## Waiting

- Maintainer: the Rust rewrite (2026-11-08) removed the plan's streamed
  Node/Bun helper, so choose how a destination runs the analyzer.
  Recommended: copy the local binary over the SSH connection to a private
  temporary path when the destination's OS and architecture match, run it,
  and delete it afterwards, so nothing stays installed. The alternative,
  running a same-version `token-max` already installed with `cargo install`,
  breaks the "no remote installation" goal.

## Related

- [T3 profiles and settings](/quest/m0/t3.md) - automatically available to SSH audits once this shared adapter lands
