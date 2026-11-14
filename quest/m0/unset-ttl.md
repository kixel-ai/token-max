# [S] Recommend a main cache TTL while it is unset

## Goal

When a Claude profile leaves the main conversation's cache TTL unset,
`token-max doctor` still gives the TTL line a verdict from the replay, and
the Bash timeout cap line, which compares against that TTL, gets one too.

Claude Code picks an unset main TTL by how the request is paid for: 1h on
a Claude plan within its included usage, 5m on usage credits or an API key
([prompt caching](https://code.claude.com/docs/en/prompt-caching#which-ttl-each-request-gets)).
token-max never reads the plan, so today the current value is unknown and
both lines stay advisory. That is the usual state for the subscription
users token-max now speaks to first, so its headline check rarely gives
them a verdict.

## Plan

Found while framing the report for subscription users (the finished
`quest/m0/subscriptions.md`, in git history), which added an `Official:`
sentence naming both defaults to the unset TTL line.

Agent's recommendation, for the implementer to revisit:

- Pinning the replay's winner is right in both billing states: a 1h winner
  matches the plan's default and keeps 1h on usage credits, and a 5m winner
  replaces the plan's 1h. So when the replay decides, recommend setting
  `promptCacheTtl` to the winner as a change (exit 1), shown as
  `unset -> 1h`, and let `doctor` apply it like any other TTL change.
- When the replay is too close to call, say so beside both defaults, as now.
- For the Bash cap, compare against both possible TTLs and give a verdict
  only when both agree.
