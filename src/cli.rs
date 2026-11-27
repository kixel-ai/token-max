//! The command line: arguments, progress, output streams, cancellation and exit codes.

use std::io::{self, IsTerminal, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::ser::Formatter;

use crate::audit::{Audit, Filter};
use crate::collections::IndexMap;
use crate::defaults::Credit;
use crate::discovery::{DiscoveryOptions, discover};
use crate::doctor::{
    BillingQuestions, Confirm, Fix, Found, Offers, Owner, Selection, Style, Total, Walking, ask,
    ask_upgrade, confirm, finish, opening, preview, stale, upgrade_line, write, wrote,
};
use crate::model::{Billing, Location, Millis, Scan, Verdict};
use crate::prices::{Catalog, PriceOptions, cancel_downloads, load_prices};
use crate::reader::{Reading, ScanOptions, scan};
use crate::replay::{AnalyzeOptions, analyze};
use crate::report::{Listing, Origin, ReportOptions, format_report, opening_note, totals_section};
use crate::safe::{ScanError, display};
use crate::sessions::{Session, export};
use crate::ssh::{self, Reply, Scope};
use crate::terminal::{WrapOptions, wrap_text};
use crate::totals::{Counts, Totals};
use crate::tui::{self, Bridge, DiffPreview, ExitKind, Hero, Tab, VibeRow, WriteResult};
use crate::upgrade::{self, Check, Install};
use crate::vibes::{self, Item};
use crate::wire::{Format, Job, Paid, Request};

const HELP: &str =
    "token-max: agent settings that use less of a Claude or ChatGPT plan, measured at API prices

Usage: token-max <command> [options]

Commands:
  doctor    Report which settings to change, then offer to change them
  vibes     Advice the transcripts can't price, with no verdict
  sessions  Each session's priced usage as JSON lines
  audit     A repository's priced transcript hotspots as JSON, for the token-audit skill

  -v, --version  Show the package version
  -h, --help     Show this help

Run token-max <command> --help for a command's options.
";

const DOCTOR_HELP: &str = "token-max doctor: agent settings that use less of a Claude or ChatGPT plan, measured at API prices

Usage: token-max doctor [options]

  --since YYYY-MM-DD|ISO  Select from midnight UTC or an instant with timezone
  --claude-dir DIR        Select a Claude profile (repeatable)
  --codex-dir DIR         Select a Codex home, including archived sessions (repeatable)
  --t3-dir DIR            Select a T3 state directory (repeatable)
  --ssh HOST              Also audit an SSH destination, putting this build there first if needed (repeatable)
  --max-cost-per-use USD  Suggest turning off a used tool, skill or MCP server that cost more than this per use (default: 1)
  --details               Show grouped check evidence, assumptions, uncertainty and sources
  --defaults              Report as a user with no settings files would see it, offering nothing
  --plain                 Plain text without color or decorative Unicode, asking line by line
  --offline               Use cached public prices and skip the release check, without network access
  --prices-file FILE      Use a local models.dev catalog instead of fetching prices
  --price-cache FILE      Cache path (default: ~/.cache/token-max/models.json)
  -v, --version           Show the package version
  -h, --help              Show this help

Unless --offline, it first asks GitHub for the latest token-max release. When that is
newer, a terminal run offers to put it in place of this binary, checked against its
published checksum, and rerun with the same options; a binary Cargo or Nix installed
gets that tool's command instead. Without a terminal, or if the check cannot run, it
says so on stderr and goes on with this build.

Prints which settings to change, with the evidence for each. When stdin and stdout are
both a terminal, it then offers the changes it can make in sections, such as never-used
skills or the cache TTLs, as a checklist (Space checks, s splits a section, Enter shows
the checked edits as a diff of each file, y writes, q stops), and writes the profile's own
settings.json or config.toml only after that y. With --plain or --details, or under
TERM=dumb, it asks about each section instead (y, n, s to split a section, or q to stop)
and writes after a final yes. A change another owner would override, such as a shell variable,
a project or managed settings file or T3, is listed to make by hand. Without a terminal it
prints the report, each change or section naming its settings and the rest one line
per kind of feature or setting, and edits nothing; --details lists every line under its
row, and each edit's exact JSON or TOML.

With --defaults it reads every user, project and local settings file, T3 Code's settings
and the shell's setting variables as unset, so each setting is at its Claude Code or Codex
default and every tool, skill and MCP server is on; managed and system policy still apply.
It prints that report and edits nothing, even on a terminal.

Without profile flags, discover local Claude/Codex profiles and T3 instances. Explicit profiles
limit the entire run. This host is always audited, and directory flags select its profiles
only. Each SSH destination discovers its own profiles with this build of token-max, copied
there or downloaded there from this version's release and cached; it prices its own history
with this host's prices, and only its report and its totals cross SSH, never its scan. Several hosts get one
total, their figures added as they are. Its changes are listed to make there. Current
settings are applied to the selected history; checks can use different windows and
baselines. Independent savings do not add.

Examples:
  token-max doctor --since 2026-11-01
  token-max doctor --claude-dir ~/.claude-work --codex-dir ~/.codex --details
  token-max doctor --offline --plain
  token-max doctor --ssh devbox --ssh user@laptop

Dollars are public API prices, not an invoice: the closest public measure of what a request
uses of a Claude or ChatGPT plan's limits, and what Claude's usage credits bill past them. A
line notes where a provider documents that a plan counts something differently, such as fast
mode. Which plan a profile is signed in to is never read; on a terminal, doctor asks how
each Claude profile pays, to price its main-agent cache TTL against the one default that
applies. Never retains or prints prompt, reply or component text. Color is automatic on
terminals; NO_COLOR disables it. Progress goes to stderr only on interactive terminals,
never with redirected output.
SIGINT/SIGTERM cancel with exit 2 and no success report; settings are written only after
the list of accepted changes is confirmed.

Every line is measured. Advice the transcripts can't price, such as a provider's guidance
or the Bash timeout cap, is in token-max vibes: when the walk ends and vibes has advice,
doctor asks whether to show it, from the same scan. Without a terminal it names the command.

When it walks through the changes on a terminal: exit 0, or 2 on failure or cancellation.
Otherwise: exit 0 for no applicable change (advisory findings may remain), 1 for a
recommended change, 2 for a scan, pricing, argument or cancellation failure.
";

const VIBES_HELP: &str = "token-max vibes: advice the transcripts can't price, with no verdict

Usage: token-max vibes [options]

  --since YYYY-MM-DD|ISO  Select from midnight UTC or an instant with timezone
  --claude-dir DIR        Select a Claude profile (repeatable)
  --codex-dir DIR         Select a Codex home, including archived sessions (repeatable)
  --t3-dir DIR            Select a T3 state directory (repeatable)
  --ssh HOST              Also read an SSH destination, putting this build there first if needed (repeatable)
  --plain                 Plain text without color or decorative Unicode, asking line by line
  --offline               Use cached public prices without network access
  --prices-file FILE      Use a local models.dev catalog without network access
  --price-cache FILE      Cache path (default: ~/.cache/token-max/models.json)
  -h, --help              Show this help

Prints one card for each piece of advice token-max doctor leaves out because no transcript
can price it: a provider's guidance, such as an effort above high or an instruction file
past its budget, settings whose saving is a guess, such as the Bash timeout cap, and
Kixel's picks, such as auto memory off where most sessions ran in a repository. Each
card says what was measured beside the advice and names whose advice it is: the provider's
page, or Kixel's pick. None of it is a verdict.

When stdin and stdout are both a terminal, the cards are a checklist, each unchecked: Space
checks a card that names the settings to change, Enter shows the checked edits as a diff of
each file, and y writes the profile's own settings.json, one edit per file. With --plain,
each such card asks instead (y, n, or q to stop), and a final yes writes. A change another
owner would override is named to make by hand. Every other card is shown with no question.
Without a terminal it prints the cards, each change naming its edit, and edits nothing.

It reads the hosts, profiles and window token-max doctor reads, with the same flags, and
on a terminal it asks the same question about how each Claude profile pays.

Exit 0: printed, or walked. Exit 2: scan, pricing, argument or cancellation failure.
";

const SESSIONS_HELP: &str =
    "token-max sessions: each transcript session's usage at public API prices

Usage: token-max sessions [options]

  --since YYYY-MM-DD|ISO  Count requests from midnight UTC or an instant with timezone
  --claude-dir DIR        Select a Claude profile (repeatable)
  --codex-dir DIR         Select a Codex home, including archived sessions (repeatable)
  --t3-dir DIR            Select a T3 state directory (repeatable)
  --ssh HOST              Also export an SSH destination's sessions, putting this build there first if needed (repeatable)
  --offline               Use cached public prices without network access
  --prices-file FILE      Use a local models.dev catalog without network access
  --price-cache FILE      Cache path (default: ~/.cache/token-max/models.json)
  -h, --help              Show this help

Prints one JSON object per line for each Claude Code or Codex session with a request in the
window, ordered across hosts by its first request. A Claude subagent and a Codex review or
spawned thread are sessions of their own. Fields: host, the --ssh destination as given, or
null for this host; harness; session, its native ID; parent, the session it was spawned
from; review, whether Codex marked it a review; cwd; repositories, remote URLs without
userinfo, resolved on the session's host, or null; repositoriesSource, recorded when Codex
recorded the repository, or checkout when read now from the Git checkout at cwd, which may
have changed since the session; branches, each Git branch
its records in the window name with the first and last time, or else the last named before
it; start and end of its requests; models, one entry per model and reasoning effort with
requests (each request once in every entry it reaches), inputTokens (uncached),
cacheWriteTokens, cacheReadTokens, outputTokens, costUsd and complete; costUsd, the exact
total, which can differ in the last digit from adding the models' costUsd; complete;
skippedLines, the unreadable lines in its files; files. A cost is a lower bound when
complete is false: a request could not be priced, or a file had unreadable or unfinished
lines.

This host's sessions are always exported, and directory flags select its profiles only.
Each SSH destination exports its own sessions with this host's prices, and only its session
lines cross SSH. A session found on several hosts is printed once per host. Any failed host
fails the whole run.

Costs are public API prices, not an invoice. Reads transcripts and Git config only
and prints no prompt or reply text. Errors go to stderr as JSON.
Exit 0: printed. Exit 2: scan, pricing, argument or cancellation failure.
";

const AUDIT_HELP: &str = "token-max audit: a repository's priced transcript hotspots as JSON

Usage: token-max audit [options]

  --all                   Audit every session, not only the current repository's
  --repo DIR              Audit the repository of the Git checkout at DIR instead
  --limit N               Keep N entries in each ranked list (default: 10)
  --since YYYY-MM-DD|ISO  Select from midnight UTC or an instant with timezone (default: 14 days ago)
  --claude-dir DIR        Select a Claude profile (repeatable)
  --codex-dir DIR         Select a Codex home, including archived sessions (repeatable)
  --t3-dir DIR            Select a T3 state directory (repeatable)
  --ssh HOST              Also audit an SSH destination, putting this build there first if needed (repeatable)
  --offline               Use cached public prices without network access
  --prices-file FILE      Use a local models.dev catalog without network access
  --price-cache FILE      Cache path (default: ~/.cache/token-max/models.json)
  -h, --help              Show this help

Prints one JSON object of where Claude Code and Codex sessions spent tokens they need not
have, priced at public API prices, for an agent such as the token-audit skill to inspect.
Each finding points at its transcript as host, file and line, so the agent reads only those
slices. By default it covers sessions in the current directory's Git repository, matched by
remote URL, or by Git directory when the repository has no remote; --repo picks another
checkout, and --all every session.

cacheMisses lists requests that lost cache reuse against the one before them, by cause:
tool_wait_ttl, a tool call outlasting the cache; idle_ttl, a gap outlasting it; control_change,
a model, effort, speed, compaction or truncation change; or prefix_bust, a changed prefix
within the cache's lifetime. toolOutputs lists tool results by tool and the ones later
requests carried the most, with what carrying them cost. sessions lists the sessions that
stand out by cost, uncached input, total input, first prompt, subagent cost, tool calls,
tool errors and repeated calls to one tool, each once with the lines of its first
requests, and ranks them per measure by index. assumptions names what the prices assume.
Each ranking keeps --limit entries.

This host is always audited, and directory flags select its profiles only. Each SSH
destination audits its own sessions with this host's prices, matching the repository by
remote URL, and only its ranked findings cross SSH. Any failed host fails the whole run.

Never prints or keeps prompt, reply, tool input or tool output text. Errors go to stderr as
JSON. Exit 0: printed. Exit 2: scan, pricing, argument or cancellation failure.
";

/// What a cancelling signal interrupts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Scanning and analyzing, before anything is written.
    Running,
    /// Waiting on the person's answers; nothing has been written to a settings file yet.
    Asking,
    /// Waiting on answers about `vibes`' advice after `doctor`'s walk wrote its changes.
    AskingMore,
    /// Writing the report or settings, which finishes whole.
    Writing,
}

/// The current phase. A cancelling signal holds this lock until the process exits, so a
/// report or a settings file is either written whole or not at all.
static PHASE: Mutex<Phase> = Mutex::new(Phase::Running);

/// What currently occupies stderr's live progress region: either a 1-line bar (`cells`)
/// or a multi-line animated piggy-bank panel (`rows`).
#[derive(Default)]
struct ProgressState {
    cells: usize,
    rows: u16,
}

/// The live progress region ending stderr, which the next output must clear first.
static PROGRESS: Mutex<ProgressState> = Mutex::new(ProgressState { cells: 0, rows: 0 });

/// Replace the progress line on stderr with `line`, whose text without color is `plain`.
/// Spaces, not escapes, blank what a longer line left, so this works on any terminal and
/// under `--plain`. A line too wide for one row shows `plain` cut to fit, since a carriage
/// return cannot reach a row the line wrapped onto, and a cut could split a color escape.
fn show_progress(line: &str, plain: &str) {
    let mut shown = PROGRESS.lock().unwrap_or_else(PoisonError::into_inner);
    if shown.rows > 0 {
        let mut stderr = io::stderr();
        let _ = crossterm::execute!(
            stderr,
            crossterm::cursor::MoveUp(shown.rows),
            crossterm::cursor::MoveToColumn(0),
            crossterm::terminal::Clear(crossterm::terminal::ClearType::FromCursorDown)
        );
        shown.rows = 0;
    }
    let room = progress_room();
    let cells = crate::terminal::cells(plain);
    let (text, cells) = if cells > room {
        let cut = crate::terminal::fit(plain, room);
        (cut, crate::terminal::cells(cut))
    } else {
        (line, cells)
    };
    let pad = " ".repeat(shown.cells.min(room).saturating_sub(cells));
    eprint!("\r{text}{pad}");
    shown.cells = cells;
}

/// Draw a multi-line animated scan block on stderr without flicker.
fn show_progress_block(lines: &[String]) {
    let mut shown = PROGRESS.lock().unwrap_or_else(PoisonError::into_inner);
    let mut stderr = io::stderr();
    let _ = crossterm::execute!(stderr, crossterm::terminal::BeginSynchronizedUpdate);
    if shown.rows > 0 {
        let _ = crossterm::execute!(
            stderr,
            crossterm::cursor::MoveUp(shown.rows),
            crossterm::cursor::MoveToColumn(0),
        );
    } else if shown.cells > 0 {
        let _ = write!(
            stderr,
            "\r{}\r",
            " ".repeat(shown.cells.min(progress_room()))
        );
        shown.cells = 0;
    }
    let _ = crossterm::execute!(
        stderr,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::FromCursorDown)
    );
    for line in lines {
        let _ = writeln!(stderr, "{line}");
    }
    let _ = crossterm::execute!(stderr, crossterm::terminal::EndSynchronizedUpdate);
    let _ = stderr.flush();
    shown.rows = u16::try_from(lines.len()).unwrap_or(u16::MAX);
}

/// The cells a progress line may take: one less than stderr's terminal is wide now, so
/// neither the line nor the spaces blanking it wrap after the terminal shrinks.
fn progress_room() -> usize {
    let columns: usize =
        terminal_size::terminal_size_of(io::stderr()).map_or(80, |(w, _)| w.0.into());
    columns.max(2) - 1
}

/// Clear the progress line or block, if one is showing, so what follows starts on a clean row.
/// The returned lock keeps another thread from drawing one again while it is held.
fn clear_progress() -> MutexGuard<'static, ProgressState> {
    let mut shown = PROGRESS.lock().unwrap_or_else(PoisonError::into_inner);
    if shown.rows > 0 {
        let mut stderr = io::stderr();
        let _ = crossterm::execute!(
            stderr,
            crossterm::terminal::BeginSynchronizedUpdate,
            crossterm::cursor::MoveUp(shown.rows),
            crossterm::cursor::MoveToColumn(0),
            crossterm::terminal::Clear(crossterm::terminal::ClearType::FromCursorDown),
            crossterm::terminal::EndSynchronizedUpdate
        );
        let _ = stderr.flush();
        shown.rows = 0;
    } else if shown.cells > 0 {
        eprint!("\r{}\r", " ".repeat(shown.cells.min(progress_room())));
        shown.cells = 0;
    }
    shown
}

/// How the progress line draws.
#[derive(Clone, Copy)]
struct Look {
    color: bool,
    unicode: bool,
}

/// The progress line, redrawn on its own thread so the reading bar moves while the scan holds
/// the thread that started it. Dropping it stops that thread and clears the line.
struct Meter {
    shared: Arc<Shown>,
    stop: Option<mpsc::Sender<()>>,
    ticker: Option<thread::JoinHandle<()>>,
}

