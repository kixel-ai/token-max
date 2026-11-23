# [M] Price turns that messages from other sessions start

## Goal

The report gives each Claude settings file a line for messages from your
other Claude Code sessions. It shows what the turns those messages started in
an idle session cost over the window, how many messages were delivered, the
current `crossSessionInbound` value and the file that sets it. The line is
evidence: it has no verdict, never changes the exit code, and `doctor` never
offers to change it.

## Plan

Decided 2026-11-23 (maintainer, /quest-plan after Claude Code's
[costs doc](https://code.claude.com/docs/en/costs#why-usage-climbs-in-a-long-session)).
That doc lists cross-session messages as a cause of usage climbing in a long
session: Claude Code delivers each one to an idle session as a new turn that
resends the full context, and a break past the cache TTL makes that turn a
cache miss. `crossSessionInbound: hold` shows the message as a notice that
reaches Claude only once you approve it, and `refuse` drops it
([settings reference](https://code.claude.com/docs/en/settings-reference#crosssessioninbound),
[cross-session messaging](https://code.claude.com/docs/en/cross-session-messaging#control-inbound-messages)).

- Cost basis: ✅ every request of each idle turn a delivered message started,
  since that turn is what `hold` removes (rejected: only the cache rebuild
  when a message wakes a session past its TTL, which is smaller but not what
  the setting removes). The work done in that turn is the capability
  traded, priced as a tool's or auto memory's would be.
- Verdict: ✅ evidence only, with no verdict and no offer (rejected: judged
  like a tool against `--max-cost-per-use` with `hold` offered; offering
  `refuse`). Orchestrators such as quest-run depend on these messages, so a
  high cost per message does not mean the messages should stop.

Recommendations:

- The reader ignores the marker today. A delivered message is a `user` record
  whose `origin.kind` is `"peer"`. Lines carrying that marker on 2026-11-23:
  desktop 7,901, nixos 267, fedora 54, kixtop 0. A forked transcript may
  repeat records, so dedupe these lines the way requests already are. Confirm
  the shape of an idle-notice delivery (`notify_when_idle`), which also starts
  a turn, by its keys only, never its text.
- A turn runs from the peer record to the next record that starts another
  turn. Count the subagent requests it spawns, since `hold` removes them too.
  Messages delivered between tool calls in a busy turn count as delivered,
  but their cost is not measured: say so on the line instead of pricing them.
- Resolve the value per the settings reference: managed settings, then
  `--settings`, then user settings, first value found. A project or local
  `hold` or `refuse` applies only when it is stricter. With no value set,
  Claude Code decides per message from the two sessions' permission-mode
  classes, so the line shows that default rather than `accept`.
- Name the setting the way other lines do and cite the two docs above.
  Codex has no equivalent, so its files get no line.

