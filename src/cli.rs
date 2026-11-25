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

use crate::collections::IndexMap;
use crate::discovery::{DiscoveryOptions, discover};
use crate::doctor::{
    BillingQuestions, Confirm, Fix, Found, Offers, Owner, Selection, Style, Walking, ask,
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
use crate::totals::Totals;
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
  --plain                 Plain text without color or decorative Unicode
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
both a terminal, it then offers the changes it can make, grouping tools, skills and MCP
servers by why they are offered (y, n, s to split a group, or q to stop), shows the
accepted edits as a diff of each file, and writes the profile's own settings.json or
config.toml only after a final yes. A change another owner would
override, such as a shell variable, a project or managed settings file or T3, is listed
to make by hand. Without a terminal it prints the report, each change naming its edit,
and edits nothing.

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
  --plain                 Plain text without color or decorative Unicode
  --offline               Use cached public prices without network access
  --prices-file FILE      Use a local models.dev catalog without network access
  --price-cache FILE      Cache path (default: ~/.cache/token-max/models.json)
  -h, --help              Show this help

Prints one card for each piece of advice token-max doctor leaves out because no transcript
can price it: a provider's guidance, such as an effort above high or an instruction file
past its budget, settings whose saving is a guess, such as the Bash timeout cap, and the
maintainer's picks, such as auto memory off where most sessions ran in a repository. Each
card says what was measured beside the advice and names whose advice it is: the provider's
page, or the maintainer's pick. None of it is a verdict.

When stdin and stdout are both a terminal, a card that names the settings to change asks
whether to change them (y, n, or q to stop), shows the accepted edits as a diff of each
file, and writes the profile's own settings.json only after a final yes, one edit per
file. A change another owner would override is named to make by hand. Every other card is
shown with no question. Without a terminal it prints the cards, each change naming its
edit, and edits nothing.

It reads the hosts, profiles and window token-max doctor reads, with the same flags, and
asks the same question about how each Claude profile pays.

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

/// The cells of the progress line ending stderr, which the next output must clear first;
/// zero when none is showing.
static PROGRESS: Mutex<usize> = Mutex::new(0);

/// Replace the progress line on stderr with `line`, whose text without color is `plain`.
/// Spaces, not escapes, blank what a longer line left, so this works on any terminal and
/// under `--plain`. A line too wide for one row shows `plain` cut to fit, since a carriage
/// return cannot reach a row the line wrapped onto, and a cut could split a color escape.
fn show_progress(line: &str, plain: &str) {
    let mut shown = PROGRESS.lock().unwrap_or_else(PoisonError::into_inner);
    let room = progress_room();
    let cells = crate::terminal::cells(plain);
    let (text, cells) = if cells > room {
        let cut = crate::terminal::fit(plain, room);
        (cut, crate::terminal::cells(cut))
    } else {
        (line, cells)
    };
    let pad = " ".repeat((*shown).min(room).saturating_sub(cells));
    eprint!("\r{text}{pad}");
    *shown = cells;
}

/// The cells a progress line may take: one less than stderr's terminal is wide now, so
/// neither the line nor the spaces blanking it wrap after the terminal shrinks.
fn progress_room() -> usize {
    let columns: usize =
        terminal_size::terminal_size_of(io::stderr()).map_or(80, |(w, _)| w.0.into());
    columns.max(2) - 1
}

/// Clear the progress line, if one is showing, so what follows starts on a clean row. The
/// returned lock keeps another thread from drawing one again while it is held.
fn clear_progress() -> MutexGuard<'static, usize> {
    let mut shown = PROGRESS.lock().unwrap_or_else(PoisonError::into_inner);
    if *shown > 0 {
        eprint!("\r{}\r", " ".repeat((*shown).min(progress_room())));
        *shown = 0;
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

/// What the progress line shows: the current phase and when it began, and how far the local
/// scan has read.
struct Shown {
    phase: Mutex<(&'static str, Instant)>,
    reading: Arc<Reading>,
    look: Look,
}

impl Meter {
    fn start(look: Look, reading: Arc<Reading>) -> Self {
        let shared = Arc::new(Shown {
            phase: Mutex::new(("", Instant::now())),
            reading,
            look,
        });
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
                    // The local scan may still be reading after the phase moves on, as
                    // when SSH hosts' prices or reports arrive first.
                    let (line, plain) = if read < total {
                        let began = *began.get_or_insert_with(Instant::now);
                        let progress = Progress {
                            read,
                            total,
                            elapsed: began.elapsed(),
                        };
                        bar(&progress, shared.look, progress_room())
                    } else {
                        // A phase that began under the bar is timed from when reading ended.
                        let since = match total {
                            0 => since,
                            _ => since.max(*read_end.get_or_insert_with(Instant::now)),
                        };
                        let line = busy(name, since.elapsed(), tick, shared.look.unicode);
                        (line.clone(), line)
                    };
                    show_progress(&line, &plain);
                }
            })
        };
        Self {
            shared,
            stop: Some(stop),
            ticker: Some(ticker),
        }
    }

    /// Show `phase` now, unless the reading bar holds the line.
    fn phase(&self, phase: &'static str) {
        let mut current = self
            .shared
            .phase
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *current = (phase, Instant::now());
        // While this host still reads, the bar stays; the ticker shows the phase after it.
        let reading = &self.shared.reading;
        if reading.read.load(Ordering::Relaxed) < reading.total.load(Ordering::Relaxed) {
            return;
        }
        let line = busy(phase, Duration::ZERO, 0, self.shared.look.unicode);
        show_progress(&line, &line);
    }
}

