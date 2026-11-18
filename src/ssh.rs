//! Audits of selected SSH destinations, each run by this build of `token-max` there.
//!
//! The coordinator runs `token-max doctor --ssh-scan` on each destination through the local OpenSSH
//! client, with its configuration, authentication and host keys. The destination reads its own
//! histories and settings, prices them with the coordinator's catalog, and sends back only its
//! printed report or its session lines (see [`crate::wire`]); its scan never leaves it.
//!
//! For a walk through a report's changes, the audit stays open: the destination keeps the
//! settings files it read, and this host sends it the person's answers and their confirm, so
//! each destination writes its own files with the same checks, locks and backups as this host.
//!
//! A destination without this build gets one first, cached there for later runs: a copy of this
//! binary when its platform matches, or else this version's release, checked against its
//! published checksum.

use std::io::{self, BufRead, BufReader, BufWriter, Read};
use std::panic::{self, AssertUnwindSafe};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::{Mutex, OnceLock, PoisonError, mpsc};

use sha2::{Digest, Sha256};

use crate::cli::{Render, Rendered, render};
use crate::discovery::DiscoveryOptions;
use crate::doctor::{Event, Offers, Owner, Style, Written, preview, write};
use crate::model::Millis;
use crate::reader::{ScanOptions, scan};
use crate::safe::{ScanError, iso, terminal_text};
use crate::sessions::{self, export};
use crate::wire::{self, Answer, Header, Job, Outcome, Request, Stream};

/// Reject a destination OpenSSH would read as an option or that cannot be printed whole.
pub fn destination(value: &str) -> Result<(), ScanError> {
    if value.is_empty()
        || value.starts_with('-')
        || value.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(ScanError::new(
            "--ssh requires a destination such as host or user@host",
        ));
    }
    Ok(())
}

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Where this version's release archives are published.
const RELEASES: &str = "https://github.com/kixel-ai/token-max/releases";

/// The command that builds a development build on a destination of another platform.
const BUILD: &str = "`cargo install --locked --git https://github.com/kixel-ai/token-max --rev <commit>` there, from this build's commit";

/// The scripts run on the destination through its login shell, which may not be POSIX: only
/// `sh` reads them, and they hold no single quotes or backslashes. Each starts by naming the
/// destination's platform as Rust's `std::env::consts` does, exiting 3 on any other, and its
/// cache directory, which holds only binaries these scripts put there.
const PLATFORM: &str = r#"case "$(uname -s)" in Linux) os=linux abi=unknown-linux-musl ;; Darwin) os=macos abi=apple-darwin ;; *) exit 3 ;; esac; case "$(uname -m)" in x86_64|amd64) arch=x86_64 ;; aarch64|arm64) arch=aarch64 ;; *) exit 3 ;; esac; cache=${XDG_CACHE_HOME:-$HOME/.cache}/token-max/ssh; "#;

/// Finds this build on the destination and runs it, or says how to put it there. Takes the
/// coordinator's OS, architecture, copy name and version, then token-max's arguments.
///
/// Prints `run` and execs a binary of this build, or prints `copy` or `download <target>` and
/// exits. On the coordinator's own platform only its copy runs, since every build from a
/// commit after a release reports that release's version. On another platform nothing else
/// can, so a binary of this version stands in, checked by the stream's header: this version's
/// cached release, else the first installed `token-max` reporting it.
///
/// Installed ones are searched on `PATH`, then in the release installers' directory, the first
/// of `$XDG_BIN_HOME`, `$XDG_DATA_HOME/../bin` and `~/.local/bin`, then in `cargo install`'s
/// `~/.cargo/bin`; non-interactive shells often leave these off `PATH`. An empty `PATH` entry
/// is the current directory, as POSIX has it.
const RUN: &str = r#"if [ "$os $arch" = "$1 $2" ]; then bin=$cache/$3; [ -x "$bin" ] || { echo copy; exit 0; }; else bin=$cache/token-max-v$4-$arch-$abi; if [ ! -x "$bin" ]; then bin=; dirs=$PATH:${XDG_BIN_HOME:-${XDG_DATA_HOME:-$HOME/.local/share}/../bin}:$HOME/.local/bin:${CARGO_HOME:-$HOME/.cargo}/bin; set -f; IFS=:; for dir in $dirs; do dir=${dir:-.}; if [ -x "$dir/token-max" ] && [ "$("$dir/token-max" --version 2>/dev/null)" = "$4" ]; then bin=$dir/token-max; break; fi; done; unset IFS; set +f; [ -n "$bin" ] || { echo download $arch-$abi; exit 0; }; fi; fi; echo run; shift 4; exec "$bin" "$@""#;

/// Picks a SHA-256 command, since Linux and macOS ship different ones.
const SUM: &str = r#"if command -v sha256sum >/dev/null; then sum=sha256sum; elif command -v shasum >/dev/null; then sum="shasum -a 256"; else exit 5; fi; mkdir -p "$cache" || exit 1; "#;

