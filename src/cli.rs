//! The command line: arguments, progress, output streams, cancellation and exit codes.

use std::io::{self, IsTerminal, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Mutex, PoisonError, mpsc};
use std::thread;

use serde::Serialize;
use serde_json::ser::Formatter;

use crate::collections::IndexSet;
use crate::discovery::{DiscoveryOptions, discover};
use crate::doctor::{
    Confirm, Fix, Offers, Owner, Style, Walking, ask, ask_billing, confirm, finish, preview, write,
    wrote,
};
use crate::model::{Harness, Location, Millis, Scan, Verdict};
use crate::prices::{Catalog, PriceOptions, cancel_downloads, load_prices};
use crate::reader::{ScanOptions, scan};
use crate::replay::{AnalyzeOptions, analyze};
use crate::report::{Listing, ReportOptions, format_report, totals_section};
use crate::safe::{ScanError, display};
use crate::sessions::{Session, export};
use crate::ssh::{self, Reply};
use crate::terminal::{WrapOptions, wrap_text};
use crate::totals::Totals;
use crate::wire::{Format, Job, Request};

const HELP: &str =
    "token-max: agent settings that use less of a Claude or ChatGPT plan, measured at API prices

Usage: token-max <command> [options]

Commands:
  doctor    Report which settings to change, then offer to change them
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
  --ssh HOST              Audit an SSH destination, putting this build there first if needed (repeatable)
  --include-local         Also audit this host when --ssh is given
  --max-cost-per-use USD  Suggest turning off a used tool, skill or MCP server that cost more than this per use (default: 1)
  --details               Show grouped check evidence, assumptions, uncertainty and sources
  --defaults              Report as a user with no settings files would see it, offering nothing
  --plain                 Plain text without color or decorative Unicode
  --offline               Use cached public prices without network access
  --prices-file FILE      Use a local models.dev catalog without network access
  --price-cache FILE      Cache path (default: ~/.cache/token-max/models.json)
  -v, --version           Show the package version
  -h, --help              Show this help

Prints which settings to change, with the evidence for each. When stdin and stdout are
both a terminal, it then offers each change it can make, one at a time (y, n, or q to
stop), shows the accepted edits as a diff of each file, and writes the profile's own
settings.json or config.toml only after a final yes. A change another owner would
override, such as a shell variable, a project or managed settings file or T3, is listed
to make by hand. Without a terminal it prints the report, each change naming its edit,
and edits nothing.

With --defaults it reads every user, project and local settings file, T3 Code's settings
and the shell's setting variables as unset, so each setting is at its Claude Code or Codex
default and every tool, skill and MCP server is on; managed and system policy still apply.
It prints that report and edits nothing, even on a terminal.

Without profile flags, discover local Claude/Codex profiles and T3 instances. Explicit profiles
limit the entire run. Directory flags select local profiles only; with --ssh
they need --include-local. Each SSH destination discovers its own profiles
with this build of token-max, copied there or downloaded there from this version's release
and cached; it prices its own history with this host's
prices, and only its report and its totals cross SSH, never its scan. Several hosts get one
total, their figures added as they are. Its changes are listed to make there. Current
settings are applied to the selected history; checks can use different windows and
baselines. Independent savings do not add.

Examples:
  token-max doctor --since 2026-11-01
  token-max doctor --claude-dir ~/.claude-work --codex-dir ~/.codex --details
  token-max doctor --offline --plain
  token-max doctor --ssh devbox --ssh user@laptop --include-local

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

When it walks through the changes on a terminal: exit 0, or 2 on failure or cancellation.
Otherwise: exit 0 for no applicable change (advisory findings may remain), 1 for a
recommended change, 2 for a scan, pricing, argument or cancellation failure.
";

const SESSIONS_HELP: &str =
    "token-max sessions: each transcript session's usage at public API prices

Usage: token-max sessions [options]

  --since YYYY-MM-DD|ISO  Count requests from midnight UTC or an instant with timezone
  --claude-dir DIR        Select a Claude profile (repeatable)
  --codex-dir DIR         Select a Codex home, including archived sessions (repeatable)
  --t3-dir DIR            Select a T3 state directory (repeatable)
  --ssh HOST              Also export an SSH destination's sessions, putting this build there first if needed (repeatable)
  --include-local         Also export this host's sessions when --ssh is given
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

