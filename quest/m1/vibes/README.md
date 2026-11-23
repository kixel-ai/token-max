# Vibes: advice token-max can't measure

## Goal

`token-max vibes` is the home for advice that the transcripts can't price:
provider guidance, the maintainer's picks and settings whose saving is a
guess. It is labelled as unmeasured, walks its suggestions the way `doctor`
does and never sets `doctor`'s exit code. `doctor` stays purely measured and
offers `vibes` when its walk ends. Each piece of advice is its own quest in
this epic, and more are expected.

## Plan

Decided 2026-11-23 (maintainer, /quest-plan after asking whether the Bash
timeout cap's 4m/55m targets are measured). They aren't: `safe_cap` in
`src/bash.rs` is a guessed margin, yet the line gives a red change and exits
1, which [#192](https://github.com/kixel-ai/token-max/pull/192) missed when it
made every other line measured. This supersedes the same day's
"✅ no `vibes` command" entry in [m0](/quest/m0/README.md). The maintainer
wants a place for advice that measurement can't back, and has more of it to
add than the Warnings section holds.

Decision prompts, chosen options marked ✅:

- Shape: ✅ a `vibes` subcommand that `doctor` offers at the end (rejected:
  a second, labelled phase inside `doctor`; drop the idea).
- Against the morning's decision: ✅ build `vibes` (rejected: keep it
  rejected and move the Bash cap into doctor's Warnings as a no-verdict
  warning).
- Its first items: ✅ the Bash cap and today's Warnings, in
  [the command quest](/quest/m1/vibes/command.md); ✅
  [a default main-session model and effort](/quest/m1/vibes/default-model.md);
  ✅ [Explore on Haiku](/quest/m1/vibes/explore-haiku.md) (rejected for now:
  cache habits from the 2026-11-17 study, which can return as their own quest
  here).

Recommendation, not asked: each `vibes` card names its source, either a
cited provider page or "the maintainer's pick", so a reader can tell whose
advice it is.