/// Caches the binary on stdin under the name `$1` once its SHA-256 is `$2` and it runs there.
/// Each step writes beside the cache so the final move is an atomic rename, and then removes
/// every other binary there, so repeated builds don't pile up.
const COPY: &str = r#"tmp=$(mktemp "$cache/.copy.XXXXXX") || exit 1; clean() { rm -f "$tmp"; }; trap clean EXIT; cat >"$tmp" || exit 1; actual=$($sum <"$tmp") || exit 1; [ "${actual%% *}" = "$2" ] || exit 6; chmod 755 "$tmp" && "$tmp" --version >/dev/null 2>&1 || exit 4; mv -f "$tmp" "$cache/$1" || exit 1; for old in "$cache"/token-max-*; do [ "$old" = "$cache/$1" ] || rm -f "$old"; done"#;

/// Caches release `$1` from the releases at `$2` for the destination's platform, as the run
/// script names it, once its archive matches the published checksum and it runs there.
const DOWNLOAD: &str = r#"for tool in curl tar; do command -v $tool >/dev/null || exit 7; done; tmp=$(mktemp -d "$cache/.download.XXXXXX") || exit 1; clean() { rm -rf "$tmp"; }; trap clean EXIT; target=$arch-$abi; archive=token-max-$target.tar.xz; curl -LsSf -o "$tmp/$archive" "$2/download/v$1/$archive" && curl -LsSf -o "$tmp/sum" "$2/download/v$1/$archive.sha256" || exit 8; read -r expected rest <"$tmp/sum"; actual=$($sum <"$tmp/$archive") || exit 1; [ -n "$expected" ] && [ "${actual%% *}" = "$expected" ] || exit 6; tar -xf "$tmp/$archive" -C "$tmp" || exit 9; bin=$tmp/token-max-$target/token-max; [ "$("$bin" --version 2>/dev/null)" = "$1" ] || exit 4; mv -f "$bin" "$cache/token-max-v$1-$target" || exit 1; for old in "$cache"/token-max-*; do [ "$old" = "$cache/token-max-v$1-$target" ] || rm -f "$old"; done"#;

/// Exit statuses the scripts and OpenSSH report.
const UNSUPPORTED: i32 = 3;
const DOES_NOT_RUN: i32 = 4;
const NO_CHECKSUM: i32 = 5;
const MISMATCH: i32 = 6;
const NO_DOWNLOADER: i32 = 7;
const NOT_DOWNLOADED: i32 = 8;
const NOT_UNPACKED: i32 = 9;
const SSH_FAILED: i32 = 255;

/// The release targets the run script names.
const TARGETS: [&str; 4] = [
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
];

/// This running binary as the run script caches it.
struct Binary {
    /// Its cache name, from its digest, so another build never runs in its place.
    name: String,
    sha256: String,
}

fn binary() -> Result<&'static Binary, ScanError> {
    static BINARY: OnceLock<Result<Binary, String>> = OnceLock::new();
    BINARY
        .get_or_init(|| {
            let bytes = std::env::current_exe()
                .and_then(std::fs::read)
                .map_err(|_| "cannot read this token-max binary to copy it".to_string())?;
            let sha256: String = Sha256::digest(&bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            Ok(Binary {
                name: format!("token-max-{}", &sha256[..16]),
                sha256,
            })
        })
        .as_ref()
        .map_err(|message| ScanError::new(message.clone()))
}

/// `script` run by the destination's `sh` with `args`, each already free of single quotes.
fn remote(script: &str, args: &[&str]) -> String {
    let mut command = format!("sh -c '{PLATFORM}{script}' sh");
    for arg in args {
        debug_assert!(!arg.contains('\''));
        command.push_str(&format!(" '{arg}'"));
    }
    command
}

fn run_command(binary: &Binary, since: Option<Millis>, defaults: bool) -> String {
    let since = since.map(iso);
    let mut args = vec![
        std::env::consts::OS,
        std::env::consts::ARCH,
        &binary.name,
        VERSION,
        "doctor",
        "--ssh-scan",
    ];
    if let Some(since) = &since {
        args.extend(["--since", since]);
    }
    if defaults {
        args.push("--defaults");
    }
    remote(RUN, &args)
}

/// Children still running, so cancellation can stop them; once cancelled, none may start.
struct Children {
    cancelled: bool,
    running: Vec<(u64, Child)>,
    next: u64,
}

static CHILDREN: Mutex<Children> = Mutex::new(Children {
    cancelled: false,
    running: Vec::new(),
    next: 0,
});

fn children() -> std::sync::MutexGuard<'static, Children> {
    CHILDREN.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether [`cancel`] ran.
pub fn cancelled() -> bool {
    children().cancelled
}

/// Stop and reap every SSH audit, and refuse to start more.
pub fn cancel() {
    let mut children = children();
    children.cancelled = true;
    for (_, child) in &mut children.running {
        let _ = child.kill();
        let _ = child.wait();
    }
    children.running.clear();
}

/// What an audit waits for after the header, in whichever order it comes.
enum Step {
    /// This host's request, once it has prices.
    Request(Request),
    /// This host failed before sending a request.
    Cancelled,
    /// The destination's outcome, which may come before the request when it fails, and the
    /// stream it came on, which a walk goes on reading.
    Outcome(
        Result<Outcome, serde_json::Error>,
        Box<Stream<BufReader<ChildStdout>>>,
    ),
}

/// A running audit; dropping it kills the child unless [`Session::stop`] already reaped it.
struct Session {
    destination: String,
    id: u64,
    // The destination stops scanning when this closes, such as when this process exits.
    stdin: Option<ChildStdin>,
}