Directory flags select local profiles only; with --ssh they need --include-local. Each SSH
destination exports its own sessions with this host's prices, and only its session lines
cross SSH. A session found on several hosts is printed once per host. Any failed host fails
the whole run.

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
    /// Writing the report or settings, which finishes whole.
    Writing,
}

/// The current phase. A cancelling signal holds this lock until the process exits, so a
/// report or a settings file is either written whole or not at all.
static PHASE: Mutex<Phase> = Mutex::new(Phase::Running);

/// The cells of the progress line ending stderr, which the next output must clear first;
/// zero when none is showing.
static PROGRESS: Mutex<usize> = Mutex::new(0);

/// Replace the progress line on stderr with `message`. Spaces, not escapes, blank what a
/// longer line left, so this works on any terminal and under `--plain`. The line is cut to
/// fit one row, since a carriage return cannot reach a row the line wrapped onto.
fn show_progress(message: &str) {
    let mut shown = PROGRESS.lock().unwrap_or_else(PoisonError::into_inner);
    let room = progress_room();
    let line = format!("token-max: {message}...");
    let text = crate::terminal::fit(&line, room);
    let cells = crate::terminal::cells(text);
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

/// Clear the progress line, if one is showing, so what follows starts on a clean row.
fn clear_progress() {
    let mut shown = PROGRESS.lock().unwrap_or_else(PoisonError::into_inner);
    if *shown > 0 {
        eprint!("\r{}\r", " ".repeat((*shown).min(progress_room())));
        *shown = 0;
    }
}

/// Enter `phase`; blocks forever if a cancelling signal already owns the process.
fn enter(phase: Phase) {
    *PHASE.lock().unwrap_or_else(PoisonError::into_inner) = phase;
}

/// Parsed flags, as `node:util` `parseArgs` would return them in strict mode.
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
    include_local: bool,
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
            "include-local" => &mut self.include_local,
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

/// Parse like strict `parseArgs`: no positionals, no unknown options, no inline values for
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

/// Milliseconds after UTC midnight for `THH:mm[:ss[.fff]]` and `Z` or `+HH:mm`, as V8's ISO
/// parser reads it: 24:00 is the next midnight, seconds stop at 59, extra fraction digits drop.
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

/// An environment variable that is set and not empty, as JavaScript `||` treats it.
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

/// Numbers print as JavaScript's `JSON.stringify` prints them, such as `1` rather than `1.0`.
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

/// Stream a value as one compact JavaScript-compatible JSON line.
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
    clear_progress();
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
    /// The total across hosts, printed first; empty for one host.
    head: String,
    style: Style,
}

/// One host in the walk.
struct WalkHost {
    /// Its name, when the walk covers another host than this one.
    label: Option<String>,
    side: Side,
    /// How its changes print: its paths from its own home.
    style: Style,
}

/// Where a host's files are read and written.
enum Side {
    /// This host's own, with its report.
    Here { owner: Owner, listing: Box<Listing> },
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
}

/// One host before its job runs here: this host's scan, or a destination's reply.
enum Scanned {
    Local(Box<Scan>),
    Remote(Reply),
}

/// What reaches the coordinator while hosts scan, in whatever order it finishes.
enum Arrival {
    Prices(Result<Catalog, ScanError>),
    Host(usize, Result<Scanned, ScanError>),
}

/// What one host's scan is analyzed and printed with.
pub struct Render<'a> {
    pub catalog: &'a Catalog,
    pub since: Option<Millis>,
    pub now: Millis,
    pub max_cost_per_use_usd: f64,
    pub format: &'a Format,
    /// The host's home directory, which its report shows as `~`.
    pub home: Option<&'a str>,
}

/// One host's printed report and whether it recommends a change; all a destination sends.
pub struct Printed {
    pub text: String,
    pub change: bool,
    /// What the report adds to a total across hosts.
    pub totals: Box<Totals>,
}

/// This host's own report, with the changes it names.
pub struct Rendered {
    pub printed: Printed,
    pub fixes: Vec<Fix>,
    /// The report, to print again after the walk.
    pub listing: Box<Listing>,
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
    let listing = format_report(&report, &fixes, format.report(options.home));
    crate::doctor::explain(&mut fixes, &listing);
    Ok(Rendered {
        printed: Printed {
            text: listing.text(),
            change,
            totals: Box::new(Totals::new(&report, &fixes)),
        },
        fixes,
        listing: Box::new(listing),
        turned_off: crate::mcp::turned_off(&report.scan.profiles),
    })
}

