# [M] Ask whether a Claude profile is on a plan or API billing

## Goal

When `token-max doctor` runs on a terminal and cannot tell whether a Claude
Code profile is paid by a Claude plan or by an API key or usage credits, it
asks once per profile, then prices that profile's main-agent cache TTL
against the one default that applies (1h on a plan, 5m on API billing)
instead of showing both defaults and "saves up to".

## Plan

Decided 2026-11-17 (maintainer, /quest-run: "Maybe we need to ask if
they're using a subscription or API keys?"). The main-agent TTL defaults to
1h on a Claude plan within its included usage and 5m on usage credits or an
API key; docs/reference.md says token-max can't tell which applies, so the
line names both.

- ✅ doctor asks (rejected: only the site shows both cases; a toggle on the
  site, which allows no JavaScript).

Recommended:

- Ask before the report, only on a terminal and only for a profile whose
  billing doctor cannot infer; without a terminal, keep today's line that
  names both defaults.
- Store nothing: ask again on the next run, so no state file can go stale
  when a profile changes billing.
- Fold the answer into the headline's comparison with the defaults and the
  `--defaults` view, which both price the main-agent TTL against its default.