impl Session {
    /// Stop the child if it is still registered, and return how it exited.
    fn stop(&mut self, kill: bool) -> Option<ExitStatus> {
        let mut child = {
            let mut children = children();
            let index = children.running.iter().position(|(id, _)| *id == self.id)?;
            children.running.swap_remove(index).1
        };
        if kill {
            let _ = child.kill();
        }
        let status = child.wait().ok();
        // Closed only now: the destination treats a closed stdin as cancellation.
        self.stdin = None;
        status
    }

    fn fail(&mut self, kill: bool, message: impl FnOnce(Option<i32>) -> String) -> ScanError {
        let status = self.stop(kill).and_then(|s| s.code());
        ScanError::new(format!("{}: {}", self.destination, message(status)))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop(true);
    }
}

/// What a destination sent back for its [`Job`].
pub enum Reply {
    Report(Rendered),
    /// A report for a walk, with the changes the destination offers and its open audit.
    Walk {
        rendered: Rendered,
        offers: Box<Offers>,
        remote: Remote,
    },
    /// Its sessions, ordered by first request, each without a host.
    Sessions(Vec<sessions::Session>),
}

/// Start `command` on `destination` with `stdin`, registered so cancellation can stop it.
fn spawn(
    destination: &str,
    command: String,
    stdin: Stdio,
) -> Result<(Session, BufReader<ChildStdout>), ScanError> {
    let mut ssh = Command::new("ssh");
    ssh.args(["-T", "-x", "-a"])
        .args(["-o", "BatchMode=yes"])
        .args(["-o", "ClearAllForwardings=yes"])
        .args(["-o", "PermitLocalCommand=no"])
        .args(["-o", "RemoteCommand=none"])
        // The destination reads a closed stdin as cancellation, so keep it open.
        .args(["-o", "StdinNull=no"])
        .args(["-o", "ForkAfterAuthentication=no"])
        // A persisted control master would hold the stream open after the audit ends.
        .args(["-o", "ControlPersist=no"])
        .arg("--")
        .arg(destination)
        .arg(command)
        .stdin(stdin)
        .stdout(Stdio::piped())
        // Login scripts may print anything; failures are reported from exit statuses.
        .stderr(Stdio::null());
    // In its own process group, so the terminal's Ctrl-C reaches only this process, which
    // decides what each client stops: every one before the confirm, and after it only a
    // destination's write it stops waiting for. Batch mode never reads the terminal.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut ssh, 0);
    let mut children = children();
    if children.cancelled {
        return Err(ScanError::new("cancelled; no report produced"));
    }
    let mut child = ssh
        .spawn()
        .map_err(|_| ScanError::new("cannot run ssh; SSH audits need an OpenSSH client"))?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().expect("piped stdout");
    children.next += 1;
    let id = children.next;
    children.running.push((id, child));
    Ok((
        Session {
            destination: destination.to_string(),
            id,
            stdin,
        },
        BufReader::new(stdout),
    ))
}

