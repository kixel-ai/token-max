//! The command line: arguments, progress, output streams, cancellation and exit codes.

use std::io::{self, IsTerminal, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Mutex, PoisonError, mpsc};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::ser::Formatter;

use crate::discovery::DiscoveryOptions;
use crate::model::{Location, Millis, Report, Verdict};
use crate::prices::{PriceOptions, cancel_downloads, load_prices};
use crate::reader::{ScanOptions, scan};
use crate::replay::{AnalyzeOptions, analyze};
use crate::report::{Host, ReportOptions, format_report};
use crate::safe::{ScanError, display};
use crate::ssh::{self, Remote};
use crate::terminal::{WrapOptions, wrap_text};

const HELP: &str = "token-max: cheaper agent settings from local usage

Usage: token-max [options]

  --since YYYY-MM-DD|ISO  Select from midnight UTC or an instant with timezone
  --claude-dir DIR        Select a Claude profile (repeatable)
  --codex-dir DIR         Select a Codex home, including archived sessions (repeatable)
  --ssh HOST              Audit an SSH destination with the token-max installed there (repeatable)
  --include-local         Also audit this host when --ssh is given
  --max-cost-per-use USD  Recommend turning off a tool, skill or MCP server that cost more than this per use (default: 1)
  --details               Show grouped check evidence, assumptions, uncertainty and sources
  --plain                 Plain text without color or decorative Unicode
  --json                  Complete structured report, schema version 1
  --offline               Use cached public prices without network access
  --prices-file FILE      Use a local models.dev catalog without network access
  --price-cache FILE      Cache path (default: ~/.cache/token-max/models.json)
  -v, --version           Show the package version
  -h, --help              Show this help

Without profile flags, discover local Claude/Codex profiles. Explicit profiles
limit the entire run. Directory flags select local profiles only; with --ssh
they need --include-local. Each SSH destination discovers its own profiles
and needs this same version installed there; only metadata crosses SSH. Current settings are applied to the selected history;
checks can use different windows and baselines. Independent savings do not add.

Examples:
  token-max --since 2026-11-01
  token-max --claude-dir ~/.claude-work --codex-dir ~/.codex --details
  token-max --offline --plain
  token-max --json > report.json
  token-max --ssh devbox --ssh user@laptop --include-local

Public API-price estimates, not subscription bills. Reads transcripts/settings
only; never edits settings or retains/prints prompt, reply or component text.
Color is automatic on terminals; NO_COLOR disables it. Progress goes to stderr
only on interactive terminals, never with --json or redirected output.
SIGINT/SIGTERM cancel with exit 2 and no success report.

Exit 0: no applicable change (advisory findings may remain).
Exit 1: change recommended. Exit 2: scan, pricing, argument or cancellation failure.
";

/// Whether this process has started its single final write. A cancelling signal holds this
/// lock until the process exits, so a report is either written whole or not at all.
static COMMITTED: Mutex<bool> = Mutex::new(false);

/// Claim the final write; blocks forever if a cancelling signal already owns the process.
fn commit() {
    *COMMITTED.lock().unwrap_or_else(PoisonError::into_inner) = true;
}