/// What the progress line shows: the current phase and when it began, how far the local scan
/// has read, and what the hosts count to. It outlives each [`Meter`] that draws it, so a host's
/// totals count from whenever they arrive.
struct Shown {
    /// The phase, empty while no meter draws the line.
    phase: Mutex<(&'static str, Instant)>,
    /// What the line last showed before its figures, unless the reading bar.
    head: Mutex<String>,
    reading: Arc<Reading>,
    /// Each host's totals by its index, this host's first, once its report is rendered.
    hosts: Mutex<Vec<Option<Totals>>>,
    /// Whose the counts are when other hosts' are left out, such as "this host".
    whose: Option<&'static str>,
    /// Whether the line tallies the savings while checking, as `doctor`'s does; `vibes`'
    /// advice is unmeasured.
    tally: bool,
    look: Look,
}

impl Shown {
    fn new(look: Look, reading: Arc<Reading>, whose: Option<&'static str>, tally: bool) -> Self {
        Shown {
            phase: Mutex::new(("", Instant::now())),
            head: Mutex::new(String::new()),
            reading,
            hosts: Mutex::new(vec![]),
            whose,
            tally,
            look,
        }
    }

    /// What the hosts count to so far: this host's from its scan until its report settles
    /// them, then added to each other host's in the order the report's total adds them.
    fn counts(&self) -> Option<Counts> {
        let hosts = self.hosts.lock().unwrap_or_else(PoisonError::into_inner);
        let here = hosts.first().cloned().flatten().map_or_else(
            || {
                *self
                    .reading
                    .counts
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
            },
            |totals| Some(totals.counts()),
        );
        let mut sum: Option<Counts> = None;
        let there = hosts.iter().skip(1).flatten().map(Totals::counts);
        for counts in here.into_iter().chain(there) {
            match &mut sum {
                Some(sum) => sum.merge(&counts),
                None => sum = Some(counts),
            }
        }
        sum
    }

    /// What the hosts tally to so far: this host's from its checks until its report settles
    /// them, then added to each other host's as the report's total adds them. The savings
    /// the changes found, and what the settings save against the defaults, None under
    /// `--defaults`.
    fn tally(&self) -> Option<(Total, Option<Credit>)> {
        let hosts = self.hosts.lock().unwrap_or_else(PoisonError::into_inner);
        let here = match hosts.first().cloned().flatten() {
            Some(totals) => Some((totals.savings, totals.credit)),
            None => self
                .reading
                .checked
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
                .map(|checked| (checked.savings(), checked.credit.clone())),
        };
        let there = hosts
            .iter()
            .skip(1)
            .flatten()
            .map(|t| (t.savings, t.credit.clone()));
        let mut sum: Option<(Total, Option<Credit>)> = None;
        for (savings, credit) in here.into_iter().chain(there) {
            match &mut sum {
                Some((total, sum)) => {
                    total.merge(&savings);
                    // Every host runs with the same `--defaults`, so all have a credit or none.
                    if let (Some(sum), Some(credit)) = (sum, &credit) {
                        sum.merge(credit);
                    }
                }
                None => sum = Some((savings, credit)),
            }
        }
        sum
    }

    /// Count host `index`'s settled `totals`, this host's at 0, in place of what its scan
    /// counted and its checks tallied, and show them now, so the line never clears before
    /// showing them.
    fn settle(&self, index: usize, totals: &Totals) {
        {
            let mut hosts = self.hosts.lock().unwrap_or_else(PoisonError::into_inner);
            if hosts.len() <= index {
                hosts.resize(index + 1, None);
            }
            hosts[index] = Some(totals.clone());
        }
        let phase = self.phase.lock().unwrap_or_else(PoisonError::into_inner);
        let (name, _) = *phase;
        // While the bar or the scan's step holds the line, the ticker shows them within its
        // next tick.
        if name.is_empty() || self.reading() {
            return;
        }
        let mut head = self.head.lock().unwrap_or_else(PoisonError::into_inner);
        // The bar may have held the line since the phase began, so no phase line shows yet.
        if head.is_empty() {
            *head = busy(name, Duration::ZERO, 0, self.look.unicode);
        }
        let line = with(head.clone(), self.figures(), self.whose, progress_room());
        show_progress(&line, &line);
    }

    /// Show the phase line `head` with the figures after it.
    fn show(&self, head: String) {
        let mut shown = self.head.lock().unwrap_or_else(PoisonError::into_inner);
        let line = with(head.clone(), self.figures(), self.whose, progress_room());
        show_progress(&line, &line);
        *shown = head;
    }

    /// The figures that follow the phase on the line: this host's checks done, then the
    /// tallies, then the counts.
    fn figures(&self) -> Vec<Piece> {
        let checked = self
            .reading
            .checked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|c| (c.done, c.of));
        let mut pieces: Vec<Piece> = checked
            .map(|(done, of)| Piece {
                text: format!("{done}/{of} checks"),
                keep: 9,
                count: true,
            })
            .into_iter()
            .collect();
        if self.tally
            && let Some((savings, credit)) = self.tally()
        {
            pieces.extend(tallies(&savings, credit.as_ref()));
        }
        pieces.extend(self.counts().map(|c| figures(&c)).unwrap_or_default());
        pieces
    }

    /// Whether this host's reading bar or the scan's step after it holds the line.
    fn reading(&self) -> bool {
        let reading = &self.reading;
        reading.read.load(Ordering::Relaxed) < reading.total.load(Ordering::Relaxed)
            || reading
                .step
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_some()
    }
}

impl Meter {
    /// Start drawing `shared`.
    fn start(shared: Arc<Shown>) -> Self {
        *shared.phase.lock().unwrap_or_else(PoisonError::into_inner) = ("", Instant::now());
        let (stop, stopped) = mpsc::channel::<()>();
        let ticker = {
            let shared = shared.clone();
            thread::spawn(move || {
                let mut began = None;
                let mut read_end = None;
                let mut tick = 0;
                while let Err(mpsc::RecvTimeoutError::Timeout) =
                    stopped.recv_timeout(Duration::from_millis(100))
                {
                    tick += 1;
                    let phase = shared.phase.lock().unwrap_or_else(PoisonError::into_inner);
                    let (name, since) = *phase;
                    if name.is_empty() {
                        continue;
                    }
                    let total = shared.reading.total.load(Ordering::Relaxed);
                    let read = shared.reading.read.load(Ordering::Relaxed);
                    let room = progress_room();
                    let height = terminal_size::terminal_size_of(io::stderr())
                        .map_or(24usize, |(_, h)| h.0.into());
                    let rich_scan = shared.look.unicode && room >= 64 && height >= 14;
                    // The local scan may still be reading after the phase moves on, as
                    // when SSH hosts' prices or reports arrive first.
                    if read < total {
                        let began = *began.get_or_insert_with(Instant::now);
                        let progress = Progress {
                            read,
                            total,
                            elapsed: began.elapsed(),
                        };
                        if rich_scan {
                            let lines = piggy_progress(name, Some(&progress), &shared, tick, room);
                            show_progress_block(&lines);
                        } else {
                            let figures = shared.figures();
                            let (line, plain) =
                                bar(&progress, figures, shared.whose, shared.look, room);
                            show_progress(&line, &plain);
                        }
                    } else {
                        // Reading has ended only once the scan knows what it reads.
                        if total != 0 {
                            read_end.get_or_insert_with(Instant::now);
                        }
                        let step = *shared
                            .reading
                            .step
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner);
                        let (name, since) = after_reading((name, since), step, read_end);
                        if rich_scan {
                            let lines = piggy_progress(name, None, &shared, tick, room);
                            show_progress_block(&lines);
                        } else {
                            shared.show(busy(name, since.elapsed(), tick, shared.look.unicode));
                        }
                    }
                }
            })
        };
        Self {
            shared,
            stop: Some(stop),
            ticker: Some(ticker),
        }
    }

    /// Show `phase` now, unless the reading bar or the scan's step holds the line.
    fn phase(&self, phase: &'static str) {
        let mut current = self
            .shared
            .phase
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *current = (phase, Instant::now());
        // While this host still reads, the bar or its step stays; the ticker shows the phase
        // after them.
        if self.shared.reading() {
            return;
        }
        self.shared
            .show(busy(phase, Duration::ZERO, 0, self.shared.look.unicode));
    }
}

impl Drop for Meter {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(ticker) = self.ticker.take() {
            let _ = ticker.join();
        }
        // Until another meter starts, a host's totals arriving draw nothing.
        *self
            .shared
            .phase
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = ("", Instant::now());
        let mut head = self
            .shared
            .head
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        head.clear();
        drop(clear_progress());
    }
}

/// What the line shows once this host's reading is done, and since when: the scan's own
/// step while it runs, as the bar holds the line before it, else `phase`, timed from when
/// reading ended if it began under the bar.
fn after_reading(
    phase: (&'static str, Instant),
    step: Option<(&'static str, Instant)>,
    read_end: Option<Instant>,
) -> (&'static str, Instant) {
    step.unwrap_or(match read_end {
        Some(end) => (phase.0, phase.1.max(end)),
        None => phase,
    })
}

/// A phase's progress line: after its first second, a spinner on `tick` and how long it has
/// run, so a long phase shows it has not stalled.
fn busy(phase: &str, elapsed: Duration, tick: usize, unicode: bool) -> String {
    let mut line = format!("token-max: {phase}...");
    if elapsed >= Duration::from_secs(1) {
        let frames: &[&str] = if unicode {
            &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
        } else {
            &["|", "/", "-", "\\"]
        };
        let seconds = elapsed.as_secs();
        line.push_str(&format!(" {} {seconds}s", frames[tick % frames.len()]));
    }
    line
}

/// A figure on the progress line, how long it stays as the row narrows (the lowest `keep` is
/// dropped first), and whether it is one of the hosts' counts, which a label may name.
struct Piece {
    text: String,
    keep: u8,
    count: bool,
}

/// The counts as the line shows them: requests and sessions as the report's header writes
/// them, tokens short, and the spend once priced.
fn figures(counts: &Counts) -> Vec<Piece> {
    let piece = |text, keep| Piece {
        text,
        keep,
        count: true,
    };
    let mut pieces = vec![
        piece(crate::report::plural(counts.requests, "request"), 5),
        piece(crate::report::plural(counts.sessions, "session"), 3),
        piece(format!("{} tokens", crate::report::short(counts.tokens)), 2),
    ];
    if let Some(spend) = &counts.spend {
        pieces.push(piece(spend.text(), 6));
    }
    pieces
}

/// The tallies as the line shows them, each kind apart as the report keeps it: what the
/// settings save against the defaults, `credit`, then what the changes found so far save on
/// top, `savings`; those that depend on how usage is paid after them, then how many were not
/// measured, dropped in the reverse order.
fn tallies(savings: &Total, credit: Option<&Credit>) -> Vec<Piece> {
    let piece = |text, keep| Piece {
        text,
        keep,
        count: true,
    };
    let saving = credit.map(Credit::tally).unwrap_or_default();
    let found = savings.tally(credit.is_some());
    let mut pieces: Vec<Piece> = saving.measured.map(|t| piece(t, 8)).into_iter().collect();
    pieces.extend(found.measured.map(|t| piece(t, 7)));
    pieces.extend(saving.up_to.into_iter().map(|t| piece(t, 4)));
    pieces.extend(found.up_to.into_iter().map(|t| piece(t, 4)));
    pieces.extend(saving.unmeasured.map(|t| piece(t, 1)));
    pieces.extend(found.unmeasured.map(|t| piece(t, 1)));
    pieces
}

/// The pieces that fit in `room` cells after `used`, each after two spaces, taking the most
/// kept first and skipping any too wide for what is left, with `whose` they are before the
/// first count; and the cells left over.
fn fit(pieces: Vec<Piece>, whose: Option<&str>, used: usize, room: usize) -> (String, usize) {
    let label = whose.map_or_else(String::new, |whose| format!("{whose}: "));
    let width = |taken: &[bool]| -> usize {
        let shown = || {
            pieces
                .iter()
                .zip(taken)
                .filter(|(_, t)| **t)
                .map(|(p, _)| p)
        };
        let label = match shown().any(|p| p.count) {
            true => crate::terminal::cells(&label),
            false => 0,
        };
        let texts: usize = shown().map(|p| 2 + crate::terminal::cells(&p.text)).sum();
        label + texts
    };
    let mut order: Vec<usize> = (0..pieces.len()).collect();
    order.sort_by_key(|&at| std::cmp::Reverse(pieces[at].keep));
    let mut taken = vec![false; pieces.len()];
    for at in order {
        taken[at] = true;
        if used + width(&taken) > room {
            taken[at] = false;
        }
    }
    let left = room.saturating_sub(used + width(&taken));
    let mut text = String::new();
    let mut labeled = false;
    for (p, _) in pieces.iter().zip(&taken).filter(|(_, t)| **t) {
        text.push_str("  ");
        if p.count && !labeled {
            text.push_str(&label);
            labeled = true;
        }
        text.push_str(&p.text);
    }
    (text, left)
}

/// `head` followed by the `figures` that fit in `room` cells, with `whose` they are.
fn with(head: String, figures: Vec<Piece>, whose: Option<&str>, room: usize) -> String {
    let (tail, _) = fit(figures, whose, crate::terminal::cells(&head), room);
    head + &tail
}

/// The widest and narrowest the reading bar's track gets.
const TRACK_CELLS: (usize, usize) = (24, 8);

/// How far reading has come: bytes `read` of `total`, `elapsed` since reading began.
struct Progress {
    read: u64,
    total: u64,
    elapsed: Duration,
}

/// The reading bar and `figures` after it, with `whose` they are, fitted to `room` cells:
/// how much is read, and when the rest should be, from the rate so far. What doesn't fit
/// beside the narrowest track is dropped, least useful first, and the track widens into what
/// is left. Returns the line and its text without color.
fn bar(
    progress: &Progress,
    figures: Vec<Piece>,
    whose: Option<&str>,
    look: Look,
    room: usize,
) -> (String, String) {
    let Progress {
        read,
        total,
        elapsed,
    } = *progress;
    let fraction = read as f64 / total as f64;
    let percent = format!(" {:>3.0}%", (fraction * 100.0).floor());
    let mut pieces = vec![Piece {
        text: format!(
            "{} of {}",
            crate::report::size(read),
            crate::report::size(total)
        ),
        keep: 1,
        count: false,
    }];
    // A rate needs a second of reading, and a few percent, to mean anything.
    if elapsed >= Duration::from_secs(1) && fraction >= 0.02 {
        let left = elapsed.as_secs_f64() * (1.0 - fraction) / fraction;
        pieces.push(Piece {
            text: format!("~{} left to read", duration(left)),
            keep: 4,
            count: false,
        });
    }
    pieces.extend(figures);
    let lead = "token-max: Reading ";
    let (open, close) = if look.unicode { ("", "") } else { ("[", "]") };
    let used = crate::terminal::cells(lead) + open.len() + close.len() + percent.len();
    let (rest, left) = fit(pieces, whose, used + TRACK_CELLS.1, room);
    let track = (TRACK_CELLS.1 + left).min(TRACK_CELLS.0);
    let rest = percent + &rest;
    // Eighths of a cell, so the bar moves smoothly on a Unicode terminal.
    let eighths = (fraction * (track * 8) as f64) as usize;
    let (full, part) = (eighths / 8, eighths % 8);
    let (done, todo) = if look.unicode {
        let partial = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"][part];
        let filled = format!("{}{partial}", "█".repeat(full));
        let left = track - full - usize::from(!partial.is_empty());
        (filled, "░".repeat(left))
    } else {
        ("#".repeat(full), "-".repeat(track - full))
    };
    let plain = format!("{lead}{open}{done}{todo}{close}{rest}");
    let line = if look.color {
        format!("{lead}{open}\x1b[35m{done}\x1b[2m{todo}\x1b[0m{close}{rest}")
    } else {
        plain.clone()
    };
    (line, plain)
}

/// Render the roomy 10-line half-block piggy-bank loading panel beside real live telemetry
/// (no decorative/random bars — only real read/check progress, request counts, and savings).
fn piggy_progress(
    phase: &str,
    reading: Option<&Progress>,
    shared: &Shown,
    tick: usize,
    room: usize,
) -> Vec<String> {
    let color = shared.look.color;
    let s = |code: &str, text: &str| {
        if color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    };
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    // Advance coin drop every 2 ticks (~200ms per stage) for a smooth, unhurried drop.
    let pig = tui::piggy_lines(color, true, tick / 2);
    let right_room = room.saturating_sub(37);
    let spin = s("1;35", FRAMES[tick % FRAMES.len()]);

    // Row 1: Brand wordmark
    let wordmark = format!(
        "{}{}  {}",
        s("1", "token-"),
        s("1;35", "max"),
        s("2", env!("CARGO_PKG_VERSION"))
    );

    // Row 3: Active phase with braille spinner
    let phase_line = format!(
        "{spin} {}",
        s(
            "1",
            crate::terminal::fit(&format!("{phase}…"), right_room.saturating_sub(3))
        )
    );

    // Row 4: Real progress bar (bytes read during reading, or completed checks during analysis)
    let checked = shared
        .reading
        .checked
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map(|c| (c.done, c.of));

    let bar_line = if let Some(p) = reading {
        let frac = (p.read as f64 / p.total.max(1) as f64).clamp(0.0, 1.0);
        let track = right_room.saturating_sub(24).clamp(10, 26);
        let filled = ((frac * track as f64).round() as usize).min(track);
        let bar = format!(
            "{}{}{}",
            s("35", &"━".repeat(filled)),
            if filled < track {
                s("35", "╸")
            } else {
                String::new()
            },
            s(
                "2",
                &"─".repeat(track.saturating_sub(filled + usize::from(filled < track)))
            )
        );
        let pct = format!(" {:>3.0}%", frac * 100.0);
        let eta = if p.elapsed >= Duration::from_secs(1) && frac >= 0.02 {
            let left = p.elapsed.as_secs_f64() * (1.0 - frac) / frac;
            format!(" · ~{} left", duration(left))
        } else {
            String::new()
        };
        let bytes = format!(
            "  {} / {}{eta}",
            crate::report::size(p.read),
            crate::report::size(p.total)
        );
        let bytes_fit = crate::terminal::fit(&bytes, right_room.saturating_sub(track + 5));
        format!("{bar}{}{}", s("1", &pct), s("2", bytes_fit))
    } else if let Some((done, of)) = checked
        && of > 0
    {
        let frac = (done as f64 / of as f64).clamp(0.0, 1.0);
        let track = right_room.saturating_sub(18).clamp(10, 26);
        let filled = ((frac * track as f64).round() as usize).min(track);
        let bar = format!(
            "{}{}{}",
            s("35", &"━".repeat(filled)),
            if filled < track {
                s("35", "╸")
            } else {
                String::new()
            },
            s(
                "2",
                &"─".repeat(track.saturating_sub(filled + usize::from(filled < track)))
            )
        );
        format!("{bar}  {}", s("2", &format!("{done}/{of} checks")))
    } else {
        String::new()
    };

    // Row 5: Real live counts (sessions, requests, API spend)
    let counts_line = shared
        .counts()
        .map(|c| {
            let mut parts = vec![];
            if c.sessions > 0 {
                parts.push(format!("{} sessions", tui::commas(c.sessions)));
            }
            if c.requests > 0 {
                parts.push(format!("{} reqs", tui::commas(c.requests)));
            }
            if let Some(spent) = c.spend {
                parts.push(crate::report::spend_text(
                    spent.usd,
                    spent.margin,
                    spent.floor,
                    spent.partial,
                ));
            }
            s("2", crate::terminal::fit(&parts.join(" · "), right_room))
        })
        .unwrap_or_default();

    // Row 6: Real live savings found so far
    let found_line = if shared.tally
        && let Some((savings, _)) = shared.tally()
    {
        let usd = savings.usd().unwrap_or(0.0);
        if usd >= 1.0 {
            s(
                "1;32",
                &format!("+~{}/mo found so far", crate::report::about(usd)),
            )
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    let right = [
        String::new(),
        String::new(),
        wordmark,
        String::new(),
        phase_line,
        bar_line,
        counts_line,
        found_line,
        String::new(),
        String::new(),
    ];

    pig.into_iter()
        .zip(right)
        .map(|(p, r)| {
            if r.is_empty() {
                format!("  {p}")
            } else {
                format!("  {p}   {r}")
            }
        })
        .collect()
}

/// Whole seconds as `42s` or `3m 05s`.
fn duration(seconds: f64) -> String {
    let seconds = seconds.ceil() as u64;
    match seconds {
        0..60 => format!("{seconds}s"),
        _ => format!("{}m {:02}s", seconds / 60, seconds % 60),
    }
}

/// Enter `phase`; blocks forever if a cancelling signal already owns the process.
fn enter(phase: Phase) {
    *PHASE.lock().unwrap_or_else(PoisonError::into_inner) = phase;
}

/// The parsed flags: each string option's value, every value of a repeatable one, and each switch.
#[derive(Debug, Default, PartialEq)]
struct Values {
    since: Option<String>,
    max_cost_per_use: Option<String>,
    repo: Option<String>,
    limit: Option<String>,
    all: bool,
    offline: bool,
    plain: bool,
    details: bool,
    defaults: bool,
    version: bool,
    help: bool,
    claude_dirs: Vec<String>,
    codex_dirs: Vec<String>,
    t3_dirs: Vec<String>,
    ssh: Vec<String>,
    /// Run as an SSH audit destination; the coordinator passes this, not people.
    ssh_scan: bool,
    prices_file: Option<String>,
    price_cache: Option<String>,
}

impl Values {
    /// The string option named `name`, or None for an unknown or boolean option.
    fn string(&mut self, name: &str, value: String) -> Option<()> {
        match name {
            "since" => self.since = Some(value),
            "max-cost-per-use" => self.max_cost_per_use = Some(value),
            "repo" => self.repo = Some(value),
            "limit" => self.limit = Some(value),
            "claude-dir" => self.claude_dirs.push(value),
            "codex-dir" => self.codex_dirs.push(value),
            "t3-dir" => self.t3_dirs.push(value),
            "ssh" => self.ssh.push(value),
            "prices-file" => self.prices_file = Some(value),
            "price-cache" => self.price_cache = Some(value),
            _ => return None,
        }
        Some(())
    }

    fn flag(&mut self, name: &str) -> Option<&mut bool> {
        Some(match name {
            "all" => &mut self.all,
            "offline" => &mut self.offline,
            "plain" => &mut self.plain,
            "details" => &mut self.details,
            "defaults" => &mut self.defaults,
            "version" => &mut self.version,
            "help" => &mut self.help,
            "ssh-scan" => &mut self.ssh_scan,
            _ => return None,
        })
    }

    fn is_string(name: &str) -> bool {
        matches!(
            name,
            "since"
                | "max-cost-per-use"
                | "repo"
                | "limit"
                | "claude-dir"
                | "codex-dir"
                | "t3-dir"
                | "ssh"
                | "prices-file"
                | "price-cache"
        )
    }
}

/// Parse strictly: no positionals, no unknown options, no inline values for
/// booleans, and a dash-led separate value is rejected as ambiguous.
fn parse(args: &[String]) -> Option<Values> {
    let mut values = Values::default();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if arg == "--" {
            // Everything after the terminator is positional.
            return rest.next().is_none().then_some(values);
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value.to_string())),
                None => (long, None),
            };
            if Values::is_string(name) {
                let value = match inline {
                    Some(value) => value,
                    None => {
                        let value = rest.next()?;
                        if value.len() > 1 && value.starts_with('-') {
                            return None;
                        }
                        value.clone()
                    }
                };
                values.string(name, value)?;
            } else {
                if inline.is_some() {
                    return None;
                }
                *values.flag(name)? = true;
            }
        } else if let Some(shorts) = arg.strip_prefix('-')
            && !shorts.is_empty()
        {
            // A group of short booleans, such as `-vh`.
            for c in shorts.chars() {
                let name = match c {
                    'v' => "version",
                    'h' => "help",
                    _ => return None,
                };
                *values.flag(name)? = true;
            }
        } else {
            return None;
        }
    }
    Some(values)
}