/// How the run script answered.
enum Found {
    Run,
    Copy,
    Download(&'static str),
}

/// Read the run script's first line, which is short, so a destination cannot make this host
/// hold more. An error says whether stdout was empty, as it is when `ssh` or the script failed.
fn found(stdout: &mut BufReader<ChildStdout>) -> Result<Found, bool> {
    let mut line = String::new();
    match stdout.take(64).read_line(&mut line) {
        Ok(0) => return Err(true),
        Ok(_) => {}
        Err(_) => return Err(false),
    }
    let found = match line.strip_suffix('\n') {
        Some("run") => Some(Found::Run),
        Some("copy") => Some(Found::Copy),
        Some(other) => other.strip_prefix("download ").and_then(|target| {
            TARGETS
                .iter()
                .find(|t| **t == target)
                .map(|t| Found::Download(t))
        }),
        None => None,
    };
    found.ok_or(false)
}

fn connection_failed(destination: &str) -> String {
    format!("SSH connection failed; check that `ssh {destination} true` works without a prompt")
}

/// Put this build on `destination`, by copying this binary or downloading its release there.
fn provision(destination: &str, found: &Found) -> Result<(), ScanError> {
    let (command, stdin) = match found {
        Found::Copy => {
            let binary = binary()?;
            let file = std::env::current_exe()
                .and_then(std::fs::File::open)
                .map_err(|_| ScanError::new("cannot read this token-max binary to copy it"))?;
            (
                remote(&format!("{SUM}{COPY}"), &[&binary.name, &binary.sha256]),
                Stdio::from(file),
            )
        }
        Found::Download(_) => (
            remote(&format!("{SUM}{DOWNLOAD}"), &[VERSION, RELEASES]),
            Stdio::null(),
        ),
        Found::Run => return Ok(()),
    };
    let (mut session, mut stdout) = spawn(destination, command, stdin)?;
    // Reading to the end, rather than waiting, keeps the child registered so cancellation can
    // stop it; nothing it prints is kept.
    let _ = io::copy(&mut stdout, &mut io::sink());
    let status = session.stop(false).and_then(|s| s.code());
    let what = match found {
        Found::Download(target) => format!("the token-max {VERSION} release for {target}"),
        _ => "this token-max".to_string(),
    };
    let message = match status {
        Some(0) => return Ok(()),
        Some(SSH_FAILED) => connection_failed(destination),
        Some(DOES_NOT_RUN) => match found {
            // Most often a Linux build linked against this host's C library.
            Found::Copy if cfg!(target_os = "linux") => format!(
                "a copy of this token-max does not run there; run a statically linked build here, such as the release installers' or one for {}-unknown-linux-musl",
                std::env::consts::ARCH
            ),
            Found::Copy => "a copy of this token-max does not run there".to_string(),
            _ => format!("{what} does not run there"),
        },
        Some(NO_CHECKSUM) => format!("needs sha256sum or shasum to check {what}"),
        Some(MISMATCH) => match found {
            Found::Copy => "the copy of this token-max arrived damaged".to_string(),
            _ => format!("{what} does not match its published checksum"),
        },
        Some(NO_DOWNLOADER) => format!("needs curl and tar to download {what}"),
        Some(NOT_DOWNLOADED) => format!("could not download {what} from {RELEASES}"),
        Some(NOT_UNPACKED) => format!("could not unpack {what}; tar there needs xz"),
        status => format!("could not put {what} there ({})", exit(status)),
    };
    Err(ScanError::new(format!("{destination}: {message}")))
}

/// Audit one destination with this build of `token-max`, putting it there first if needed,
/// and failing on any error there.
///
/// The destination scans while this host loads prices; once `request` delivers the catalog and
/// job, it analyzes and sends back what the job asks for. A destination that fails first is
/// reported at once, without waiting for the request.
pub fn audit(
    destination: &str,
    since: Option<Millis>,
    defaults: bool,
    request: mpsc::Receiver<Request>,
) -> Result<Reply, ScanError> {
    let command = run_command(binary()?, since, defaults);
    let mut provisioned = false;
    let (mut session, stdout) = loop {
        let (mut session, mut stdout) = spawn(destination, command.clone(), Stdio::piped())?;
        let found = found(&mut stdout);
        let found = match found {
            Ok(Found::Run) => break (session, stdout),
            // Only once: a binary just put there that the script still cannot find means the
            // cache does not keep it.
            Ok(found) if !provisioned => found,
            Ok(_) => {
                return Err(session.fail(true, |_| {
                    "token-max was put in its cache there but is still missing; another build may have replaced it, so run again".into()
                }));
            }
            // Anything but an empty stdout is not the script, which may never end.
            Err(empty) => {
                return Err(session.fail(!empty, |status| match status {
                    Some(SSH_FAILED) => connection_failed(destination),
                    Some(UNSUPPORTED) => {
                        "unsupported platform; SSH audits support Linux and macOS destinations on x86_64 or arm64".into()
                    }
                    status => format!("could not look for token-max there ({})", exit(status)),
                }));
            }
        };
        session.stop(true);
        provision(destination, &found)?;
        provisioned = true;
    };
    let mut stream = Stream::new(stdout);
    let header = match stream.header() {
        Ok(header) => header,
        Err(error) => {
            // Anything but an empty stdout is not a token-max stream, which may never end.
            return Err(session.fail(!error.is_eof(), |status| match status {
                Some(SSH_FAILED) => connection_failed(destination),
                status => format!(
                    "token-max there did not start an SSH audit ({})",
                    exit(status)
                ),
            }));
        }
    };
    let local = wire::build();
    if header.token_max_scan != local {
        let remote = terminal_text(&header.token_max_scan);
        return Err(session.fail(true, |_| {
            format!(
                "the token-max {VERSION} there is build {remote}, not this build {local}; run a released token-max here, or build this one there with {BUILD}"
            )
        }));
    }
    // The destination may fail before this host has prices to send, so its outcome is read
    // while the request is still pending.
    let (steps, arrived) = mpsc::channel();
    let forward = steps.clone();
    std::thread::spawn(move || {
        // Dropped without a request when this host fails first, and then stops every destination.
        let _ = forward.send(request.recv().map_or(Step::Cancelled, Step::Request));
    });
    std::thread::spawn(move || {
        let outcome = stream.value();
        let _ = steps.send(Step::Outcome(outcome, Box::new(stream)));
    });
    // What the job asked for, once it is sent: sessions, or a report and whether it is walked.
    let mut requested = None;
    let (outcome, mut stream) = loop {
        match arrived.recv() {
            Ok(Step::Request(request)) => {
                requested = Some(match request.job {
                    Job::Sessions => None,
                    Job::Report { walk, .. } => Some(walk),
                });
                if let Some(stdin) = session.stdin.as_mut() {
                    // A destination that already failed has stopped reading; its outcome says why.
                    let _ = wire::write(&mut BufWriter::new(stdin), &request);
                }
            }
            Ok(Step::Outcome(outcome, stream)) => break (outcome, stream),
            Ok(Step::Cancelled) | Err(_) => {
                return Err(ScanError::new("cancelled; no report produced"));
            }
        }
    };
    let invalid = |session: &mut Session, error: serde_json::Error| {
        let (line, column) = (error.line(), error.column());
        session.fail(true, |status| match status {
            Some(SSH_FAILED) => "SSH connection lost during the audit".into(),
            _ => format!(
                "invalid SSH audit stream at line {line}, column {column} ({})",
                exit(status)
            ),
        })
    };
    let (scanned, reply, offers) = match outcome {
        Ok(Outcome::Report {
            since,
            text,
            change,
            totals,
            offers,
        }) => (
            since,
            Reply::Report(Rendered {
                text,
                change,
                totals: Box::new(totals),
                fixes: vec![],
                listing: None,
                turned_off: vec![],
            }),
            offers,
        ),
        Ok(Outcome::Sessions { since, sessions }) => (since, Reply::Sessions(sessions), None),
        Ok(Outcome::Reviewed { .. } | Outcome::Wrote { .. }) => {
            return Err(session.fail(true, |_| {
                "token-max sent a different result than requested".into()
            }));
        }
        Ok(Outcome::Error { message, location }) => {
            session.stop(true);
            return Err(ScanError {
                message: format!("{destination}: {}", terminal_text(&message)),
                location,
            });
        }
        Err(error) => return Err(invalid(&mut session, error)),
    };
    if scanned != since {
        return Err(session.fail(true, |_| {
            "token-max scanned a different window than requested".into()
        }));
    }
    match (requested, &reply, offers) {
        (None, _, _) => {
            return Err(session.fail(true, |_| {
                "token-max sent its result before its request".into()
            }));
        }
        (Some(Some(true)), Reply::Report(_), Some(offers)) => {
            if let Err(error) = offers.check() {
                return Err(session.fail(true, |_| format!("token-max there {error}")));
            }
            let Reply::Report(rendered) = reply else {
                unreachable!("matched a report")
            };
            return Ok(Reply::Walk {
                rendered,
                offers,
                remote: Remote { session, stream },
            });
        }
        (Some(None), Reply::Sessions(_), None) | (Some(Some(false)), Reply::Report(_), None) => {}
        _ => {
            return Err(session.fail(true, |_| {
                "token-max sent a different result than requested".into()
            }));
        }
    }
    if let Err(error) = stream.end() {
        return Err(invalid(&mut session, error));
    }
    let status = session.stop(false).and_then(|s| s.code());
    if status != Some(0) {
        return Err(ScanError::new(format!(
            "{destination}: token-max failed after sending its result ({})",
            exit(status)
        )));
    }
    Ok(reply)
}

/// A destination's audit kept open for this host's walk through its changes.
pub struct Remote {
    session: Session,
    stream: Box<Stream<BufReader<ChildStdout>>>,
}

impl Remote {
    /// Send `answer` and read the destination's reply to it, failing with what `doing` was
    /// cut short when the stream breaks.
    fn exchange(&mut self, answer: &Answer, doing: &str) -> Result<Outcome, ScanError> {
        let sent = self
            .session
            .stdin
            .as_mut()
            .map(|stdin| wire::write(&mut BufWriter::new(stdin), answer));
        let outcome = match sent {
            Some(Ok(())) => self.stream.value(),
            _ => {
                return Err(self
                    .session
                    .fail(true, |_| format!("SSH connection lost {doing}")));
            }
        };
        match outcome {
            Ok(Outcome::Error { message, location }) => {
                self.session.stop(true);
                Err(ScanError {
                    message: format!("{}: {}", self.session.destination, terminal_text(&message)),
                    location,
                })
            }
            Ok(outcome) => Ok(outcome),
            // Cancelled from the terminal, which stopped this client.
            Err(_) if cancelled() => Err(self.session.fail(true, |_| format!("cancelled {doing}"))),
            Err(error) if error.is_eof() || error.is_io() => Err(self
                .session
                .fail(true, |_| format!("SSH connection lost {doing}"))),
            Err(error) => {
                let (line, column) = (error.line(), error.column());
                Err(self.session.fail(true, |status| {
                    format!(
                        "invalid SSH audit stream at line {line}, column {column} ({}) {doing}",
                        exit(status)
                    )
                }))
            }
        }
    }

