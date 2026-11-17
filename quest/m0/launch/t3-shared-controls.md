# [M] Read shared settings for controls T3 leaves alone

## Goal

On a Claude home shared by native Claude Code and T3 Code, doctor reads a
control's current value from the shared settings whenever T3 does not
override that control: the prompt cache TTLs and every control that, like
them, T3 leaves to the home's settings. A few requests it cannot match to
native Claude Code or a T3 thread no longer turn the profile's current value
unknown, and the replay's winner is recommended like any other: a saving
over requests that were only partly attributable is extrapolated, with `~`
and its margin. A control T3 does override, through its environment, launch
arguments or its own setting such as `autoCompactWindow`, stays advisory
while ownership is unresolved.

## Plan

Decided 2026-11-17 (maintainer, /quest-plan after the four-host snapshot in
[kixel-ai/token-max#155](https://github.com/kixel-ai/token-max/pull/155)): ✅
the goal above (rejected: the cache TTLs only, which leaves the idle and
background lines on the same profile blocked for the same reason; also
resolving the controls T3 overrides, which would guess which value ran).

Found on the snapshot: one unmatched request in the window marks every
profile it could belong to as ambiguous (`t3.rs`, `ambiguous_profiles`),
which clears both TTLs, the idle and background controls, and the compaction
comparison. On fedora 36 subagent requests whose root session has no
transcript did that to `~/.claude`; on desktop 271,025 requests that predate
T3's oldest retained turn item did. Both TTL lines then read "not enough
evidence", leaving out the snapshot's largest finding: a 1h subagent TTL at
~$4,143 less over 30 days.

Recommended:

- Decide ambiguity per control, not per profile: a control resolves to one
  current value when every layer that could set it applies to every possible
  owner alike, that is the home's and the project's settings, with no T3
  override and no layer only one owner reads. Native runs can also take a
  control from a flag, `--settings` or a variable exported in the user's
  shell; where doctor can see such a layer for only one owner, the control
  stays unresolved. On fedora the `claudeAgent` instance sets no environment
  or launch arguments, only its home.
- Where the replay prices the owners apart, such as T3's own idle modes,
  keep the unmatched requests out of the per-owner comparison and
  extrapolate over them, as the unpriced ones are since
  [kixel-ai/token-max#143](https://github.com/kixel-ai/token-max/pull/143),
  and say on the line that this assumes the unmatched requests split between
  owners as the matched ones do, which the margin does not cover.
- This keeps m0's rule that unresolved current state is advisory; it only
  stops calling a current value unresolved when every possible owner reads
  the same one.