/// Parse `--since` as midnight UTC of a date or an ISO instant with an explicit offset.
fn since(value: &str) -> Result<Millis, ScanError> {
    let b = value.as_bytes();
    let date = b.len() >= 10
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[7] == b'-'
        && b[8..10].iter().all(u8::is_ascii_digit);
    let instant = || {
        if b[10] != b'T' {
            return false;
        }
        let rest = &value[11..];
        let offset = rest.len() >= 6 && {
            let o = &rest.as_bytes()[rest.len() - 6..];
            matches!(o[0], b'+' | b'-')
                && o[1..3].iter().all(u8::is_ascii_digit)
                && o[3] == b':'
                && o[4..6].iter().all(u8::is_ascii_digit)
        };
        (rest.ends_with('Z') || offset) && !rest.contains(['\n', '\r', '\u{2028}', '\u{2029}'])
    };
    if !(date && (b.len() == 10 || instant())) {
        return Err(ScanError::new(
            "--since requires YYYY-MM-DD (UTC) or an ISO instant with timezone",
        ));
    }
    let invalid = || ScanError::new("invalid --since date");
    let day: jiff::civil::Date = value[..10].parse().map_err(|_| invalid())?;
    let midnight = day
        .to_zoned_datetime(jiff::tz::TimeZone::UTC)
        .map_err(|_| invalid())?
        .timestamp()
        .as_millisecond();
    if b.len() == 10 {
        return Ok(midnight);
    }
    let time = iso_time(&b[10..]).ok_or_else(invalid)?;
    Ok(midnight + time)
}