    /// The destination's report with the walk's `events` marked on it, and its preview of
    /// each file the accepted changes would write; nothing is written there yet.
    pub fn review(&mut self, events: &[Event]) -> Result<(String, String), ScanError> {
        let answer = Answer::Review {
            events: events.to_vec(),
        };
        match self.exchange(&answer, "during the walk; no settings were changed there")? {
            Outcome::Reviewed { report, preview } => Ok((report, preview)),
            _ => Err(self.session.fail(true, |_| {
                "token-max sent a different result than requested".into()
            })),
        }
    }

    /// Write the reviewed changes there, returning each file it reported written, and how the
    /// audit failed, before or after that report.
    pub fn write(mut self) -> (Vec<Written>, Option<ScanError>) {
        let doing = "while it was writing, so it may have changed its settings files; check them and their .token-max.bak backups";
        let written = match self.exchange(&Answer::Write, doing) {
            Ok(Outcome::Wrote { written }) => written,
            Ok(_) => {
                let error = self.session.fail(true, |_| {
                    format!("token-max sent a different result than requested {doing}")
                });
                return (vec![], Some(error));
            }
            Err(error) => return (vec![], Some(error)),
        };
        // Its report stands: the files it names were written, whatever follows.
        let error = if self.stream.end().is_err() {
            Some(self.session.fail(true, |_| {
                "token-max sent more than its result after writing".into()
            }))
        } else {
            match self.session.stop(false).and_then(|s| s.code()) {
                Some(0) => None,
                // Ctrl-C stopped the client after its report and clean end, which already
                // confirm the write; the signal cut nothing short.
                None if cancelled() => None,
                status => Some(ScanError::new(format!(
                    "{}: token-max failed after writing ({})",
                    self.session.destination,
                    exit(status)
                ))),
            }
        };
        (written, error)
    }
}

fn exit(status: Option<i32>) -> String {
    match status {
        Some(code) => format!("exit {code}"),
        None => "terminated".into(),
    }
}

/// What reaches a destination on stdin.
enum Incoming {
    Request(Request),
    Answer(Answer),
}

/// A destination's side of a walk: its files as it read them, its report, and how to print.
struct Walked {
    owner: Owner,
    listing: Box<crate::report::Listing>,
    style: Style,
}

/// Run as an SSH audit destination: scan this host, stream what the coordinator's job asks for
/// to stdout and return the exit code. For a walk, then answer the coordinator's review and
/// write the changes it confirms.
///
/// Diagnostics travel in the stream, since the coordinator discards this host's stderr. The
/// coordinator's request arrives on stdin while this host scans, and the audit stops when stdin
/// closes, which happens when the coordinator or its connection ends, unless it already asked
/// this host to write.
pub fn serve(since: Option<Millis>, defaults: bool) -> u8 {
    let (sender, incoming) = mpsc::channel();
    std::thread::spawn(move || {
        let mut stdin = BufReader::new(io::stdin());
        let mut stream = Stream::new(&mut stdin);
        let Ok(request) = stream.request() else {
            // Not this build's coordinator, or it is gone.
            std::process::exit(2);
        };
        let walk = matches!(request.job, Job::Report { walk: true, .. });
        let _ = sender.send(Incoming::Request(request));
        if walk {
            // Ends with the write, or exits once stdin closes.
            loop {
                let Ok(answer) = stream.value::<Answer>() else {
                    std::process::exit(2);
                };
                // Once asked to write, the write finishes even if the coordinator goes.
                let write = matches!(answer, Answer::Write);
                let _ = sender.send(Incoming::Answer(answer));
                if write {
                    return;
                }
            }
        }
        drop(stream);
        let _ = io::copy(&mut stdin, &mut io::sink());
        std::process::exit(2);
    });
    let mut stdout = BufWriter::with_capacity(1 << 16, io::stdout().lock());
    let header = Header {
        token_max_scan: wire::build(),
    };
    if wire::write(&mut stdout, &header).is_err() {
        return 2;
    }
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let mut discovery = DiscoveryOptions::host()?;
        if defaults {
            discovery.pretend_defaults();
        }
        let scan = scan(&ScanOptions { discovery, since })?;
        // The stdin thread keeps the sender until it exits this process.
        let Ok(Incoming::Request(request)) = incoming.recv() else {
            unreachable!("the request comes first")
        };
        let (max_cost_per_use_usd, format, walk) = match &request.job {
            Job::Report {
                max_cost_per_use_usd,
                format,
                walk,
            } => (*max_cost_per_use_usd, format, *walk),
            Job::Sessions => {
                let sessions = export(&scan, &request.catalog)?;
                return Ok((Outcome::Sessions { since, sessions }, None));
            }
        };
        let home = std::env::home_dir().map(|dir| dir.to_string_lossy().into_owned());
        let Rendered {
            text,
            change,
            totals,
            fixes,
            listing,
            turned_off,
        } = render(
            scan,
            Render {
                catalog: &request.catalog,
                since,
                now: crate::reader::now(),
                max_cost_per_use_usd,
                format,
                home: home.as_deref(),
            },
        )?;
        let walked = walk.then(|| Walked {
            owner: Owner::new(fixes, turned_off, home.clone(), totals.monthly),
            listing: listing.expect("this host's report has its listing"),
            style: Style {
                color: format.color,
                unicode: format.unicode,
                width: format.width,
                home,
                host: format.host.clone(),
            },
        });
        let offers = walked.as_ref().map(|w| Box::new(w.owner.offers().clone()));
        let outcome = Outcome::Report {
            since,
            text,
            change,
            totals: *totals,
            offers,
        };
        Ok((outcome, walked))
    }))
    .unwrap_or_else(|_| Err(ScanError::new("analysis failed; no report produced")));
    let (outcome, walked) = match result {
        Ok((outcome, walked)) => (outcome, walked),
        Err(error) => (failure(error), None),
    };
    let failed = matches!(outcome, Outcome::Error { .. });
    if wire::write(&mut stdout, &outcome).is_err() || failed {
        return 2;
    }
    match walked {
        Some(walked) => walk_here(&walked, &incoming, &mut stdout),
        None => 0,
    }
}

