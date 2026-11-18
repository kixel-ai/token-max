# [M] Resolve CLAUDE_AUTOCOMPACT_PCT_OVERRIDE in the compaction window check

## Goal

The compaction window check replays from the trigger Claude Code actually
uses when `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` is set, and a recommendation that
changes the trigger removes the override and sets `autoCompactWindow` (or the
environment window that owns it) to the target, so one setting owns the
trigger. The doctor walk shows both edits before applying them.

## Plan

Found 2026-11-17 while comparing
[inevolin/claude-code-usage-analysis](https://github.com/inevolin/claude-code-usage-analysis),
which warns that the override thrashes on 200K-context models. `controls.rs`
resolves `CLAUDE_CODE_AUTO_COMPACT_WINDOW`, `autoCompactWindow` and
`modelSettings.<model>.autoCompactWindow`, but never reads the override; only
`t3.rs` lists it among the environment keys a T3 instance passes through.

Decided 2026-11-17 (maintainer, /quest-plan): ✅ resolve the override into the
baseline and recommend one knob, removing the override when the trigger
changes (rejected: keep both and adjust the window so window × percent hits
the target, which leaves the user doing arithmetic to read their settings;
advisory only while the override is set, which gives up a measurable answer).

Per the [environment variable docs](https://code.claude.com/docs/en/env-vars),
the override is a percentage (1-100) of the auto-compact window, can only
lower the trigger (values above the default percentage are ignored), applies
only to sessions that compact before the model's context limit, and applies
to subagents too. The default percentage is undocumented; the
[model config docs](https://code.claude.com/docs/en/model-config#default-auto-compact-thresholds)
give native 1M models a default trigger of about 967K. Recommended: where the
effective trigger depends on that undocumented default, use the observed
automatic-compaction trigger as the unset window already does, and leave the
row advisory when neither resolves it. Resolve the override from every
environment owner the window resolves from, T3 instance environments
included.

Removing the override is one edit that moves the trigger of every model the
owner runs, subagent models included, so the recommendation covers each
model the transcripts show under that owner: the target model gets its
target window, and every other model whose trigger the override lowered gets
a `modelSettings.<model>.autoCompactWindow` (or the T3 instance's
equivalent) at its current effective trigger, so its trigger doesn't move.
When any of those models' current trigger is unresolved, the row stays
advisory.

The edit removes the override from every scope that sets it, not only the
winning one, since a lower-precedence value would otherwise take over (a 50%
user override left behind a removed project one fires at half the new
window). An owner the walk can't edit, such as the shell environment that
launched Claude Code, keeps the row advisory and names that owner.

Transcripts don't record the override, so an observed trigger is evidence of
the current one only when it postdates the override's last change: use only
observations from sessions that started after the owning settings file was
last modified, and leave the row advisory when the owner has no such time,
such as the shell environment.

## Related

- [Extrapolate the idle compaction replays](/quest/m0/launch/extrapolate-idle.md) - the neighbouring compaction replay