/// Milliseconds after UTC midnight for `THH:mm[:ss[.fff]]` and `Z` or `+HH:mm`:
/// 24:00 is the next midnight, seconds stop at 59, extra fraction digits drop.
fn iso_time(b: &[u8]) -> Option<Millis> {
    let two = |at: usize, max: i64| -> Option<i64> {
        let digits = b.get(at..at + 2)?;
        if !digits.iter().all(u8::is_ascii_digit) {
            return None;
        }
        let n = i64::from(digits[0] - b'0') * 10 + i64::from(digits[1] - b'0');
        (n <= max).then_some(n)
    };
    if b.first() != Some(&b'T') || b.get(3) != Some(&b':') {
        return None;
    }
    let (hour, minute) = (two(1, 24)?, two(4, 59)?);
    let mut i = 6;
    let (mut second, mut millis) = (0, 0);
    if b.get(i) == Some(&b':') {
        second = two(i + 1, 59)?;
        i += 3;
        if b.get(i) == Some(&b'.') {
            let digits = b[i + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
            if digits == 0 {
                return None;
            }
            for k in 0..3 {
                let digit = if k < digits { b[i + 1 + k] - b'0' } else { 0 };
                millis = millis * 10 + i64::from(digit);
            }
            i += 1 + digits;
        }
    }
    if hour == 24 && (minute, second, millis) != (0, 0, 0) {
        return None;
    }
    let offset = match b.get(i..)? {
        b"Z" => 0,
        [sign @ (b'+' | b'-'), _, _, b':', _, _] => {
            let minutes = two(i + 1, 23)? * 60 + two(i + 4, 59)?;
            if *sign == b'+' { minutes } else { -minutes }
        }
        _ => return None,
    };
    Some(((hour * 60 + minute - offset) * 60 + second) * 1000 + millis)
}

/// Parse `--max-cost-per-use` as a plain dollar amount, with an optional leading `$`.
fn max_cost_per_use(value: &str) -> Result<f64, ScanError> {
    let value = value.strip_prefix('$').unwrap_or(value);
    let (whole, fraction) = match value.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (value, None),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    match value.parse::<f64>() {
        Ok(n) if digits(whole) && fraction.is_none_or(digits) && n.is_finite() => Ok(n),
        _ => Err(ScanError::new(
            "--max-cost-per-use requires a dollar amount such as 1 or 0.50",
        )),
    }
}

/// An environment variable as text; unset and non-Unicode values read as absent.
fn env(name: &str) -> Option<String> {
    std::env::var_os(name).map(|v| v.to_string_lossy().into_owned())
}

/// An environment variable that is set and not empty; an empty one reads as unset.
fn env_nonempty(name: &str) -> Option<String> {
    env(name).filter(|v| !v.is_empty())
}

/// The terminal's width when stdout is a terminal, which output wraps to.
fn width() -> Option<usize> {
    let stdout = io::stdout();
    if !stdout.is_terminal() {
        return None;
    }
    let columns = terminal_size::terminal_size_of(&stdout).map_or(0, |(w, _)| w.0);
    Some(if columns == 0 { 80 } else { columns.into() })
}

fn help(text: &str) -> String {
    let width = width();
    text.split('\n')
        .map(|row| {
            // Option descriptions continue under their own column.
            let continuation = row.strip_prefix("  -").and_then(|rest| {
                let bytes = rest.as_bytes();
                let mut i = 0;
                while i < bytes.len() {
                    if bytes[i] == b' ' {
                        let run = bytes[i..].iter().take_while(|&&b| b == b' ').count();
                        if run >= 2 && i + run < bytes.len() {
                            return Some(3 + i + run);
                        }
                        i += run;
                    } else {
                        i += 1;
                    }
                }
                None
            });
            wrap_text(
                row,
                WrapOptions {
                    width,
                    indent: 0,
                    continuation,
                },
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Numbers print in their shortest round-trip form, integers without a fraction, such as `1`
/// rather than `1.0`.
struct JsFormatter;

impl JsFormatter {
    fn number<W: ?Sized + Write>(writer: &mut W, n: serde_json::Number) -> io::Result<()> {
        writer.write_all(crate::exposure::number(&n).as_bytes())
    }
}

impl Formatter for JsFormatter {
    fn write_f64<W: ?Sized + Write>(&mut self, writer: &mut W, value: f64) -> io::Result<()> {
        let n = serde_json::Number::from_f64(value).expect("serde_json writes non-finite as null");
        Self::number(writer, n)
    }
    fn write_f32<W: ?Sized + Write>(&mut self, writer: &mut W, value: f32) -> io::Result<()> {
        self.write_f64(writer, value.into())
    }
    fn write_u64<W: ?Sized + Write>(&mut self, writer: &mut W, value: u64) -> io::Result<()> {
        Self::number(writer, value.into())
    }
    fn write_i64<W: ?Sized + Write>(&mut self, writer: &mut W, value: i64) -> io::Result<()> {
        Self::number(writer, value.into())
    }
    /// Terminal controls and direction overrides print as `\uXXXX`, which decodes to the same
    /// string, so a path or URL from a transcript or a destination cannot drive the terminal.
    fn write_string_fragment<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        fragment: &str,
    ) -> io::Result<()> {
        let mut start = 0;
        for (at, c) in fragment.char_indices() {
            if crate::safe::unsafe_char(c) {
                writer.write_all(&fragment.as_bytes()[start..at])?;
                write!(writer, "\\u{:04x}", c as u32)?;
                start = at + c.len_utf8();
            }
        }
        writer.write_all(&fragment.as_bytes()[start..])
    }
}

/// Stream a value as one compact JSON line, numbers formatted by `JsFormatter`.
fn write_json(writer: impl Write, value: &impl Serialize) -> io::Result<()> {
    let mut writer = io::BufWriter::new(writer);
    let mut serializer = serde_json::Serializer::with_formatter(&mut writer, JsFormatter);
    value.serialize(&mut serializer).map_err(io::Error::other)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

/// Print only a safe diagnostic; never transcript content or partial findings.
fn diagnose(json: bool, error: &ScanError) {
    #[derive(Serialize)]
    struct Diagnostic<'a> {
        error: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        location: Option<&'a Location>,
    }
    // Held while the diagnostic prints, so no progress line is drawn over it.
    let _progress = clear_progress();
    let mut stderr = io::stderr().lock();
    let _ = if json {
        write_json(
            &mut stderr,
            &Diagnostic {
                error: &error.message,
                location: error.location.as_ref(),
            },
        )
    } else {
        let location = error
            .location
            .as_ref()
            .map(|l| format!(" at {}:{}", display(&l.file), l.line))
            .unwrap_or_default();
        writeln!(stderr, "token-max: {}{location}", error.message)
    };
}

/// What a successful run prints, decided before anything is written.
enum Output {
    Text(String),
    /// Help for a command line that names no command, which goes to stderr.
    Usage(String),
    /// Already streamed, as an SSH audit destination does.
    Written,
    /// A report, then the walk through its changes on this terminal.
    Walk(Box<Walk>),
}

/// Every host's changes, to walk through on this terminal.
struct Walk {
    hosts: Vec<WalkHost>,
    /// The report across hosts, to print again after `doctor`'s walk.
    listing: Option<Box<Listing>>,
    /// What `doctor`'s walk prints before its questions: the total across hosts, then this
    /// host's header, and what the settings save against the defaults.
    head: String,
    style: Style,
    /// Which command's changes the hosts offer; `doctor`'s walk goes on to `vibes`'.
    command: Command,
    /// Show the exact JSON or TOML of the edits left to make.
    details: bool,
    /// Use the rich inline crossterm TUI rather than plain line-by-line prompts.
    rich_tui: bool,
    /// Summary hero metadata for the interactive TUI.
    hero: Option<Hero>,
}

/// The command a run's hosts are read for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Command {
    Doctor,
    Vibes,
}

/// One host in the walk.
struct WalkHost {
    /// Its name, when the walk covers another host than this one.
    label: Option<String>,
    side: Side,
    /// How its changes print: its paths from its own home.
    style: Style,
    /// What `vibes` has for it, read again there once `doctor`'s walk is over.
    items: Vec<Item>,
    /// Whether a walk wrote its files, so a destination's audit is ended, not dropped.
    wrote: bool,
}

/// Where a host's files are read and written.
enum Side {
    /// This host's own.
    Here {
        owner: Owner,
        /// Its spend every 30 days.
        monthly: f64,
        /// The report lines whose changes the walk wrote, which can move what `vibes` advises.
        written: Vec<Origin>,
    },
    /// A destination's, through its open audit.
    There {
        offers: Box<Offers>,
        remote: ssh::Remote,
    },
}

impl WalkHost {
    fn offers(&self) -> &Offers {
        match &self.side {
            Side::Here { owner, .. } => owner.offers(),
            Side::There { offers, .. } => offers,
        }
    }

    /// Go on from `doctor`'s walk to `vibes`': its advice as it reads after whatever `doctor`
    /// wrote, and the changes it names, checked against the files as they are now.
    fn advise(&mut self) -> Result<(), ScanError> {
        match &mut self.side {
            Side::Here {
                owner,
                monthly,
                written,
            } => {
                self.items = vibes::settled(std::mem::take(&mut self.items), written);
                *owner = vibes::owner(&self.items, self.style.home.clone(), *monthly);
            }
            Side::There { offers, remote } => {
                let (items, theirs) = remote.vibes()?;
                self.items = items;
                *offers = theirs;
            }
        }
        Ok(())
    }
}

/// Every host's advice as it reads once the walk asks the sections their changes share.
fn join(hosts: &mut [WalkHost]) {
    vibes::joined(&mut hosts.iter_mut().map(|h| &mut h.items).collect::<Vec<_>>());
}

/// One host before its job runs here: this host's scan, or a destination's reply.
enum Scanned {
    Local(Box<Scan>),
    Remote(Reply),
}

/// What reaches the coordinator while hosts scan, in whatever order it finishes.
enum Arrival {
    Prices(Result<Catalog, ScanError>),
    /// A report destination's profiles, which it scans once their answers are sent.
    Profiles(usize, ssh::Profiles),
    Host(usize, Result<Scanned, ScanError>),
}

/// What one host's scan is analyzed and printed with.
pub struct Render<'a> {
    /// The prices every request is costed with.
    pub catalog: &'a Catalog,
    /// Requests before this instant stay out of the report, when set.
    pub since: Option<Millis>,
    /// The instant the report is generated at.
    pub now: Millis,
    /// A context item that cost more than this per use is advised off.
    pub max_cost_per_use_usd: f64,
    /// How the report is printed.
    pub format: &'a Format,
    /// The host's home directory, which its report shows as `~`.
    pub home: Option<&'a str>,
    /// Where the report header's counts go once priced, for a progress line.
    pub reading: Option<&'a Reading>,
}

/// One host's report and whether it recommends a change; all a destination sends.
pub struct Printed {
    /// The report, which joins every other host's in a run of several.
    pub listing: Box<Listing>,
    /// Whether the report recommends a change.
    pub change: bool,
    /// What the report adds to a total across hosts.
    pub totals: Box<Totals>,
    /// The advice the transcripts cannot price, which `vibes` shows and `doctor` offers.
    pub vibes: Vec<Item>,
}

/// This host's own report, with the changes it names.
pub struct Rendered {
    /// The report and its totals.
    pub printed: Printed,
    /// The edits the report recommends, which the walk offers to apply.
    pub fixes: Vec<Fix>,
    /// MCP servers whose tools are off already, which the walk offers to remove.
    pub turned_off: Vec<crate::mcp::Definition>,
}

/// Analyze one host's scan and print its report.
pub fn render(scan: Scan, options: Render<'_>) -> Result<Rendered, ScanError> {
    let report = analyze(AnalyzeOptions {
        scan,
        catalog: options.catalog,
        since: options.since,
        now: options.now,
        max_cost_per_use_usd: options.max_cost_per_use_usd,
        reading: options.reading,
    })?;
    let change = report
        .checks
        .iter()
        .any(|c| c.action() == crate::model::Action::Change)
        || crate::context::setting_rows(&report.context).any(|c| {
            c.verdict == Some(Verdict::Off)
                && crate::defaults::applied(&report.scan.profiles, c).is_none()
        });
    let format = options.format;
    // Computed once, since each reads the files that may outrank a switch: the edits the
    // report names and the walk's offers are the same value.
    let mut fixes = crate::doctor::fixes(&report, format.unicode);
    let mut listing = format_report(&report, &fixes, format.report(options.home));
    crate::doctor::explain(&mut fixes, &listing);
    let vibes = std::mem::take(&mut listing.vibes);
    Ok(Rendered {
        printed: Printed {
            totals: Box::new(Totals::new(&report, &fixes, listing.credit())),
            listing: Box::new(listing),
            change,
            vibes,
        },
        fixes,
        turned_off: crate::mcp::turned_off(&report.scan.profiles),
    })
}

/// Whether options that only the coordinator decides were given to a destination, which scans
/// with its own discovery while only the window crosses SSH.
fn chosen_here(values: &Values) -> bool {
    values.offline
        || values.prices_file.is_some()
        || values.price_cache.is_some()
        || !values.claude_dirs.is_empty()
        || !values.codex_dirs.is_empty()
        || !values.t3_dirs.is_empty()
        || !values.ssh.is_empty()
}

/// The `--since` bound; an empty value is absent.
fn window(values: &Values) -> Result<Option<Millis>, ScanError> {
    match values.since.as_deref() {
        None | Some("") => Ok(None),
        Some(value) => since(value).map(Some),
    }
}

/// Where public prices come from.
fn price_options(
    prices_file: Option<String>,
    price_cache: Option<String>,
    offline: bool,
) -> Result<PriceOptions, ScanError> {
    let cache = match price_cache.filter(|cache| !cache.is_empty()) {
        Some(cache) => PathBuf::from(cache),
        None => {
            // The XDG spec ignores an empty or relative base, which would put the cache under
            // whatever directory the command ran in.
            let base = env("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .filter(|base| base.is_absolute());
            let base = match base {
                Some(base) => base,
                None => std::env::home_dir()
                    .ok_or_else(|| ScanError::new("cannot discover home directory"))?
                    .join(".cache"),
            };
            base.join("token-max").join("models.json")
        }
    };
    Ok(PriceOptions {
        file: prices_file
            .filter(|file| !file.is_empty())
            .map(PathBuf::from),
        cache,
        offline,
    })
}

/// `token-max sessions`: price each session on the selected hosts.
fn sessions(args: &[String]) -> Result<(Output, u8), ScanError> {
    let invalid = || ScanError::new("invalid arguments; use token-max sessions --help");
    let mut values = parse(args).ok_or_else(invalid)?;
    if values.help {
        return Ok((Output::Text(help(SESSIONS_HELP)), 0));
    }
    let report_only = values.plain
        || values.details
        || values.defaults
        || values.version
        || values.max_cost_per_use.is_some()
        || audit_only(&values);
    if report_only {
        return Err(invalid());
    }
    let since = window(&values)?;
    if values.ssh_scan {
        if chosen_here(&values) {
            return Err(invalid());
        }
        return Ok((Output::Written, ssh::serve(since, Scope::Sessions)));
    }
    let (discovery, destinations) = hosts(&mut values)?;
    let replies = gather(Gather {
        since,
        defaults: false,
        discovery,
        destinations,
        prices: price_options(values.prices_file, values.price_cache, values.offline)?,
        job: Job::Sessions,
        home: None,
        progress: None,
        ask: None,
    })?;
    let mut sessions: Vec<(jiff::Timestamp, Session)> = vec![];
    for (host, reply) in replies {
        let Reply::Sessions(found) = reply else {
            unreachable!("a sessions job gets sessions");
        };
        for mut session in found {
            let start = session.start.parse().map_err(|_| {
                ScanError::new(format!(
                    "{}: invalid session start",
                    host.as_deref().unwrap_or("this host")
                ))
            })?;
            session.host = host.clone();
            sessions.push((start, session));
        }
    }
    // Stable, so a tie keeps each host's own order, then the order hosts were selected in.
    sessions.sort_by_key(|(start, _)| *start);
    let mut lines = vec![];
    for (_, session) in sessions {
        write_json(&mut lines, &session)
            .map_err(|_| ScanError::new("analysis failed; no report produced"))?;
    }
    let lines = String::from_utf8(lines).expect("JSON is UTF-8");
    Ok((Output::Text(lines), 0))
}

/// Whether any option only `token-max audit` takes is set.
fn audit_only(values: &Values) -> bool {
    values.all || values.repo.is_some() || values.limit.is_some()
}

/// `token-max audit`: rank the selected sessions' hotspots on every host.
fn forensics(args: &[String]) -> Result<(Output, u8), ScanError> {
    let invalid = || ScanError::new("invalid arguments; use token-max audit --help");
    let mut values = parse(args).ok_or_else(invalid)?;
    if values.help {
        return Ok((Output::Text(help(AUDIT_HELP)), 0));
    }
    if values.plain
        || values.details
        || values.defaults
        || values.version
        || values.max_cost_per_use.is_some()
        || (values.all && values.repo.is_some())
    {
        return Err(invalid());
    }
    let limit = match values.limit.as_deref() {
        None => 10,
        Some(value) => value
            .parse::<usize>()
            .ok()
            .filter(|&n| n > 0 && value.bytes().all(|b| b.is_ascii_digit()))
            .ok_or_else(|| ScanError::new("--limit requires a whole number above zero"))?,
    };
    if values.ssh_scan {
        // The coordinator names the window and sends the filter and limit with its job.
        if chosen_here(&values) || audit_only(&values) || values.since.is_none() {
            return Err(invalid());
        }
        return Ok((Output::Written, ssh::serve(window(&values)?, Scope::Audit)));
    }
    let now = crate::reader::now();
    let since = window(&values)?.unwrap_or(now - 14 * 24 * 60 * 60 * 1000);
    let filter = if values.all {
        Filter::All
    } else {
        let (dir, named) = match values.repo.take() {
            Some(dir) => (dir, true),
            None => {
                let cwd = std::env::current_dir()
                    .map_err(|_| ScanError::new("cannot read the current directory"))?;
                (cwd.to_string_lossy().into_owned(), false)
            }
        };
        let absolute =
            std::path::absolute(&dir).map_err(|_| ScanError::new("--repo requires a directory"))?;
        let found = Filter::of(&absolute.to_string_lossy())?;
        match found {
            Some(filter) => filter,
            None if named => {
                return Err(ScanError::new(format!(
                    "--repo {dir} is not in a Git checkout"
                )));
            }
            None => {
                return Err(ScanError::new(
                    "the current directory is not in a Git checkout; use --all or --repo DIR",
                ));
            }
        }
    };
    if matches!(filter, Filter::Common(_)) && !values.ssh.is_empty() {
        return Err(ScanError::new(
            "the repository has no remote, so SSH hosts cannot match it; use --all",
        ));
    }
    let (discovery, destinations) = hosts(&mut values)?;
    let hosts_named: Vec<Option<String>> = std::iter::once(None)
        .chain(destinations.iter().cloned().map(Some))
        .collect();
    let replies = gather(Gather {
        since: Some(since),
        defaults: false,
        discovery,
        destinations,
        prices: price_options(values.prices_file, values.price_cache, values.offline)?,
        job: Job::Audit {
            filter: filter.clone(),
            limit,
            since,
        },
        home: None,
        progress: None,
        ask: None,
    })?;
    let mut audits = vec![];
    for (host, reply) in replies {
        let Reply::Audit(mut found) = reply else {
            unreachable!("an audit job gets audits");
        };
        if let Some(host) = &host {
            found.on(host);
        }
        audits.push(*found);
    }
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Printed<'a> {
        since: String,
        scope: Selected<'a>,
        hosts: Vec<Option<String>>,
        limit: usize,
        totals: crate::audit::Totals,
        cache_misses: crate::audit::CacheMisses,
        tool_outputs: crate::audit::ToolOutputs,
        sessions: crate::audit::Ranked,
        assumptions: &'a [&'a str],
    }
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Selected<'a> {
        all: bool,
        /// The remotes sessions were matched by, without scheme or user.
        remotes: Option<&'a [String]>,
        /// The Git directory sessions were matched by, for a repository with no remote.
        git_dir: Option<&'a str>,
    }
    let audit = Audit::merge(audits, limit);
    let printed = Printed {
        since: crate::safe::iso(since),
        scope: Selected {
            all: filter == Filter::All,
            remotes: match &filter {
                Filter::Remotes(remotes) => Some(remotes),
                _ => None,
            },
            git_dir: match &filter {
                Filter::Common(dir) => Some(dir),
                _ => None,
            },
        },
        hosts: hosts_named,
        limit,
        totals: audit.totals,
        cache_misses: audit.cache_misses,
        tool_outputs: audit.tool_outputs,
        sessions: audit.sessions.ranked(),
        assumptions: &crate::audit::ASSUMPTIONS,
    };
    let mut text = vec![];
    write_json(&mut text, &printed)
        .map_err(|_| ScanError::new("analysis failed; no report produced"))?;
    Ok((
        Output::Text(String::from_utf8(text).expect("JSON is UTF-8")),
        0,
    ))
}

/// Run one command, returning the output and exit code without writing either.
fn run(args: &[String]) -> Result<(Output, u8), ScanError> {
    match args.first().map(String::as_str) {
        None => return Ok((Output::Usage(help(HELP)), 2)),
        Some("doctor") => return audit(&args[1..], Command::Doctor),
        Some("vibes") => return audit(&args[1..], Command::Vibes),
        Some("sessions") => return sessions(&args[1..]),
        Some("audit") => return forensics(&args[1..]),
        Some(_) => {}
    }
    let invalid = || ScanError::new("invalid arguments; use --help");
    let values = parse(args).ok_or_else(invalid)?;
    let bare = Values {
        help: values.help,
        version: values.version,
        ..Values::default()
    };
    if values != bare {
        return Err(ScanError::new(
            "options follow a command, such as token-max doctor --since 2026-11-01; use --help",
        ));
    }
    if values.help {
        return Ok((Output::Text(help(HELP)), 0));
    }
    if values.version {
        return Ok((Output::Text(format!("{}\n", env!("CARGO_PKG_VERSION"))), 0));
    }
    Err(invalid())
}

/// The selected hosts: this host's discovery, and the SSH destinations.
///
/// Takes the directory flags, which select this host's profiles.
fn hosts(values: &mut Values) -> Result<(DiscoveryOptions, Vec<String>), ScanError> {
    for (index, destination) in values.ssh.iter().enumerate() {
        ssh::destination(destination)?;
        if values.ssh[..index].contains(destination) {
            return Err(ScanError::new(format!(
                "--ssh {destination} is selected twice"
            )));
        }
    }
    let mut discovery = DiscoveryOptions::host()?;
    discovery.claude_dirs = std::mem::take(&mut values.claude_dirs);
    discovery.codex_dirs = std::mem::take(&mut values.codex_dirs);
    discovery.t3_dirs = std::mem::take(&mut values.t3_dirs);
    Ok((discovery, std::mem::take(&mut values.ssh)))
}

/// A run across the selected hosts.
struct Gather {
    since: Option<Millis>,
    /// Pretend the settings files away on every host, for `doctor --defaults`.
    defaults: bool,
    /// This host's discovery.
    discovery: DiscoveryOptions,
    destinations: Vec<String>,
    prices: PriceOptions,
    /// What every host makes of its scan; a report's host label is set per host.
    job: Job,
    /// This host's home directory, which its report shows as `~`.
    home: Option<String>,
    /// Show each phase, and a bar while this host's transcripts are read, on stderr.
    progress: Option<Look>,
    /// Ask on this terminal how each host's Claude profiles pay, before that host scans.
    ask: Option<Ask>,
}

/// The billing questions: how each Claude profile on the Anthropic API with transcripts pays,
/// which picks its main TTL's default.
struct Ask {
    /// This host's profiles to ask about, by root.
    roots: Vec<String>,
    /// How the questions print, with this host's paths from its home.
    style: Style,
}

/// The terminal failed, as when it closed.
fn terminal_failed() -> ScanError {
    ScanError::new("cannot use the terminal; no report produced")
}

/// The billing questions in progress, and what arrives from the hosts meanwhile.
struct Asking<'a> {
    questions: BillingQuestions,
    results: &'a mpsc::Receiver<Arrival>,
    /// Each destination's profiles, by host index, once they arrive and until asked about.
    profiles: Vec<Option<ssh::Profiles>>,
    /// Everything else, for the loop that renders.
    early: Vec<Arrival>,
}

impl Asking<'_> {
    /// Keep `arrival`, failing on a host's or the prices' error.
    fn keep(&mut self, arrival: Arrival) -> Result<(), ScanError> {
        match arrival {
            Arrival::Profiles(at, profiles) => self.profiles[at] = Some(profiles),
            Arrival::Host(_, Err(error)) | Arrival::Prices(Err(error)) => return Err(error),
            arrival => self.early.push(arrival),
        }
        Ok(())
    }

    /// Ask how each profile at `roots` pays, first failing on any error that has arrived, so
    /// a failed host stops the run at the next question rather than after the last.
    fn ask(
        &mut self,
        roots: &[String],
        style: &Style,
    ) -> Result<IndexMap<String, Billing>, ScanError> {
        let mut answers = IndexMap::default();
        for root in roots {
            while let Ok(arrival) = self.results.try_recv() {
                self.keep(arrival)?;
            }
            let answer = self
                .questions
                .ask(
                    root,
                    style,
                    &mut io::stdin().lock(),
                    &mut io::stdout().lock(),
                )
                .map_err(|_| terminal_failed())?;
            if let Some(billing) = answer {
                answers.insert(root.clone(), billing);
            }
        }
        Ok(answers)
    }

    /// Destination `index`'s profiles, waiting for them to arrive.
    fn wait(&mut self, index: usize) -> Result<ssh::Profiles, ScanError> {
        loop {
            if let Some(profiles) = self.profiles[index].take() {
                return Ok(profiles);
            }
            let arrival = self
                .results
                .recv()
                .map_err(|_| ScanError::new("cancelled; no report produced"))?;
            self.keep(arrival)?;
        }
    }
}

