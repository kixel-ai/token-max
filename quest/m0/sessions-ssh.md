# [M] Export sessions from SSH hosts

## Goal

`token-max sessions --ssh HOST [--include-local]` prints the priced
sessions of every selected host as JSON lines, ordered across hosts by
first request as `SESSIONS_HELP` promises. Each line carries a new
`host` field: the destination as given to `--ssh`, or null for this host.
Every host is priced from this host's catalog, and the first failed host
fails the whole run, as `--ssh` reports do today. Only the session lines
cross SSH, never a scan or transcript text.

Every line, local or remote, also carries `repositories`, resolved on the
host where the session ran: the repository a Codex session recorded when
there is one, otherwise the remote URLs of the Git checkout now at the
session's `cwd`. It is null when neither exists, so a consumer never
resolves another host's path on its own. A `repositoriesSource` field
says which (`recorded` or `checkout`), since a checkout read at export
time may have been replaced since the session; a consumer treats
`checkout` as unverified evidence. Each URL is printed without its
userinfo, so a credential such as `https://user:token@host` never leaves
the host.

Out of scope: merging a session that appears on several hosts, such as a
history copied between them. Each host's copy is printed with its own
`host`, and the consumer decides what to count, as the report never adds
amounts across hosts.

## Plan

Decided 2026-11-12 (maintainer, /quest-plan, asked whether CLIProxyAPI's
request history could replace reading transcripts over SSH):

- Source? ✅ transcripts read on each host (rejected: the CLIProxyAPI
  gateway's usage records). The gateway keeps no history in v8 (its usage
  queue drops a record once read or after at most an hour), its records
  carry no session, working directory, branch or harness, and it sees only
  Claude traffic routed through it, not Codex or host logins.
- Scope? ✅ one `host` field per line, no cross-host merge (rejected:
  folding a session found on several hosts into one line). quest's
  `quest usage` counts such a session once and says so; see
  [its quest](https://github.com/moq-dev/quest/blob/main/quest/m0/routing-evidence/usage-token-max.md).
- Repository? quest matches sessions to PRs by repository and today reads
  the remotes of each `cwd` on its own host, which for another host's path
  is missing or a different checkout, so the host that ran a session
  resolves them (raised by quest's review of
  [moq-dev/quest#669](https://github.com/moq-dev/quest/pull/669)).
- Milestone? ✅ m0 (rejected: m1), because quest's m0 routing evidence
  compares cost per PR across every host.

Guidance: reuse the `--ssh-scan` stream in `src/ssh.rs` and `src/wire.rs`,
with the request naming the export instead of a report format, and
`--ssh` with directory flags needing `--include-local` as the report does.
Find the checkout containing `cwd` with `project_directories` and
`main_checkout` in `src/discovery.rs`, so a subdirectory or linked worktree
resolves, and read its remotes from Git config rather than running `git`.
Add `src/sessions.rs` to the stream fingerprint in `wire::build()`, since
the session lines now cross SSH.
Update `SESSIONS_HELP`, `DOCTOR_HELP`'s SSH line in `src/cli.rs`, the
`src/sessions.rs` module comment that session IDs never cross SSH, the
README's `sessions` line, the `sessions` and SSH sections of
`docs/reference.md`, and the SSH section of `docs/semantics.md`.
