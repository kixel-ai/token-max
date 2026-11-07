# [M] Polished terminal report

## Goal

Running `token-max` gives a clear, polished terminal report that puts useful
setting changes first, shows their cost evidence, and makes the next action
obvious. It remains readable in a narrow terminal, copied logs, and pipes;
`--json` remains a stable automation interface. Reporting never edits a
setting or exposes transcript content.

## Plan

Decided 2026-11-06 (maintainer, /quest-plan): include a "slick CLI" in the
first release. The existing replay quest supplies parsing, checks, flags,
and structured results; this quest owns presentation rather than another
analyzer. It joins m0 under the session's first-release priority. The
maintainer delegated packaging/language choices; use the TypeScript runtime
chosen for replay.

- Make the default a concise report with a restrained visual identity,
  clear spacing and typography, and color that supports readable text.
  Start with the analysis window, profiles, request coverage, and API-price
  framing, then actionable changes and advisory findings. Avoid a claimed
  total saving: checks have separate baselines and m0 forbids summing them.
- For each recommendation, make current and suggested values, the owning
  profile/file/tool, the reason, and estimated window/monthly net cost easy
  to scan. Keep measured usage, modelled estimates, and unpriced guidance
  visually and verbally distinct. Unknown is never formatted as zero.
  Show the comparison baseline, relevant coverage, assumptions, and
  break-evens in reachable text details as well as JSON. Advisory low-use
  rankings must not look like required changes.
- Use a layout that adapts to available width instead of truncating setting
  names, owner paths, or money. Color and Unicode are enhancements; support
  `NO_COLOR`, a plain rendering, and non-TTY output. No fullscreen mode or
  interactive prompt is needed to obtain a complete report.
- Show useful scanning/pricing/check phases on interactive terminals,
  without claiming a percentage where the total is unknown. Progress stays
  off stdout and is disabled for JSON and non-interactive output. End
  cleanly on cancellation; emit no success report or recommendation after
  a failed scan. Honor the shared exit codes rather than using presentation
  to change the checks' decisions.
- Keep `--help` approachable, including `--since`, profile selection,
  `--json`, how to view evidence, and exit codes. Missing histories, no
  selected requests, all-clear results, offline cached pricing, and errors
  need intentional output with a useful next action. Sanitize terminal
  control characters in metadata-derived labels and paths.
- Verify rendering with synthetic reports covering priced changes,
  guidance-only findings, unknowns, multiple profiles, long names, narrow
  terminals, plain output, JSON, and failed scans. Add representative README
  examples; no real transcript content or invented real-host savings.

## Required

- [Replay and the cache TTL check](/quest/m0/replay.md) - shared CLI flags, structured findings, evidence, and exit semantics
