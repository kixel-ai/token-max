# Token audit

Find where the user's Claude Code and Codex sessions spent tokens they need not have, rank the causes by dollars, and offer fixes the user approves one at a time. Settings such as the cache TTL, models, effort, tools and MCP servers belong to `token-max doctor`: name it in one line if one matters, without auditing them.

## Privacy

Transcripts hold the user's private prompts, replies and tool output, and anything you read stays in this session's history.

- Never read a transcript whole or print a raw transcript line. Read only the lines `token-max audit` points at, always through the filter `token-max skill filter` prints.
- When a finding can only be diagnosed from raw text, give its file and line for the user to read instead of reading it.
- Never copy secrets or transcript quotes into the report or a file; describe findings with counts and dollars.

## 1. Measure

Run `token-max audit` in the repository the user means. Pass `--all` when they ask about the whole host, `--repo DIR` for another checkout, `--since DATE` for a start they name, and `--ssh HOST` for each remote host they name. It prints one JSON object, small enough to read whole at the default `--limit`; `token-max audit --help` explains its fields.

## 2. Inspect

Read transcript lines only through the `jq` program `token-max skill filter` prints, without retyping or saving it. Read several windows of one file in one command, since a harness may ask before each:

```sh
sed -n '120,140p;300,310p' FILE | jq -c "$(token-max skill filter)"
ssh HOST "sed -n '120,140p' 'FILE'" | jq -c "$(token-max skill filter)"
```

Use the second form when a pointer's `host` is not null; the filter runs here, so the remote host needs no `jq`. The filter keeps record types, times, models, effort, tool names, each command's program, subcommand and flags, file paths, statuses, exit codes and sizes, and replaces all other text with its length.

Look at, in order of cost:

- each `cacheMisses` hotspot, from its `previous` line to its `at` line;
- each `toolOutputs` hotspot's `call` and `at` lines;
- a window around the `streak.at` of the `longestStreaks` and `mostToolErrors` sessions;
- the `earlyLines` of the `costliest` and `largestStart` sessions, for what each had to discover before working.

Keep windows to tens of lines and stop reading once a cause is clear.

## 3. Diagnose

- Cache busts. `tool_wait_ttl`: a command outlasted the cache; run it in the background, speed it up, or cap it. `idle_ttl`: the session sat past the cache's lifetime; when a long session resumes after a break, `/clear` with a short summary can cost less than rereading the whole history. `control_change`: a model, effort or speed switch mid-session rewrote the cache. `prefix_bust`: the prompt's prefix changed, such as an instruction or memory file edited mid-session, tools or MCP servers connecting, or a hook injecting changing text.
- Tool output bloat: large results that many later requests carried. Look for noisy build, test or log output (quiet flags, only failures, output written to a file and its tail read), whole files read where a range would do, and the same file read again.
- Loops: long runs of one tool, a failing command retried, repeated errors. Name what would have stopped them, such as a missing command in the instruction file, a broken script, or a hook.
- Unused context: a large `startInputTokens` is always-loaded instruction files, memory and tool definitions. Read the repository's `AGENTS.md`, `CLAUDE.md` and what they import: a rule the inspected sessions never needed can move to a skill, a nested `AGENTS.md` or a path-scoped rule, or be deleted.
- Missing context: what sessions rediscover in their first requests, such as the same build or test command found by trial, the same files read to learn the layout, or the same failure worked around. Check the instruction files first so nothing is duplicated; a line earns its place when it costs less than the rediscovery, since every session pays for it.

## 4. Report and fix

Rank findings by dollars. For each, give the cause, the evidence (sessions, events, tokens and dollars from `token-max audit`, marked measured; your own figures marked estimated), and one minimal fix: an instruction-file line to add or remove, a hook or script, a skill split, or a habit. End with what was audited (`totals`, the window and hosts) and what you did not inspect.

Then offer each fix that edits a file one at a time: show the change, and apply it only after the user agrees. Never edit a transcript.
