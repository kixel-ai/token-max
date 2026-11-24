# Short doctor output

## Goal

`doctor`'s report and walk fit on about a screen: changes come in a few
sections, such as cache TTL, never-used tools, never-used skills, never-used
MCP servers and auto memory, each one row and one yes/no. Settings-file edits
show once, at the very end, and the diagnostic preamble moves to `--details`.
max.kixel.me's transcript, regenerated from that output, shrinks with it.
What is measured, the verdict rules and `vibes` don't change.

## Plan

Decided 2026-11-24 (maintainer, /quest-plan). The site's run of
`doctor --defaults --ssh desktop --ssh fedora` was 421 lines: per host about
30 lines of preamble, then one row per change, mostly a single skill or tool
(about 80 rows), each followed by its settings-file edit, then an EVERYTHING
ELSE section of 266 to 394 lines that the site cuts to one. The maintainer:
"nobody cares about the actual changes to the config file until the very
end. it's noise".

Decision prompts, chosen options marked ✅:

- Goal: ✅ sections, edits at the end, a short preamble, the site regenerated,
  in both the report and the walk (rejected: the report only; drop).
- Sections: ✅ by kind and reason, spanning profiles, shared by the walk and
  the report, both cache TTLs as one section and a setting with no kind its
  own (rejected: by kind only, which would let a yes to never-used features
  remove one in use; per profile, which repeats each section).
- EVERYTHING ELSE: ✅ one line per section, every line under `--details`,
  with AGENTS.md's "every setting and feature on its own line" amended to
  say so (rejected: only under `--details`; keep it).
- Preamble: ✅ the usage title, the headline and one dim line counting what
  was left out or assumed, pointing to `--details` (rejected: title and
  headline only, which hides what wasn't measured; trim the wording only).
- Several hosts: ✅ one merged report whose sections span hosts, asked once
  in the walk (rejected: a shorter report per host).
- Edits without a terminal: ✅ a closing section per file, one line per key
  with a list's members on one wrapped line, the exact JSON or TOML under
  `--details` (rejected: one summary line per file; every edit moved to the
  end). The walk's confirm diff is already the end, so it stays.
- Release: ✅ 0.1.0 waits for this epic, so the first release and the
  promoted page show the short output (rejected: ship 0.1.0 first).

Each child changing doctor's output or questions reruns
`assets/demo/walk.tape`; the release quest reruns `assets/demo/site.py`.
`site.py`'s results section parses each change row of two wide runs, by
host, profile and setting, to split the savings; each child that changes
those rows keeps that parse working, such as by reading `--details`, which
keeps member rows, so the release rerun neither fails its totals check nor
publishes a wrong breakdown.