/// The `--since` bound; an empty value is absent, as JavaScript treats a falsy option.
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
        || values.ssh_scan
        || values.max_cost_per_use.is_some();
    if report_only {
        return Err(invalid());
    }
    let since = window(&values)?;
    let (discovery, destinations) = hosts(&mut values)?;
    let replies = gather(Gather {
        since,
        defaults: false,
        discovery,
        destinations,
        prices: price_options(values.prices_file, values.price_cache, values.offline)?,
        job: Job::Sessions,
        home: None,
        progress: false,
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
        Some("doctor") => return doctor(&args[1..]),
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

/// The selected hosts: this host's discovery when it is selected, and the SSH destinations.
///
/// Takes the directory flags, which select this host's profiles.
fn hosts(values: &mut Values) -> Result<(Option<DiscoveryOptions>, Vec<String>), ScanError> {
    for (index, destination) in values.ssh.iter().enumerate() {
        ssh::destination(destination)?;
        if values.ssh[..index].contains(destination) {
            return Err(ScanError::new(format!(
                "--ssh {destination} is selected twice"
            )));
        }
    }
    let remote = !values.ssh.is_empty();
    if values.include_local && !remote {
        return Err(ScanError::new("--include-local requires --ssh"));
    }
    let local = !remote || values.include_local;
    if !local
        && !(values.claude_dirs.is_empty()
            && values.codex_dirs.is_empty()
            && values.t3_dirs.is_empty())
    {
        return Err(ScanError::new(
            "--claude-dir, --codex-dir and --t3-dir select local profiles; add --include-local to read them with --ssh",
        ));
    }
    let discovery = if local {
        let mut discovery = DiscoveryOptions::host()?;
        discovery.claude_dirs = std::mem::take(&mut values.claude_dirs);
        discovery.codex_dirs = std::mem::take(&mut values.codex_dirs);
        discovery.t3_dirs = std::mem::take(&mut values.t3_dirs);
        Some(discovery)
    } else {
        None
    };
    Ok((discovery, std::mem::take(&mut values.ssh)))
}

/// A run across the selected hosts.
struct Gather {
    since: Option<Millis>,
    /// Pretend the settings files away on every host, for `doctor --defaults`.
    defaults: bool,
    /// This host, when it is selected.
    discovery: Option<DiscoveryOptions>,
    destinations: Vec<String>,
    prices: PriceOptions,
    /// What every host makes of its scan; a report's host label is set per host.
    job: Job,
    /// This host's home directory, which its report shows as `~`.
    home: Option<String>,
    /// Print each phase to stderr.
    progress: bool,
}

/// Scan every selected host and load prices at once, then run the job on each, failing on the
/// first error. Returns each host's reply in selection order, this host first, as None.
///
/// Destinations run the job on their own scans, so this host only ever holds its own.
fn gather(run: Gather) -> Result<Vec<(Option<String>, Reply)>, ScanError> {
    let Gather {
        since,
        defaults,
        discovery,
        destinations,
        prices,
        job,
        home,
        progress,
    } = run;
    let phase = |message: &str| {
        if progress {
            show_progress(message);
        }
    };
    let local = discovery.is_some();
    let remote = !destinations.is_empty();
    // Each host's report is named once the run covers several.
    let labeled = |host: Option<&str>| match &job {
        Job::Report {
            max_cost_per_use_usd,
            format,
            walk,
        } => Job::Report {
            max_cost_per_use_usd: *max_cost_per_use_usd,
            format: Format {
                host: host.map(str::to_string),
                ..format.clone()
            },
            walk: *walk,
        },
        Job::Sessions => Job::Sessions,
    };
    let (sender, results) = mpsc::channel();
    let mut hosts: Vec<Option<String>> = vec![];
    let mut requests = vec![];
    // A lone local scan runs on this thread, so the analysis reuses the memory the scan
    // frees rather than leaving it to another thread's allocator arena.
    let mut inline = None;
    if let Some(discovery) = discovery {
        let index = hosts.len();
        hosts.push(None);
        let job = move || {
            panic::catch_unwind(|| scan(&ScanOptions { discovery, since }))
                .unwrap_or_else(|_| Err(ScanError::new("analysis failed; no report produced")))
                .map(|scan| Scanned::Local(Box::new(scan)))
        };
        if remote {
            let sender = sender.clone();
            thread::spawn(move || {
                let _ = sender.send(Arrival::Host(index, job()));
            });
        } else {
            inline = Some((index, job));
        }
    }
    for destination in destinations {
        let sender = sender.clone();
        let index = hosts.len();
        let (request, receiver) = mpsc::channel();
        requests.push((request, labeled(Some(&destination))));
        hosts.push(Some(destination.clone()));
        thread::spawn(move || {
            let result =
                panic::catch_unwind(|| ssh::audit(&destination, since, defaults, receiver))
                    .unwrap_or_else(|_| Err(ScanError::new("analysis failed; no report produced")))
                    .map(Scanned::Remote);
            let _ = sender.send(Arrival::Host(index, result));
        });
    }
    phase(match (local, remote) {
        (true, false) => "Scanning local histories",
        (true, true) => "Scanning local histories and SSH hosts",
        (false, _) => "Scanning SSH hosts",
    });
    let local_job = labeled(remote.then_some("this host"));
    let mut replies: Vec<Option<Reply>> = hosts.iter().map(|_| None).collect();
    let gathered = (|| {
        let inline = inline
            .map(|(index, job)| job().map(|scan| (index, scan)))
            .transpose()?;
        phase("Loading public prices");
        // Prices load beside the scans, so whichever fails first stops the run at once.
        thread::spawn(move || {
            let catalog = panic::catch_unwind(|| load_prices(&prices))
                .unwrap_or_else(|_| Err(ScanError::new("analysis failed; no report produced")));
            let _ = sender.send(Arrival::Prices(catalog));
        });
        let mut catalog = None;
        // Hosts that finished before the prices arrived.
        let mut scanned: Vec<_> = inline.into_iter().collect();
        for arrival in results {
            match arrival {
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
    clear_progress();
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

/// `token-max doctor`: audit the selected hosts, then walk through their changes when stdin
/// and stdout are a terminal.
fn doctor(args: &[String]) -> Result<(Output, u8), ScanError> {
    let invalid = || ScanError::new("invalid arguments; use token-max doctor --help");
    let mut values = parse(args).ok_or_else(invalid)?;
    if values.help {
        return Ok((Output::Text(help(DOCTOR_HELP)), 0));
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
        // A destination scans with its own discovery; only the window crosses SSH.
        let other = values.offline
            || values.plain
            || values.details
            || values.include_local
            || values.max_cost_per_use.is_some()
            || values.prices_file.is_some()
            || values.price_cache.is_some()
            || !values.claude_dirs.is_empty()
            || !values.codex_dirs.is_empty()
            || !values.t3_dirs.is_empty()
            || !values.ssh.is_empty();
        if other {
            return Err(invalid());
        }
        return Ok((Output::Written, ssh::serve(since, values.defaults)));
    }
    let (mut discovery, destinations) = hosts(&mut values)?;
    if values.defaults
        && let Some(discovery) = &mut discovery
    {
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
    // This host's Claude profiles on the Anthropic API whose billing picks the main TTL's
    // default, asked before the scan; without a terminal the report names both defaults.
    if terminal && let Some(discovery) = &mut discovery {
        // T3 Code's instances share a native root, and so its account and answer.
        let roots: Vec<String> = discover(discovery)?
            .profiles
            .into_iter()
            .filter(|p| {
                p.harness == Harness::Claude && p.provider.is_none() && !p.histories.is_empty()
            })
            .map(|p| p.root)
            .collect::<IndexSet<_>>()
            .into_iter()
            .collect();
        let failed = |_| ScanError::new("cannot use the terminal; no report produced");
        enter(Phase::Asking);
        discovery.billing = ask_billing(
            &roots,
            &style,
            &mut io::stdin().lock(),
            &mut io::stdout().lock(),
        )
        .map_err(failed)?;
        enter(Phase::Running);
    }
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
        },
        home: home.clone(),
        progress: stdout && io::stderr().is_terminal() && !term_dumb,
    })?;
    let sections = replies.len();
    let mut code = 0;
    let mut text = String::new();
    let mut hosts = vec![];
    let mut sum: Option<Totals> = None;
    for (ssh, reply) in replies {
        // Each host's report follows the walk, marked with its answers.
        let (printed, side) = match reply {
            Reply::Here(rendered) => {
                let Rendered {
                    printed,
                    fixes,
                    listing,
                    turned_off,
                } = *rendered;
                let side = walk.then(|| Side::Here {
                    owner: Owner::new(fixes, turned_off, home.clone(), printed.totals.monthly),
                    listing,
                });
                (printed, side)
            }
            Reply::Report(printed) => (printed, None),
            Reply::Walk {
                printed,
                offers,
                remote,
            } => (printed, Some(Side::There { offers, remote })),
            Reply::Sessions(_) => unreachable!("a report job gets reports"),
        };
        code = code.max(u8::from(printed.change));
        match &mut sum {
            Some(sum) => sum.merge(&printed.totals),
            None => sum = Some(*printed.totals.clone()),
        }
        if let Some(side) = side {
            let (label, host_style) = match &side {
                Side::Here { .. } => (
                    (sections > 1).then(|| "this host".to_string()),
                    style.clone(),
                ),
                Side::There { offers, .. } => (
                    ssh.clone(),
                    Style {
                        home: offers.home.clone(),
                        host: ssh.clone(),
                        ..style.clone()
                    },
                ),
            };
            hosts.push(WalkHost {
                label,
                side,
                style: host_style,
            });
            continue;
        }
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
    let head = match sum {
        Some(sum) if sections > 1 => {
            let mut head = totals_section(
                &sum,
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
        return Ok((Output::Walk(Walk { hosts, head, style }), 0));
    }
    Ok((Output::Text(format!("{head}{text}")), code))
}

/// Walk through every host's changes, print each host's report with the accepted ones first,
/// then write them once the person confirms: this host's first, then each destination's
/// there, stopping at the first host that fails.
fn walk(walk: Walk) -> Result<(), ScanError> {
    let Walk {
        mut hosts,
        head,
        style,
    } = walk;
    let failed = |_| ScanError::new("cannot use the terminal; no settings were changed");
    let mut stdout = io::stdout().lock();
    let mut stdin = io::stdin().lock();
    let title = hosts.iter().find_map(|h| match &h.side {
        Side::Here { listing, .. } => Some(listing.title()),
        Side::There { .. } => None,
    });
    write!(stdout, "{head}{}", title.unwrap_or_default()).map_err(failed)?;
    let walking: Vec<Walking> = hosts
        .iter()
        .map(|h| Walking {
            label: h.label.as_deref(),
            offers: h.offers(),
            style: &h.style,
        })
        .collect();
    let selections = ask(&walking, &style, &mut stdin, &mut stdout).map_err(failed)?;
    let summaries: Vec<_> = walking
        .iter()
        .zip(&selections)
        .map(|(w, s)| s.summary(w.offers))
        .collect();
    drop(walking);
    // Each host makes its own plan from the answers, and the report reads as its own section
    // after the questions.
    let mut plans = vec![];
    let mut previews = String::new();
    let mut gap = summaries.iter().any(|s| s.answered());
    for (host, selection) in hosts.iter_mut().zip(&selections) {
        let (report, preview) = match &mut host.side {
            Side::Here { owner, listing } => {
                let plan = owner.plan(selection.events())?;
                let report = listing.review(plan.choices(), &plan.total("Accepted"));
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
        let gap = if std::mem::replace(&mut gap, true) {
            "\n"
        } else {
            ""
        };
        write!(stdout, "{gap}{report}").map_err(failed)?;
        previews += &preview;
    }
    stdout.flush().map_err(failed)?;
    let styles: Vec<Style> = hosts.iter().map(|h| h.style.clone()).collect();
    let request = Confirm {
        preview: &previews,
        hosts: summaries.iter().zip(&styles).collect(),
    };
    let yes = confirm(&request, &style, &mut stdin, &mut stdout).map_err(failed)?;
    enter(Phase::Writing);
    let mut written = vec![];
    let mut error = None;
    // Hosts left unwritten after one fails.
    let mut skipped = vec![];
    for ((host, plan), summary) in hosts.into_iter().zip(plans).zip(&summaries) {
        if !yes || !summary.accepted() {
            written.push(vec![]);
            continue;
        }
        // Ctrl-C while writing lets the write under way finish and starts no other.
        if error.is_none() && ssh::cancelled() {
            error = Some(ScanError::new("cancelled while writing"));
        }
        if error.is_some() {
            skipped.push(host.label.unwrap_or_else(|| "this host".into()));
            written.push(vec![]);
            continue;
        }
        let (files, failed) = match (host.side, plan) {
            (Side::Here { .. }, Some(plan)) => match write(&plan) {
                Ok(files) => (files, None),
                Err(e) => (vec![], Some(e)),
            },
            (Side::There { remote, .. }, None) => remote.write(),
            _ => unreachable!("a plan for each host here"),
        };
        written.push(files);
        error = failed;
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
        finish(&done, &style)
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
                        &ScanError::new("doctor failed while applying changes"),
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