/// Scan every selected host and load prices at once, then run the job on each, failing on the
/// first error. Returns each host's reply in selection order, this host first, as None.
///
/// Destinations connect first, so their profiles arrive while this host's are asked about;
/// each host scans once its own are answered. Destinations run the job on their own scans, so
/// this host only ever holds its own.
fn gather(run: Gather) -> Result<Vec<(Option<String>, Reply)>, ScanError> {
    let Gather {
        since,
        defaults,
        mut discovery,
        destinations,
        prices,
        job,
        home,
        progress,
        ask,
    } = run;
    let reading = Arc::new(Reading::default());
    // A destination's `vibes` sends no usage, so the line counts only this host's.
    let whose = (!destinations.is_empty() && matches!(job, Job::Report { vibes: true, .. }))
        .then_some("this host");
    let tally = matches!(job, Job::Report { vibes: false, .. });
    let shown = progress.map(|look| Arc::new(Shown::new(look, reading.clone(), whose, tally)));
    let scope = match job {
        Job::Report { .. } => Scope::Report { defaults },
        Job::Sessions => Scope::Sessions,
        Job::Audit { .. } => Scope::Audit,
    };
    let remote = !destinations.is_empty();
    // Each host's report is named once the run covers several.
    let labeled = |host: Option<&str>| match &job {
        Job::Report {
            max_cost_per_use_usd,
            format,
            walk,
            vibes,
        } => Job::Report {
            max_cost_per_use_usd: *max_cost_per_use_usd,
            format: Format {
                host: host.map(str::to_string),
                ..format.clone()
            },
            walk: *walk,
            vibes: *vibes,
        },
        Job::Sessions => Job::Sessions,
        Job::Audit { .. } => job.clone(),
    };
    let (sender, results) = mpsc::channel();
    {
        // Prices load beside the scans and questions, so whichever fails first stops the run.
        let sender = sender.clone();
        // The progress line's counts price what the local scan has read once prices load.
        let counting = progress.map(|_| reading.clone());
        thread::spawn(move || {
            let catalog = panic::catch_unwind(|| load_prices(&prices))
                .unwrap_or_else(|_| Err(ScanError::new("analysis failed; no report produced")));
            if let (Some(reading), Ok(catalog)) = (&counting, &catalog) {
                let _ = reading.catalog.set(catalog.clone());
            }
            let _ = sender.send(Arrival::Prices(catalog));
        });
    }
    let mut hosts: Vec<Option<String>> = vec![None];
    let mut requests = vec![];
    // Each destination's answers, sent once given; at once when nobody is asked.
    let mut payments = vec![];
    for destination in destinations {
        let sender = sender.clone();
        let index = hosts.len();
        let (request, receiver) = mpsc::channel();
        let (pay, payment) = mpsc::channel();
        requests.push((request, labeled(Some(&destination))));
        if ask.is_none() {
            let _ = pay.send(Paid::default());
        }
        payments.push(pay);
        hosts.push(Some(destination.clone()));
        let shown = shown.clone();
        thread::spawn(move || {
            let audit = || {
                let connected = ssh::connect(&destination, since, scope)?;
                let paid = match connected.profiles() {
                    Some(profiles) => {
                        let _ = sender.send(Arrival::Profiles(index, profiles.clone()));
                        // Dropped unanswered when this host fails first.
                        payment
                            .recv()
                            .map_err(|_| ScanError::new("cancelled; no report produced"))?
                    }
                    None => Paid::default(),
                };
                connected.audit(paid, receiver)
            };
            let result = panic::catch_unwind(AssertUnwindSafe(audit))
                .unwrap_or_else(|_| Err(ScanError::new("analysis failed; no report produced")));
            // Counted as it arrives, even while this host's report is still being rendered.
            if let (Some(shown), Ok(Reply::Report(printed) | Reply::Walk { printed, .. })) =
                (&shown, &result)
            {
                shown.settle(index, &printed.totals);
            }
            let _ = sender.send(Arrival::Host(index, result.map(Scanned::Remote)));
        });
    }
    let stop = |error: ScanError| {
        cancel_downloads();
        ssh::cancel();
        error
    };
    let mut asking = Asking {
        questions: BillingQuestions::default(),
        results: &results,
        profiles: hosts.iter().map(|_| None).collect(),
        early: vec![],
    };
    if let Some(ask) = &ask {
        enter(Phase::Asking);
        discovery.billing = asking.ask(&ask.roots, &ask.style).map_err(stop)?;
    }
    // A lone local scan runs on this thread, so the analysis reuses the memory the scan
    // frees rather than leaving it to another thread's allocator arena.
    let mut inline = None;
    {
        let options = ScanOptions {
            discovery,
            since,
            reach: scope.reach(),
            // Counting costs the scan time, so only a progress line has it count.
            reading: progress.map(|_| reading.clone()),
        };
        let job = move || {
            panic::catch_unwind(|| scan(&options))
                .unwrap_or_else(|_| Err(ScanError::new("analysis failed; no report produced")))
                .map(|scan| Scanned::Local(Box::new(scan)))
        };
        if remote {
            thread::spawn(move || {
                let _ = sender.send(Arrival::Host(0, job()));
            });
        } else {
            drop(sender);
            inline = Some((0, job));
        }
    }
    if let Some(ask) = &ask {
        let asked = (|| {
            for (index, pay) in (1..).zip(&payments) {
                // Shown while a destination still connects, or this host still reads.
                let waiting = (asking.profiles[index].is_none())
                    .then_some(shown.clone())
                    .flatten()
                    .map(|shown| {
                        let meter = Meter::start(shown);
                        meter.phase("Connecting to SSH hosts");
                        meter
                    });
                let profiles = asking.wait(index)?;
                drop(waiting);
                let style = Style {
                    home: profiles.home,
                    host: hosts[index].clone(),
                    ..ask.style.clone()
                };
                let profiles = asking.ask(&profiles.roots, &style)?;
                let _ = pay.send(Paid { profiles });
            }
            Ok(())
        })();
        asked.map_err(stop)?;
        enter(Phase::Running);
    }
    let Asking {
        questions, early, ..
    } = asking;
    if ask.is_some() {
        questions
            .finish(&mut io::stdout().lock())
            .map_err(|_| stop(terminal_failed()))?;
    }
    drop(payments);
    let meter = shown.map(Meter::start);
    let phase = |message: &'static str| {
        if let Some(meter) = &meter {
            meter.phase(message);
        }
    };
    phase(if remote {
        "Scanning local histories and SSH hosts"
    } else {
        "Scanning local histories"
    });
    let local_job = labeled(remote.then_some("this host"));
    let mut replies: Vec<Option<Reply>> = hosts.iter().map(|_| None).collect();
    let gathered = (|| {
        let inline = inline
            .map(|(index, job)| job().map(|scan| (index, scan)))
            .transpose()?;
        phase("Loading public prices");
        let mut catalog = None;
        // Hosts that finished before the prices arrived.
        let mut scanned: Vec<_> = inline.into_iter().collect();
        for arrival in early.into_iter().chain(results) {
            match arrival {
                // Asked about already, or nobody is asked.
                Arrival::Profiles(..) => {}
                Arrival::Host(index, result) => scanned.push((index, result?)),
                Arrival::Prices(loaded) => {
                    let loaded = loaded?;
                    for (request, job) in std::mem::take(&mut requests) {
                        // A destination that already failed reports itself through `results`.
                        let _ = request.send(Request {
                            catalog: loaded.clone(),
                            job,
                        });
                    }
                    phase(match job {
                        Job::Report { .. } => "Checking settings",
                        Job::Sessions => "Pricing sessions",
                        Job::Audit { .. } => "Auditing sessions",
                    });
                    catalog = Some(loaded);
                }
            }
            let Some(catalog) = &catalog else { continue };
            for (index, scanned) in scanned.drain(..) {
                replies[index] = Some(match scanned {
                    Scanned::Local(mut scan) => match &local_job {
                        Job::Report {
                            max_cost_per_use_usd,
                            format,
                            ..
                        } => {
                            // Prices that loaded after the scan last counted price its counts
                            // while the analysis works up to settling them.
                            if meter.is_some() && !reading.priced() {
                                let subagents = scan
                                    .subagents
                                    .iter()
                                    .map(|s| (s.session.clone(), s.clone()))
                                    .collect();
                                crate::tally::recount(&reading, &mut scan.requests, &subagents);
                            }
                            Reply::Here(Box::new(render(
                                *scan,
                                Render {
                                    catalog,
                                    since,
                                    now: crate::reader::now(),
                                    max_cost_per_use_usd: *max_cost_per_use_usd,
                                    format,
                                    home: home.as_deref(),
                                    reading: meter.as_ref().map(|_| &*reading),
                                },
                            )?))
                        }
                        Job::Sessions => Reply::Sessions(export(&scan, catalog)?),
                        Job::Audit {
                            filter,
                            limit,
                            since,
                        } => {
                            let options = crate::audit::Options {
                                filter,
                                since: *since,
                                limit: *limit,
                            };
                            Reply::Audit(Box::new(crate::audit::audit(&scan, catalog, &options)?))
                        }
                    },
                    Scanned::Remote(reply) => reply,
                });
                // The line ends on what the report's header or total counts; a
                // destination's totals counted when they arrived.
                if let (Some(meter), Some(Reply::Here(rendered))) = (&meter, &replies[index]) {
                    meter.shared.settle(index, &rendered.printed.totals);
                }
            }
        }
        Ok(())
    })();
    drop(meter);
    gathered.inspect_err(|_| {
        cancel_downloads();
        ssh::cancel();
    })?;
    Ok(hosts
        .into_iter()
        .zip(replies)
        .map(|(host, reply)| (host, reply.expect("every host sent a result")))
        .collect())
}

/// `token-max doctor` or `token-max vibes`: audit the selected hosts, then walk through the
/// command's changes when stdin and stdout are a terminal.
fn audit(args: &[String], command: Command) -> Result<(Output, u8), ScanError> {
    let advice = command == Command::Vibes;
    let invalid = || {
        ScanError::new(if advice {
            "invalid arguments; use token-max vibes --help"
        } else {
            "invalid arguments; use token-max doctor --help"
        })
    };
    let mut values = parse(args).ok_or_else(invalid)?;
    if audit_only(&values) {
        return Err(invalid());
    }
    // Advice has no verdict to tune, evidence cards to expand or settings to pretend away.
    if advice
        && (values.details
            || values.defaults
            || values.version
            || values.ssh_scan
            || values.max_cost_per_use.is_some())
    {
        return Err(invalid());
    }
    if values.help {
        let text = if advice { VIBES_HELP } else { DOCTOR_HELP };
        return Ok((Output::Text(help(text)), 0));
    }
    if values.version {
        return Ok((Output::Text(format!("{}\n", env!("CARGO_PKG_VERSION"))), 0));
    }
    let since = window(&values)?;
    let max_cost_per_use_usd = match values.max_cost_per_use.as_deref() {
        None => 1.0,
        Some(value) => max_cost_per_use(value)?,
    };
    if values.ssh_scan {
        if values.plain
            || values.details
            || values.max_cost_per_use.is_some()
            || chosen_here(&values)
        {
            return Err(invalid());
        }
        return Ok((
            Output::Written,
            ssh::serve(
                since,
                Scope::Report {
                    defaults: values.defaults,
                },
            ),
        ));
    }
    let (mut discovery, destinations) = hosts(&mut values)?;
    if values.defaults {
        discovery.pretend_defaults();
    }
    let stdout = io::stdout().is_terminal();
    let terminal = stdout && io::stdin().is_terminal();
    let term_dumb = env("TERM").as_deref() == Some("dumb");
    let unicode_locale = env_nonempty("LC_ALL")
        .or_else(|| env_nonempty("LC_CTYPE"))
        .or_else(|| env_nonempty("LANG"))
        .is_some_and(|locale| {
            let locale = locale.to_ascii_lowercase();
            locale.contains("utf8") || locale.contains("utf-8")
        });
    let color = stdout && !values.plain && env_nonempty("NO_COLOR").is_none() && !term_dumb;
    let unicode = stdout && !values.plain && !term_dumb && unicode_locale;
    // A report with the settings files pretended away has no changes to make.
    let walk = terminal && !values.defaults;
    // The walk is the inline TUI on a terminal big enough to draw one, unless `--plain` asks
    // for the line-by-line questions or `--details` for the report's full evidence, which
    // only the line-by-line walk prints.
    let rich_tui = walk && !values.plain && !values.details && !term_dumb && tui::fits();
    let format = Format {
        width: width(),
        color,
        unicode,
        details: values.details,
        host: None,
    };
    let home = std::env::home_dir().map(|dir| dir.to_string_lossy().into_owned());
    let style = format.style(home.clone());
    upgrade::tidy();
    // `doctor` alone asks for the latest release, unless offline, alongside discovery on a
    // terminal and alongside the whole scan otherwise.
    let mut check = (!values.offline && command == Command::Doctor).then(|| {
        thread::spawn(|| {
            let releases = upgrade::releases();
            let check = upgrade::check(&releases);
            (releases, check)
        })
    });
    // On a terminal, offer a newer release before anything else, then open with what the run
    // is about to read, then ask how each host's Claude profiles on the Anthropic API pay,
    // which picks each one's main TTL default and the advice that follows from it; without a
    // terminal the report names both defaults.
    let ask = if terminal {
        let found = discover(&discovery)?;
        // A run reporting as a default user would changes nothing, so it only says how.
        if let Some((releases, check)) = check.take().map(joined)
            && let Some(code) = offer_upgrade(&releases, check, !values.defaults, &style)?
        {
            return Ok((Output::Written, code));
        }
        let failed = |_| ScanError::new("cannot use the terminal; no report produced");
        let opening = opening(&Found::list(&found), &destinations, &style);
        io::stdout()
            .lock()
            .write_all(opening.as_bytes())
            .map_err(failed)?;
        Some(Ask {
            roots: found.billing_roots(),
            style: style.clone(),
        })
    } else {
        None
    };
    let replies = gather(Gather {
        since,
        defaults: values.defaults,
        discovery,
        destinations,
        prices: price_options(values.prices_file, values.price_cache, values.offline)?,
        job: Job::Report {
            max_cost_per_use_usd,
            format,
            walk,
            vibes: advice,
        },
        home: home.clone(),
        progress: (stdout && io::stderr().is_terminal() && !term_dumb)
            .then_some(Look { color, unicode }),
        ask,
    })?;
    // Without a terminal the check ran alongside the scan, and the news goes to stderr.
    if let Some((_, check)) = check.take().map(joined) {
        warn_upgrade(check);
    }
    let sections = replies.len();
    let mut code = 0;
    let mut hosts = vec![];
    // Each host's report, joined into one across hosts.
    let mut listings: Vec<(Option<String>, Listing)> = vec![];
    // Each host's advice off a terminal, for `vibes` to print.
    let mut cards: Vec<(Option<String>, Vec<Item>, Style)> = vec![];
    let mut sum: Option<Totals> = None;
    for (ssh, reply) in replies {
        let there = |theirs: &Option<String>| Style {
            home: theirs.clone(),
            host: ssh.clone(),
            ..style.clone()
        };
        let here = || {
            (
                (sections > 1).then(|| "this host".to_string()),
                style.clone(),
            )
        };
        let (printed, side, items) = match reply {
            Reply::Here(rendered) => {
                let Rendered {
                    printed,
                    fixes,
                    turned_off,
                } = *rendered;
                let monthly = printed.totals.monthly;
                let side = walk.then(|| {
                    let (fixes, turned_off) = if advice {
                        (vibes::fixes(&printed.vibes), vec![])
                    } else {
                        (fixes, turned_off)
                    };
                    Side::Here {
                        owner: Owner::new(fixes, turned_off, home.clone(), monthly),
                        monthly,
                        written: vec![],
                    }
                });
                let items = printed.vibes.clone();
                (Some(printed), side, items)
            }
            Reply::Report(printed) => {
                let items = printed.vibes.clone();
                (Some(printed), None, items)
            }
            Reply::Walk {
                printed,
                offers,
                remote,
            } => {
                let items = printed.vibes.clone();
                (Some(printed), Some(Side::There { offers, remote }), items)
            }
            Reply::Vibes(ssh::Advice {
                items,
                home: theirs,
                walk: None,
            }) => {
                cards.push((ssh.clone(), items, there(&theirs)));
                continue;
            }
            Reply::Vibes(ssh::Advice {
                items,
                walk: Some((offers, remote)),
                ..
            }) => (None, Some(Side::There { offers, remote }), items),
            Reply::Sessions(_) | Reply::Audit(_) => unreachable!("a report job gets reports"),
        };
        if let Some(printed) = printed {
            code = code.max(u8::from(printed.change));
            match &mut sum {
                Some(sum) => sum.merge(&printed.totals),
                None => sum = Some(*printed.totals.clone()),
            }
            listings.push((ssh.clone(), *printed.listing));
        }
        if let Some(side) = side {
            let (label, host_style) = match &side {
                Side::Here { .. } => here(),
                Side::There { offers, .. } => (ssh.clone(), there(&offers.home)),
            };
            hosts.push(WalkHost {
                label,
                side,
                style: host_style,
                items,
                wrote: false,
            });
            continue;
        }
        if advice {
            let (label, style) = here();
            cards.push((label, vibes::resolved(items), style));
            continue;
        }
        // Off a terminal, `doctor` names how much advice `vibes` has.
        cards.push((ssh.clone(), items, style.clone()));
    }
    // A card leaving its change to a section the walk asks only for several hosts' changes
    // together reads as the walk asks it.
    vibes::joined(
        &mut hosts
            .iter_mut()
            .map(|h| &mut h.items)
            .chain(cards.iter_mut().map(|(_, items, _)| items))
            .collect::<Vec<_>>(),
    );
    // One report across hosts, which a total of every host's figures opens.
    let options = ReportOptions {
        width: style.width,
        color,
        unicode,
        details: values.details,
        host: None,
    };
    // The settings each host already has right, for the TUI's scorecard; a destination's
    // names are its own text, so they are escaped like the rest of what it sends.
    let mut kept = vec![];
    let mut kept_features = 0usize;
    if rich_tui {
        for (host, listing) in &listings {
            let (rows, extra) = listing.kept();
            kept_features += extra;
            let text = |t: String| match host {
                Some(_) => crate::safe::terminal_text(&t),
                None => t,
            };
            kept.extend(rows.into_iter().map(|r| {
                tui::KeptItem {
                    host_label: host
                        .clone()
                        .or_else(|| (sections > 1).then(|| "this host".into())),
                    name: text(r.name),
                    value: text(r.value),
                    saving_usd: r.saving_usd,
                    default_value: r.default_value.map(text),
                    summary: text(r.summary),
                }
            }));
        }
        kept.sort_by(|a, b| match (a.saving_usd, b.saving_usd) {
            (Some(x), Some(y)) => y.total_cmp(&x),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        });
    }
    let listing = match (listings.len(), &sum) {
        (0, _) => None,
        (1, _) => listings.pop().map(|(_, listing)| listing),
        (_, Some(sum)) => Some(Listing::join(listings, sum, options)),
        (_, None) => unreachable!("every report adds to the sum"),
    };
    if walk {
        // A walk opens with the report's title, or several hosts' total, and what the settings
        // save against the defaults, so the credit comes before any question.
        let head = match (&sum, &listing) {
            (Some(sum), _) if sections > 1 && !advice => {
                let mut head = totals_section(sum, sections, options);
                head.push('\n');
                head
            }
            (Some(sum), Some(listing)) if !advice => {
                let credit = sum.credit("this host's").map_or_else(String::new, |c| {
                    let note = opening_note(&c, sum.partial);
                    let note = if note.is_empty() {
                        note
                    } else {
                        style.dim(&note)
                    };
                    style.line(&c) + &note
                });
                format!("{}{credit}", listing.title())
            }
            _ => String::new(),
        };
        let hero = rich_tui.then(|| {
            let mut h = Hero {
                host_count: hosts.len(),
                vibes_pending: hosts.iter().map(|h| vibes::pending(&h.items)).sum(),
                kept,
                kept_features,
                caveats: vec![
                    "~ marks figures extrapolated across unpriced requests; costs use public API rates from models.dev.".into(),
                    "Each change is priced independently; overlapping changes may save slightly less together.".into(),
                    "Run with --details or --plain for every untouched setting and full footnote ledger.".into(),
                ],
                ..Hero::default()
            };
            if let Some(sum) = &sum {
                h.requests = sum.requests;
                h.tokens = sum.tokens;
                h.sessions = sum.sessions;
                h.monthly_spend = sum.monthly;
                h.spend_text = crate::report::spend_text(
                    sum.spend,
                    sum.margin,
                    sum.floor,
                    sum.partial,
                );
                h.credit = sum
                    .credit
                    .as_ref()
                    .and_then(|c| c.short_text(sum.monthly));
            }
            h
        });
        let walk = Walk {
            hosts,
            listing: listing.filter(|_| !advice).map(Box::new),
            head,
            style,
            command,
            details: values.details,
            rich_tui,
            hero,
        };
        return Ok((Output::Walk(Box::new(walk)), 0));
    }
    if advice {
        let hosts: Vec<vibes::Host> = cards
            .iter()
            .map(|(label, items, style)| vibes::Host {
                label: label.as_deref(),
                items,
                offers: None,
                style,
            })
            .collect();
        return Ok((Output::Text(vibes::text(&hosts, &style)), 0));
    }
    let text = listing.map_or_else(String::new, |l| l.text());
    // Off a terminal, the advice this report leaves out is one command away. Under
    // `--defaults` the count is of settings pretended away, which `vibes` reads as they are.
    let pending: usize = cards
        .iter()
        .map(|(_, items, _)| vibes::pending(items))
        .sum();
    let hint = if pending > 0 && !values.defaults {
        let hint = format!(
            "{} Run token-max vibes with the same profile, host, --since and price options to see {}.",
            vibes::hint(pending),
            if pending == 1 { "it" } else { "them" }
        );
        format!("\n{}", style.line(&hint))
    } else {
        String::new()
    };
    Ok((Output::Text(format!("{text}{hint}")), code))
}

