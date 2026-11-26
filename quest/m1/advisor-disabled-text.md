# [XS] Advisor line names the variable that blocks the tool

## Goal

When no `advisorModel` is set and something blocks the advisor tool, such as
`CLAUDE_CODE_DISABLE_ADVISOR_TOOL` or a provider with no advisor, the Advisor
line names that blocker. Today it says that a session's `--advisor` flag or
`/advisor` turns the advisor on, which isn't true while the blocker is in
place.

## Plan

Planned 2026-11-25 (maintainer, /quest-plan), from
[kixel-ai/token-max#258](https://github.com/kixel-ai/token-max/pull/258)'s
review notes: ✅ fix this text (rejected: drop it). ✅ Drop the other note,
that the compaction window and idle checks can show $0 for a linked summary
that kept only advisor calls (rejected: a quest for it). That case needs a
summary made only of advisor calls; reopen it if real data shows one.

The text is the `(None, _)` arm in `src/report.rs`'s advisor settings line. It
should use `control.blocked`, as the `(Some(m), Some(b))` arm does. List
#258 under `## Caused by`.
