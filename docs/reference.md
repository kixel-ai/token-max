# Reference

Every flag, report section and check. [Replay semantics](semantics.md) covers
the accounting underneath.

## Running it

`token-max doctor` prints the report, then, when stdin and stdout are both a
terminal, walks through its changes. [`token-max vibes`](#vibes) prints the
advice the transcripts can't price, which `doctor` leaves out. Bare
`token-max` prints the commands and
exits 2; options follow a command. Relative paths resolve from the working
directory. Reports go to stdout; diagnostics go to stderr; exit codes are as
described below. Ctrl-C cancels the running analysis: SIGINT or SIGTERM exits 2
with no success report.

`curl -LsSf https://max.kixel.me/run.sh | sh` and, on Windows,
`irm https://max.kixel.me/run.ps1 | iex` run `token-max doctor` from the
latest release without installing it. Each downloads the release archive for
this platform, refuses it unless it matches the release's `.sha256` file, and
keeps the binary in `token-max/run` under `$XDG_CACHE_HOME` or `~/.cache`,
beside the price cache. Later runs reuse it while it is still the latest
release; a newer one is downloaded beside it, and deleting the directory
reclaims the space. `run.sh` hands `doctor` the
terminal from `/dev/tty`, since its stdin is the piped script, so the changes
are still offered. Options go after `sh -s --`, as in
`curl -LsSf https://max.kixel.me/run.sh | sh -s -- --since 2026-11-01`, or
in PowerShell,
`& ([scriptblock]::Create((irm https://max.kixel.me/run.ps1))) --since 2026-11-01`.
`install.sh` and `install.ps1` redirect to the release's own installers.

### Newer releases

Claude Code and Codex change fast enough that an older build's advice can be
out of date, so `doctor` first asks
`https://github.com/kixel-ai/token-max/releases/latest` for the latest
release, as `run.sh` does, alongside finding this host's transcripts on a
terminal and alongside the whole scan otherwise. Only `doctor` checks, and
`--offline` skips it. No answer is remembered, so a
declined release is offered again on the next run. The check gives up after 5
seconds; when it cannot run, doctor says so on one line and goes on with this
build.

When the latest release is newer than this build, a terminal run asks before
its opening: `Upgrade to v0.0.2 and rerun?`. A yes downloads that release's
archive for this platform, refuses it unless it matches the release's
`.sha256` file, unpacks the binary with `tar` (Windows' own `tar.exe` there),
and checks that it runs and reports that version. Only then does it rename
the new binary over the running one, from the same directory, and run it with
the same options. A failure before the rename leaves the old binary as it
was, and this run goes on with it. A rerun that cannot start leaves the new
binary installed and ends the run with its error, asking to run doctor again,
since the old binary is gone. Windows cannot replace a running
program, so there the old binary moves aside to `token-max.exe.old` first,
and a later run deletes it. Ctrl-C waits for the rename, then ends the run
without rerunning. A download that stops answering for 30 seconds fails
the upgrade. With `--defaults`, which changes nothing, doctor only says to run
it without `--defaults` to upgrade.

A binary under `/nix/store`, or in Cargo's `bin` directory (`$CARGO_HOME/bin`
or `~/.cargo/bin`), belongs to that tool, so doctor prints the command that
upgrades it instead of asking; so does one in the run scripts' cache, which
names each binary for its release. A binary in a directory this user cannot
write, such as a root-owned `/usr/local/bin`, or on a platform with no
prebuilt release, gets a link to the install methods instead. Telling that
directory apart means creating and deleting a file in it, so only a terminal
run about to ask does. Without a terminal, the same news is one line on
stderr, which says to run doctor on a terminal to upgrade a binary it might
replace. A declined or failed upgrade
changes neither the report nor the exit code. `run.sh` and `run.ps1` always
run the latest release, so they never see the offer.

## Applying changes

On a terminal, `doctor` opens with one header line naming what it read: the
requests, the sessions they ran in (each subagent counted with the session
that spawned it), the window and their cost. It then walks through each change it can
make, after a headline counting its questions and the changes they cover,
across every settings file, recommended and worth considering alike, biggest
monthly saving first. Each change is a card. Its first line is a counter, such
as `[1/4]`, and the setting's name, with its saving per 30 days in dollars and
as a percent of this host's spend over the window (the base the report's
percents use) at the right. Next comes the current value and the offered one,
such as `5m -> 1h`, with whether it is recommended or worth considering.
Then `What` gives one plain sentence on what the setting or
feature does (see [What each line does](#what-each-line-does)), and `Why`
gives what was measured, such as the requests a replay covered or a tool's
uses and what each cost, and why the change makes sense. A card leaves out the
edit, which repeats the value change; the confirm shows every accepted edit
once, as a diff. A change whose saving was not measured, such as turning off
a Codex catalog skill, comes after the measured ones and says so.

When the walk ends, and [`vibes`](#vibes) has advice for these profiles,
`doctor` asks whether to show it. A yes walks through `vibes`' cards from the
same scan, with nothing read or asked again, and with its own confirm before
anything more is written. Without a terminal, the report ends with one line
naming `token-max vibes`.

A tool, skill or MCP server's change joins a group of its kind and reason, such
as never-used skills or MCP servers over $1 per use, whatever it saves. The
reasons are never used, no use recorded where the window cannot rule one out,
over `--max-cost-per-use` per use, and a redundant copy of a Codex catalog
skill whose most-used copy stays. Each reason is its own group, so a yes to
never-used features never also removes one in use. A group is one question
ranked by its combined saving, which names every member and takes `y` for all,
`n` for none, `q` to quit or `s` to split it and answer each member, as
`git add -p` splits a hunk; a group of one is asked as that change. Features whose
savings were not measured, such as Codex catalog skills, are grouped the same
way after the measured questions. A group saving under $1/mo in total,
including a lone small change such as a check, is not asked: one dim line says
how many changes were left out, and the list after the walk shows them among
those not accepted.

Each question takes `y`, `n` or `q`, and a group also `s`; anything else asks
again. An answer only selects: after each yes, a running total sums the
accepted changes' own savings, in dollars and as a percent of this host's
spend, and says that overlapping changes may save less, since each saving holds
every other setting constant. An accepted change whose saving was not measured
is counted beside the total, not in it, and so is one that saves only when an
unset default would otherwise be the other value, such as an unset main-agent
cache TTL, as an "up to" figure. A saving that leaves part of its cost
unmeasured, such as Codex memory generation, counts what was measured, and the
total says so. A `q`, or end of input, ends the walk and leaves the rest
unaccepted.

The report follows, its list grouped: the accepted changes first with the
total, then the ones not accepted, then the steps to take by hand, such as a
change something else would override, each saying why, then every other line,
each group ranked as the report ranks it. Then the confirm shows how each settings file would change, as
a diff with a line of context around each change, removed lines in red and
added ones in green. Values that may be secrets show as `••••` (`****`
without Unicode), keeping their keys: each variable or header of an MCP server
(`env` and `headers` in Claude Code, `env` and `http_headers` in Codex), each
variable in a Claude settings file's `env` block that no accepted change sets,
and each in Codex's `shell_environment_policy.set`. Then one question asks whether to write the accepted
changes, naming each file; nothing is written before that yes, end of input is a no,
and a no says nothing was written. Each file is then written once, with every
edit accepted for it. A change not accepted keeps its edit in the list, to
make by hand, which also suits a file another tool rewrites, such as a dotfiles
manager. Either answer exits 0.

Only a profile's own `settings.json` or Codex `config.toml` is edited,
including Claude's `env` block. An edit sets the setting that decides the
current value, as its line's `Set in` names it, so a change is applied only
when that owner is the profile's file, or the default no file sets. A line
that covers several profiles, such as native Claude Code and a T3 instance
sharing `~/.claude`, is one edit to the file they share. A tool, skill or MCP
line whose requests were copied into the histories of profiles with different
files, so any of them could have made them, is a step by hand naming each
file. A shell
variable, a project, local or managed settings file, or T3 that owns it makes
the change a step by hand naming that owner, and so does a Codex skill whose
`SKILL.md` path the catalog does not resolve. A tool, skill or MCP switch is
left by hand when a file that outranks the profile's for some recorded session,
such as a project's settings, a managed file or a named Codex profile, sets the
same switch; a Claude `permissions.deny` rule adds to every file's rules, so it
is always applied. Auto memory is left by hand when a named profile or a project
whose trust is unrecorded may set it, naming those files. Two lines that want
one setting at different values, in one file or two names a symlink makes one
file, leave it by hand too.

The edit keeps the file's other settings and their order. `settings.json` is
rewritten as indented JSON with the file's indentation, the form Claude Code
writes it in; `config.toml` keeps its comments and formatting. A list gains the
value once, and a Codex `[[skills.config]]` entry with the same `path` is
updated in place. Each file is read when the walk starts and again before
writing; if it changed in between, nothing is written and the run fails. A
file is replaced atomically with its owner, group and permission bits kept, and
its replacement is never readable by more people than the file was; a new file
is readable only by you. Other access controls, such as an ACL, are not copied.
The file is checked once more just before it is replaced. A symlink is written through to its target, which the summary names, and
profiles whose files resolve to one file share one write. A read-only target, or one
that cannot take the edit, such as a `permissions` that is not an object, makes
the change a step by hand naming the resolved path. Ctrl-C at any question,
the last included, exits 2 with no settings changed.

### Backups

Before replacing a settings file, `doctor` copies it as it was to
`<file>.token-max.bak` beside it (beside the target of a symlink), so the last
accepted change is undone by copying the backup back. There is one backup per
file, overwritten on each write; an older change is not kept. The copy has the
file's owner and permissions, since these files can hold secrets such as an MCP
server's `env`. The confirm names each backup before the question, and the
closing line names each one written.

A file git tracks with no uncommitted changes gets no backup, since a `.bak`
beside a project's `.claude/settings.json` would show up to be committed; the
confirm says git keeps its previous version. A tracked file with uncommitted
changes, a tracked one marked `skip-worktree` or `assume-unchanged` (whose
changes git does not show), an untracked one, or any file when `git` is not
installed is backed up. A file that does not exist yet has nothing to back up.

Every backup is staged as a temporary sibling before any settings file is
written, and each replaces the earlier `.bak` only once its own file is
written. A failure to stage one writes nothing and leaves every earlier backup
as it was; the run fails with the reason. If the last step fails after its file
was written, such as a directory sitting at the `.bak` path, the error names the
staged copy, which is kept as the file's previous text. For `~/.claude.json`, the backup
holds the text re-read under Claude Code's lock, the text the removal was made
to, not what the walk read at the start.

### Removing MCP servers

Turning a never-used MCP server off leaves it configured. After a yes to
one, or to a group of them, the walk offers to remove each one's configuration
too, in one question for the group and one for each server answered alone. It
names the entry and file: a Claude Code user-scope `mcpServers.<name>` or a
local-scope `projects.<path>.mcpServers.<name>` in `~/.claude.json` (or
`.claude.json` inside the directory a `CLAUDE_CONFIG_DIR` names, even
`~/.claude`), or a Codex `[mcp_servers.<id>]` table, with its subtables, in
`config.toml`. Comments directly above a removed table go with it. A Codex
server that a project's or named profile's configuration also declares is a
step by hand, since that layer may rely on the rest of the user's table, and
so is one the system `/etc/codex/config.toml` declares, which would start
again once the user's table turning it off is gone. So is a table the user's
`config.toml` no longer declares when the walk runs. Where token-max cannot remove a
server, it names the step to take by hand instead of asking: a
project's `.mcp.json`, which everyone using the project shares, a plugin's
server (`plugin_*`), a claude.ai connector (`claude_ai_*`), or a server it
found in none of those files, such as one from `--mcp-config` or
`managed-mcp.json`.

The last question offers to remove servers that are off already, as cleanup
that saves nothing: a Claude Code server whose tools the user's
`settings.json` denies with `mcp__<name>` or `mcp__<name>__*`, or a
local-scope server turned off in its project with `/mcp`
(`disabledMcpServers`), and a Codex server with `enabled = false`. A server
with only some tools denied is not offered, nor a Codex server another layer
also declares, since that layer may turn it back on. The confirm's diff shows every
line it removes, with secret values masked. Claude Code rewrites `~/.claude.json` while it runs, so that file is
not refused when it changed during the walk, unless an entry it would remove
now holds something other than what the diff showed. It is written before any other
file, under Claude Code's own lock (`~/.claude.json.lock`), taken before any
backup is staged and held until it is replaced and its backup committed, and the removal is made to its
current text. A lock held for over 5 seconds stops the run before anything is
written. A stale lock, one not refreshed for 10 seconds, is not taken over:
the error asks to remove it if no Claude Code is running, since Claude Code
takes it over itself. Claude Code re-reads the file under that lock before each save,
so the removal survives sessions already running, which keep the server until
they restart.

Without a terminal, `doctor` changes nothing: each change's line ends with its
edit and file, or its step by hand.

### On SSH hosts

With `--ssh`, the walk offers every host's changes in one list, biggest saving
first across hosts. A card for another host's change, or any card when the run
covers several hosts, names its host under `Host`, and a group only gathers one
host's changes. Each host's last question offers to remove its servers that are
off already. The running total sums the accepted changes of every host, as a
percent of the hosts' spend together. Each host's report follows the walk,
marked with its answers, and the confirm shows every host's diffs and names
each file as `host:path`, such as `devbox:~/.claude/settings.json`.

Each destination's own token-max reads, checks and writes its files, so the
rules above hold there as they do here: which edits apply, the masked diff,
the check that a file did not change during the walk, Claude Code's lock and
the `.token-max.bak` backups. The destination sends the changes it can make and
keeps its files; this host sends back only which ones were accepted, and the
destination makes those and nothing else. After the yes, this host's files are
written first, then each destination's in the order given. A host that fails
stops the run before any later host is written: the summary names what each
host already wrote, and the error names the host that failed and the hosts left
unwritten. A destination's write is done once it reports its files: one that
fails after that fails the run when every host has been walked, and stops no
later host's write. If the connection drops while a destination writes, its files may or
may not have changed, and the error says to check them and their backups.

Ctrl-C before the confirm stops every destination with nothing written. After
the yes, Ctrl-C starts no further host's write. A write already under way on
this host finishes. A destination's write may still finish there after this
host stops waiting for it, so the error says to check that host's files and
backups. The run exits 2 when the Ctrl-C left a host unwritten or stopped
waiting for one, naming each. A Ctrl-C that lands as the last write finishes
cuts nothing short, so the run reports what it wrote and exits 0.

## Profiles

Without directory flags, discovery includes `~/.claude`, `~/.codex`,
`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, and recognizable `~/.claude*` and `~/.codex*`
profiles. Codex includes archived sessions; Claude includes nested subagents.
T3 instances are included from `~/.t3/userdata`, an existing `~/.t3/dev`,
and `$T3CODE_HOME/userdata`. Repeatable `--claude-dir`, `--codex-dir` and
`--t3-dir` flags select only those roots across both harnesses. `--t3-dir`
takes a state directory containing `settings.json` and optionally `statev2.sqlite`. `--details` shows every discovered root's selection. A missing or
inaccessible explicitly selected root fails the run. Missing default roots do not.

The report gives each setting, tool, skill or MCP server one line per settings
file that owns it, combining every profile that file controls. Native Claude
Code and a T3 instance that read `~/.claude/settings.json` share its lines:
their requests, costs and uses are added together, each source's history
replayed on its own, and the line recommends the value with the lower total.
Claude's and Codex's files, each other home, a T3 instance's own settings, and
each `--ssh` host's files stay separate lines. Ownership is per setting: a T3
instance whose own setting decides fast mode, effort or the compaction window
gets its own line for that setting while sharing the native file's other
lines. A setting a shell variable, a project, local or managed file decides,
and every setting of a disabled T3 instance or one whose settings could not
be resolved, keeps a line per profile naming it. Two paths that resolve to one
file, such as a symlinked `settings.json`, are one file.

A request is counted once on each line: one copied into several profiles'
histories, or one T3 could not settle between instances, counts once on the
line those profiles share. When those profiles have different settings files,
each file's line for a setting counts it, and
a piece of context gets one line naming every file. A tool, skill or MCP
server's use counts on every line that shares a settings file with the
profiles that recorded it, so no line offers to turn off what one of its
files used.

## Prices

The first online run fetches only `https://models.dev/api.json`, caching validated
prices at `~/.cache/token-max/models.json` (or under `XDG_CACHE_HOME`).
Use `--offline` for that cache, `--price-cache FILE` to choose its location, or
`--prices-file FILE` for a local models.dev catalog instead of fetching one.
A failed price fetch fails the run; offline use is explicit. Cached prices include
their file timestamp, so an old catalog remains visible.

## Plans and API prices

token-max is written for people on a Claude or ChatGPT plan. No provider
publishes a plan's limits in tokens or dollars, so it measures with public API
prices, the closest public measure of what a request uses of those limits, and
the report's first line says so. It never reads which plan a profile is signed
in to, so a fact that differs by plan is stated conditionally, as in "on a
Claude plan", except where `doctor` asks (see [How a profile pays](#how-a-profile-pays)).

What the providers document, checked 2026-11-14:

- Claude Code's [costs page](https://code.claude.com/docs/en/costs#why-usage-climbs-in-a-long-session)
  says a cached re-read draws plan usage at the cached token rate, and that
  cache misses, compaction, subagents and background requests all draw on it.
  OpenAI's [Codex pricing](https://learn.chatgpt.com/docs/pricing) says model,
  context, reasoning, tool use and caching all affect usage, and prices its
  credits per token with the API's split of input, cached input and output.
  Neither says how much of a limit a token is.
- Past a Claude plan's limits, [usage credits](https://support.claude.com/en/articles/11145838-use-claude-code-with-your-pro-or-max-plan)
  are billed at standard API rates, so there the dollars are the charge.
- Claude Code's `/usage` shows a Claude plan's own usage bars and a breakdown
  of what counted against them on this machine. token-max adds what other
  settings would have cost, replayed over the same sessions.

Where a provider documents a rule that API prices alone don't show, the line's
evidence under `--details` states it before the setting that changes it, and
the list ends with the source. Fable weekly use is the exception: its rule is
a [`vibes`](#vibes) card, shown when the window holds a Fable request:

| Line | On a plan |
| --- | --- |
| Main-agent prompt cache TTL, while unset | Claude Code [requests 1h](https://code.claude.com/docs/en/prompt-caching#which-ttl-each-request-gets) for the main conversation on a Claude plan within its included usage, and 5m once usage credits or an API key pay for it. When `doctor` asked how the profile pays, the line shows that default, such as `1h (default)`, and prices against it alone. Otherwise token-max can't tell which applies, so the line shows `unset` and names both. When the replay picks a winner, setting it is the cheaper choice either way, so the line recommends `unset -> 1h` (or `5m`) and `doctor` offers to set it; it shows the replay's gap as "saves up to", saved only while the other default would apply. |
| Claude fast mode | A Claude plan [pays fast mode only from usage credits](https://code.claude.com/docs/en/fast-mode#requirements), at its API rates, even with plan usage left. |
| Codex fast mode | A ChatGPT plan [counts Fast at 2.5x and Ultrafast at 8x](https://learn.chatgpt.com/docs/pricing) standard against included usage, and 2x and 6x on credits. The API premium priced is the [documented API rate](semantics.md#speed-controls-and-premiums), which can be lower, so included usage can draw faster than it shows. |
| Prompt suggestions | Claude Code [skips them](https://code.claude.com/docs/en/costs#background-token-usage) while a plan is near its usage limit, which the estimate, run on every eligible turn, cannot see. |
| Fable weekly use | Anthropic [caps Fable models](https://support.claude.com/en/articles/15424964-claude-fable-models-on-your-plan) at up to half the weekly usage limit on Max, Ultra and Team and seat-based Enterprise premium seats, drawing it faster than other models, runs them on usage credits on Pro and Team and seat-based Enterprise standard seats, and bills them at API rates on usage-based Enterprise. The line shows weekly API-price spend and no share of the cap, as [Fable weekly use](#fable-weekly-use) explains. |
| Codex compaction window and keepalive pings | ChatGPT credits [charge cache writes as ordinary input](https://learn.chatgpt.com/docs/pricing), while API prices from GPT-5.6 on charge 1.25x for a 30-minute write, so a replay that trades writes for reads can come out differently on credits. |

Every other line's dollars are API prices with no documented plan rule beside
them.

### How a profile pays

On a terminal, before the report, `doctor` asks how each Claude profile on the
Anthropic API pays, on this host and then on each `--ssh` destination in the
order given: a Claude plan, or an API key or usage credits. It asks once per
profile root that has transcripts, including one T3 Code shares, and skips
profiles under Bedrock, Vertex or Foundry, whose default is 5m. A
destination's profile is named after its host, as in `devbox:~/.claude`.
Answer `p`, `a` or `s` to skip. A plan answer prices the main-agent TTL
against 1h and an API answer against 5m, in its line, the comparison with the
defaults and `--defaults`. A skip, end of input, which skips every later
question, or a run without a terminal keeps naming both defaults. Nothing is
stored: the next run asks again, so a profile that changes how it pays is
never priced from a stale answer.

An answer holds only where every recorded project of the profile resolves the
same default. A project whose settings send its requests to Bedrock, Vertex or
Foundry defaults to 5m under the same unset setting, so under a plan answer the
profile's main default goes back to unknown: the line names both defaults and
offers to pin the replay's winner, which outranks both.

A plan's requests past its included usage are paid with usage credits and get
5m, and the transcripts don't mark which ones those were. A plan answer prices
them at 1h too, and the line and its `--details` uncertainty say so.

## Window

`--since YYYY-MM-DD` means midnight UTC; ISO instants must include a timezone.
Without it, each Claude bucket starts at its latest observed write-TTL transition,
or the available history when no transition is recorded. A transition can also be
a billing or agent-policy change. Reports label today's settings applied to
history and do not invent historical settings. Recommendations name the owning
setting or environment override.

## Exit codes and unreadable lines

`vibes` exits `0`, or `2` for a failed scan, price fetch, invalid argument or
cancellation: advice never sets an exit code, in `vibes` or in `doctor`.
When `doctor` walks through the changes on a terminal, it exits `0`, or
`2` for a failure or cancellation. Otherwise its exit codes are `0` for no applicable
change, `1` for a recommended setting or feature change, and `2`
for a failed scan, price fetch, invalid argument, or cancellation. A failed run emits no
success report or partial recommendations. Invalid selected usage fails with only
a file and line. Complete lines that are not JSON objects, which harnesses leave
after crashes and concurrent writes, are skipped and counted in the report;
their usage counts as missing, and each one counts as an unpriced request in
that profile's comparisons. Unterminated final lines are deferred, and
`--details` counts the files that had one.

An unreadable line does not by itself keep an unused tool, skill or MCP server
from being recommended off. It holds the recommendation back, as an advisory,
in two cases. Its bytes name the feature as a call records it, such as
`mcp__server__tool` or a skill's name (any case), or a Codex completion's
`"server"`, after decoding JSON escapes like `\u0041gent`; a mention in prose
counts too. Or its transcript has a tool
result whose id matches no parsed call, in the parsed records or in the
unreadable lines: a crash can cut a record off, leaving no name to search, and
every call that ran leaves a result, so such a result shows a call is missing.
That second case cannot say which feature the call used, so it holds back every
never-used feature that transcript's profile carries. One miss is accepted: a call cut
off inside its name whose client died before writing its result counts as
unused, though it may have run; the default report's skipped-lines line says so,
and `--details` repeats it on each red row beside unreadable lines. A deferred line always holds back the
recommendation. Only hashes of the names and ids are kept in memory, never the
text.

## Reading the report

The default report says what was read and what it cost at API prices, then
gives every setting and feature one line. Each line names its verdict, the
setting or feature, its current value (and the value to change to, for a
change), what it saves per 30 days or costs over the window, that figure's
share of this host's spend, the profile when several were read, and the
setting that changes it. For a change `doctor` offers,
that last column is the exact edit and file, or the step to take by hand.

Every line is measured: replayed or priced from your own transcripts at public
API prices, or marked unknown where they can't price it. No verdict rests on
anyone's preference or on a provider's advice alone.

Advice that the transcripts can't price, such as a provider's guidance, is no
part of this report: [`token-max vibes`](#vibes) shows it, and `doctor` only
offers to.

The lines come in two groups. **Changes** come first: every line recommended
(red, exit 1) or worth considering (yellow, exit 0), ranked together by what
they save per 30 days, biggest first, with those whose saving was not
measured after the rest. A feature turned off saves what it cost, assuming
nothing else changes. **Everything else** follows, ranked by what it costs,
biggest first, with lines whose cost is unknown or that have no figure after.
When several profiles were read, each line also names the settings file that
owns it, or the profile when no file does (see [Profiles](#profiles)).
The headline counts both kinds of change. The line under it sums what they
save together, as the walk's running total would if every change were
accepted, such as "Together, these changes save about $40/mo, 9% of this
host's spend; overlapping changes may save less", since each saving holds
every other setting constant. A dot's color carries the verdict:
red, yellow, green for a value already at its recommendation, and a dim hollow
dot for no verdict, either for lack of evidence or because nothing can be set.
Without color a word replaces the dot: `change`, `consider`, `keep`,
`unknown` or `info`.

Every percent is of this host's spend over the window at API prices: every
request it recorded, shared histories counted once. A cost's percent divides
its total by the host's, since a profile's history lies within the host's
window; a saving's compares its monthly rate with the host's. When some
requests could not be priced, each profile's spend is extrapolated from the ones
that could, its 95% margins combined in quadrature since the profiles are
separate samples, and the title shows it as `~$9,392 ± $4.72`. An unpriced
request is never extrapolated below what it is known to cost, such as its tokens
beside an unknown server fee. A profile that priced none of its requests adds
only its known part, a lower bound, shown as `~$9,392 ± $4.72 plus at least
$120`, so a share can read high; the line explaining the percents says so.

A `~` marks any figure extrapolated over requests that could not be priced, such
as a saving of `~$3,270/mo ± $410`, followed by its 95% margin of error under the
assumption that the unpriced requests resemble the priced ones. A figure priced
in full has neither. Costs read the same way: what a tool, skill, MCP server,
subagent type, main-session effort or Fable week cost is extrapolated from its
own priced requests to all of them, such as `~$2,880 ± $40`, and one with no
priced request is unknown. A line summing pieces of context, such as System
prompt and core tools, adds their margins, and reads `at least` only beside a
piece none of whose requests could be priced. The walk's and `--defaults`
headline's totals add their changes' margins, since savings from overlapping
requests can err the same way.

Comparisons between setting values are per 30 days at the window's rate; what
features and context cost, and how often they were used, are totals over the
whole window.

`--details` puts each line's evidence under it: what the replay held constant,
such as the same sessions, and what it could not measure, such as the detail a
smaller compaction window or idle summary loses, then the setting that changes
it. A line
without enough evidence says why there, such as no request with recorded usage
to compare; a line extrapolated over unpriced requests names the share measured.
Unresolved current ownership gives
no verdict and does not cause exit 1. The list then cites its sources, and the
evidence cards follow.

Features sent with every request, such as a tool, MCP server or skill, show what
they cost and how often they were used. One never used is recommended off,
whatever it cost. One that cost more than `--max-cost-per-use` (default $1) per
use is worth considering off, an advisory that exits 0; the rest are kept. An
unused one stays an advisory when the window cannot rule out a use: it was first
sent after the window began by every source of its line, an unreadable transcript line names it or may be
a cut-off call (see [Exit codes and unreadable lines](#exit-codes-and-unreadable-lines)),
a transcript was still being written, or, for a Codex skill, its uses are only
partly recorded. A Claude skill's uses count only the model's own calls, because hiding it
with `skillOverrides` set to `user-invocable-only` still lets you type `/name`.
Skills synced from claude.ai share one line, since only `syncClaudeAiSkills`
turns them off in settings, and that also removes typed uses, so those count.
Codex skills have separate listing-cost lines by absolute `SKILL.md` path,
with requested uses and confirmed file loads distinguished. Disable one with
`[[skills.config]]`, its `path`, and `enabled = false` in `config.toml`; this
also removes explicit invocation. Same-named copies are flagged even without
prices, with one copy retained. Repeated discovery of the same path needs the
redundant source removed. Zero observations do not prove nonuse, and listing
cost is not promised savings: Codex can backfill the freed catalog space.
Auto memory, Claude Code's `MEMORY.md` and system prompt section or Codex's
memory summary, is judged as a skill is, with each memory file read back or
written as a use: on and never used is recommended off, and above
`--max-cost-per-use` per use worth considering off. Its line shows the total
cost of the loaded memory, the memory files read back and the writes, and how
often it was read or written. A total that leaves out what could not be priced,
such as Codex's extraction and consolidation, proves neither that memory was
free nor that it stayed under the limit per use, so those lines and one that
may be on because a Codex setting such as a `--profile` selection is not
recorded give no verdict, as do lines whose price depends on a service tier a
request that carried or wrote memory did not record. A transcript line that could not be read could hold a
read or write, so memory with none recorded is then only worth considering
off. A profile with memory off shows it as kept.
Claude plugin skills, which `skillOverrides` does not reach, and the system
prompt, core tools such as Bash and Read, and the skill and subagent lists have
no switch, so their lines give no verdict.
A switch is an edit such as `"enableArtifact": false` or a bare tool name added
to `permissions.deny`, which `doctor` offers to make (see
[Applying changes](#applying-changes)). `--details` lists every piece,
including those no longer sent.

A run with no histories or no requests in the selected window gives a next step;
no applicable changes does not imply complete visibility or optimal settings.

## Savings against the defaults

Under the headline and its total, one line sums what this host's current settings save
against the Claude Code and Codex defaults, such as "Compared with defaults,
your settings save about $945/mo, 2% of this host's spend". Every line whose
setting differs from its default gets a note under it naming the default and
what the current value saves against it, such as `default 5m, saves $410/mo`,
or `costs $12/mo more` when the current value costs more. The line shows on
every run, including one with nothing left to change, and reads "No
setting was found to differ from its Claude Code or Codex default." when none
differs. A line whose current value is unknown gets no note, and that
sentence does not claim it matches its default.

Each saving is priced like its line's own comparison, holding every other
setting at its current value, so the sum adds savings that may count the same
requests and says that overlapping settings may save less. A saving that
holds only for one of two possible defaults is summed apart as "up to", and
one that cannot be measured is counted, never priced at zero.

| Setting | Default | What the current value saves |
| --- | --- | --- |
| Subagent prompt cache TTL | 5m | The TTL replay's gap, when the line has a verdict; otherwise not measured. |
| Main-agent prompt cache TTL | 1h on a Claude plan within its included usage, 5m on usage credits or an API key; 5m under Bedrock, Vertex or Foundry | The replay's gap as "up to", saved only while the default would be the other value; against the answered default alone when `doctor` asked [how the profile pays](#how-a-profile-pays); under a cloud provider, the replay's gap against 5m. "Up to" savings and costs are bounded apart, never netted. |
| Compaction window | The model's tuned window (see [Compaction window](#compaction-window)), or Codex's per-model limit | Not measured: a window smaller than the default can't be replayed at the default, since the detail its summaries dropped is gone. |
| Idle compaction | On | The on and off replay under the current settings. |
| T3 idle compaction | `onReturn` | The difference between the two modes' replays. |
| Fast mode | Off | The measured premium, as a cost. |
| Background generation | On | Not measured: its requests are estimated only where it runs. |
| Auto memory | On for Claude Code, off for Codex | Codex memories on cost what the line measured; Claude Code's off is not measured. |
| Main-session effort | The model's own level | Not measured: a different effort changes the output. A `maxEffortLevel` that lowers the default counts as a difference. |
| Bash, MCP and Codex tool output caps | 30,000 characters, 25,000 tokens, Codex's built-in limit | Not measured, as their lines say. A cap set to its default counts as the default. |
| A tool, skill or MCP server turned off in the profile's settings file | On | Estimated: what it cost per request while it was still sent, over every request its line made in the window. |

A tool, skill or MCP server is off when the profile's own `settings.json` or
`config.toml` turns it off: `"enableArtifact": false`, `"enableWorkflows":
false` or `"syncClaudeAiSkills": false`, a bare tool or `mcp__server` name in
`permissions.deny`, a `skillOverrides` value other than `on`,
`features.apps = false`, an MCP server's `enabled = false` or a tool in its
`disabled_tools`, or a `[[skills.config]]` entry with `enabled = false`. It
gets its own `info` line, showing `off` or `typed only`, and its saving is an
estimate. When no request in the window carried it, the saving is unknown,
never zero, and so is a Codex catalog skill's, since removing one can backfill
the catalog. A switch set after the latest request, such as one the walk just
wrote, counts as off too, so the next run neither offers it again nor drops
its saving, and an MCP server's switch counts for its tools too. A switch that
a file outranking the profile's sets otherwise adds no line: the feature's own
line says what it costs. Any outranking file that sets it otherwise counts,
even when a file above that one sets it off again, so in that case doctor
may offer the switch again rather than call a feature off that might still
be on.

## As a default user sees it

`token-max doctor --defaults` prints the report a user with no settings files
would get on this host's own transcripts. Every user, project and local
Claude Code and Codex settings file, Codex's named profile files, T3 Code's
`settings.json` and the shell's setting variables, such as
`CLAUDE_CODE_PROMPT_CACHE_TTL`, read as unset, so each setting is at its
documented default. Managed and system policy (`managed-settings.json`,
`/etc/codex/config.toml`, `/etc/codex/requirements.toml`) still apply, since a
user can't change them. What says where each CLI keeps its files stays:
`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `T3CODE_HOME` and each T3 instance's home.
So does where its requests went, which sets their prices:
`CLAUDE_CODE_USE_BEDROCK`, `CLAUDE_CODE_USE_VERTEX` and
`CLAUDE_CODE_USE_FOUNDRY`, from the shell or a settings file's `env`, and
Codex's `model_provider` and `model_providers`.
Each MCP server a Codex file declares stays declared, without its `enabled`,
`enabled_tools` or `disabled_tools`, since a server is something installed
rather than a setting's value. A T3 thread's model selection is T3's default,
so fast mode is off.

Every check then runs as it would for that user, and the headline counts the
changes they would make. The line under it sums what those changes save from
the defaults, as the walk's running total does, and says overlapping changes
may save less; the [savings against the defaults](#savings-against-the-defaults)
line would only restate that everything matches, so it is left out.

One rule decides whether each tool, skill or MCP server is on, before any
verdict. One that managed or system policy turns off stays off, whatever the
latest request carried, including a tool a system MCP `enabled_tools`
allowlist leaves out, and its `info` line says policy keeps it off. Otherwise
one that a user, project, local or named profile file turns off is on,
including a tool a Codex `enabled_tools` allowlist leaves out, a server a
`mcp__<server>__*` rule denies, and a Claude Code server turned off with `/mcp`
in the project, which `.claude.json` records. A named profile file counts only
for a server the files a default user reads declare, and a T3 instance reads
the project files where its requests ran, as its CLI does.
While it was off no request carried it, so its cost is scaled from the
requests that did carry it to every request its line made in the window,
while its verdict and cost per use come from the uses recorded while it was
sent; its evidence says how many requests carried it. An MCP server's total is
its own part, scaled on the requests that carried the server, plus each of its
tools' totals, scaled on their own. One that no request in the window carried
gets an `unknown` line costing unknown, never zero. Claude Code's auto memory is
scaled the same way, from the main-agent requests that carried it. When it is
on but no recorded request carried it, as for a user who turned it off before
the window, its cost is unknown on any run, not zero.

The report is read-only: it offers nothing and writes nothing, even on a
terminal, and each change still names the edit a default user would make.
Exit codes are those of a run without a terminal, so a script can still tell
whether a default user would have anything to change. With `--ssh`, each
destination reads its own files the same way.

## MCP servers and instruction budgets

MCP servers include separate tool-schema and discovery-context costs. Those
details are part of the server total. Codex tool recommendations use
`mcp_servers.<id>.disabled_tools` only when the declaring server id is unambiguous and native completions prove
the raw tool name in every owning profile;
`enabled_tools` is the corresponding allow list. The report explains Claude's
`ENABLE_TOOL_SEARCH` and per-server `alwaysLoad`, and Codex's `omit_tools_from`
surfaces. These exposure controls do not establish net savings: search turns,
returned definitions, cache changes and repeated loading are not completely
attributable. A small frequently used server can be cheaper to load upfront.
An MCP tool with no switch of its own, or whose server's off already covers
it, has no line: its server's line includes its cost, and `--details` lists it.
App controls require the actual app id, which these transcripts do not map to
the exposed app namespaces; the report names the supported controls without
inventing per-app costs or a settings patch.

Each instruction file Claude Code or Codex loads gets a line with what it cost
over the window, its latest recorded length, and what an average 100 lines of
it cost per 30 days. Each request's cost of the file is shared evenly over its
own revision's lines, so a file edited during the window prices 100 lines at
every revision it sent, and a revision under 100 lines counts whole. Lines
differ in length, so this is about what shortening the file by 100 lines saves,
assuming the rest of the work is unchanged, not what removing particular lines
saves. A file now under 100 lines gives what the whole file cost instead, which
removing it saves. Neither figure exceeds what the file cost. What a line of
instructions is worth is not recorded, so the line gives no verdict and
`doctor` offers no change.

A CLAUDE.md of 200 recorded lines or more, past Claude Code's documented
guidance, gets a [`vibes`](#vibes) card. So does a Codex AGENTS.md at or above the selected
profiles' `project_doc_max_bytes` from the system and user configuration (32
KiB by default), where Codex may have cut it short: the recorded rendered bytes
include headers and wrappers, so only reaching the budget can mean truncation.
The card names the file that sets the budget, or says it is the default. A
project `.codex/config.toml` at a recorded directory or a named profile that
sets it gives no card, since the transcripts do not record which applied;
neither do profiles with different or unreadable budgets, or records with no
rendered text. Command-line overrides and original file sizes are unavailable.
Output-cap lines
still recommend no value because truncation-related read-back cannot be priced
from these transcripts.

An auto-compact window without enough evidence also shows how large that
model's requests got (median and 90th percentile), to judge whether a smaller
window would matter.

## How context is priced

Each piece is sized from the request shapes Claude Code records (tools, then the
system prompt, then messages) and converted to tokens with a characters-to-tokens
ratio measured from the same transcripts. It is priced at the cache position it
occupied in that request: cached read, 1h write, 5m write, then ordinary input.
Attachments have no recorded position, so one is priced only when every
position its history allows pays the same rate. Requests that could not be
priced are extrapolated from those that could, with a `~` and a 95% margin from
the priced requests' spread, and a piece with none is shown as unknown. An MCP
server's tools ride on its requests, so its total keeps its own margin rather
than adding its tools'.
Removing a piece can also shift later cache writes and compaction, which this
does not model. Claude requests without a recorded shape cannot price their
context pieces. Codex prices rendered chronological context with a quarter-token
per-character estimate; metadata-only pieces remain unknown.

Comparisons use the requests priced under both options and extrapolate to the
rest, assuming requests with missing usage resemble the measured ones, so a
verdict stands whenever any request was priced. The figure carries a `~` and its
margin of error, and `--details` says what share of requests was measured.

## What each line does

Every line says in one sentence what its setting or feature does, such as
"Lets Claude publish an HTML or Markdown file to claude.ai as an artifact: a
private, interactive page you can share" for the Artifact tool that
`enableArtifact` turns off. `--details` prints it under the line as `What:`,
and the walk opens each change with it. A grouped question opens with what its
kind of feature is, such as what skills are, and each member's own sentence
shows when you split the group. The default report keeps each line to one
line.

Checked settings, built-in Claude Code tools, subagent types and the other
fixed lines use a sentence written for token-max. The tools' follow Claude
Code's [tools reference](https://code.claude.com/docs/en/tools-reference),
checked 2026-11-15. A skill or MCP tool uses the first sentence of the
description it ships, shortened to at most 200 characters: a skill's entry in
the skill listing its transcripts record, and a tool's own definition where a
transcript records one, as Claude Code's prompt and tool snapshots and Codex's
provided tools do. A Codex MCP namespace's description describes its server.
These descriptions are configuration, not conversation, so they are kept and
printed; skills synced from claude.ai share one line and one sentence. A
feature with neither says it has no recorded description, and an MCP server
without one names its tools instead.

## Details and terminal layout

Use `--details` for each line's What sentence and evidence under it, the sources, then evidence
cards: every check's profiles, owner, baseline, assumptions, uncertainty, cost
components, break-evens, sensitivity scenarios, cache witnesses and sources,
and every piece of context, including MCP tools counted in their server's line
and pieces no longer sent. A piece of context several sources share is broken
down by the profiles that recorded its requests, with each one's requests and
cost. A background feature's evidence names three of the directories it ran in
and counts the rest. Repeated idle periods, cache witnesses and attempt estimates are
grouped by their recorded outcomes and reasons. Token ranges and combined costs
summarize modeled compactions; geometry sources are listed once. Identifiers in
grouped cache and attempt evidence are examples. No evidence is hidden behind
the walk's prompts or a top-N limit. The layout wraps long paths and
names on narrow terminals, where the name column narrows first and a line too
long for the width wraps under its name. A name or profile too long for its
column pushes the rest of its own line along rather than widening every line.
Redirected output keeps each line whole so paths and edits remain copyable.
Every line but the header is indented two cells. On terminals, the report's
colored dot carries each line's verdict, with a legend above the lines; a line
with a verdict keeps its name, value and figure bright, its saving green and
the rest dim, and a line without one is dim throughout. The headline counts
the changes in their verdict colors, notes on what was read are dim, and each
section heading has a rule after it. The walk colors its cards the same way:
the verdict in its color, the saving green, and labels dim. `NO_COLOR`
disables color, and `--plain` also disables decorative Unicode, and either
puts the verdict word back in place of the dot. Evidence cards keep their
verdict words and only add color, and each names its claim. Redirected output
is plain, and progress appears only on interactive stderr.
Unicode decorations require a UTF-8 locale. Progress is one line that shows each
phase in turn and clears itself before the report. While this host's
transcripts are read it is a bar of the bytes read out of the total discovery
found, with the time left to read them at the rate so far; the analysis after
reading has no measured fraction, so its phases show only a spinner and how
long each has run.

On a terminal, `doctor` opens with its name, says it is about to scan this
host's transcripts, and lists each profile root it found with its transcript
files and their size, then the SSH destinations it adds, or how to add one with
`--ssh`, before it asks how a profile pays.

## Machine-readable output

The report is for people and has no machine-readable form; `--json` was
removed. Each machine output is its own sub-command with a named consumer, such
as the session export below.

IDs in the report are opaque hashes. Shared histories are read once, and a
request whose profile is ambiguous counts once on the line its profiles share
(see [Profiles](#profiles)). Independent check savings must not be summed;
every total the report gives, such as the walk's running total or the
[savings against the defaults](#savings-against-the-defaults), says that
overlapping changes may save less.

## Session export

`token-max sessions` prints each session's usage at public API prices, one
JSON object per line, for tools that tie sessions to their own records, such
as moq-dev/quest's `quest usage`. It takes `--since`, the profile and price
options, and `--ssh` as `doctor` does;
`token-max sessions --help` lists every field.

```sh
token-max sessions --since 2026-11-01
token-max sessions --ssh devbox
```

A Claude subagent, and a Codex review or spawned thread, is a session of its
own that names the session it came from. Each session lists its native ID,
working directory, every Git branch its records name with the first and last
time, and its tokens and cost per model and reasoning effort. Branches come
from records in the `--since` window; when none names one, as in a Codex
session begun before it (Codex names its branch only when a session starts),
the last branch named before the window is listed. A request whose parts ran on
several models, such as an advisor call, counts once in each model's entry. A request copied
into another transcript, as a fork does, counts once, with its original
session. A cost is a lower bound, marked `"complete": false`, when a request
could not be priced or a transcript had unreadable or unfinished lines.
Compaction summaries the transcripts never recorded are not estimated here.

With `--since`, only the transcripts the window depends on are read whole, so
a recent window costs what its own transcripts hold rather than the whole
history. Every transcript is first scanned for its record times and the
identifiers it shares with others: sessions, threads and their parents, fork
sources, requests and their API records, turns, and the record and tool keys
T3 links. A transcript is read whole when one of its records is timed in the
window, or is a request with no time, or when it shares an identifier with a
transcript read whole, such as an earlier copy of a streamed reply, a resumed
session's earlier branch and directory, or a subagent's parent. The lines
printed are the ones reading every transcript would print. Modification times
are not used, since a clock change or a copy can leave a transcript's earlier
than the records inside. A malformed record in a transcript left unread does not
fail the export. `doctor` reads every transcript, since its checks use the
history before the window too.

Each line names its `host`: the `--ssh` destination as given, or null for this
host. Lines from every host are ordered together by first request; a session
whose history was copied between hosts is printed once per host, each with its
own `host`, and the consumer decides which to count.

Each line's `repositories` is resolved on the host that ran the session, so a
consumer never reads another host's path on its own. It is the repository
Codex recorded when the session started (`"repositoriesSource": "recorded"`),
or else the remote URLs of the Git checkout now containing `cwd`, a
subdirectory, linked worktree or symlinked directory included (`"checkout"`). A
checkout is read when the export runs and may have been replaced since the
session, so treat `checkout` as unverified. It is null when the directory no
longer exists or this user cannot read it (another user's, or a dropped network
mount), is in no checkout, or the checkout has no remote or an unreadable
config. URLs are read from the
repository's own config file, in the order it lists them, without following
includes or applying `insteadOf` rewrites, and are printed without userinfo, so
a credential such as `https://user:token@host` never leaves the host. A config
Git would reject fails the export, naming the file and line.

Every string prints with terminal controls and direction overrides escaped as
`\uXXXX`, which decodes to the same text, so a path, branch or URL, from this
host or a destination, cannot drive the terminal that shows the output.

Native session IDs never reach the report, which hashes them. Over SSH, they
cross only in a destination's session lines.

## Cache TTL

A profile's TTL resolves to one value only when every request in it, across all
of its recorded projects, gets that value from the same owner. The projects are
those its own requests ran in, once T3 has attributed them, so a project only a T3
instance used never unsettles native Claude Code's TTL. Where a project's own
settings file sets it, or a project defaults differently, the TTL line stays
unresolved for the profile, since no one edit to the user file covers every
project, and so do the checks that apply one TTL across the profile: the Bash
cap, keepalive pings, compaction windows and T3's idle modes. Native idle
compaction resolves each directory's TTL instead. A project directory that no
longer exists runs nothing more and decides nothing. An unset main TTL
whose billing is unknown stays pinnable while every project leaves it to the same
setting.

Unlinked API attempts stay in coverage. A TTL comparison may estimate input from
matching adjacent measured requests, including growing-context retry geometry
within one five-minute cache lifetime, with their IDs and assumptions reported;
original usage and historical billing stay unknown. Unsupported inputs leave their
own parts unpriced. Sensitivity scenarios accompany the aggregate
winner, and unknown fixed charges can cancel without pretending they cost zero.

Cache replay preserves known, estimated and unknown predecessor states, including
unsupported attempts before the selected window. Missing cohort metadata spans
compatible histories; `--details` groups
their states and reasons. A comparison
stays unpriced when those histories imply different cache charges, while invariant
charges can still be compared.

## Compaction window

Compaction-window comparisons run after the TTL changes `doctor` recommends, as
keepalive and idle comparisons do, so an advisory TTL is not applied; they use
the current window as their baseline. Claude controls resolve environment overrides,
managed and user settings, including `modelSettings.<model>.autoCompactWindow`;
Codex uses `model_auto_compact_token_limit`. An unset Claude window uses each
model's latest observed automatic-compaction trigger, else Claude Code's
documented default. A window larger than the model's context window replays
at that context window, as Claude Code caps it. Unresolved defaults, disabled
compaction, unsupported versions/scopes, and ambiguous named Codex profiles stay
advisory.

`CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` resolves into the replayed trigger from every
environment the window reads, a T3 instance's included. An explicit window
below a context Claude Code establishes for the model compacts at that
percentage of it. A recorded project whose override differs from the home's
leaves the comparison unpriced, since its sessions compact elsewhere. Elsewhere, as under
an unset or capped window, the trigger rests on Claude Code's undocumented
default percentage, so the latest automatic compaction on the model counts
only from a session that started after every file setting the override, a
window or the context size last changed. A setting in the shell, which keeps no
such time, or no such compaction leaves the model's line advisory. A value of
100 or more never lowers a trigger; one that is not a number, or is below the
documented minimum of 1, leaves every line that reads it advisory.

A recommended window under the override makes one setting own the trigger: the
change removes the override from every scope that sets it, recorded projects
included. Alone on its line, the target's window changes where its owner holds
it. When the line runs other models, the target goes in its own `modelSettings`
row, every other model the line's history shows, even one that last ran before
the window, is pinned at its current trigger in its own row (an exact dated row
when one exists), and every setting of `CLAUDE_CODE_AUTO_COMPACT_WINDOW` comes
out, since it outranks every row. The walk shows each of these edits before
applying them. An owner the walk cannot edit, such as the shell, a project file
or a T3 instance, leaves the whole change as a step by hand naming it.

The change is checked, not argued. Every scope that reads a changed setting is
resolved again with the changes applied: the home, each recorded project, and
any other line, such as a T3 instance's, that reads the same file or
environment. Each target must land on its target and every other model on its
current trigger. Any model that would not, or whose current trigger is
unresolved, keeps the line advisory, and the line names the model and where its
trigger would move. Requests with no recorded model keep it advisory too, since
no row holds their trigger. No recorded compaction shows a trigger under the
changed settings, so an unset window there rests on the tuned default.

Candidates come from current controls, price-tier boundaries, provider limits,
and measured resumed-prefix geometry. Added compactions use observed summary and resumed-prefix sizes in matching
cohorts; unsupported affected candidates stay unpriced. Observed compactions remain
charged once, including when earlier hypothetical passes might replace them.
Summary generation and cold resumed prefixes are included in net savings. Larger
observed summaries/prefixes, cold-summary costs, and the extra re-read spend that
would erase a saving accompany the aggregate winner. Lost detail and changed work
remain unmeasured risks, not zero-cost observations.

## Idle compaction

Idle compaction compares on and off over the same window, first with current
controls and then after applicable TTL and window changes. The latter aggregate
estimate decides: positive on-minus-off cost favors off, negative favors on, and
a zero tie, such as when no gap was long enough to trigger, is not enough evidence. Final periods incur expected summary cost
without an invented return benefit. A sensitivity crossing zero does not veto the
point estimate. A recommended 5m main TTL or a window at/below the idle threshold
makes idle compaction inapplicable.

The native toggle defaults to true, requires Claude Code v2.1.290+, enabled
auto-compaction and a 1h main TTL; versions before v2.1.314 also need confirmed
feature availability. The threshold defaults to 200,000 tokens, with a 100,000
floor for `CLAUDE_CODE_IDLE_COMPACT_MIN_TOKENS`. This check does not tune it.
Current user, project/local, managed and active-profile environment owners are
read separately from historical metadata. Missing directories, unknown execution
mode/version/flags or unresolved controls leave the sessions they affect
unpriced. On and off are paired session by session, and a session that cannot be
priced both ways is extrapolated from the priced sessions where idle compaction
could act, marked with `~` and a 95% margin; a session it could not act on
counts exactly, even when its replay failed. With no such session priced, the line is not enough evidence. Native CLI interactivity needs REPL provenance; Desktop
labels alone do not establish local/SSH execution. T3's proactive policies are
outside this native check.

Only verified native informational notices attribute an applied idle summary.
`precomputed` alone does not. Where comparable known-on outcomes exist, their
same-version/client/flag applied fraction estimates unobserved activation,
including final periods. Otherwise the deciding assumption leaves the client
open and awake for one documented near-expiry trigger. Closed/open alternatives,
summary-size and timing sensitivity, the activation break-even and extra re-read
cost per resumed summary accompany the recommendation. Missing boundaries never
prove that discarded or unlogged summaries did not run. Without linked billed
summary output and full resumed-prefix calibration, affected hypothetical costs
stay unknown; `postTokens` alone cannot establish the full request.

## T3 Code

T3's Claude Code and Codex instances reuse their native histories, including
retained histories of disabled instances. Shared homes, Codex shadow-home
symlinks and request identities are deduplicated. T3 Usage aggregates are never
added to native billed requests. Unsupported providers are explicit coverage
gaps. Each T3 state gets a line naming the model its
`textGenerationModelSelection` resolves to for titles, branch names, PR text and
other generated text, with the narrower selections that replace it; their cost
is unmeasured because T3 runs them in ephemeral sessions. Native terminal
suggestions, recaps, goal check-ins and keepalive guidance do not control them.

The report names T3's `providerInstances.<id>.config.autoCompactWindow` when
it overrides a native Claude window. Thread speed selection belongs to
`modelSelection.options.fastMode` or `.serviceTier`, with Codex's native fallback
when omitted. Current browser/new-thread selections are unavailable and remain
advisory. A uniquely joined historical requested tier never fills a billed tier.
Disabled instances and unresolved environment, launch or machine-policy overrides
keep historical cost but receive advisory current guidance.

An instance that reads a native home's settings shares that file's lines with
native Claude Code or Codex, such as the cache TTL, the Bash cap, auto memory
and every tool, skill and MCP server (see [Profiles](#profiles)). The settings
T3 itself decides, such as thread fast mode and effort and an instance's
window override, are the instance's own lines. Instances whose idle compaction
inherits the global `idleCompaction` share one idle line naming T3's
`settings.json`. A request T3 could not settle leaves open only the
controls its possible owners read on different lines; lines that act for one
owner alone extrapolate over it instead (see [T3 evidence](t3.md#requests-t3-cannot-settle)).

Optional SQLite metadata uses the bundled SQLite library; no system SQLite
executable is needed. One read-only transaction supplies a consistent WAL view,
then closes before analysis. Queries return allowlisted native identities, run
state, token counts, timestamps, requested speed and lifecycle predicates only.
No message text, credentials or connection catalog is exported. Schema, size,
query-budget and read-only WAL failures leave native usage intact with an
explicit metadata gap. The native file and database snapshot bounds are reported
separately; later or ambiguous evidence cannot supply a historical join.

`t3-idle-compaction` compares `off`, `onReturn` and `beforeCacheExpires`
under current and recommended TTL/window controls using the shared cache,
compaction calibration and API pricing. Each mode's cost is what it changes
from the recorded history, so charges every mode shares cancel. The proactive
timer starts two minutes before the observed cache expiry, for eligible
completed main threads with at least 200,000 tokens. Five-minute and one-hour
caches both apply. In-memory timers disappear on server restart, so generation
remains conditional on server liveness. `onReturn` assumes the SDK offers a
resume dialog when a new run resumes a cold session of at least 200,000 tokens
and the user accepts it; the adapter does not infer an unrecorded acceptance.
It needs no T3 lifecycle guard. `off` also suppresses that dialog. Summaries
pay warm or cold input plus output; later requests pay their full changed
prefixes, and final proactive summaries have no invented return benefit.
The lifecycle guards for each gap are rebuilt from T3's event log at every
completed-run event and when the timer would fire. T3 keeps about one day of
that log, so the check's window starts where the log is complete, and gaps
that open earlier stay as recorded in every mode. Unknown lifecycle or missing
compatible full-prefix calibration leaves that session unpriced; each mode is
paired with the current one session by session and extrapolated over such
sessions, as the native check does. A mode no session prices, or an unresolved
window, leaves the line unpriced, and the report names the first missing input. See [T3 evidence and sources](t3.md) for supported schema
and limits.

## Other hosts over SSH

`--ssh HOST` (repeatable) audits a machine you reach with OpenSSH, using your
SSH configuration, keys and known hosts without prompting, along with this
host, which every run audits. You install nothing
there: each destination runs this build of token-max, which token-max puts
there itself. Linux and macOS destinations on x86_64 or arm64 are supported.
`token-max sessions` takes the same flags, and each destination sends only its
priced session lines. `token-max vibes` takes them too, and each destination
sends only its cards and, for a walk, the changes they offer.

```sh
token-max doctor --ssh devbox --ssh user@laptop
```

The destination discovers that host's own profiles from its home and
environment and reads its transcripts and settings, priced from the one catalog
this host loads and sends it. For a report it runs `token-max doctor --ssh-scan`,
which runs every check there and sends back only its finished report. For
session lines it runs `token-max sessions --ssh-scan`, which sends back only
the priced session lines of that host's transcripts. Either way its transcripts
and scan never leave it, and this host never holds more than its own scan.

Destinations connect while this host's profiles are asked about. For a report,
each first sends the roots of the Claude profiles it asks about, waits for the
answers [How a profile pays](#how-a-profile-pays) describes, and scans once
they arrive, while later hosts are asked. A destination that fails while
earlier questions are asked stops the run before the next question.

Each run first asks the destination its OS and architecture, over the
connection that then runs the audit:

- **On this host's platform**, it runs a copy of this exact binary. Without
  one, token-max copies itself there over SSH, checked by its SHA-256. An
  installed token-max is not used, since every build after a release reports
  that release's version.
- **On another platform**, it runs this version's release. Failing that, it
  runs the first installed `token-max` whose `--version` matches, on the
  non-interactive `PATH`, then in the release installers' directory (the
  first of `$XDG_BIN_HOME`, `$XDG_DATA_HOME/../bin` and `~/.local/bin`),
  then in `$CARGO_HOME/bin` (default `~/.cargo/bin`). With neither, the destination downloads the
  release from GitHub and checks it against its published checksum, which
  needs `curl`, `tar` and `xz` there. Matching the version is not matching
  the build, so a destination whose build differs from this one fails,
  naming the binary that ran there: run a released token-max here, or delete
  that binary there and build this commit with the `cargo install` command it
  gives, since the first binary of this version found is the one that runs.

Copies and downloads are cached in `~/.cache/token-max/ssh` there (or under
`$XDG_CACHE_HOME`): a copy named by its digest, a release by version and
target. Later runs reuse them, and each new one replaces the binaries cached
there before it. Either step checks that the binary runs there before caching
it. A copy linked against this host's libraries fails with a statically
linked build to run here instead, such as the release installers'.

`--claude-dir`, `--codex-dir` and `--t3-dir` select this host's profiles only.

Each host is analyzed on its own and gets its own section, labeled with the
destination, or `this host`. A run of several hosts heads its report with one
total: the hosts' requests, the input and output tokens they recorded (cache reads and writes included) and their sessions, their spend at API prices, and the sum of what the
changes they offer save every 30 days, with each saving's percent of the hosts'
spend together. Offered changes may overlap, within a host or between hosts, so
applying them all can save less than that sum, as the total says. The hosts' figures are added as they are, so a history copied between
hosts counts on each, and the total counts it twice; token-max does not
compare request IDs across hosts. A figure marked `~` on any host keeps its `~`
in the total, and the hosts' margins of error combine in quadrature, which
assumes each host's history is an independent sample; where histories are
copied between hosts the errors are correlated, so the total's margin can be
too small. Any failed
host, connection or stream before the reports are printed fails the whole run
with exit 2 and no report, and Ctrl-C then stops every destination. After
the reports, a failure during the walk, a Ctrl-C during its questions, or a
Ctrl-C after the yes that leaves a host unwritten also exits 2. On a terminal,
`doctor` walks through every host's changes and writes each on its own host, as
[On SSH hosts](#on-ssh-hosts) describes. Each host's percents
are of that host's own spend, and the total's of the hosts' spend together.

## Keepalive pings

The `keepalive` check asks whether pinging an idle session just under its cache
lifetime would cost less than the cache misses it avoids. It compares leaving
sessions idle, Claude idle compaction where that check recommends it, and
pinging for each cap on how long to keep going. Claude uses the main TTL `doctor`
recommends changing to, else the current one, else the one the latest main-agent
request wrote; an advisory TTL is not applied. Codex uses the documented 30-minute lifetime of models that bill cache
writes. Each cap replays the entire session with the same recommended compaction
window as the idle alternative, carrying ping context and cache warmth forward. A ping is the median recorded request, and a session that never came
back pings for the whole cap, since transcripts do not record when a client
closed. The details show every cap, the same caps if those sessions closed at
once, and how soon they would need to close for pinging to pay.

Neither harness has a capped keepalive setting: a `/loop` task pings on a fixed
schedule until cancelled or for seven days, and Codex cannot schedule pings. A
priced result is therefore advisory and never changes the exit code. Current
ping state stays unknown: recorded turns, including scheduled-task turns, are
activity, and their metadata does not establish whether they are keepalive pings.

## Fast mode

The `fast-mode` check names the current effective local preference, its owner
and the standard-speed replacement: Claude `fastMode: false`, or Codex
`service_tier = "default"`. When the current setting's premium over the
recorded usage is above zero, turning it off is worth considering (yellow, exit
0), and `doctor` offers it when the profile's own file owns the setting: the
premium is measured, but the speed it buys is not valued. A premium measured at
nothing keeps the setting, and one that can't be priced, such as with no
recorded usage, gives no verdict. Use `/fast off` for an active session.
Standard current settings never receive a change because of historical premium
sessions.

Claude resolution includes `fastModePerSessionOptIn`, managed `fastMode: false`,
project/local files at recorded working directories, model support and the active
profile's `CLAUDE_CODE_DISABLE_FAST_MODE`. Codex resolves local system/user
settings, independent fast/ultrafast feature gates and local requirements.
Unresolved named/legacy profile selection, project trust/overrides,
unsupported providers and unsupported models stay advisory. Unrecorded CLI/session overrides
and remote account policy are unknown, not inferred from old sessions.

Each card separates observed response-speed premiums from a current-setting
estimate applied to recorded usage. Native Codex usage records do not record a
billed tier; requested turn/thread settings are retained separately and never
count as a billed premium. Claude response usage can record billed speed. Missing
speed, usage or exact pricing stays unknown. Dollar comparisons use the same
model and standard API rates, preserving recorded cache reads, write lifetimes,
geography and context tiers. These are **rate-only API-price equivalents**:
Claude's separate fast/standard caches mean cache-switch effects are excluded.
They hold work constant, do not value latency, and make no combined savings
claim. A plan counts fast mode its own way; see
[Plans and API prices](#plans-and-api-prices). Fast mode compares with the current
profile, independently of the TTL/window/idle recommendations.

Measured Codex usage without a billed tier receives a **conditional API equivalent**
in the shared pricing layer. It uses a supported historical requested tier, or
standard rates when the request omits a tier or says `auto`. Omitted and `auto`
can inherit a nonstandard OpenAI project default; requests can also be served at
a different tier. Today's config never establishes a historical tier. Unsupported
requested tiers, conflicting evidence, missing usage and unavailable prices stay
unknown. Recorded effective tiers always take precedence.

Reports count conditional requests separately from recorded tier coverage.
`--details` shows standard, fast and ultrafast served-tier scenarios for
conditionally priced parts, preserving recorded tiers and all context/cache
categories. Unsupported scenarios, including Flex and Scale without verified
rates, remain unknown. These are sensitivity examples, not bounds on actual
charges. Context keep/off, compaction and keepalive winners that depend on missing tiers stay
advisory because tier sensitivity could reverse them. SSH reports use the same pricing and evidence.

## Background generation

The `background-generation` check prints one line per feature: Claude prompt
suggestions, session recaps and goal check-ins, and Codex automatic recaps. Each
names its current value and the setting that turns it off:
`promptSuggestionEnabled`, `awaySummaryEnabled`,
`CLAUDE_CODE_GOAL_CHECKIN_MINUTES=0` or `tui.auto_recap = false`.

Separate lines retain distinct owners across recorded projects and charge only
their requests. Claude project/local settings and their `env` blocks participate;
Codex uses trusted project config layers, closest to the working directory first.
Missing directories, unknown trust and unrecorded profile selection stay advisory.
When two lines of one feature would read the same, each tag names its group inside
the brackets: why it resolved apart (`working directory changed`, `working directory
missing`, `working directory absent`, `launch environment conflict`, or for Codex
`legacy profile configuration`, `named config profiles`, `named profiles change root
or trust` or `project trust unknown`),
then how many directories, then one of them. A line that already differs carries none.
Current files are applied to history as an estimate, not proof of past settings.

Neither client logs these requests, so suggestion and recap costs are estimated
at each recorded interactive-terminal turn where they would have run. Their uses
are not recorded either, so one never counts as unused: a whole cost above
`--max-cost-per-use` is worth considering off, an advisory, and one below it is
kept. Goal check-ins and Codex recaps
cannot be told apart in transcripts, so their cost is shown as unknown.

## Vibes

`token-max vibes` is the advice the transcripts can't price, which `doctor`
leaves out so that every line of its report is measured. It reads the hosts,
profiles and window `doctor` reads, takes the same selection flags (`--since`,
the directory flags, `--ssh`, `--plain` and the price
flags), and asks the same question about [how a profile pays](#how-a-profile-pays).

Each piece of advice is a card: the setting or feature, the change advised
when it names one, what the setting does, the advice beside what was measured,
and `Source`, which says whose advice it is, a provider's page or the
maintainer's pick. A card has no verdict and no saving, and nothing here sets
an exit code.

| Card | When |
| --- | --- |
| [Bash timeout cap](#bash-timeout-cap) | Every Claude settings file: the cap beside the long waits measured under it. |
| Main-session effort | The default level is above `high`: Anthropic's [model guide](https://claude.dev/blog/which-claude-model-should-you-build-with/) and [model configuration docs](https://code.claude.com/docs/en/model-config#choose-an-effort-level) for Claude, OpenAI's [reasoning guide](https://developers.openai.com/api/docs/guides/reasoning) for Codex. |
| An instruction file | A CLAUDE.md of 200 lines or more, past Claude Code's [guidance](https://code.claude.com/docs/en/memory#write-effective-instructions), or an AGENTS.md at or above the byte budget Codex [loads](https://learn.chatgpt.com/docs/agent-configuration/agents-md), with what each 100 lines of it costs. |
| Explore subagent | Claude's built-in Explore ran off Haiku: what it cost, the same tokens on the cheapest model, and the model guide's tip. |
| Fable weekly use | Any Fable request in the window: what they cost, and Anthropic's [plan rule](https://support.claude.com/en/articles/15424964-claude-fable-models-on-your-plan). |
| [Code intelligence](#code-intelligence) | A Claude session read or edited a file of a language whose official language server plugin it ran without, or whose server isn't on `PATH`: Anthropic's [costs page](https://code.claude.com/docs/en/costs#install-code-intelligence-plugins-for-typed-languages). |

The effort, instruction-file, subagent and Fable lines stay in `doctor`, where
their costs are measured; only the advice about them is here.

On a terminal, a card that names the settings to change ends with `Change`,
the edit and its file, and asks `Accept?` with `y`, `n` or `q` to stop. Every
other card is shown with no question. The accepted edits are then shown as a
diff of each file and written only after a final yes, with the checks, lock
and [backups](#backups) `doctor` uses. One card is one edit to one file, even
when it sets two keys. A change another owner would override, such as a shell
variable or T3, is named on its card as a step by hand. Without a terminal,
`vibes` prints the cards, each change naming its edit, and edits nothing.

`doctor` offers `vibes` when its walk ends, counting the cards that advise
something, as `vibes`' own heading does: a setting already as advised still
has its card in `vibes`, and is not worth the question. Under `--defaults`,
`doctor` names no count, since `vibes` reads the settings as they are. With `--ssh`, each host's cards name their host, and
each destination checks and writes its own files, as in
[`doctor`'s walk](#on-ssh-hosts).

### Bash timeout cap

Claude Code waits on a foreground Bash call for at most
`max(BASH_MAX_TIMEOUT_MS, BASH_DEFAULT_TIMEOUT_MS)`, 600000 ms by default,
then moves the command to the background. A wait at least as long as the cache
TTL lets the cache expire before the model replies, so the next request
rewrites the whole prefix.

The card compares the cap with the main-agent TTL in effect now: the current
setting, else the lifetime the latest main-agent request wrote. It never uses
a TTL `doctor` recommends changing to. The card `doctor` hands off to comes
from the scan made before `doctor` wrote anything, so right after a TTL change
is written it still advises the cap for the old TTL: accepted after a change
from 1h to 5m, a 55m cap outlasts the new cache until the next run advises 4m.
Run `token-max vibes` again after changing the TTL. It advises 240000 ms
under a 5m cache and 3300000 ms under 1h, a margin that leaves room for the
model's reply on either side of the wait, whenever the cap is anything else: a
higher cap comes down, and a lower one goes up. That margin is the maintainer's pick, not a measurement, and the card
says so. A cap already there, or one with no TTL to compare with or an owner
T3 left unsettled, has a card that advises nothing.

The card counts the foreground Bash waits that outlasted the cache the
starting request wrote, how many hit their timeout or slept in a polling loop,
and what their cache misses cost at API prices over the window: the prefix the next request
wrote instead of read, at the write rate minus the read rate. Those are
measured. A wait of 5m or more whose request recorded no cache lifetime, under a TTL the
card can't resolve, is counted as not compared rather than as short; a shorter
one fits under either lifetime. What another cap would save is not: a lower cap doesn't kill a
command, which keeps running in the background, where checking on it takes
later turns the transcripts can't predict; a higher cap trades those check-ins
for longer foreground waits. Some commands need longer in the foreground, so
the card is advice to weigh: `doctor` gives it no line or verdict, and offers
it only through `vibes` once its own walk ends. When the owner of the cap
can't be resolved, the card says the cap it read may not be the one the
sessions ran under. Project and local settings,
which override the user file the edit goes to, are not read, and the card
says so.

Background commands and the user's own processes are unaffected by the cap.
Codex's `background_terminal_max_timeout` bounds one poll of a background
terminal, and each poll reads the cache, so it has no card.

### Code intelligence

A [code intelligence plugin](https://code.claude.com/docs/en/plugins/code-intelligence)
runs a language server for Claude Code, so Claude looks up symbols instead of
searching text and sees the type errors its edits introduce. Anthropic's
[costs page](https://code.claude.com/docs/en/costs#install-code-intelligence-plugins-for-typed-languages)
recommends one for typed languages; what it saves is not measured.

Each Claude settings file gets one card per language of that page's table
whose files its sessions read or edited (`Read`, `Edit`, `MultiEdit` or
`Write`), by the extensions each plugin's marketplace entry maps: the share of
the window's sessions that touched one, a subagent's counted as its parent's,
and the projects they ran in. Only the language is kept, never the path. A
project is the main checkout of the Git repository holding the session's
directory, or for a directory removed since, such as a worktree, the nearest
one that remains. Marker files such as `Cargo.toml` are not read: most
recorded directories are gone by the time the report runs.

A card names the plugin's state, read from `installed_plugins.json` in the
profile's plugins directory (`CLAUDE_CODE_PLUGIN_CACHE_DIR`, else `plugins`
in the profile) and `enabledPlugins` in the user and managed settings, and
whether its server's program is on the `PATH` token-max runs with, which may
not be the one Claude Code starts with:

| State | Change |
| --- | --- |
| Not installed, or installed only for other projects | By hand: `claude plugin install <plugin>@claude-plugins-official`, since `enabledPlugins` turns a plugin on but doesn't download it. |
| Installed and turned off in the user settings | `enabledPlugins` set to `true` there, offered as an edit. |
| Turned off by managed settings | None; the card advises nothing. |
| Installed, with its server not on `PATH` | By hand: install the program its README names. |

A plugin installed everywhere with its server on `PATH` gets no card.
Project and local `enabledPlugins`, plugins synced from claude.ai and plugins
of your own that run the same server are not read.

## Main-session effort

Each settings file that decides it gets one `Main-session effort` line, and a
T3 instance its own: the reasoning effort its main
session defaults to, the level it runs at, what each recorded model and effort
pair cost over the window and how many requests it ran, and the setting that
changes the default. A different effort changes the output tokens, so no cost
is repriced, no saving is claimed, and the line gives no verdict; `doctor`
never offers to change it. A default token-max cannot resolve, such as a T3
thread's selection, is unknown.

A default above `high` gets a [`vibes`](#vibes) card citing the provider's own guidance. For
Claude, `xhigh` and `max` cite Anthropic's
[model guide](https://claude.dev/blog/which-claude-model-should-you-build-with/)
(past `high`, test the next model up at a lower effort), and `max` also the
[model configuration docs](https://code.claude.com/docs/en/model-config#choose-an-effort-level)
(`max` is prone to overthinking; test it before adopting it broadly). For
Codex, `xhigh` and `max` cite OpenAI's
[reasoning guide](https://developers.openai.com/api/docs/guides/reasoning)
(`xhigh` only where evals show a benefit; `max` for the most complex tasks,
evaluated against `xhigh`). See
[default effort](semantics.md#main-session-effort) for how the default
resolves.

## Subagent models

The report gives every Claude Code subagent
type (Explore, Plan, general-purpose, forks and each custom agent) and every
Codex agent role its own line: what it cost over how many runs, the models and
reasoning efforts it ran on, where its model came from, and what the same
recorded tokens would cost on the provider's cheapest current model (the
newest Haiku, or the cheapest current GPT). Like feature costs, these are
totals over the whole window. Each line names the settings that change the
model and the effort, such as `model:` and `effort:` in
`~/.claude/agents/Explore.md`, or a Codex role file's `model` and `[agents]
default_subagent_model`. A file that replaces a built-in such as Explore must
restate its tool restriction and system prompt.

The lines are evidence, never recommendations: their dot is dim, with no
verdict, and they do not change the exit code or offer a change. A cheaper model trades
capability, and the main agent's extra work after a weaker subagent cannot be
attributed. The reprice assumes the same turns, and a lower effort's cost
cannot be measured. The source is unknown when today's settings and agent
files do not explain the recorded model, for example after a file changed.

When Claude's built-in Explore ran off Haiku, a [`vibes`](#vibes) card gives what it cost,
the same tokens on the cheapest model, and the tip from Anthropic's
[model guide](https://claude.dev/blog/which-claude-model-should-you-build-with/):
a user agent named `Explore` with `model: haiku`. A run of an agent file
named `Explore`, which replaces the built-in, gets none. Other types get no card:
the guide splits subagents by whether they search or edit, which the
transcripts don't record.
See [subagent models](semantics.md#subagent-models).

## Fable weekly use

The report gives every Claude settings file whose profiles sign in to
Anthropic its own Fable line, shared by native Claude Code and the T3
instances that use it: what its Fable requests cost at API prices in each 7-day span
of the window, counted back from the report. With any Fable request, a
[`vibes`](#vibes) card gives their cost beside the plan rule Anthropic documents in
[Fable models on your plan](https://support.claude.com/en/articles/15424964-claude-fable-models-on-your-plan),
as [Plans and API prices](#plans-and-api-prices) describes: on a Claude Max or
Ultra plan or a Team or seat-based Enterprise premium seat, Fable models can
use up to half the weekly usage limit and use it faster than other models; on
Pro or a Team or seat-based Enterprise standard seat they run on usage credits;
on usage-based Enterprise they bill at API rates. Weeks before the profile's first Fable request
are left out.

No plan limit is published in tokens or dollars, so the line shows no share of
the limit and gives no verdict: its dot is dim, and it never changes the exit
code or offers a change. The figures are partial evidence, which the card
says too. The cap counts Fable
use on every Claude surface, such as claude.ai and Claude Desktop, while the
line covers only its profiles' recorded sessions, and it may include requests
paid with usage credits after the cap was reached. The plan's weekly reset time
is not recorded, so spans end at the report rather than at a reset. When
a Claude profile with another settings file that signs in to Anthropic reads
the same transcripts,
the line says how many Fable requests it shares, since whose they were is
unknown; one on a cloud provider has no plan limit and is not counted. A profile
with `CLAUDE_CODE_USE_BEDROCK`, `CLAUDE_CODE_USE_VERTEX` or
`CLAUDE_CODE_USE_FOUNDRY` is billed by its cloud provider and gets no line.
Change the model with `/model` or the `model` setting.
See [Fable weekly use](semantics.md#fable-weekly-use).

## Messages from other sessions

Each Claude settings file gets one `Messages from other sessions` line: what
the turns that messages from your other Claude Code sessions started in an
idle session cost over the window, how many such messages arrived, how many
more arrived between tool calls of a running turn, the current
`crossSessionInbound` value and the file that sets it. Claude Code starts a
new turn for a message to an idle session, which resends the whole
conversation, and a break past the cache TTL makes that turn a cache miss
([costs doc](https://code.claude.com/docs/en/costs#why-usage-climbs-in-a-long-session)).
An idle notice another session sends back when asked with `notify_when_idle`
counts as a message. A subagent's report to its own session is not one: no
setting gates it.

`crossSessionInbound` set to `refuse` drops each message, and `hold` shows it
as a notice and keeps it undelivered until an `accept` applies
([settings reference](https://code.claude.com/docs/en/settings-reference#crosssessioninbound),
[cross-session messaging](https://code.claude.com/docs/en/cross-session-messaging#control-inbound-messages)),
so the cost is every request of those turns, the subagents they started
included: the work done in them is what the setting trades. With no value
set, Claude Code decides per message from the two sessions' permission modes,
holding one only when one session bypasses permission prompts and the other
does not, so the line shows `unset, by permission mode` rather than `accept`.
Messages read between tool calls are counted but not priced, since what one
added to its turn cannot be told apart.

The line is evidence, never a recommendation: its dot is dim, with no
verdict, and it never changes the exit code or offers a change. Sessions that
coordinate, such as an orchestrator and its workers, depend on these messages,
so a high cost per message is no reason to stop them. Codex has no such
setting and gets no line. See
[Messages from other sessions](semantics.md#messages-from-other-sessions).