/// Parsed flags, as `node:util` `parseArgs` would return them in strict mode.
#[derive(Debug, Default)]
struct Values {
    since: Option<String>,
    max_cost_per_use: Option<String>,
    json: bool,
    offline: bool,
    plain: bool,
    details: bool,
    version: bool,
    help: bool,
    claude_dirs: Vec<String>,
    codex_dirs: Vec<String>,
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
            "ssh" => self.ssh.push(value),
            "prices-file" => self.prices_file = Some(value),
            "price-cache" => self.price_cache = Some(value),
            _ => return None,
        }
        Some(())
    }

    fn flag(&mut self, name: &str) -> Option<&mut bool> {
        Some(match name {
            "json" => &mut self.json,
            "offline" => &mut self.offline,
            "plain" => &mut self.plain,
            "details" => &mut self.details,
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
        .to_zoned(jiff::tz::TimeZone::UTC)
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

fn help() -> String {
    let width = width();
    HELP.split('\n')
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
    fn number<W: ?Sized + Write>(writer: &mut W, value: f64) -> io::Result<()> {
        let n = serde_json::Number::from_f64(value).expect("serde_json writes non-finite as null");
        writer.write_all(crate::exposure::number(&n).as_bytes())
    }
}

/// Integers beyond 2^53 print as the nearest double, as JavaScript holds them.
const SAFE: u64 = 1 << 53;

impl Formatter for JsFormatter {
    fn write_f64<W: ?Sized + Write>(&mut self, writer: &mut W, value: f64) -> io::Result<()> {
        Self::number(writer, value)
    }
    fn write_f32<W: ?Sized + Write>(&mut self, writer: &mut W, value: f32) -> io::Result<()> {
        Self::number(writer, value.into())
    }
    fn write_u64<W: ?Sized + Write>(&mut self, writer: &mut W, value: u64) -> io::Result<()> {
        if value > SAFE {
            return Self::number(writer, value as f64);
        }
        write!(writer, "{value}")
    }
    fn write_i64<W: ?Sized + Write>(&mut self, writer: &mut W, value: i64) -> io::Result<()> {
        if value.unsigned_abs() > SAFE {
            return Self::number(writer, value as f64);
        }
        write!(writer, "{value}")
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
    Json(Box<Report>),
    Hosts(Hosts),
    /// Already streamed, as an SSH audit destination does.
    Written,
}

/// The JSON report of a run with `--ssh`: one schema version 1 report per host.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Hosts {
    schema_version: u32,
    hosts: Vec<HostReport>,
}

#[derive(Serialize)]
struct HostReport {
    /// The SSH destination, or null for this host.
    ssh: Option<String>,
    report: Report,
}

fn now() -> Millis {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after the Unix epoch");
    i64::try_from(elapsed.as_millis()).expect("clock within range")
}

/// Run one atomic scan, returning the output and exit code without writing either.
fn run(args: &[String]) -> Result<(Output, u8), ScanError> {
    let values = parse(args).ok_or_else(|| ScanError::new("invalid arguments; use --help"))?;
    if values.help {
        return Ok((Output::Text(help()), 0));
    }
    if values.version {
        return Ok((Output::Text(format!("{}\n", env!("CARGO_PKG_VERSION"))), 0));
    }
    // An empty value is absent, as JavaScript treats a falsy option.
    let since = match values.since.as_deref() {
        None | Some("") => None,
        Some(value) => Some(since(value)?),
    };
    let max_cost_per_use_usd = match values.max_cost_per_use.as_deref() {
        None => 1.0,
        Some(value) => max_cost_per_use(value)?,
    };
    if values.ssh_scan {
        // A destination scans with its own discovery; only the window crosses SSH.
        let other = values.json
            || values.offline
            || values.plain
            || values.details
            || values.include_local
            || values.max_cost_per_use.is_some()
            || values.prices_file.is_some()
            || values.price_cache.is_some()
            || !values.claude_dirs.is_empty()
            || !values.codex_dirs.is_empty()
            || !values.ssh.is_empty();
        if other {
            return Err(ScanError::new("invalid arguments; use --help"));
        }
        return Ok((Output::Written, ssh::serve(since)));
    }
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
    if !local && !(values.claude_dirs.is_empty() && values.codex_dirs.is_empty()) {
        return Err(ScanError::new(
            "--claude-dir and --codex-dir select local profiles; add --include-local to audit them with --ssh",
        ));
    }
    let stdout = io::stdout().is_terminal();
    let term_dumb = env("TERM").as_deref() == Some("dumb");
    let interactive = stdout && io::stderr().is_terminal() && !term_dumb;
    let progress = interactive && !values.json;
    let phase = |message: &str| {
        if progress {
            eprintln!("token-max: {message}...");
        }
    };
    // Every host scans at once; the first failure stops the rest.
    let (sender, results) = mpsc::channel();
    let mut hosts: Vec<Option<String>> = vec![];
    if local {
        let mut discovery = DiscoveryOptions::host()?;
        discovery.claude_dirs = values.claude_dirs;
        discovery.codex_dirs = values.codex_dirs;
        let sender = sender.clone();
        let index = hosts.len();
        hosts.push(None);
        thread::spawn(move || {
            let result = panic::catch_unwind(|| scan(&ScanOptions { discovery, since }))
                .unwrap_or_else(|_| Err(ScanError::new("analysis failed; no report produced")))
                .map(|scan| Remote {
                    scan,
                    home: std::env::home_dir().map(|dir| dir.to_string_lossy().into_owned()),
                });
            let _ = sender.send((index, result));
        });
    }
    for destination in values.ssh {
        let sender = sender.clone();
        let index = hosts.len();
        hosts.push(Some(destination.clone()));
        thread::spawn(move || {
            let result = panic::catch_unwind(|| ssh::audit(&destination, since))
                .unwrap_or_else(|_| Err(ScanError::new("analysis failed; no report produced")));
            let _ = sender.send((index, result));
        });
    }
    drop(sender);
    phase(match (local, remote) {
        (true, false) => "Scanning local histories",
        (true, true) => "Scanning local histories and SSH hosts",
        (false, _) => "Scanning SSH hosts",
    });
    let mut scans: Vec<Option<Remote>> = hosts.iter().map(|_| None).collect();
    let scanned = (|| {
        for (index, result) in results {
            scans[index] = Some(result?);
        }
        phase("Loading public prices");
        let cache = match values.price_cache {
            Some(cache) => PathBuf::from(cache),
            None => {
                let base = match env("XDG_CACHE_HOME") {
                    Some(base) => PathBuf::from(base),
                    None => std::env::home_dir()
                        .ok_or_else(|| ScanError::new("cannot discover home directory"))?
                        .join(".cache"),
                };
                base.join("token-max").join("models.json")
            }
        };
        load_prices(&PriceOptions {
            file: values
                .prices_file
                .filter(|file| !file.is_empty())
                .map(PathBuf::from),
            cache,
            offline: values.offline,
        })
    })();
    let catalog = scanned.inspect_err(|_| ssh::cancel())?;
    phase("Checking settings");
    let now = now();
    let unicode_locale = env_nonempty("LC_ALL")
        .or_else(|| env_nonempty("LC_CTYPE"))
        .or_else(|| env_nonempty("LANG"))
        .is_some_and(|locale| {
            let locale = locale.to_ascii_lowercase();
            locale.contains("utf8") || locale.contains("utf-8")
        });
    let options = ReportOptions {
        width: width(),
        color: stdout && !values.plain && env_nonempty("NO_COLOR").is_none() && !term_dumb,
        unicode: stdout && !values.plain && !term_dumb && unicode_locale,
        details: values.details,
        host: None,
    };
    let sections = hosts.len();
    let mut code = 0;
    let mut text = String::new();
    let mut reports = vec![];
    for (ssh, scan) in hosts.into_iter().zip(scans) {
        let Remote { scan, home } = scan.expect("every host sent a result");
        let report = analyze(AnalyzeOptions {
            scan,
            catalog: &catalog,
            since,
            now,
            max_cost_per_use_usd,
        })?;
        let change = report
            .checks
            .iter()
            .any(|c| c.action() == crate::model::Action::Change)
            || report
                .context
                .iter()
                .any(|c| c.verdict == Some(Verdict::Off));
        code = code.max(u8::from(change));
        if values.json {
            reports.push(HostReport { ssh, report });
            continue;
        }
        let host = Host {
            name: ssh.as_deref().unwrap_or("this host"),
            home: home.as_deref(),
        };
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&format_report(
            &report,
            ReportOptions {
                host: remote.then_some(host),
                ..options
            },
        ));
    }
    if sections > 1 && !values.json {
        text.push_str(
            "\nEach host is analyzed on its own. A history copied between hosts counts on each, so don't add amounts across hosts.\n",
        );
    }
    let output = match (values.json, remote) {
        (false, _) => Output::Text(text),
        (true, false) => Output::Json(Box::new(reports.pop().expect("one host").report)),
        (true, true) => Output::Hosts(Hosts {
            schema_version: 1,
            hosts: reports,
        }),
    };
    Ok((output, code))
}

fn json_output(value: &impl Serialize) -> io::Result<()> {
    panic::catch_unwind(AssertUnwindSafe(|| write_json(io::stdout().lock(), value)))
        .unwrap_or_else(|_| Err(io::Error::other("serialization failed")))
}

/// Run the command line and return its exit code: 0 no change, 1 change recommended, 2 failure.
pub fn main() -> ExitCode {
    // Panic payloads could quote transcript data; failures print only a safe message.
    panic::set_hook(Box::new(|_| {}));
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let json = args.iter().any(|a| a == "--json");
    let handler = ctrlc::set_handler(move || {
        let committed = COMMITTED.lock().unwrap_or_else(PoisonError::into_inner);
        if *committed {
            // The report or diagnostic is already being written whole.
            return;
        }
        // The analysis may be blocked in I/O, so exit now, holding the lock so nothing else
        // is written, after deleting any partial price download it would have cleaned up.
        cancel_downloads();
        ssh::cancel();
        diagnose(json, &ScanError::new("cancelled; no report produced"));
        std::process::exit(2);
    });
    let result = match handler {
        Ok(()) => panic::catch_unwind(AssertUnwindSafe(|| run(&args))),
        Err(_) => Ok(Err(ScanError::new("analysis failed; no report produced"))),
    };
    let result =
        result.unwrap_or_else(|_| Err(ScanError::new("analysis failed; no report produced")));
    commit();
    let written = match result {
        Ok((Output::Text(text), code)) => io::stdout()
            .lock()
            .write_all(text.as_bytes())
            .and_then(|()| io::stdout().lock().flush())
            .map(|()| code),
        Ok((Output::Json(report), code)) => json_output(&*report).map(|()| code),
        Ok((Output::Hosts(hosts), code)) => json_output(&hosts).map(|()| code),
        Ok((Output::Written, code)) => Ok(code),
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
