# m0: first release

## Goal

`@kixelated/token-max` 0.1.0 is published on npm, and its README shows how to run it
with npx or bunx, without a global install or Rust toolchain. It replays selected
local and SSH-host Claude Code and Codex histories, including T3 provider instances,
against public API prices and reports which agent settings should change.
For each setting it prints the current value, the recommended one, the file
or tool that owns it, and the evidence. Where costs can be quantified, it
shows estimated net savings over the window and per month; otherwise it
gives an applicable provider-guidance warning and says the dollars are
unknown. It exits nonzero when anything should change and never edits
settings. moq-dev/quest's `SETUP.md` points to it.

## Plan

Decided 2026-11-05 (maintainer, /quest-plan in moq-dev/quest,
[moq-dev/quest#512](https://github.com/moq-dev/quest/pull/512) and
[#513](https://github.com/moq-dev/quest/pull/513)). The maintainer asked for
a command that "reads transcripts and determines if settings need to be
changed", such as whether sub-agents should use a 5m or 1h cache based on
public API pricing. It was first planned as `quest doctor`; the same day the
maintainer asked whether a tool separate from quest could "win some ez
internet points", and it became this standalone tool. moq-dev/quest's
token waste epic runs it for its closing re-measure.

Survey, 2026-11-05: no tool recommends settings from transcripts for both
harnesses, and none covers the compaction window or the Bash cap.

- [ccusage](https://github.com/ccusage/ccusage) (19.3k stars) reports usage
  and cost for about 18 harnesses but recommends nothing. It is Rust, but
  its crates are not on crates.io, and its JSON is per day and per session,
  without per-request times, context sizes, or the 5m/1h write split a
  replay needs.
- [cebert/cache-ttl-analyzer](https://github.com/cebert/cache-ttl-analyzer)
  replays one Claude session at 5m and 1h, main conversation only.
  [Jerry0022/dotclaude#613](https://github.com/Jerry0022/dotclaude/pull/613)
  is the same check as a prompt (5m cost them 24-38% more).
- [jdx/jouet](https://github.com/jdx/jouet) audits tokens and drafts agent
  rules, not settings. Claude-Code-Usage-Monitor and claude-cost-optimizer
  are a live monitor and cost guides.

Decision prompts, chosen options marked ✅:

- Checks in the first version: ✅ cache TTL, ✅ compaction window, ✅ Bash
  and wait caps (rejected: review efforts, which moq-dev/quest's model check
  owns). The skills catalog check is the agent's addition, and the keepalive
  check follows the maintainer's question about pinging idle sessions.
- Applying: ✅ recommend only (rejected: `--fix`). The maintainer's dotfiles
  rewrite the agent settings every hour, so a direct edit would be
  overwritten; the output names the owner instead.
- Where it lives: ✅ a standalone tool (rejected: `quest doctor`; a `doctor`
  command proposed to ccusage, which would reach its users at once but waits
  on its maintainer).
- Data: ✅ its own per-request transcript parser and
  [models.dev](https://models.dev) prices, the catalog ccusage also uses
  (rejected: ccusage's output, which lacks the per-request detail).
- Language: ✅ Node-compatible TypeScript, with Bun for development
  (supersedes Rust, 2026-11-06: the maintainer delegated the choice and
  explicitly allowed TypeScript; only a Rust stub exists). Distribution:
  ✅ npm through npx and bunx (rejected: requiring Cargo or a native-binary
  wrapper). Repository: ✅ kixelated. Name: ✅ token-max, the
  maintainer's pick (191 GitHub repositories share the name, and
  [Kalmantic/jusTokenMax](https://github.com/Kalmantic/jusTokenMax), a
  token-reduction toolkit, is close in name and topic).
- Quest wrapper: ✅ none (rejected: `quest doctor` calling token-max).

Expanded 2026-11-06 (maintainer, /quest-plan): ✅ include fast-mode,
background-generation, skills-usage, and MCP/loaded-context checks in m0
(rejected: m1). The goal is to quantify settings from transcripts where
possible and warn from Claude/Codex guidance where they cannot be priced.
Fast mode should be off. Extend the existing skills catalog quest rather
than create a second owner for it. Model and review effort selection remain
outside this expansion.

Also requested 2026-11-06: ✅ a polished terminal report and one-command npm
distribution in m0. Replay owns the TypeScript/tooling migration; the two
new quests separately own presentation and packaging. The npm identifier
is `@kixelated/token-max`; the command remains `token-max`. Keep the report
usable in scripts and avoid a second analysis implementation.

The epic's own remaining work is the end-to-end first-release validation,
publishing the complete 0.1.0 package after every child merges, verifying
the child's first-use docs against the published package, and clearing
Quest's release handoff. Packaging preparation supplies the built artifact, packed-consumer tests, and
[protected release path](../../docs/releasing.md). This epic still owns the
0.1.0 version change, publication, and published first-use verification after
all children merge. No package was published during preparation. Use the scoped package under the delegated distribution choice;
do not infer publishability from a registry 404.

Expanded 2026-11-07 (/quest-plan, then "afk spawn and merge whatever you want"):
✅ include T3-aware Claude/Codex profiles/settings and explicit SSH host audits
in m0. Reuse the existing analyzers and render one report with host/profile
ownership and proven cross-source deduplication. T3's own idle modes are
distinct from native Claude's. Other T3 providers and automatic discovery
from its encrypted desktop connection catalog are outside this scope.

Recommendations for every check:

- A replay assumes sessions would behave the same under the other setting.
  Recommend the lower estimated aggregate net cost, including extra work
  the setting would add. State the replay assumptions, show uncertainty
  and break-evens beside the estimate, and do not require a win on every
  period or in every sensitivity scenario. Missing inputs that cannot be
  estimated leave their own requests unpriced; a recommendation stands when
  those requests, each bounded by the largest measured difference, cannot
  flip it, and the report says what share was measured. Uncertainty alone
  never blocks one.
- Distinguish measured token usage, estimated attribution or replay, and
  guidance without dollars in both text and JSON. Missing attribution is
  unknown, never zero. Exit 2 on a failed scan, 1 when a change is
  recommended, and 0 otherwise. Emit a change when an applicable effective
  setting differs from a priced net winner, fast mode is enabled, the Bash
  foreground cap is at or above its effective cache TTL, or the known
  current ping policy differs from the estimated net winner among no
  keepalive, the caps, and idle compaction where supported. With a supported
  mechanism, this includes starting or changing a winning cap and stopping
  active pings when another policy wins. Unknown ping state or an unsupported
  mechanism stays advisory.
  Guidance also emits a change for a documented budget violation, duplicate
  listing, or a skill/MCP with positive recurring overhead and no observed
  use under complete exposure and invocation visibility across the window.
  Every automatic finding must still apply to the profile's current
  effective configuration; historical-only or unresolved current state is
  advisory. Unused findings for components introduced during the window
  stay advisory. Rare-use rankings, incomplete visibility, unknown costs
  alone, and conditional suggestions are advisory and exit zero unless
  another check emits a change. Cite the applicable provider/version
  guidance; no invented size or rarity cutoffs.
- Cost attribution is not automatically removable waste: a skill, tool, or
  check-in can avoid other work. State the fixed-behavior assumption and
  account for observable extra reads or requests. Name each comparison's
  baseline. Fast mode, background generation, skills, and context each
  compare with the recorded/current profile baseline, holding other
  settings constant; recommendations are not applied between these checks.
  The existing TTL, window, idle, and keepalive checks retain their stated
  evaluation order and label conditional baselines. m0 reports per-check
  savings only, with no summed or joint savings total. API-price equivalents
  are not a claim about subscription bills.
- Default to a window since the setting last changed, read from the
  transcripts, so old behavior does not argue against a setting already in
  place. If no change history exists, use explicit `--since` or the available
  transcript window and label the replay as applying current settings to
  historical sessions.
- Discover and report all transcript profiles, honoring `CLAUDE_CONFIG_DIR`
  and `CODEX_HOME` and explicit directory selection. Resolve settings and
  recommendations per profile, deduplicate shared histories, and never
  print prompt or reply text. Processing and token estimation stay local;
  analysis network access is limited to public price data and explicitly
  selected SSH audit destinations; package-manager
  installation downloads are separate. Do not upload
  transcript, instruction, skill, or schema content to a counting service.

## Waiting

- Maintainer: restore npm publisher authentication and verify authority for `@kixelated` before the first release. On 2026-11-07, `npm whoami` still returned E401; public packages suggest scope ownership but do not establish this session's authority. An agent can prepare and test without credentials, but cannot complete the account owner's login or grant itself rights. Check the authenticated identity and scope permissions up front. Configure trusted publishing near the actual release, respecting its first-publish expiry and whether it supports a new package; otherwise bootstrap by publishing the tested complete tarball with public access, then configure it for later releases. Follow [release preparation](../../docs/releasing.md): the main-only `npm` environment has been configured and verified by an agent, without a manual reviewer gate; bootstrap the tested complete tarball using environment-only authentication if needed, and configure OIDC near a subsequent release within its two-day validation window. An agent can automate protected execution, but cannot sign in as the owner or grant itself scope rights. No release was attempted.

Decided 2026-11-08 (maintainer, after the first run on the maintainer's
host): ✅ recommend from the measured requests when the missing ones cannot
flip the result (rejected: any unpriced request blocks the check; on that
host 40 of 17,029 requests blocked every check). ✅ skip and count unreadable
transcript lines (rejected: fail the run; the host has 36 truncated or
interleaved lines, several recent). ✅ price a custom `ANTHROPIC_BASE_URL` as
the public Anthropic API equivalent (rejected: unpriced). ✅ a default report
of one plain verdict per setting, with the evidence cards under `--details`.
✅ scale work in its own quest.

## Required

- [Background-generation check](/quest/m0/background-generation.md) - attributes recap, suggestion, and goal-check-in costs when observable, with guidance and explicit unknowns otherwise
- [Bash cap check](/quest/m0/bash-cap.md) - flags a Bash timeout cap at or above the cache TTL, and waits that hit the cap or poll
- [Skills catalog and usage check](/quest/m0/skills-catalog.md) - prices recurring listings, detects duplicates, and ranks unused or rarely used skills for trimming
- [MCP and loaded-context check](/quest/m0/context.md) - attributes exposed tool and instruction context, identifies unused MCPs, and gives guidance-backed trimming warnings
- [Keepalive check](/quest/m0/keepalive.md) - whether capped pings beat leaving sessions idle and, where supported, idle compaction after costs and client lifetime bounds
- [T3 profiles and settings](/quest/m0/t3.md) - native history discovery, T3 ownership, and T3-specific idle policies without double billing
- [SSH host audits](/quest/m0/ssh.md) - selected remote audits with the same artifact and a combined privacy-preserving report
- [Scan large histories](/quest/m0/scale.md) - a default run completes on a host with tens of gigabytes of transcripts, and `--json` and `--details` stay usable
