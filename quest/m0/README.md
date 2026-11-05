# m0: first release

## Goal

token-max 0.1.0 is published on crates.io, and its README shows how to
install and run it. It replays this host's Claude Code and Codex transcripts
against public API prices and reports which agent settings should change.
For each setting it prints the current value, the recommended one, the file
or tool that owns it, the evidence, and what the change would have saved
over the window and per month. It exits nonzero when anything should change
and never edits settings. moq-dev/quest's `SETUP.md` points to it.

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
  owns). The skills catalog check is the agent's addition.
- Applying: ✅ recommend only (rejected: `--fix`). The maintainer's dotfiles
  rewrite the agent settings every hour, so a direct edit would be
  overwritten; the output names the owner instead.
- Where it lives: ✅ a standalone tool (rejected: `quest doctor`; a `doctor`
  command proposed to ccusage, which would reach its users at once but waits
  on its maintainer).
- Data: ✅ its own per-request transcript parser and
  [models.dev](https://models.dev) prices, the catalog ccusage also uses
  (rejected: ccusage's output, which lacks the per-request detail).
- Language: ✅ Rust. Repository: ✅ kixelated. Name: ✅ token-max, the
  maintainer's pick (191 GitHub repositories share the name, and
  [Kalmantic/jusTokenMax](https://github.com/Kalmantic/jusTokenMax), a
  token-reduction toolkit, is close in name and topic).
- Quest wrapper: ✅ none (rejected: `quest doctor` calling token-max).

Recommendations for every check:

- A replay assumes sessions would behave the same under the other setting.
  Say so in the output, and recommend a change only when the saving clears
  that uncertainty.
- Default to a window since the setting last changed, read from the
  transcripts, so old behavior does not argue against a setting already in
  place.
- Read sessions where each CLI keeps them, honoring `CLAUDE_CONFIG_DIR` and
  `CODEX_HOME`, and never print prompt or reply text.

## Required

- [Replay and the cache TTL check](/quest/m0/replay.md) - the transcript reader, prices, report, and the first check, which reproduces the audit's cache figures
- [Compaction window check](/quest/m0/compaction.md) - recommends Claude Code's and Codex's auto-compact thresholds from the spend above each candidate
- [Bash cap check](/quest/m0/bash-cap.md) - flags a Bash timeout cap at or above the cache TTL, and waits that hit the cap or poll
- [Skills catalog check](/quest/m0/skills-catalog.md) - flags duplicate entries and a Codex skills catalog over its budget