impl Drop for Meter {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(ticker) = self.ticker.take() {
            let _ = ticker.join();
        }
        drop(clear_progress());
    }
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

/// The widest and narrowest the reading bar's track gets.
const TRACK_CELLS: (usize, usize) = (24, 8);

/// How far reading has come: bytes `read` of `total`, `elapsed` since reading began.
struct Progress {
    read: u64,
    total: u64,
    elapsed: Duration,
}

/// The reading bar, its track narrowed to fit `room` cells: how much is read, and when the
/// rest should be, from the rate so far. Returns the line and its text without color.
fn bar(progress: &Progress, look: Look, room: usize) -> (String, String) {
    let Progress {
        read,
        total,
        elapsed,
    } = *progress;
    let fraction = read as f64 / total as f64;
    let mut rest = format!(
        " {:>3.0}%  {} of {}",
        (fraction * 100.0).floor(),
        crate::report::size(read),
        crate::report::size(total)
    );
    // A rate needs a second of reading, and a few percent, to mean anything.
    if elapsed >= Duration::from_secs(1) && fraction >= 0.02 {
        let left = elapsed.as_secs_f64() * (1.0 - fraction) / fraction;
        rest.push_str(&format!("  ~{} left to read", duration(left)));
    }
    let lead = "token-max: Reading ";
    let (open, close) = if look.unicode { ("", "") } else { ("[", "]") };
    let used = crate::terminal::cells(lead) + open.len() + close.len() + rest.len();
    let track = room
        .saturating_sub(used)
        .clamp(TRACK_CELLS.1, TRACK_CELLS.0);
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
    Walk(Walk),
}

/// Every host's changes, to walk through on this terminal.
struct Walk {
    hosts: Vec<WalkHost>,
    /// What `doctor`'s walk prints before its questions: the total across hosts, then this
    /// host's header, and what the settings save against the defaults.
    head: String,
    style: Style,
    /// Which command's changes the hosts offer; `doctor`'s walk goes on to `vibes`'.
    command: Command,
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
    /// What `vibes` has for it: known from the start here, and once asked for there.
    items: Vec<Item>,
    /// How many pieces of advice `doctor` offers `vibes` for.
    pending: usize,
    /// Whether a walk wrote its files, so a destination's audit is ended, not dropped.
    wrote: bool,
}