/// The release check's result once its thread finishes, with the releases it asked.
fn joined(handle: thread::JoinHandle<(String, Check)>) -> (String, Check) {
    handle.join().unwrap_or_else(|_| {
        let failed = Check::Failed("the check failed".into());
        (upgrade::releases(), failed)
    })
}

/// Say on a terminal what the release check found, offering to upgrade and rerun when `offer`
/// and a newer release can replace this binary. Returns the rerun's exit code once it has run
/// in this process's place, and fails when it cannot start; otherwise this run goes on with
/// this build.
fn offer_upgrade(
    releases: &str,
    check: Check,
    offer: bool,
    style: &Style,
) -> Result<Option<u8>, ScanError> {
    let failed = |_| ScanError::new("cannot use the terminal; no report produced");
    let mut stdout = io::stdout().lock();
    // A leading line break is a blank line before the text, which styling would escape.
    let say = |stdout: &mut io::StdoutLock, text: &str, note: bool| {
        let (gap, text) = text
            .strip_prefix('\n')
            .map_or(("", text), |text| ("\n", text));
        let line = format!("{gap}{}", upgrade_line(text, note, style));
        stdout
            .write_all(line.as_bytes())
            .and_then(|()| stdout.flush())
            .map_err(failed)
    };
    let latest = match check {
        Check::Current => return Ok(None),
        // A failed check is a diagnostic, so it goes to stderr as it does without a terminal.
        failed @ Check::Failed(_) => {
            warn_upgrade(failed);
            return Ok(None);
        }
        Check::Newer(latest) => latest,
    };
    let (exe, target) = match upgrade::install() {
        Install::Replace { .. } if !offer => {
            let how = "Run token-max doctor without --defaults to upgrade.";
            let text = format!("\ntoken-max {} {how}", stale(&latest));
            say(&mut stdout, &text, false)?;
            return Ok(None);
        }
        // Only a run about to ask probes the directory, since probing writes beside the binary.
        Install::Replace { exe, target } => match upgrade::writable(&exe) {
            Ok(()) => (exe, target),
            Err(why) => {
                let text = format!("\ntoken-max {} {}", stale(&latest), reinstall(&why));
                say(&mut stdout, &text, false)?;
                return Ok(None);
            }
        },
        Install::Manager(advice) => {
            let text = format!("\ntoken-max {} {advice}", stale(&latest));
            say(&mut stdout, &text, false)?;
            return Ok(None);
        }
        Install::Reinstall(why) => {
            let text = format!("\ntoken-max {} {}", stale(&latest), reinstall(&why));
            say(&mut stdout, &text, false)?;
            return Ok(None);
        }
    };
    enter(Phase::Asking);
    let yes = ask_upgrade(&latest, style, &mut unbuffered_stdin()?, &mut stdout).map_err(failed)?;
    enter(Phase::Running);
    if !yes {
        return Ok(None);
    }
    say(&mut stdout, &format!("Downloading {latest}..."), true)?;
    let installed = upgrade::stage(releases, &latest, &exe, target).and_then(|staged| {
        // Windows' swap is two renames, which a cancellation must not split.
        enter(Phase::Writing);
        let installed = staged.install();
        enter(Phase::Running);
        installed
    });
    if let Err(why) = installed {
        let version = upgrade::VERSION;
        let text = format!("Could not upgrade: {why}. Continuing with v{version}.");
        say(&mut stdout, &text, false)?;
        return Ok(None);
    }
    // A Ctrl-C during the swap waited for it to finish, so the upgrade stands but nothing reruns.
    if ssh::cancelled() {
        return Err(ScanError::new(format!(
            "cancelled after upgrading to {latest}; no report produced"
        )));
    }
    say(
        &mut stdout,
        &format!("Upgraded to {latest}; rerunning."),
        false,
    )?;
    // This build's binary is gone, and an SSH audit copies the binary at its path, so a rerun
    // that cannot start ends the run rather than go on with this build.
    rerun(&exe).map(Some).map_err(|error| {
        ScanError::new(format!(
            "could not rerun {latest}: {error}; it is installed, so run token-max doctor again"
        ))
    })
}

/// Stdin read a byte at a time, so the answer takes no more than its own line: the rerun
/// reads the same input, and anything typed ahead is its.
fn unbuffered_stdin() -> Result<io::BufReader<std::fs::File>, ScanError> {
    let failed = |_| ScanError::new("cannot use the terminal; no report produced");
    #[cfg(unix)]
    let owned = std::os::fd::AsFd::as_fd(&io::stdin())
        .try_clone_to_owned()
        .map_err(failed)?;
    #[cfg(windows)]
    let owned = std::os::windows::io::AsHandle::as_handle(&io::stdin())
        .try_clone_to_owned()
        .map_err(failed)?;
    Ok(io::BufReader::with_capacity(1, std::fs::File::from(owned)))
}

/// Run the upgraded binary at `exe` with this run's arguments, in this process's place.
#[cfg(unix)]
fn rerun(exe: &std::path::Path) -> io::Result<u8> {
    use std::os::unix::process::CommandExt;
    Err(std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .exec())
}

/// Run the upgraded binary at `exe` with this run's arguments, and wait for its exit code:
/// Windows cannot replace a running process's program.
#[cfg(windows)]
fn rerun(exe: &std::path::Path) -> io::Result<u8> {
    // The rerun owns the console and cancels itself on Ctrl-C, so this process only waits.
    enter(Phase::Writing);
    let status = std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .status();
    enter(Phase::Running);
    Ok(status?
        .code()
        .map_or(2, |code| u8::try_from(code).unwrap_or(2)))
}

/// How to upgrade a binary this run cannot replace, for the reason `why`, and no package
/// manager owns.
fn reinstall(why: &str) -> String {
    let mut why = why.to_string();
    if let Some(first) = why.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    format!(
        "{why}, so install it again as {} describes.",
        upgrade::INSTALL
    )
}

/// Say on stderr what the release check found when there is no terminal to offer an upgrade on.
fn warn_upgrade(check: Check) {
    let line = match check {
        Check::Current => return,
        Check::Failed(why) => format!("could not check for a newer release: {why}"),
        Check::Newer(latest) => {
            let how = match upgrade::install() {
                Install::Replace { .. } => "Run token-max doctor on a terminal to upgrade.".into(),
                Install::Manager(advice) => advice.into(),
                Install::Reinstall(why) => reinstall(&why),
            };
            format!("{} {how}", stale(&latest))
        }
    };
    eprintln!("token-max: {}", crate::safe::terminal_text(&line));
}

/// Walk through every host's changes, print each host's report with the accepted ones first,
/// then write them once the person confirms. After `doctor`'s walk, offer `vibes`' advice
/// from the same scan and walk through it the same way.
fn walk(walk: Walk) -> Result<(), ScanError> {
    if walk.rich_tui {
        return run_tui(walk);
    }
    let Walk {
        mut hosts,
        listing,
        head,
        style,
        command,
        details,
        ..
    } = walk;
    let mut stdout = io::stdout().lock();
    let mut stdin = io::stdin().lock();
    let walked = (|| {
        if command == Command::Doctor {
            let failed = |_| ScanError::new("cannot use the terminal; no settings were changed");
            write!(stdout, "{head}").map_err(failed)?;
            let walking: Vec<Walking> = hosts
                .iter()
                .map(|h| Walking {
                    label: h.label.as_deref(),
                    offers: h.offers(),
                    style: &h.style,
                })
                .collect();
            let selections = ask(&walking, &style, &mut stdin, &mut stdout).map_err(failed)?;
            drop(walking);
            let settling = Settle {
                listing: listing.as_deref(),
                style: &style,
            };
            settle(&mut hosts, &selections, &settling, &mut stdin, &mut stdout)?;
            // Every edit not written, declined at its card or at the confirm, so it can still
            // be made by hand.
            let options = ReportOptions {
                width: style.width,
                color: style.color,
                unicode: style.unicode,
                details,
                host: None,
            };
            let unwritten: String = hosts
                .iter()
                .zip(&selections)
                .map(|(h, s)| {
                    let left = s.unwritten(h.offers(), h.wrote, &h.style);
                    crate::report::closing(&left, "NOT WRITTEN", options)
                })
                .collect();
            write!(stdout, "{unwritten}").map_err(|_| unusable(hosts.iter().any(|h| h.wrote)))?;
            let wrote = hosts.iter().any(|h| h.wrote);
            let pending = vibes_after(&mut hosts)?;
            if pending == 0 {
                return Ok(());
            }
            let failed = |_| unusable(wrote);
            if !vibes::offer(pending, &style, &mut stdin, &mut stdout).map_err(failed)? {
                return Ok(());
            }
            writeln!(stdout).map_err(failed)?;
            open_vibes(&mut hosts)?;
        }
        let wrote = hosts.iter().any(|h| h.wrote);
        let asking: Vec<vibes::Host> = hosts
            .iter()
            .map(|h| vibes::Host {
                label: h.label.as_deref(),
                items: &h.items,
                offers: Some(h.offers()),
                style: &h.style,
            })
            .collect();
        let selections =
            vibes::ask(&asking, &style, &mut stdin, &mut stdout).map_err(|_| unusable(wrote))?;
        drop(asking);
        // Cards with no change to make leave nothing to confirm or to report written.
        let changes = hosts
            .iter()
            .zip(&selections)
            .any(|(h, s)| s.summary(h.offers()).answered());
        if changes {
            let settling = Settle {
                listing: None,
                style: &style,
            };
            settle(&mut hosts, &selections, &settling, &mut stdin, &mut stdout)?;
        }
        Ok(())
    })();
    // A destination that wrote is told the audit is over, and must end it cleanly; the
    // rest are dropped, which stops them.
    let mut ended = Ok(());
    for host in hosts {
        if let (Side::There { remote, .. }, true) = (host.side, host.wrote)
            && let Err(error) = remote.end()
            && ended.is_ok()
        {
            ended = Err(error);
        }
    }
    walked.and(ended)
}

/// How many pieces of advice `vibes` has once `doctor`'s walk ends, none when the walk should
/// not offer them: a host that wrote reads its advice again, since what it wrote can change it.
fn vibes_after(hosts: &mut [WalkHost]) -> Result<usize, ScanError> {
    let wrote = hosts.iter().any(|h| h.wrote);
    if !wrote && hosts.iter().all(|h| vibes::pending(&h.items) == 0) {
        return Ok(0);
    }
    enter(if wrote {
        Phase::AskingMore
    } else {
        Phase::Asking
    });
    // A Ctrl-C while the changes were written ends the run after them. Read once the phase
    // has moved on, since one that lands after that exits in the handler.
    if ssh::cancelled() {
        return Ok(0);
    }
    for host in hosts.iter_mut().filter(|h| h.wrote) {
        host.advise()?;
    }
    join(hosts);
    Ok(hosts.iter().map(|h| vibes::pending(&h.items)).sum())
}

/// Go on from `doctor`'s walk to `vibes`' on every host that did not read its advice again.
fn open_vibes(hosts: &mut [WalkHost]) -> Result<(), ScanError> {
    for host in hosts.iter_mut().filter(|h| !h.wrote) {
        host.advise()?;
    }
    join(hosts);
    Ok(())
}

