# [S] Try and promote the token-audit skill

## Goal

The maintainer has run `/token-audit` on a real repository, and the skill is
promoted where they choose, or they decided not to promote it.

## Plan

The skill shipped in [kixel-ai/token-max#273](https://github.com/kixel-ai/token-max/pull/273)
(`token-max audit`), [#275](https://github.com/kixel-ai/token-max/pull/275)
(the skill, its filter and `token-max skill install`) and
[#278](https://github.com/kixel-ai/token-max/pull/278) (the `vibes` card).
`docs/reference.md#token-audit-skill` documents it, and the README's Run and
More sections carry one factual line each. The README has no section that
pitches it, since the maintainer writes that copy.

Notes for a README or site section, if the maintainer wants one:

- What it finds that `doctor` can't: cache busts from commands, bloated and
  repeated tool output, always-loaded context never used, context rediscovered.
- The privacy line: `token-max audit` prints only metadata (tool names,
  counts, times, prices, session IDs and directories) and
  `(host, file, line)` pointers, and the agent reads those lines through a
  filter that drops prompt, reply and tool text.
- How to get it: `token-max skill install` or `npx skills add`.
- Whether a run on the maintainer's hosts gives a figure worth quoting, such
  as the cache-miss dollars by cause.

The installed skill runs `token-max skill audit`, which 0.0.1 lacks, so it
works without `npx @kixel/crabx` only from
[Release 0.1.0](/quest/m1/release-0.1.md) on.

Decided 2026-11-27 (maintainer, /quest-run): ✅ keep this follow-up and
promote the skill in the README and on max.kixel.me (rejected: README only;
not yet; drop).

## Waiting

- Maintainer (action): after token-max 0.1.0 is released, run `token-max skill install`, ask Claude Code for `/token-audit` (or Codex for `$token-audit`) in a repository you work in, then promote it in the README and on max.kixel.me with your own copy. Report anything to change as a new quest. Only the maintainer can judge the findings and post as themselves.

## Required

- [Release 0.1.0](/quest/m1/release-0.1.md) - ships a `token-max` with `token-max skill`, which the installed skill runs