/// Where a host's files are read and written.
enum Side {
    /// This host's own, with its report when the walk is `doctor`'s.
    Here {
        owner: Owner,
        listing: Option<Box<Listing>>,
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
                listing,
                monthly,
                written,
            } => {
                self.items = vibes::settled(std::mem::take(&mut self.items), written);
                *owner = vibes::owner(&self.items, self.style.home.clone(), *monthly);
                *listing = None;
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
}

/// One host's printed report and whether it recommends a change; all a destination sends.
pub struct Printed {
    /// The printed report.
    pub text: String,
    /// Whether the report recommends a change.
    pub change: bool,
    /// What the report adds to a total across hosts.
    pub totals: Box<Totals>,
    /// How many pieces of advice `vibes` has, which `doctor` offers to show.
    pub vibes: usize,
}

/// This host's own report, with the changes it names.
pub struct Rendered {
    /// The report and its totals.
    pub printed: Printed,
    /// The edits the report recommends, which the walk offers to apply.
    pub fixes: Vec<Fix>,
    /// The report, to print again after the walk.
    pub listing: Box<Listing>,
    /// MCP servers whose tools are off already, which the walk offers to remove.
    pub turned_off: Vec<crate::mcp::Definition>,
    /// The advice the transcripts cannot price, which `vibes` shows.
    pub vibes: Vec<Item>,
}

/// Analyze one host's scan and print its report.
pub fn render(scan: Scan, options: Render<'_>) -> Result<Rendered, ScanError> {
    let report = analyze(AnalyzeOptions {
        scan,
        catalog: options.catalog,
        since: options.since,
        now: options.now,
        max_cost_per_use_usd: options.max_cost_per_use_usd,
    })?;
    let change = report
        .checks
        .iter()
        .any(|c| c.action() == crate::model::Action::Change)
        || crate::context::setting_rows(&report.context).any(|c| {
            c.verdict == Some(Verdict::Off) && crate::defaults::applied(&report, c).is_none()
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
            text: listing.text(),
            change,
            totals: Box::new(Totals::new(&report, &fixes, listing.credit())),
            vibes: vibes::pending(&vibes),
        },
        fixes,
        listing: Box::new(listing),
        turned_off: crate::mcp::turned_off(&report.scan.profiles),
        vibes,
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
        || values.max_cost_per_use.is_some();
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

/// Run one command, returning the output and exit code without writing either.
fn run(args: &[String]) -> Result<(Output, u8), ScanError> {
    match args.first().map(String::as_str) {
        None => return Ok((Output::Usage(help(HELP)), 2)),
        Some("doctor") => return audit(&args[1..], Command::Doctor),
        Some("vibes") => return audit(&args[1..], Command::Vibes),
        Some("sessions") => return sessions(&args[1..]),
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
    let scope = match job {
        Job::Report { .. } => Scope::Report { defaults },
        Job::Sessions => Scope::Sessions,
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
    };
    let (sender, results) = mpsc::channel();
    {
        // Prices load beside the scans and questions, so whichever fails first stops the run.
        let sender = sender.clone();
        thread::spawn(move || {
            let catalog = panic::catch_unwind(|| load_prices(&prices))
                .unwrap_or_else(|_| Err(ScanError::new("analysis failed; no report produced")));
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
                .unwrap_or_else(|_| Err(ScanError::new("analysis failed; no report produced")))
                .map(Scanned::Remote);
            let _ = sender.send(Arrival::Host(index, result));
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
            reading: Some(reading.clone()),
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
                    .then_some(progress)
                    .flatten()
                    .map(|look| {
                        let meter = Meter::start(look, reading.clone());
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
    let meter = progress.map(|look| Meter::start(look, reading.clone()));
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
                    });
                    catalog = Some(loaded);
                }
            }
            let Some(catalog) = &catalog else { continue };
            for (index, scanned) in scanned.drain(..) {
                replies[index] = Some(match scanned {
                    Scanned::Local(scan) => match &local_job {
                        Job::Report {
                            max_cost_per_use_usd,
                            format,
                            ..
                        } => Reply::Here(Box::new(render(
                            *scan,
                            Render {
                                catalog,
                                since,
                                now: crate::reader::now(),
                                max_cost_per_use_usd: *max_cost_per_use_usd,
                                format,
                                home: home.as_deref(),
                            },
                        )?)),
                        Job::Sessions => Reply::Sessions(export(&scan, catalog)?),
                    },
                    Scanned::Remote(reply) => reply,
                });
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
    // A report with the settings files pretended away has no changes to make.
    let walk = terminal && !values.defaults;
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
    // which picks each one's main TTL default; without a terminal the report names both
    // defaults.
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
    let mut text = String::new();
    let mut hosts = vec![];
    // Each host's advice off a terminal, for `vibes` to print.
    let mut cards: Vec<(Option<String>, Vec<Item>, Style)> = vec![];
    let mut sum: Option<Totals> = None;
    // Advice `doctor` leaves to `vibes`, counted across hosts.
    let mut pending = 0;
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
        // Each host's report follows the walk, marked with its answers.
        let (printed, side, items) = match reply {
            Reply::Here(rendered) => {
                let Rendered {
                    printed,
                    fixes,
                    listing,
                    turned_off,
                    vibes: items,
                } = *rendered;
                let monthly = printed.totals.monthly;
                let side = walk.then(|| {
                    let (fixes, turned_off, listing) = if advice {
                        (vibes::fixes(&items), vec![], None)
                    } else {
                        (fixes, turned_off, Some(listing))
                    };
                    Side::Here {
                        owner: Owner::new(fixes, turned_off, home.clone(), monthly),
                        listing,
                        monthly,
                        written: vec![],
                    }
                });
                (Some(printed), side, items)
            }
            Reply::Report(printed) => (Some(printed), None, vec![]),
            Reply::Walk {
                printed,
                offers,
                remote,
            } => (Some(printed), Some(Side::There { offers, remote }), vec![]),
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
            Reply::Sessions(_) => unreachable!("a report job gets reports"),
        };
        if let Some(printed) = &printed {
            code = code.max(u8::from(printed.change));
            pending += printed.vibes;
            match &mut sum {
                Some(sum) => sum.merge(&printed.totals),
                None => sum = Some(*printed.totals.clone()),
            }
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
                pending: printed.as_ref().map_or(0, |p| p.vibes),
                wrote: false,
            });
            continue;
        }
        if advice {
            let (label, style) = here();
            cards.push((label, vibes::resolved(items), style));
            continue;
        }
        let Some(printed) = printed else {
            unreachable!("a report without a walk is printed")
        };
        if !text.is_empty() {
            text.push('\n');
        }
        // A destination's report is its own text, so escape all but our own styles.
        match ssh {
            Some(_) => text.push_str(&crate::safe::remote_text(&printed.text, color)),
            None => text.push_str(&printed.text),
        }
    }
    // Several hosts get one total at the head, beside each host's own section.
    let head = match &sum {
        Some(sum) if sections > 1 && !advice => {
            let mut head = totals_section(
                sum,
                sections,
                ReportOptions {
                    width: style.width,
                    color,
                    unicode,
                    ..ReportOptions::default()
                },
            );
            head.push('\n');
            head
        }
        _ => String::new(),
    };
    if walk {
        // This host's header follows the total, and one host's is followed by what its
        // settings save against the defaults, as several hosts' total is, so the credit
        // comes before any question.
        let title = hosts.iter().find_map(|h| match &h.side {
            Side::Here { listing, .. } => listing.as_ref().map(|l| l.title().to_string()),
            Side::There { .. } => None,
        });
        let credit = match &sum {
            Some(sum) if sections == 1 && !advice => {
                sum.credit("this host's").map_or_else(String::new, |c| {
                    let note = opening_note(&c, sum.partial);
                    let note = if note.is_empty() {
                        note
                    } else {
                        style.dim(&note)
                    };
                    style.line(&c) + &note
                })
            }
            _ => String::new(),
        };
        let head = format!("{head}{}{credit}", title.unwrap_or_default());
        let walk = Walk {
            hosts,
            head,
            style,
            command,
        };
        return Ok((Output::Walk(walk), 0));
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
    // Off a terminal, the advice this report leaves out is one command away. Under
    // `--defaults` the count is of settings pretended away, which `vibes` reads as they are.
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
    Ok((Output::Text(format!("{head}{text}{hint}")), code))
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
    let Walk {
        mut hosts,
        head,
        style,
        command,
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
            settle(&mut hosts, &selections, &style, &mut stdin, &mut stdout)?;
            let wrote = hosts.iter().any(|h| h.wrote);
            // What a host wrote can change its advice, so its count is read again below.
            if !wrote && hosts.iter().all(|h| h.pending == 0) {
                return Ok(());
            }
            enter(if wrote {
                Phase::AskingMore
            } else {
                Phase::Asking
            });
            // A Ctrl-C while the changes were written ends the run after them. Read once the
            // phase has moved on, since one that lands after that exits in the handler.
            if ssh::cancelled() {
                return Ok(());
            }
            // A host that wrote reads its advice again for the count; the rest only on a yes.
            for host in hosts.iter_mut().filter(|h| h.wrote) {
                host.advise()?;
            }
            let pending: usize = hosts
                .iter()
                .map(|h| match h.wrote {
                    true => vibes::pending(&h.items),
                    false => h.pending,
                })
                .sum();
            if pending == 0 {
                return Ok(());
            }
            let failed = |_| unusable(wrote);
            if !vibes::offer(pending, &style, &mut stdin, &mut stdout).map_err(failed)? {
                return Ok(());
            }
            writeln!(stdout).map_err(failed)?;
            for host in hosts.iter_mut().filter(|h| !h.wrote) {
                host.advise()?;
            }
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
            settle(&mut hosts, &selections, &style, &mut stdin, &mut stdout)?;
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

/// The terminal failed under a walk, after `wrote` says whether settings were written.
fn unusable(wrote: bool) -> ScanError {
    ScanError::new(if wrote {
        "cannot use the terminal; no further settings were changed"
    } else {
        "cannot use the terminal; no settings were changed"
    })
}

/// Print each host's report with the walk's `selections` marked on it, show what the accepted
/// changes would write, and write them once the person confirms: this host's first, then
/// each destination's there, stopping at the first host that fails.
fn settle(
    hosts: &mut [WalkHost],
    selections: &[Selection],
    style: &Style,
    stdin: &mut impl io::BufRead,
    stdout: &mut impl Write,
) -> Result<(), ScanError> {
    let before = hosts.iter().any(|h| h.wrote);
    let failed = |_| unusable(before);
    let summaries: Vec<_> = hosts
        .iter()
        .zip(selections)
        .map(|(h, s)| s.summary(h.offers()))
        .collect();
    // Each host makes its own plan from the answers, and the report reads as its own section
    // after the questions.
    let mut plans = vec![];
    let mut previews = String::new();
    let mut gap = summaries.iter().any(|s| s.answered());
    for (host, selection) in hosts.iter_mut().zip(selections) {
        let (report, preview) = match &mut host.side {
            Side::Here { owner, listing, .. } => {
                let plan = owner.plan(selection.events())?;
                let report = listing.as_ref().map_or_else(String::new, |listing| {
                    listing.review(plan.choices(), &plan.total("Accepted"))
                });
                let shown = preview(&plan, &host.style);
                plans.push(Some(plan));
                (report, shown)
            }
            Side::There { remote, .. } => {
                let (report, shown) = remote.review(selection.events())?;
                plans.push(None);
                (
                    crate::safe::remote_text(&report, style.color),
                    crate::safe::remote_text(&shown, style.color),
                )
            }
        };
        // `vibes` has no report to print again.
        if !report.is_empty() {
            let gap = if std::mem::replace(&mut gap, true) {
                "\n"
            } else {
                ""
            };
            write!(stdout, "{gap}{report}").map_err(failed)?;
        }
        previews += &preview;
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
    // The session export keeps its diagnostics machine-readable too.
    let json = args.first().is_some_and(|a| a == "sessions");
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
            match panic::catch_unwind(AssertUnwindSafe(|| walk(w))) {
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
            bar(&progress, look, room)
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