/// An error as the stream carries it.
fn failure(error: ScanError) -> Outcome {
    Outcome::Error {
        message: error.message,
        location: error.location,
    }
}

/// Answer the coordinator's review of this host's changes, then write them once it confirms.
fn walk_here(
    walked: &Walked,
    incoming: &mpsc::Receiver<Incoming>,
    stdout: &mut impl io::Write,
) -> u8 {
    let reviewed = panic::catch_unwind(AssertUnwindSafe(|| {
        let Ok(Incoming::Answer(Answer::Review { events })) = incoming.recv() else {
            return Err(ScanError::new(
                "the walk sent no answers; no settings were changed",
            ));
        };
        let plan = walked.owner.plan(&events)?;
        // Under its own header, which this host prints only for its own report.
        let listing = &walked.listing;
        let report = format!(
            "{}{}",
            listing.title(),
            listing.review(plan.choices(), &plan.total("Accepted"))
        );
        let preview = preview(&plan, &walked.style);
        Ok((plan, Outcome::Reviewed { report, preview }))
    }))
    .unwrap_or_else(|_| Err(ScanError::new("doctor failed while reviewing changes")));
    let plan = match reviewed {
        Ok((plan, outcome)) => {
            if wire::write(stdout, &outcome).is_err() {
                return 2;
            }
            plan
        }
        Err(error) => {
            let _ = wire::write(stdout, &failure(error));
            return 2;
        }
    };
    // A coordinator that does not confirm closes stdin, which exits this process.
    let Ok(Incoming::Answer(Answer::Write)) = incoming.recv() else {
        let error = ScanError::new("the walk sent another answer than a confirm");
        let _ = wire::write(stdout, &failure(error));
        return 2;
    };
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| write(&plan)))
        .unwrap_or_else(|_| Err(ScanError::new("doctor failed while applying changes")))
        .map_or_else(failure, |written| Outcome::Wrote { written });
    let failed = matches!(outcome, Outcome::Error { .. });
    match wire::write(stdout, &outcome) {
        Ok(()) if !failed => 0,
        _ => 2,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use sha2::{Digest, Sha256};

    use super::{COPY, DOWNLOAD, PLATFORM, RUN, SUM};
    use crate::test_support::Workspace;

    /// This host's release target, as the run script names it.
    fn target() -> String {
        let abi = match std::env::consts::OS {
            "linux" => "unknown-linux-musl",
            _ => "apple-darwin",
        };
        format!("{}-{abi}", std::env::consts::ARCH)
    }

    fn script(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A stand-in release binary of `version`.
    fn release(path: &Path, version: &str) {
        script(
            path,
            &format!(r#"[ "$1" = --version ] && echo {version} && exit; echo "ran $*""#),
        );
    }

    /// A `PATH` holding only the commands the scripts run, linked from this one wherever they
    /// are, such as NixOS's store, so an installed `token-max` can't stand in for a test's. Its
    /// leading empty entry is the current directory.
    fn tools(home: &Path) -> String {
        let tools = home.join("tools");
        if !tools.exists() {
            std::fs::create_dir(&tools).unwrap();
            let path = std::env::var_os("PATH").unwrap();
            // Linux ships sha256sum and macOS shasum; either checks a binary.
            let sums = ["sha256sum", "shasum"];
            for tool in [
                "sh", "uname", "cat", "mkdir", "mktemp", "chmod", "mv", "rm", "curl", "tar", "xz",
            ]
            .into_iter()
            .chain(sums)
            {
                let Some(found) = std::env::split_paths(&path)
                    .map(|dir| dir.join(tool))
                    .find(|file| file.is_file())
                else {
                    // macOS's tar unpacks xz itself.
                    assert!(
                        sums.contains(&tool) || tool == "xz",
                        "{tool} is not on PATH"
                    );
                    continue;
                };
                let found = std::env::current_dir().unwrap().join(found);
                std::os::unix::fs::symlink(found, tools.join(tool)).unwrap();
            }
        }
        format!(":{}", tools.display())
    }

    /// `script` as a destination's `sh` runs it, with `home` and only [`tools`].
    fn command(home: &Path, script: &str, args: &[&str]) -> Command {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(format!("{PLATFORM}{script}"))
            .arg("sh")
            .args(args)
            .env_clear()
            .env("HOME", home)
            .env("PATH", tools(home))
            .current_dir(home);
        command
    }

    /// Run `script` as a destination's `sh` does, returning its status and stdout.
    fn sh(home: &Path, script: &str, args: &[&str]) -> (Option<i32>, String) {
        let output = command(home, script, args).output().unwrap();
        (
            output.status.code(),
            String::from_utf8(output.stdout).unwrap(),
        )
    }

    fn cache(home: &Path) -> PathBuf {
        home.join(".cache/token-max/ssh")
    }

    fn cached(home: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(cache(home))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn scripts_survive_single_quoting() {
        for script in [PLATFORM, RUN, SUM, COPY, DOWNLOAD] {
            assert!(!script.contains(['\'', '\\']), "{script}");
        }
    }

    #[test]
    fn the_run_script_picks_this_build_or_says_how_to_get_it() {
        let workspace = Workspace::new();
        let home = &workspace.home;
        let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
        let run = |os: &str, version: &str| {
            sh(home, RUN, &[os, arch, "token-max-copy", version, "doctor"])
        };
        let download = format!("download {}\n", target());
        // Nothing there yet.
        assert_eq!(run(os, "0.0.1"), (Some(0), "copy\n".into()));
        assert_eq!(run("plan9", "0.0.1"), (Some(0), download.clone()));
        // Only on another platform does an installed binary of this version stand in for
        // this build, the first that reports it, even past one that does not.
        let installed = home.join(".cargo/bin/token-max");
        release(&installed, "0.0.1");
        release(&home.join(".local/bin/token-max"), "0.0.2");
        assert_eq!(run(os, "0.0.1"), (Some(0), "copy\n".into()));
        assert_eq!(run("plan9", "0.0.1"), (Some(0), "run\nran doctor\n".into()));
        assert_eq!(run("plan9", "0.0.2"), (Some(0), "run\nran doctor\n".into()));
        assert_eq!(run("plan9", "0.0.3"), (Some(0), download.clone()));
        release(&installed, "0.0.0");
        assert_eq!(run(os, "0.0.0"), (Some(0), "copy\n".into()));
        assert_eq!(run("plan9", "0.0.0"), (Some(0), "run\nran doctor\n".into()));
        std::fs::remove_file(&installed).unwrap();
        std::fs::remove_file(home.join(".local/bin/token-max")).unwrap();
        // The empty entry in this `PATH` is the current directory, the home here.
        let current = home.join("token-max");
        release(&current, "0.0.1");
        assert_eq!(run("plan9", "0.0.1"), (Some(0), "run\nran doctor\n".into()));
        std::fs::remove_file(&current).unwrap();
        // Cached binaries: the copy on this platform, the release on another.
        script(&cache(home).join("token-max-copy"), r#"echo "copy $*""#);
        release(
            &cache(home).join(format!("token-max-v0.0.1-{}", target())),
            "0.0.1",
        );
        assert_eq!(run(os, "0.0.1"), (Some(0), "run\ncopy doctor\n".into()));
        assert_eq!(run("plan9", "0.0.1"), (Some(0), "run\nran doctor\n".into()));
        assert_eq!(run("plan9", "0.0.2"), (Some(0), download));
    }

    /// Releases of `version` for this host under a `file:` URL, with a checksum that lies when
    /// `corrupt`.
    fn publish(root: &Path, version: &str, corrupt: bool) -> String {
        let folder = format!("token-max-{}", target());
        let stage = root.join("stage");
        release(&stage.join(&folder).join("token-max"), version);
        let dir = root.join(format!("releases/download/v{version}"));
        std::fs::create_dir_all(&dir).unwrap();
        let archive = dir.join(format!("{folder}.tar.xz"));
        let status = Command::new("tar")
            .arg("-cJf")
            .arg(&archive)
            .arg("-C")
            .arg(&stage)
            .arg(&folder)
            .status()
            .unwrap();
        assert!(status.success());
        let mut hash: String = Sha256::digest(std::fs::read(&archive).unwrap())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if corrupt {
            hash = hash.replace(|c: char| c != '0', "0");
        }
        let name = archive.file_name().unwrap().to_str().unwrap();
        std::fs::write(
            dir.join(format!("{name}.sha256")),
            format!("{hash}  {name}\n"),
        )
        .unwrap();
        format!("file://{}", root.join("releases").display())
    }

    #[test]
    fn the_download_script_caches_only_a_checked_release() {
        let workspace = Workspace::new();
        let home = &workspace.home;
        let releases = publish(&home.join("good"), "0.0.1", false);
        let corrupt = publish(&home.join("bad"), "0.0.2", true);
        let stale = cache(home).join("token-max-0000000000000000");
        script(&stale, "exit 2");
        let download = format!("{SUM}{DOWNLOAD}");
        assert_eq!(sh(home, &download, &["0.0.2", &corrupt]).0, Some(6));
        assert_eq!(sh(home, &download, &["0.0.3", &releases]).0, Some(8));
        // A release that names another version is not this one.
        let renamed = home.join("good/releases/download/v0.0.4");
        std::fs::rename(home.join("good/releases/download/v0.0.1"), &renamed).unwrap();
        assert_eq!(sh(home, &download, &["0.0.4", &releases]).0, Some(4));
        std::fs::rename(&renamed, home.join("good/releases/download/v0.0.1")).unwrap();
        // Failures leave the cache as it was.
        assert_eq!(cached(home), ["token-max-0000000000000000"]);
        assert_eq!(
            sh(home, &download, &["0.0.1", &releases]),
            (Some(0), "".into())
        );
        let name = format!("token-max-v0.0.1-{}", target());
        assert_eq!(cached(home), [name]);
        let (os, arch) = ("plan9", std::env::consts::ARCH);
        assert_eq!(
            sh(home, RUN, &[os, arch, "token-max-copy", "0.0.1", "doctor"]),
            (Some(0), "run\nran doctor\n".into())
        );
    }

    #[test]
    fn the_copy_script_caches_only_an_intact_binary_that_runs() {
        let workspace = Workspace::new();
        let home = &workspace.home;
        let binary = home.join("binary");
        release(&binary, "0.0.1");
        let bytes = std::fs::read(&binary).unwrap();
        let hash: String = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let copy = |sha256: &str| {
            let output = command(home, &format!("{SUM}{COPY}"), &["token-max-copy", sha256])
                .stdin(std::fs::File::open(&binary).unwrap())
                .output()
                .unwrap();
            output.status.code()
        };
        assert_eq!(copy(&"0".repeat(64)), Some(6));
        assert_eq!(cached(home), Vec::<String>::new());
        assert_eq!(copy(&hash), Some(0));
        assert_eq!(cached(home), ["token-max-copy"]);
        assert_eq!(
            std::fs::read(cache(home).join("token-max-copy")).unwrap(),
            bytes
        );
    }
}
