# Live stats while doctor works

## Goal

A `doctor` run shows what it has found while it works, not only at the end:
the progress line counts up sessions, requests, tokens and spend as it reads,
then tallies the savings as it checks, and the walk opens with what the
current settings already save against the defaults. A run with nothing to
change still credits the settings already in place.

## Plan

Decided 2026-11-24 (maintainer, /quest-plan). The maintainer asked for "live
stats while parsing the files", "ideally savings too", and for the run to
"flag how much we're saving/spending versus the defaults already so even if
token-max doesn't flag anything new, it still gets some credit".

The credit already exists: [#129](https://github.com/kixel-ai/token-max/pull/129)
added the "Compared with defaults, your settings save about $945/mo" line
(`src/defaults.rs`), but on a terminal it printed only in the report after the
walk, where it was easy to miss, until it also opened the walk.

Measured 2026-11-24 on the maintainer's `fedora` host at main 3c30ba7
(`doctor --defaults --offline`, 9.3 GB, 338,298 requests in 6,747 sessions
over 14 days): the reading bar ran about 36s, then "Scanning local
histories..." spun 69s after reading ended, then "Checking settings..." 43s.
Nothing on the line moves for about two minutes after the bar fills.

Decision prompts, chosen options marked ✅:

- Scope: ✅ live counts while reading, a live savings tally while checking,
  and the credit at the walk's opening (rejected: live counts and the credit
  only, with no tally while checking; drop).
- Live figures: ✅ end on exactly the numbers the report's header and lines
  show (rejected: raw counts marked `~`, which overshoot shared histories and
  then disagree with the header).
- SSH hosts: ✅ add each host's totals to the line when its result arrives,
  as the header's one total across hosts does (rejected: this host only;
  stream progress from destinations, which changes the SSH wire for a
  progress line).
- Release: ✅ only the credit at the walk's opening gates 0.1.0,
  so 0.1.0 and the README's demo show the credit up front, and the live
  counters ship after it (rejected: gate on all three; gate on none). The
  site's demo runs `doctor --defaults`, which has no credit to show.
- The 69s after reading: ✅ its own quest (rejected: drop it).

The line is shared by `doctor` and `vibes` and stays on interactive terminals
only, as now. `vibes` gets the reading counts, for this host only under
`--ssh` since its replies carry no usage, and no savings tally. A change to doctor's terminal output reruns
`assets/demo/walk.tape` and `assets/demo/site.py`.