/// Interactive inline TUI runner backed by local and SSH `WalkHost`s.
fn run_tui(walk: Walk) -> Result<(), ScanError> {
    struct WalkBridge<'a> {
        hosts: &'a mut [WalkHost],
        style: &'a Style,
        start_vibes: bool,
        /// Independent vibes owners for local hosts so peeking at Vibes never overwrites
        /// the Measured `Owner`.
        vibes_owners: Vec<Option<Owner>>,
        /// The cards those owners were built from, as they read before anything is written;
        /// `host.items` keeps the variants that follow a measured write.
        vibes_items: Vec<Option<Vec<Item>>>,
        vibes_loaded: bool,
        measured_plans: Vec<Option<crate::doctor::Plan>>,
        measured_work: Vec<bool>,
        vibes_plans: Vec<Option<crate::doctor::Plan>>,
        vibes_work: Vec<bool>,
    }

    impl WalkBridge<'_> {
        fn preview_measured_events(
            &mut self,
            events: &[Vec<crate::doctor::Event>],
        ) -> Result<DiffPreview, ScanError> {
            let mut text = String::new();
            let mut plans = vec![];
            let mut has_work = vec![];
            let mut file_count = 0usize;
            let mut change_count = 0usize;
            for (host, host_events) in self.hosts.iter_mut().zip(events) {
                let any_yes = host_events
                    .iter()
                    .any(|e| !matches!(e, crate::doctor::Event::Choose { yes: false, .. }));
                if !any_yes {
                    plans.push(None);
                    has_work.push(false);
                    continue;
                }
                change_count += host_events
                    .iter()
                    .filter(|e| {
                        matches!(
                            e,
                            crate::doctor::Event::Choose { yes: true, .. }
                                | crate::doctor::Event::Cleanup { .. }
                        )
                    })
                    .count();
                let shown = match &mut host.side {
                    Side::Here { owner, .. } => {
                        let plan = owner.plan(host_events)?;
                        let shown = preview(&plan, &host.style);
                        if !shown.is_empty() {
                            file_count += 1;
                        }
                        plans.push(Some(plan));
                        shown
                    }
                    Side::There { remote, .. } => {
                        let shown = remote.review(host_events)?;
                        let safe = crate::safe::remote_text(&shown, self.style.color);
                        if !safe.is_empty() {
                            file_count += 1;
                        }
                        plans.push(None);
                        safe
                    }
                };
                if !shown.is_empty() {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(shown.trim_start_matches('\n'));
                }
                has_work.push(true);
            }
            self.measured_plans = plans;
            self.measured_work = has_work;
            Ok(DiffPreview {
                text,
                file_count: file_count.max(1),
                change_count,
                backups: vec![],
            })
        }

        fn preview_vibes_events(
            &mut self,
            events: &[Vec<crate::doctor::Event>],
        ) -> Result<DiffPreview, ScanError> {
            let mut text = String::new();
            let mut plans = vec![];
            let mut has_work = vec![];
            let mut file_count = 0usize;
            let mut change_count = 0usize;
            for (idx, (host, host_events)) in self.hosts.iter_mut().zip(events).enumerate() {
                let any_yes = host_events
                    .iter()
                    .any(|e| !matches!(e, crate::doctor::Event::Choose { yes: false, .. }));
                if !any_yes {
                    plans.push(None);
                    has_work.push(false);
                    continue;
                }
                change_count += host_events
                    .iter()
                    .filter(|e| matches!(e, crate::doctor::Event::Choose { yes: true, .. }))
                    .count();
                let shown = if let Some(owner) = self.vibes_owners.get(idx).and_then(Option::as_ref)
                {
                    let plan = owner.plan(host_events)?;
                    let shown = preview(&plan, &host.style);
                    if !shown.is_empty() {
                        file_count += 1;
                    }
                    plans.push(Some(plan));
                    shown
                } else {
                    match &mut host.side {
                        Side::Here { owner, .. } => {
                            let plan = owner.plan(host_events)?;
                            let shown = preview(&plan, &host.style);
                            if !shown.is_empty() {
                                file_count += 1;
                            }
                            plans.push(Some(plan));
                            shown
                        }
                        Side::There { remote, .. } => {
                            let shown = remote.review(host_events)?;
                            let safe = crate::safe::remote_text(&shown, self.style.color);
                            if !safe.is_empty() {
                                file_count += 1;
                            }
                            plans.push(None);
                            safe
                        }
                    }
                };
                if !shown.is_empty() {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(shown.trim_start_matches('\n'));
                }
                has_work.push(true);
            }
            self.vibes_plans = plans;
            self.vibes_work = has_work;
            Ok(DiffPreview {
                text,
                file_count: file_count.max(1),
                change_count,
                backups: vec![],
            })
        }

        fn write_planned(
            &mut self,
            plans: Vec<Option<crate::doctor::Plan>>,
            has_work: Vec<bool>,
        ) -> WriteResult {
            enter(Phase::Writing);
            let mut lines = vec![];
            let mut files_written = 0usize;
            let mut error = None;
            let mut skipped = vec![];
            for ((host, plan), active) in self.hosts.iter_mut().zip(plans).zip(has_work) {
                if !active {
                    continue;
                }
                if error.is_none() && ssh::cancelled() {
                    error = Some(ScanError::new("cancelled while writing"));
                }
                if error.is_some() {
                    skipped.push(host.label.clone().unwrap_or_else(|| "this host".into()));
                    continue;
                }
                let result = match (&mut host.side, plan) {
                    (Side::Here { written, .. }, Some(plan)) => write(&plan).inspect(|_| {
                        written.extend(plan.accepted());
                    }),
                    (Side::There { remote, .. }, None) => remote.write(),
                    _ => unreachable!("a plan for each local host"),
                };
                match result {
                    Ok(files) => {
                        host.wrote = true;
                        files_written += files.len();
                        let formatted = wrote(&files, &host.style);
                        for line in formatted.lines() {
                            let trimmed = line.trim();
                            if !trimmed.is_empty() {
                                lines.push(trimmed.to_string());
                            }
                        }
                    }
                    Err(failed) => {
                        error = Some(failed);
                    }
                }
            }
            // The hosts written before a failure stay written, so their receipts go with it.
            let failed = error.map(|mut err| {
                if !skipped.is_empty() {
                    err.message += &format!("; nothing was written on {}", skipped.join(", "));
                }
                err
            });
            WriteResult {
                files_written,
                lines,
                failed,
            }
        }
    }

    impl Bridge for WalkBridge<'_> {
        fn preview_measured(
            &mut self,
            events: &[Vec<crate::doctor::Event>],
        ) -> Result<DiffPreview, ScanError> {
            self.preview_measured_events(events)
        }

        fn write_measured(&mut self) -> WriteResult {
            let plans = std::mem::take(&mut self.measured_plans);
            let work = std::mem::take(&mut self.measured_work);
            self.write_planned(plans, work)
        }

        fn load_vibes(&mut self) -> Result<Vec<VibeRow>, ScanError> {
            if !self.vibes_loaded {
                self.vibes_owners = vec![];
                self.vibes_items = vec![];
                for host in self.hosts.iter_mut() {
                    if self.start_vibes {
                        // Already in `vibes` command: `host.side` holds the vibes owner directly.
                        self.vibes_owners.push(None);
                        self.vibes_items.push(None);
                        continue;
                    }
                    match &mut host.side {
                        Side::Here {
                            monthly, written, ..
                        } => {
                            let items = vibes::settled(host.items.clone(), written);
                            let owner = vibes::owner(&items, host.style.home.clone(), *monthly);
                            self.vibes_owners.push(Some(owner));
                            self.vibes_items.push(Some(items));
                        }
                        // Asking a destination for `vibes` swaps its walk to `vibes`' offers,
                        // which would leave Measured's answers naming the wrong changes there,
                        // so its cards show read-only until `token-max vibes` asks for them.
                        Side::There { .. } => {
                            self.vibes_owners.push(None);
                            self.vibes_items.push(None);
                        }
                    }
                }
                self.vibes_loaded = true;
            }
            let mut rows = vec![];
            for (host_idx, host) in self.hosts.iter().enumerate() {
                let owner = self.vibes_owners.get(host_idx).and_then(Option::as_ref);
                let offers = match (&host.side, owner) {
                    (_, Some(owner)) => Some(owner.offers()),
                    (Side::There { .. }, None) if !self.start_vibes => None,
                    _ => Some(host.offers()),
                };
                let items = self.vibes_items.get(host_idx).and_then(Option::as_ref);
                for (item_idx, item) in items.unwrap_or(&host.items).iter().enumerate() {
                    // Each version as `vibes`' walk would ask it, with the change it offers.
                    let version = |item: &Item, origin: Origin, after: Vec<Origin>| {
                        let change = offers
                            .and_then(|o| o.change(origin))
                            .filter(|_| !item.quiet);
                        let (offer, accepts) = match change {
                            Some(crate::doctor::Change::Offer(offer, fix)) => {
                                (Some(offer), fix.origins.clone())
                            }
                            _ => (None, vec![]),
                        };
                        tui::VibeVersion {
                            after,
                            item: item.clone(),
                            offer,
                            accepts,
                        }
                    };
                    let mut versions = vec![version(item, Origin::Vibe(item_idx), vec![])];
                    for (k, after) in item.after.iter().enumerate().filter(|(_, a)| a.walked()) {
                        versions.push(version(
                            &after.item,
                            Origin::Walked(item_idx, k),
                            after.origins.clone(),
                        ));
                    }
                    rows.push(VibeRow {
                        host: host_idx,
                        host_label: host.label.clone(),
                        versions,
                        shown: 0,
                        checked: false,
                    });
                }
            }
            Ok(rows)
        }

        fn preview_vibes(&mut self, rows: &[VibeRow]) -> Result<DiffPreview, ScanError> {
            let events = tui::vibes_events_from(rows, self.hosts.len());
            self.preview_vibes_events(&events)
        }

        fn write_vibes(&mut self) -> WriteResult {
            let plans = std::mem::take(&mut self.vibes_plans);
            let work = std::mem::take(&mut self.vibes_work);
            self.write_planned(plans, work)
        }
    }

    let Walk {
        mut hosts,
        style,
        command,
        hero,
        ..
    } = walk;
    // `doctor`'s checklist, then, as its walk does, an offer of `vibes`' from the same scan.
    let mut phase = command;
    let mut hero = hero.unwrap_or_default();
    let outcome = loop {
        let start_tab = match phase {
            Command::Doctor => Tab::Measured,
            Command::Vibes => Tab::Vibes,
        };
        let app = {
            let walking: Vec<Walking> = hosts
                .iter()
                .map(|h| Walking {
                    label: h.label.as_deref(),
                    offers: h.offers(),
                    style: &h.style,
                })
                .collect();
            let mut app = tui::App::new(
                &walking,
                std::mem::take(&mut hero),
                start_tab,
                style.color,
                style.unicode,
            );
            app.wrote = hosts.iter().any(|h| h.wrote);
            app
        };
        let mut bridge = WalkBridge {
            hosts: &mut hosts,
            style: &style,
            start_vibes: phase == Command::Vibes,
            vibes_owners: vec![],
            vibes_items: vec![],
            vibes_loaded: false,
            measured_plans: vec![],
            measured_work: vec![],
            vibes_plans: vec![],
            vibes_work: vec![],
        };
        let outcome = tui::run(app, &style, &mut bridge);
        if phase == Command::Vibes || !matches!(outcome, Ok(ExitKind::Done)) {
            break outcome;
        }
        let wrote = hosts.iter().any(|h| h.wrote);
        let more = vibes_after(&mut hosts).and_then(|pending| {
            if pending == 0 {
                return Ok(None);
            }
            match tui::offer_vibes(pending, &style).map_err(|_| unusable(wrote))? {
                Some(true) => {}
                Some(false) => return Ok(None),
                None => return Err(cancelled(wrote)),
            }
            open_vibes(&mut hosts)?;
            Ok(Some(pending))
        });
        match more {
            Ok(Some(pending)) => {
                phase = Command::Vibes;
                hero = Hero {
                    host_count: hosts.len(),
                    vibes_pending: pending,
                    ..Hero::default()
                };
            }
            Ok(None) => break Ok(ExitKind::Done),
            Err(error) => break Err(error),
        }
    };

    let wrote = hosts.iter().any(|h| h.wrote);
    let mut ended = Ok(());
    for host in hosts {
        if let (Side::There { remote, .. }, true) = (host.side, host.wrote)
            && let Err(error) = remote.end()
            && ended.is_ok()
        {
            ended = Err(error);
        }
    }
    match outcome {
        Ok(ExitKind::Done) => ended,
        Ok(ExitKind::Cancelled) => Err(cancelled(wrote)),
        Err(error) => Err(error),
    }
}

/// A walk cancelled after `wrote` says whether settings were written.
fn cancelled(wrote: bool) -> ScanError {
    ScanError::new(if wrote {
        "cancelled; no further settings were changed"
    } else {
        "cancelled; no settings were changed"
    })
}

/// The terminal failed under a walk, after `wrote` says whether settings were written.
fn unusable(wrote: bool) -> ScanError {
    ScanError::new(if wrote {
        "cannot use the terminal; no further settings were changed"
    } else {
        "cannot use the terminal; no settings were changed"
    })
}

/// What a walk's answers are settled with: the report to print again with them, for
/// `doctor`'s walk, and how the walk prints.
struct Settle<'a> {
    listing: Option<&'a Listing>,
    style: &'a Style,
}

