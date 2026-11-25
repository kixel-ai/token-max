# [S] Count the auto memory card's sessions by T3's settled owner

## Goal

The `Auto memory` vibes card's share of a Claude line's main sessions that ran
in a repository counts each session for the line that ran it. A session a T3
thread ran, sharing the native profile's `~/.claude` history, no longer counts
toward the native line, and a session T3 could not settle is neither silently
dropped nor counted twice. The card never says no session ran when unsettled
ones did.

## Plan

Decided 2026-11-25 (maintainer, /quest-run): ✅ keep it (rejected: drop it).

Today `memory::check` counts every request naming one of the line's profiles,
before T3's ownership is considered, while the setting itself is read at the
profile's own settled latest directory (`discovery::owned_by`, from
[#217](https://github.com/kixel-ai/token-max/pull/217)). A first attempt in
#217 filtered the sessions with `owned_by`, which dropped unsettled sessions
whose candidates were all on the line and could report "0 of 0 sessions"; it
was reverted.

Recommendation: count a session for the line when T3 settled it to one of the
line's profiles, or when every candidate owner of an unsettled one is on the
line. Leave out the rest, and say how many were left out, as the card already
does for sessions with no recorded directory. Add a unit test with a settled,
a shared and an all-on-the-line unsettled session.