/// Print the report with the walk's `selections` marked on it, show what the accepted changes
/// would write, and write them once the person confirms: this host's first, then each
/// destination's there, stopping at the first host that fails.
fn settle(
    hosts: &mut [WalkHost],
    selections: &[Selection],
    settling: &Settle<'_>,
    stdin: &mut impl io::BufRead,
    stdout: &mut impl Write,
) -> Result<(), ScanError> {
    let style = settling.style;
    let before = hosts.iter().any(|h| h.wrote);
    let failed = |_| unusable(before);
    let summaries: Vec<_> = hosts
        .iter()
        .zip(selections)
        .map(|(h, s)| s.summary(h.offers()))
        .collect();
    // Each host checks the answers against its own files and makes its own plan from them.
    let mut plans = vec![];
    let mut previews = String::new();
    let mut choices: Vec<crate::report::Chosen> = vec![];
    for (host, selection) in hosts.iter_mut().zip(selections) {
        let preview = match &mut host.side {
            Side::Here { owner, .. } => {
                let plan = owner.plan(selection.events())?;
                choices.extend(plan.choices().iter().map(|(o, c)| (None, *o, c.clone())));
                let shown = preview(&plan, &host.style);
                plans.push(Some(plan));
                shown
            }
            Side::There { remote, offers } => {
                let shown = remote.review(selection.events())?;
                let label = host.label.clone();
                let theirs = selection.choices(offers);
                choices.extend(theirs.into_iter().map(|(o, c)| (label.clone(), o, c)));
                plans.push(None);
                crate::safe::remote_text(&shown, style.color)
            }
        };
        previews += &preview;
    }
    // The report reads as its own section after the questions; `vibes` has none.
    if let Some(listing) = settling.listing {
        let answered: Vec<(&Selection, &Offers)> = selections
            .iter()
            .zip(hosts.iter().map(|h| h.offers()))
            .collect();
        let total = crate::doctor::accepted(&answered, "Accepted");
        let gap = if summaries.iter().any(|s| s.answered()) {
            "\n"
        } else {
            ""
        };
        let report = listing.review(&choices, &total);
        write!(stdout, "{gap}{report}").map_err(failed)?;
    }
    stdout.flush().map_err(failed)?;
    let styles: Vec<Style> = hosts.iter().map(|h| h.style.clone()).collect();
    let request = Confirm {
        preview: &previews,
        hosts: summaries.iter().zip(&styles).collect(),
    };
    let yes = confirm(&request, style, stdin, stdout).map_err(failed)?;
    enter(Phase::Writing);
    let mut written = vec![];
    let mut error = None;
    // Hosts left unwritten after one fails.
    let mut skipped = vec![];
    for ((host, plan), summary) in hosts.iter_mut().zip(plans).zip(&summaries) {
        if !yes || !summary.accepted() {
            written.push(vec![]);
            continue;
        }
        // Ctrl-C while writing lets the write under way finish and starts no other.
        if error.is_none() && ssh::cancelled() {
            error = Some(ScanError::new("cancelled while writing"));
        }
        if error.is_some() {
            skipped.push(host.label.clone().unwrap_or_else(|| "this host".into()));
            written.push(vec![]);
            continue;
        }
        let result = match (&mut host.side, plan) {
            (Side::Here { written, .. }, Some(plan)) => write(&plan).inspect(|_| {
                written.extend(plan.accepted());
            }),
            (Side::There { remote, .. }, None) => remote.write(),
            _ => unreachable!("a plan for each host here"),
        };
        match result {
            Ok(files) => {
                host.wrote = true;
                written.push(files);
            }
            Err(failed) => {
                written.push(vec![]);
                error = Some(failed);
            }
        }
    }
    let done: Vec<_> = summaries
        .iter()
        .zip(&written)
        .zip(&request.hosts)
        .map(|((summary, written), (_, style))| (summary, written.as_slice(), *style))
        .collect();
    // After a failure, only what was written for certain: the failed host may have written
    // some of its files, which its error names, so nothing may read as nothing written.
    let closing = if error.is_some() {
        let lines: String = done.iter().map(|(_, w, style)| wrote(w, style)).collect();
        if lines.is_empty() {
            lines
        } else {
            format!("\n{lines}")
        }
    } else {
        finish(&done, style)
    };
    // The settings are written, so a failure to say so must not read as nothing changed.
    let _ = stdout.write_all(closing.as_bytes());
    let _ = stdout.flush();
    match error {
        Some(mut error) if !skipped.is_empty() => {
            error.message += &format!("; nothing was written on {}", skipped.join(", "));
            Err(error)
        }
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Run the command line and return its exit code: 0 no change, or done on a terminal; 1 change
/// recommended; 2 failure.
pub fn main() -> ExitCode {
    // Panic payloads could quote transcript data; failures print only a safe message.
    panic::set_hook(Box::new(|_| {}));
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    // The session export and the audit keep their diagnostics machine-readable too.
    let json = args
        .first()
        .is_some_and(|a| a == "sessions" || a == "audit");
    let handler = ctrlc::set_handler(move || {
        let phase = PHASE.lock().unwrap_or_else(PoisonError::into_inner);
        let message = match *phase {
            // This host's files and the output are written whole. A destination's write
            // finishes or not without this host, which stops waiting for it and starts no
            // other host's write.
            Phase::Writing => {
                ssh::cancel();
                return;
            }
            Phase::Asking => "cancelled; no settings were changed",
            Phase::AskingMore => "cancelled; no further settings were changed",
            Phase::Running => "cancelled; no report produced",
        };
        // The analysis may be blocked in I/O, so exit now, holding the lock so nothing else
        // is written, after deleting any partial price download it would have cleaned up.
        // Always leave the terminal out of raw mode with cursor visible if TUI was active.
        tui::restore_if_active();
        cancel_downloads();
        ssh::cancel();
        diagnose(json, &ScanError::new(message));
        std::process::exit(2);
    });
    let result = match handler {
        Ok(()) => panic::catch_unwind(AssertUnwindSafe(|| run(&args))),
        Err(_) => Ok(Err(ScanError::new("analysis failed; no report produced"))),
    };
    let result =
        result.unwrap_or_else(|_| Err(ScanError::new("analysis failed; no report produced")));
    // The walk's report leads into its prompts, so Ctrl-C while it prints cancels the walk;
    // anything else is written whole.
    enter(match result {
        Ok((Output::Walk(_), _)) => Phase::Asking,
        _ => Phase::Writing,
    });
    let written = match result {
        Ok((Output::Text(text), code)) => io::stdout()
            .lock()
            .write_all(text.as_bytes())
            .and_then(|()| io::stdout().lock().flush())
            .map(|()| code),
        Ok((Output::Usage(text), code)) => {
            let _ = io::stderr().lock().write_all(text.as_bytes());
            Ok(code)
        }
        Ok((Output::Written, code)) => Ok(code),
        Ok((Output::Walk(w), code)) => {
            // The walk runs on this thread, outside the panic guard around the analysis.
            match panic::catch_unwind(AssertUnwindSafe(|| walk(*w))) {
                Ok(Ok(())) => Ok(code),
                Ok(Err(error)) => {
                    diagnose(json, &error);
                    return ExitCode::from(2);
                }
                Err(_) => {
                    diagnose(
                        json,
                        &ScanError::new("token-max failed while applying changes"),
                    );
                    return ExitCode::from(2);
                }
            }
        }
        Err(error) => {
            diagnose(json, &error);
            return ExitCode::from(2);
        }
    };
    match written {
        Ok(code) => ExitCode::from(code),
        Err(_) => {
            diagnose(json, &ScanError::new("analysis failed; no report produced"));
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_host_stops_the_questions_before_the_next_one() {
        let (sender, results) = mpsc::channel();
        let profiles = ssh::Profiles {
            home: None,
            roots: vec![],
        };
        sender.send(Arrival::Profiles(2, profiles)).unwrap();
        sender
            .send(Arrival::Host(1, Err(ScanError::new("devbox: gone"))))
            .unwrap();
        let mut asking = Asking {
            questions: BillingQuestions::default(),
            results: &results,
            profiles: vec![None, None, None],
            early: vec![],
        };
        let style = Style {
            color: false,
            unicode: false,
            width: None,
            home: None,
            host: None,
        };
        // Failing before it prompts, so this test never reads its own stdin.
        let error = asking
            .ask(&["/home/me/.claude".into()], &style)
            .unwrap_err();
        assert_eq!(error.message, "devbox: gone");
        // What arrived before the failure is kept.
        assert!(asking.profiles[2].is_some());
    }

    #[test]
    fn the_reading_bar_fills_by_eighths_fits_its_row_and_estimates_only_after_a_second() {
        let unicode = Look {
            color: false,
            unicode: true,
        };
        let plain = Look {
            color: false,
            unicode: false,
        };
        let second = Duration::from_secs(1);
        let at = |read, total, elapsed, look, room| {
            let progress = Progress {
                read,
                total,
                elapsed,
            };
            bar(&progress, vec![], None, look, room)
        };
        // 1/64 of 24 cells is 3/8 of the first one.
        let (line, text) = at(1, 64, second, unicode, 200);
        assert_eq!(
            line,
            format!("token-max: Reading ▍{}   1%  1 B of 64 B", "░".repeat(23))
        );
        assert_eq!(text, line);
        let (line, _) = at(500, 1000, Duration::from_millis(999), unicode, 200);
        assert!(line.ends_with(" 50%  500 B of 1.0 kB"), "{line}");
        let (line, _) = at(500, 1000, 4 * second, unicode, 200);
        assert!(line.ends_with("  ~4s left to read"), "{line}");
        let (line, _) = at(999, 1000, 100 * second, plain, 200);
        assert!(line.contains(&format!("[{}-]", "#".repeat(23))), "{line}");
        assert!(
            line.ends_with(" 99%  999 B of 1.0 kB  ~1s left to read"),
            "{line}"
        );
        // A GB-scale line narrows its track to keep the estimate on an 80-column row, down
        // to eight cells, past which the line is cut instead.
        let gb = 1_000_000_000;
        let (line, _) = at(3 * gb, 8 * gb, 60 * second, unicode, 79);
        assert_eq!(crate::terminal::cells(&line), 79, "{line}");
        assert!(line.ends_with("  ~1m 40s left to read"), "{line}");
        let (line, _) = at(3 * gb, 8 * gb, 60 * second, plain, 10);
        assert!(line.contains("[###-----]"), "{line}");
        let color = Look {
            color: true,
            unicode: true,
        };
        let (line, text) = at(1, 2, second, color, 200);
        assert!(line.contains('\x1b') && !text.contains('\x1b'));
        assert_eq!(text, at(1, 2, second, unicode, 200).0);
    }

    #[test]
    fn sizes_and_durations_round_at_their_unit_boundaries() {
        use crate::report::size;
        assert_eq!(size(999), "999 B");
        assert_eq!(size(1_000), "1.0 kB");
        assert_eq!(size(999_499), "999 kB");
        assert_eq!(size(999_500), "1.0 MB");
        assert_eq!(size(9_949_999_999), "9.9 GB");
        assert_eq!(size(9_950_000_000), "10 GB");
        assert_eq!(duration(0.2), "1s");
        assert_eq!(duration(59.0), "59s");
        assert_eq!(duration(59.5), "1m 00s");
        assert_eq!(duration(125.0), "2m 05s");
        use crate::report::short;
        assert_eq!(short(999), "999");
        assert_eq!(short(999_499), "999k");
        assert_eq!(short(999_500), "1.0M");
        assert_eq!(short(9_949_999_999), "9.9B");
        assert_eq!(short(12_000_000_000_000), "12T");
    }

    #[test]
    fn the_line_counts_beside_its_phase_dropping_the_least_useful_figure_to_fit() {
        use crate::model::Margin;
        use crate::totals::Spent;
        let look = Look {
            color: false,
            unicode: true,
        };
        let mut counts = Counts {
            requests: 78_786,
            tokens: 1_912_345_678,
            sessions: 1_798,
            spend: Some(Spent {
                usd: 4_081.2,
                margin: Some(Margin::Usd(2.04)),
                ..Spent::default()
            }),
        };
        let progress = Progress {
            read: 4_500_000_000,
            total: 10_000_000_000,
            elapsed: Duration::from_secs(10),
        };
        let reading = |counts: &Counts, room| bar(&progress, figures(counts), None, look, room).0;
        assert_eq!(
            reading(&counts, 200),
            format!(
                "token-max: Reading {}▊{}  45%  4.5 GB of 10 GB  ~13s left to read  78,786 requests  1,798 sessions  1.9B tokens  ~$4,081 ± $2.04",
                "█".repeat(10),
                "░".repeat(13)
            )
        );
        // Sizes go first, then the track narrows, then the most useful figures that fit stay:
        // at 79 cells the estimate and sessions don't, but the narrower tokens still do.
        let narrowed = [
            (
                119,
                "token-max: Reading █████▊░░░░░░░  45%  ~13s left to read  78,786 requests  1,798 sessions  1.9B tokens  ~$4,081 ± $2.04",
            ),
            (
                79,
                "token-max: Reading ███▌░░░░  45%  78,786 requests  1.9B tokens  ~$4,081 ± $2.04",
            ),
            (
                59,
                "token-max: Reading ████████░░░░░░░░░░  45%  ~$4,081 ± $2.04",
            ),
        ];
        for (room, line) in narrowed {
            assert_eq!(reading(&counts, room), line);
            assert!(crate::terminal::cells(line) <= room);
        }
        let checking = busy("Checking settings", Duration::from_secs(12), 3, true);
        let after =
            |counts: &Counts, whose, room| with(checking.clone(), figures(counts), whose, room);
        assert_eq!(
            after(&counts, None, 79),
            "token-max: Checking settings... ⠸ 12s  78,786 requests  ~$4,081 ± $2.04"
        );
        // Under `vibes --ssh` the counts are this host's alone, which the label says before
        // whichever count is left first.
        assert_eq!(
            after(&counts, Some("this host"), 200),
            "token-max: Checking settings... ⠸ 12s  this host: 78,786 requests  1,798 sessions  1.9B tokens  ~$4,081 ± $2.04"
        );
        assert_eq!(
            after(&counts, Some("this host"), 70),
            "token-max: Checking settings... ⠸ 12s  this host: ~$4,081 ± $2.04"
        );
        assert_eq!(
            bar(&progress, figures(&counts), Some("this host"), look, 70).0,
            "token-max: Reading ████████░░░░░░░░░░  45%  this host: ~$4,081 ± $2.04"
        );
        // A spend too wide for the row is skipped, and the counts that fit still show.
        let mut partial = counts;
        if let Some(spend) = &mut partial.spend {
            spend.partial = true;
        }
        assert_eq!(
            reading(&partial, 79),
            "token-max: Reading ████████▌░░░░░░░░░░  45%  ~13s left to read  78,786 requests"
        );
        // Before prices load, the counts show without a spend.
        counts.spend = None;
        assert_eq!(
            after(&counts, None, 200),
            "token-max: Checking settings... ⠸ 12s  78,786 requests  1,798 sessions  1.9B tokens"
        );
    }

    /// The line counts this host as its scan reads, then ends on the hosts' total as the
    /// report adds it, whichever order their results arrive in.
    #[test]
    fn the_counts_end_on_the_hosts_total() {
        use crate::model::Margin;
        let host = |requests, spend, margin| Totals {
            requests,
            tokens: requests * 1000,
            sessions: requests / 10,
            spend,
            margin: Some(Margin::Usd(margin)),
            ..Totals::default()
        };
        let (here, there) = (host(700, 41.3, 0.7), host(300, 12.1, 2.4));
        let look = Look {
            color: false,
            unicode: false,
        };
        let shown = Shown::new(look, Arc::new(Reading::default()), None, true);
        assert_eq!(shown.counts(), None);
        let read = Counts {
            requests: 650,
            ..Counts::default()
        };
        *shown.reading.counts.lock().unwrap() = Some(read);
        assert_eq!(shown.counts(), Some(read));
        // A destination's total joins what this host has read so far, which has no spend
        // until prices load.
        shown.settle(1, &there);
        let mut joined = read;
        joined.merge(&there.counts());
        assert_eq!(shown.counts(), Some(joined));
        assert_eq!(joined.spend, None);
        shown.settle(0, &here);
        let mut total = here.clone();
        total.merge(&there);
        assert_eq!(shown.counts(), Some(total.counts()));
    }

    /// While this host checks, the line tallies its checks so far beside each destination's
    /// settled figures, then ends on the hosts' total as the report adds it.
    #[test]
    fn the_tallies_end_on_the_hosts_total() {
        use crate::defaults::{Baseline, Saving};
        use crate::doctor::Saved;
        use crate::model::Margin;
        use crate::totals::Checked;
        let credit = |usd| {
            Credit::of(&[Baseline {
                default: "5m".into(),
                saving: Saving::Saves(usd),
                margin: Some(Margin::Usd(3.0)),
            }])
        };
        let found = |usd| {
            let mut total = Total::default();
            total.count(&Saved {
                saving: Some(usd),
                margin: None,
                verb: "saves",
            });
            total
        };
        let finding = |usd| {
            let saved = Saved {
                saving: Some(usd),
                margin: None,
                verb: "saves",
            };
            crate::totals::Finding { saved, edit: None }
        };
        let host = |requests, saving, credited| Totals {
            requests,
            savings: found(saving),
            credit: Some(credit(credited)),
            ..Totals::default()
        };
        let (here, there) = (host(700, 310.0, 945.0), host(300, 20.0, 55.0));
        let look = Look {
            color: false,
            unicode: true,
        };
        let shown = Shown::new(look, Arc::new(Reading::default()), None, true);
        assert_eq!(shown.tally(), None);
        // Partway through this host's checks, a destination's report arrives.
        let mut checked = Checked::new(11, false);
        checked.done = 7;
        checked.find([finding(300.0)]);
        checked.credit = Some(credit(900.0));
        *shown.reading.checked.lock().unwrap() = Some(checked);
        shown.settle(1, &there);
        let checking = busy("Checking settings", Duration::from_secs(12), 3, true);
        assert_eq!(
            with(checking.clone(), shown.figures(), None, 200),
            "token-max: Checking settings... ⠸ 12s  7/11 checks  saving ~$955/mo ± $4.24 vs defaults  found $320/mo more  300 requests  0 sessions  0 tokens  $0"
        );
        // Settled, this host's tallies are its report's own figures.
        shown.reading.checked.lock().unwrap().as_mut().unwrap().done = 11;
        shown.settle(0, &here);
        let mut total = here.clone();
        total.merge(&there);
        let (savings, credit) = shown.tally().unwrap();
        assert_eq!(credit, total.credit);
        assert_eq!(savings.tally(true), total.savings.tally(true));
        assert_eq!(
            with(checking, shown.figures(), None, 200),
            "token-max: Checking settings... ⠸ 12s  11/11 checks  saving ~$1,000/mo ± $4.24 vs defaults  found $330/mo more  1,000 requests  0 sessions  0 tokens  $0"
        );
    }

    /// Each tally keeps its qualifier, as the report's lines do; a figure whose qualifier
    /// won't fit the row is dropped whole rather than folded into another.
    #[test]
    fn the_tallies_keep_each_kind_apart() {
        use crate::defaults::{Baseline, Saving};
        use crate::doctor::Saved;
        use crate::model::Margin;
        use crate::totals::Checked;
        let baseline = |saving, margin| Baseline {
            default: "5m".into(),
            saving,
            margin,
        };
        let credit = Credit::of(&[
            baseline(Saving::Saves(900.0), Some(Margin::Usd(12.0))),
            baseline(Saving::Estimated(45.0), None),
            baseline(Saving::UpTo(-12.0), None),
            baseline(Saving::NotMeasured, None),
        ]);
        let found: Vec<crate::totals::Finding> = [
            (Some(300.0), "saves"),
            (Some(10.0), crate::report::AT_LEAST),
            (Some(40.0), crate::report::UP_TO),
            (None, "saves"),
        ]
        .into_iter()
        .map(|(saving, verb)| crate::totals::Finding {
            saved: Saved {
                saving,
                margin: None,
                verb,
            },
            edit: None,
        })
        .collect();
        let look = Look {
            color: false,
            unicode: true,
        };
        let checking = busy("Checking settings", Duration::from_secs(12), 3, true);
        let line = |tally, credit: Option<&Credit>, room| {
            let shown = Shown::new(look, Arc::new(Reading::default()), None, tally);
            let mut checked = Checked::new(11, false);
            checked.done = 7;
            checked.find(found.clone());
            checked.credit = credit.cloned();
            *shown.reading.checked.lock().unwrap() = Some(checked);
            with(checking.clone(), shown.figures(), None, room)
        };
        assert_eq!(
            line(true, Some(&credit), 400),
            "token-max: Checking settings... ⠸ 12s  7/11 checks  saving ~$945/mo ± $12 vs defaults, 1 estimated, 1 more not measured  found $310/mo more, 1 priced in part, 1 more not measured  costing up to $12/mo more vs defaults, depending on billing  found up to $40/mo more, depending on billing"
        );
        let narrowed = [
            (
                110,
                "token-max: Checking settings... ⠸ 12s  7/11 checks  found $310/mo more, 1 priced in part, 1 more not measured",
            ),
            (79, "token-max: Checking settings... ⠸ 12s  7/11 checks"),
        ];
        for (room, expected) in narrowed {
            let shown = line(true, Some(&credit), room);
            assert_eq!(shown, expected);
            assert!(crate::terminal::cells(&shown) <= room);
        }
        // Under `--defaults` every setting is its default, so the changes are all there is.
        assert_eq!(
            line(true, None, 200),
            "token-max: Checking settings... ⠸ 12s  7/11 checks  found $310/mo, 1 priced in part, 1 more not measured  found up to $40/mo, depending on billing"
        );
        // `vibes` counts its checks but tallies nothing, since its advice is unmeasured.
        assert_eq!(
            line(false, Some(&credit), 200),
            "token-max: Checking settings... ⠸ 12s  7/11 checks"
        );
    }

    #[test]
    fn the_scans_step_holds_the_line_and_a_phase_begun_under_the_bar_counts_from_its_end() {
        let start = Instant::now();
        let at = |seconds| start + Duration::from_secs(seconds);
        let phase = ("Scanning local histories", at(0));
        let step = ("Reading T3 Code's database", at(40));
        // Before reading ends, or with nothing to read, a phase counts from its own start.
        assert_eq!(after_reading(phase, None, None), phase);
        assert_eq!(
            after_reading(phase, None, Some(at(36))),
            ("Scanning local histories", at(36))
        );
        // A phase begun after reading ended keeps its own start.
        let checking = ("Checking settings", at(50));
        assert_eq!(after_reading(checking, None, Some(at(36))), checking);
        assert_eq!(after_reading(checking, Some(step), Some(at(36))), step);
    }

    #[test]
    fn a_phase_shows_a_spinner_only_after_its_first_second() {
        assert_eq!(
            busy("Checking settings", Duration::from_millis(999), 3, true),
            "token-max: Checking settings..."
        );
        assert_eq!(
            busy("Checking settings", Duration::from_secs(12), 3, false),
            "token-max: Checking settings... \\ 12s"
        );
    }

    #[test]
    fn json_lines_escape_every_terminal_control() {
        let text = "a\u{1b}b\u{7f}c\u{9b}d\u{202e}e\u{2069}f\u{e9}";
        let mut line = vec![];
        write_json(&mut line, &text).unwrap();
        let line = String::from_utf8(line).unwrap();
        assert_eq!(
            line,
            "\"a\\u001bb\\u007fc\\u009bd\\u202ee\\u2069f\u{e9}\"\n"
        );
        assert_eq!(serde_json::from_str::<String>(&line).unwrap(), text);
    }

    /// Expected instants are what Node's `Date.parse` returns for the same text.
    #[test]
    fn since_reads_dates_and_instants_like_javascript() {
        let at = |text: &str| since(text).map_err(|e| e.message);
        for (text, millis) in [
            ("2026-11-01", 1_793_491_200_000),
            ("2026-11-01T00:00Z", 1_793_491_200_000),
            ("2026-11-01T24:00Z", 1_793_577_600_000),
            ("2026-11-01T24:00:00.000Z", 1_793_577_600_000),
            ("2026-11-01T00:00:00.1239Z", 1_793_491_200_123),
            ("2026-11-01T00:00:00+23:59", 1_793_404_860_000),
            ("2026-11-01T10:00:00.5+05:30", 1_793_507_400_500),
            ("0000-01-01T00:00:00Z", -62_167_219_200_000),
        ] {
            assert_eq!(at(text), Ok(millis), "{text}");
        }
        for text in [
            "2026-02-30",
            "2026-13-01",
            "2026-11-01T24:00:00.001Z",
            "2026-11-01T00:00:60Z",
            "2026-11-01T00:60Z",
            "2026-11-01T00:00:00+24:00",
            "2026-11-01T00:00:00-00:60",
            "2026-11-01T00:00:00.Z",
            "2026-11-01T00:00:00,5Z",
            "2026-11-01T00:00:00 Z",
            "2026-11-01T1:00Z",
            "2026-11-01T00Z",
        ] {
            assert_eq!(at(text), Err("invalid --since date".into()), "{text}");
        }
        for text in [
            "2026-11",
            "2026-11-01T00:00:00",
            "2026-11-01 00:00Z",
            "2026-11-01T00:00:00+0530",
            "2026-11-01T00:00:00Z\n",
        ] {
            assert!(
                at(text).unwrap_err().starts_with("--since requires"),
                "{text:?}"
            );
        }
    }
}
