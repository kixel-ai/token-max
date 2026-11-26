//! Audits of selected SSH destinations, each run by this build of `token-max` there.
//!
//! The coordinator runs `token-max doctor --ssh-scan`, or `token-max sessions --ssh-scan` for
//! session lines, on each destination through the local OpenSSH client, with its configuration,
//! authentication and host keys. The destination reads its own histories and settings, prices
//! them with the coordinator's catalog, and sends back only its printed report or its session
//! lines (see [`crate::wire`]); its scan never leaves it.
//!
//! For a walk through a report's changes, the audit stays open: the destination keeps the
//! settings files it read, and this host sends it the person's answers and their confirm, so
//! each destination writes its own files with the same checks, locks and backups as this host.
//! `doctor`'s walk can go on to `vibes`' in the same audit, so the destination scans once.
//!
//! A destination without this build gets one first, cached there for later runs: a copy of this
//! binary when its platform matches, or else this version's release, checked against its
//! published checksum.

use std::io::{self, BufRead, BufReader, BufWriter, Read};
use std::panic::{self, AssertUnwindSafe};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError, mpsc};

use sha2::{Digest, Sha256};

use crate::cli::{Printed, Render, Rendered, render};
use crate::discovery::{DiscoveryOptions, discover};
use crate::doctor::{Event, Offers, Owner, Plan, Style, Written, preview, write as write_plan};
use crate::model::Millis;
use crate::reader::{Reach, ScanOptions, scan};
use crate::safe::{ScanError, iso, terminal_text};
use crate::sessions::{self, export};
use crate::vibes::Item;
use crate::wire::{self, Answer, Header, Job, Limit, Outcome, Paid, Request, Stream};

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

/// Where release archives are published.
pub(crate) const RELEASES: &str = "https://github.com/kixel-ai/token-max/releases";

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
/// Prints `run` and the path of the binary it execs, or prints `copy` or `download <target>`
/// and exits. The path goes through `printf %s`, since dash's `echo` would act on a `\c` in it. On the coordinator's own platform only its copy runs, since every build from a
/// commit after a release reports that release's version. On another platform nothing else
/// can, so a binary of this version stands in, checked by the stream's header: this version's
/// cached release, else the first installed `token-max` reporting it.
///
/// Installed ones are searched on `PATH`, then in the release installers' directory, the first
/// of `$XDG_BIN_HOME`, `$XDG_DATA_HOME/../bin` and `~/.local/bin`, then in `cargo install`'s
/// `~/.cargo/bin`; non-interactive shells often leave these off `PATH`. An empty `PATH` entry
/// is the current directory, as POSIX has it.
const RUN: &str = r#"if [ "$os $arch" = "$1 $2" ]; then bin=$cache/$3; [ -x "$bin" ] || { echo copy; exit 0; }; else bin=$cache/token-max-v$4-$arch-$abi; if [ ! -x "$bin" ]; then bin=; dirs=$PATH:${XDG_BIN_HOME:-${XDG_DATA_HOME:-$HOME/.local/share}/../bin}:$HOME/.local/bin:${CARGO_HOME:-$HOME/.cargo}/bin; set -f; IFS=:; for dir in $dirs; do dir=${dir:-.}; if [ -x "$dir/token-max" ] && [ "$("$dir/token-max" --version 2>/dev/null)" = "$4" ]; then bin=$dir/token-max; break; fi; done; unset IFS; set +f; [ -n "$bin" ] || { echo download $arch-$abi; exit 0; }; fi; fi; printf "run %s" "$bin"; echo; shift 4; exec "$bin" "$@""#;

/// Picks a SHA-256 command, since Linux and macOS ship different ones.
const SUM: &str = r#"if command -v sha256sum >/dev/null; then sum=sha256sum; elif command -v shasum >/dev/null; then sum="shasum -a 256"; else exit 5; fi; mkdir -p "$cache" || exit 1; "#;

/// Caches the binary on stdin under the name `$1` once its SHA-256 is `$2` and it runs there.
/// Each step writes beside the cache so the final move is an atomic rename, and then removes
/// every other binary there, so repeated builds don't pile up.
const COPY: &str = r#"tmp=$(mktemp "$cache/.copy.XXXXXX") || exit 1; clean() { rm -f "$tmp"; }; trap clean EXIT; cat >"$tmp" || exit 1; actual=$($sum <"$tmp") || exit 1; [ "${actual%% *}" = "$2" ] || exit 6; chmod 755 "$tmp" && "$tmp" --version >/dev/null 2>&1 || exit 4; mv -f "$tmp" "$cache/$1" || exit 1; for old in "$cache"/token-max-*; do [ "$old" = "$cache/$1" ] || rm -f "$old"; done"#;

/// Caches release `$1` from the releases at `$2` for the destination's platform, as the run
/// script names it, once its archive matches the published checksum and it runs there.
const DOWNLOAD: &str = r#"for tool in curl tar; do command -v $tool >/dev/null || exit 7; done; tmp=$(mktemp -d "$cache/.download.XXXXXX") || exit 1; clean() { rm -rf "$tmp"; }; trap clean EXIT; target=$arch-$abi; archive=token-max-$target.tar.xz; curl -LsSf -o "$tmp/$archive" "$2/download/v$1/$archive" && curl -LsSf -o "$tmp/sum" "$2/download/v$1/$archive.sha256" || exit 8; read -r expected rest <"$tmp/sum"; actual=$($sum <"$tmp/$archive") || exit 1; [ -n "$expected" ] && [ "${actual%% *}" = "$expected" ] || exit 6; tar -xf "$tmp/$archive" -C "$tmp" || exit 9; bin=$tmp/token-max-$target/token-max; [ "$("$bin" --version 2>/dev/null)" = "$1" ] || exit 4; mv -f "$bin" "$cache/token-max-v$1-$target" || exit 1; for old in "$cache"/token-max-*; do [ "$old" = "$cache/token-max-v$1-$target" ] || rm -f "$old"; done"#;

/// The longest path Linux allows, and more than macOS does.
const PATH_MAX: u64 = 4096;

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
            let sha256 = crate::safe::hex(&Sha256::digest(&bytes));
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

/// What a destination scans for, which it must know before its job arrives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// A report, with every settings file pretended away for `--defaults`.
    Report { defaults: bool },
    /// Session lines.
    Sessions,
}

impl Scope {
    /// The transcripts the scan reads.
    pub fn reach(self) -> Reach {
        match self {
            Scope::Report { .. } => Reach::History,
            Scope::Sessions => Reach::Window,
        }
    }

    /// The most this host reads of the destination's stream: each value the audit reads,
    /// once, and a walk's two reviews and writes, `doctor`'s and then `vibes`'.
    fn stream(self) -> Limit {
        match self {
            Scope::Report { .. } => Limit::stream(&[
                Limit::HEADER,
                Limit::PROFILES,
                Limit::REPORT,
                Limit::PREVIEW,
                Limit::WROTE,
                Limit::ADVICE,
                Limit::PREVIEW,
                Limit::WROTE,
                Limit::END,
            ]),
            Scope::Sessions => Limit::stream(&[Limit::HEADER, Limit::SESSIONS, Limit::END]),
        }
    }
}

fn run_command(binary: &Binary, since: Option<Millis>, scope: Scope) -> String {
    let since = since.map(iso);
    let command = match scope {
        Scope::Report { .. } => "doctor",
        Scope::Sessions => "sessions",
    };
    let mut args = vec![
        std::env::consts::OS,
        std::env::consts::ARCH,
        &binary.name,
        VERSION,
        command,
        "--ssh-scan",
    ];
    if let Some(since) = &since {
        args.extend(["--since", since]);
    }
    if scope == (Scope::Report { defaults: true }) {
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
        Result<Outcome, wire::Error>,
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

    /// The destination's own diagnostic, which it sent in place of a result.
    fn remote_error(&mut self, error: ScanError) -> ScanError {
        self.stop(true);
        ScanError {
            message: format!("{}: {}", self.destination, terminal_text(&error.message)),
            location: error.location,
        }
    }

    /// The destination sent a result other than the one requested, which `doing` cut short
    /// when it names the step.
    fn wrong_result(&mut self, doing: Option<&str>) -> ScanError {
        self.fail(true, |_| match doing {
            Some(doing) => format!("token-max sent a different result than requested {doing}"),
            None => "token-max sent a different result than requested".into(),
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop(true);
    }
}

/// What a host sent back for its [`Job`].
pub enum Reply {
    /// This host's own report, which no destination sends.
    Here(Box<Rendered>),
    /// A destination's report.
    Report(Printed),
    /// A report for a walk, with the changes the destination offers and its open audit.
    Walk {
        printed: Printed,
        offers: Box<Offers>,
        remote: Remote,
    },
    /// A destination's advice for `vibes`.
    Vibes(Advice),
    /// Its sessions, ordered by first request, each without a host.
    Sessions(Vec<sessions::Session>),
}

/// What `vibes` has on a destination.
pub struct Advice {
    pub items: Vec<Item>,
    /// The destination's home directory, which its files are named from.
    pub home: Option<String>,
    /// The changes the items name and the destination's open audit, for a walk.
    pub walk: Option<(Box<Offers>, Remote)>,
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
    // Windows' CREATE_NEW_PROCESS_GROUP, which also ignores Ctrl-C in the new group.
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut ssh, 0x0000_0200);
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
    /// The binary the script runs, as a path there.
    Run(String),
    Copy,
    Download(&'static str),
}

/// Read the run script's first line, at most a word and a path as long as Linux and macOS
/// allow, so a destination cannot make this host hold more. An error says whether stdout was empty, as it is when `ssh` or the script failed.
fn found(stdout: &mut BufReader<ChildStdout>) -> Result<Found, bool> {
    let mut line = String::new();
    match stdout.take(PATH_MAX + 16).read_line(&mut line) {
        Ok(0) => return Err(true),
        Ok(_) => {}
        Err(_) => return Err(false),
    }
    let found = match line.strip_suffix('\n') {
        Some("copy") => Some(Found::Copy),
        Some(other) if other.starts_with("run ") && other.len() > 4 => {
            Some(Found::Run(other[4..].to_string()))
        }
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
        Found::Run(_) => return Ok(()),
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

/// A destination running this build's audit, before it scans: for a report, waiting for how
/// its Claude profiles pay.
pub struct Connected {
    session: Session,
    stream: Stream<BufReader<ChildStdout>>,
    since: Option<Millis>,
    profiles: Option<Profiles>,
}

/// A report destination's Claude profiles whose billing picks their main TTL's default.
#[derive(Clone, Debug)]
pub struct Profiles {
    /// Its home directory, which its roots are shown from as `~`.
    pub home: Option<String>,
    /// Each profile's root, as that host names it; [`Connected::audit`] answers by these.
    pub roots: Vec<String>,
}

/// Start this build of `token-max` on `destination`, putting it there first if needed, and
/// read its header and, for a report, the profiles it asks about, failing on any error there.
pub fn connect(
    destination: &str,
    since: Option<Millis>,
    scope: Scope,
) -> Result<Connected, ScanError> {
    let command = run_command(binary()?, since, scope);
    let mut provisioned = false;
    let (mut session, stdout, ran) = loop {
        let (mut session, mut stdout) = spawn(destination, command.clone(), Stdio::piped())?;
        let found = found(&mut stdout);
        let found = match found {
            Ok(Found::Run(ran)) => break (session, stdout, ran),
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
    let mut stream = Stream::new(stdout, scope.stream());
    let header = match stream.value::<Header>(Limit::HEADER) {
        Ok(header) => header,
        Err(error) => {
            // Anything but an empty stdout is not a token-max stream, which may never end.
            return Err(
                session.fail(!error.is_eof(), |status| match (status, &error) {
                    (Some(SSH_FAILED), _) => connection_failed(destination),
                    (_, wire::Error::Exceeded(_)) => invalid_stream(&error, status, None),
                    (status, _) => format!(
                        "token-max there did not start an SSH audit ({})",
                        exit(status)
                    ),
                }),
            );
        }
    };
    let local = wire::build();
    if header.token_max_scan != local {
        let remote = terminal_text(&header.token_max_scan);
        let ran = terminal_text(&ran);
        // On another platform the script runs the first binary of this version it finds, so
        // removing that one is what lets a build of this commit run instead.
        return Err(session.fail(true, |_| {
            format!(
                "the token-max {VERSION} there, {ran}, is build {remote}, not this build {local}; run a released token-max here, or delete it there and build this one with {BUILD}"
            )
        }));
    }
    let profiles = match scope {
        Scope::Report { .. } => match stream.value::<Outcome>(Limit::PROFILES) {
            Ok(Outcome::Profiles { home, roots }) => Some(Profiles { home, roots }),
            Ok(Outcome::Error(error)) => return Err(session.remote_error(error)),
            Ok(_) => return Err(session.wrong_result(None)),
            Err(error) => {
                return Err(session.fail(true, |status| match status {
                    Some(SSH_FAILED) => "SSH connection lost during the audit".into(),
                    _ => invalid_stream(&error, status, None),
                }));
            }
        },
        Scope::Sessions => None,
    };
    Ok(Connected {
        session,
        stream,
        since,
        profiles,
    })
}

impl Connected {
    /// For a report, the profiles the destination asks about; it scans once
    /// [`Connected::audit`] sends their answers.
    pub fn profiles(&self) -> Option<&Profiles> {
        self.profiles.as_ref()
    }

    /// Send a report's `paid` answers, by the roots [`Connected::profiles`] lists, so the
    /// destination scans; once `request` delivers the catalog and job, it analyzes and sends
    /// back what the job asks for. A destination that fails first is reported at once,
    /// without waiting for the request.
    pub fn audit(self, paid: Paid, request: mpsc::Receiver<Request>) -> Result<Reply, ScanError> {
        let Connected {
            mut session,
            mut stream,
            since,
            profiles,
        } = self;
        if profiles.is_some()
            && let Some(stdin) = session.stdin.as_mut()
        {
            // A destination that already failed has stopped reading; its outcome says why.
            let _ = wire::write(&mut BufWriter::new(stdin), &paid);
        }
        // The destination may fail before this host has prices to send, so its outcome is read
        // while the request is still pending.
        let (steps, arrived) = mpsc::channel();
        let forward = steps.clone();
        std::thread::spawn(move || {
            // Dropped without a request when this host fails first, and then stops every destination.
            let _ = forward.send(request.recv().map_or(Step::Cancelled, Step::Request));
        });
        let limit = match profiles {
            Some(_) => Limit::REPORT,
            None => Limit::SESSIONS,
        };
        std::thread::spawn(move || {
            let outcome = stream.value(limit);
            let _ = steps.send(Step::Outcome(outcome, Box::new(stream)));
        });
        // What the job asked for, once it is sent: sessions, or whether it is walked and
        // whether it is `vibes`' advice instead of the report.
        let mut requested = None;
        let (outcome, mut stream) = loop {
            match arrived.recv() {
                Ok(Step::Request(request)) => {
                    requested = Some(match request.job {
                        Job::Sessions => None,
                        Job::Report { walk, vibes, .. } => Some((walk, vibes)),
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
        let invalid = |session: &mut Session, error: wire::Error| {
            session.fail(true, |status| match status {
                Some(SSH_FAILED) => "SSH connection lost during the audit".into(),
                _ => invalid_stream(&error, status, None),
            })
        };
        // The window scanned, the reply, the offers of a walk, and whether it is `vibes`' advice.
        let (scanned, reply, offers, vibes) = match outcome {
            Ok(Outcome::Report {
                since,
                listing,
                change,
                totals,
                offers,
                vibes,
            }) => (
                since,
                Reply::Report(Printed {
                    listing,
                    change,
                    totals,
                    vibes,
                }),
                offers,
                false,
            ),
            Ok(Outcome::Vibes {
                since,
                items,
                home,
                offers,
            }) => {
                let advice = Advice {
                    items,
                    home,
                    walk: None,
                };
                (since, Reply::Vibes(advice), offers, true)
            }
            Ok(Outcome::Sessions { since, sessions }) => {
                (since, Reply::Sessions(sessions), None, false)
            }
            Ok(Outcome::Profiles { .. } | Outcome::Reviewed { .. } | Outcome::Wrote { .. }) => {
                return Err(session.wrong_result(None));
            }
            Ok(Outcome::Error(error)) => return Err(session.remote_error(error)),
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
            (Some(Some((true, asked))), Reply::Report(_) | Reply::Vibes(_), Some(offers))
                if asked == vibes =>
            {
                if let Err(error) = offers.check() {
                    return Err(session.fail(true, |_| format!("token-max there {error}")));
                }
                let remote = Remote {
                    since,
                    wrote: false,
                    session,
                    stream,
                };
                return Ok(match reply {
                    Reply::Report(printed) => Reply::Walk {
                        printed,
                        offers,
                        remote,
                    },
                    Reply::Vibes(mut advice) => {
                        advice.walk = Some((offers, remote));
                        Reply::Vibes(advice)
                    }
                    _ => unreachable!("matched a report or advice"),
                });
            }
            (Some(None), Reply::Sessions(_), None) => {}
            (Some(Some((false, asked))), Reply::Report(_) | Reply::Vibes(_), None)
                if asked == vibes => {}
            _ => return Err(session.wrong_result(None)),
        }
        if let Err(error) = stream.end() {
            return Err(invalid(&mut session, error));
        }
        let status = session.stop(false).and_then(|s| s.code());
        if status != Some(0) {
            return Err(ScanError::new(format!(
                "{}: token-max failed after sending its result ({})",
                session.destination,
                exit(status)
            )));
        }
        Ok(reply)
    }
}

/// A destination's audit kept open for this host's walk through its changes.
pub struct Remote {
    /// The window the audit was asked for, which every result of its scan names.
    since: Option<Millis>,
    /// Whether it was asked to write, so a later failure doesn't say nothing changed there.
    wrote: bool,
    session: Session,
    stream: Box<Stream<BufReader<ChildStdout>>>,
}

impl Remote {
    /// Send `answer` and read the destination's reply to it, failing with what `doing` was
    /// cut short when the stream breaks.
    fn exchange(
        &mut self,
        answer: &Answer,
        limit: Limit,
        doing: &str,
    ) -> Result<Outcome, ScanError> {
        let sent = self
            .session
            .stdin
            .as_mut()
            .map(|stdin| wire::write(&mut BufWriter::new(stdin), answer));
        let outcome = match sent {
            Some(Ok(())) => self.stream.value(limit),
            _ => {
                return Err(self
                    .session
                    .fail(true, |_| format!("SSH connection lost {doing}")));
            }
        };
        match outcome {
            Ok(Outcome::Error(error)) => Err(self.session.remote_error(error)),
            Ok(outcome) => Ok(outcome),
            // Cancelled from the terminal, which stopped this client.
            Err(_) if cancelled() => Err(self.session.fail(true, |_| format!("cancelled {doing}"))),
            Err(error) if error.is_eof() || error.is_io() => Err(self
                .session
                .fail(true, |_| format!("SSH connection lost {doing}"))),
            Err(error) => Err(self
                .session
                .fail(true, |status| invalid_stream(&error, status, Some(doing)))),
        }
    }

    /// The destination's preview of each file the walk's `events` would write, once it
    /// checked them against its files; nothing is written there yet.
    pub fn review(&mut self, events: &[Event]) -> Result<String, ScanError> {
        let answer = Answer::Review {
            events: events.to_vec(),
        };
        let doing = match self.wrote {
            true => "during the walk; no further settings were changed there",
            false => "during the walk; no settings were changed there",
        };
        match self.exchange(&answer, Limit::PREVIEW, doing)? {
            Outcome::Reviewed { preview } => Ok(preview),
            _ => Err(self.session.wrong_result(None)),
        }
    }

    /// Write the reviewed changes there, returning each file it reported written. The audit
    /// stays open for [`Remote::vibes`] or [`Remote::end`].
    pub fn write(&mut self) -> Result<Vec<Written>, ScanError> {
        self.wrote = true;
        match self.exchange(&Answer::Write, Limit::WROTE, WRITING)? {
            Outcome::Wrote { written } => Ok(written),
            _ => Err(self.session.wrong_result(Some(WRITING))),
        }
    }

    /// What `vibes` has there after `doctor`'s walk, from the same scan: its advice, and the
    /// changes it offers against its files as they are now.
    pub fn vibes(&mut self) -> Result<(Vec<Item>, Box<Offers>), ScanError> {
        let doing = "before its advice arrived; no further settings were changed there";
        match self.exchange(&Answer::Vibes, Limit::ADVICE, doing)? {
            Outcome::Vibes {
                since,
                items,
                offers: Some(offers),
                ..
            } if since == self.since => match offers.check() {
                Ok(()) => Ok((items, offers)),
                Err(error) => Err(self
                    .session
                    .fail(true, |_| format!("token-max there {error}"))),
            },
            _ => Err(self.session.wrong_result(None)),
        }
    }

    /// End the audit after its writes, and check that the destination ended it cleanly: its
    /// reports of what it wrote stand either way.
    pub fn end(mut self) -> Result<(), ScanError> {
        let sent = self
            .session
            .stdin
            .as_mut()
            .map(|stdin| wire::write(&mut BufWriter::new(stdin), &Answer::Done));
        let sent = matches!(sent, Some(Ok(())));
        if !sent || self.stream.end().is_err() {
            // Ctrl-C stopped the client, which its report of the write came before.
            if cancelled() {
                self.session.stop(true);
                return Ok(());
            }
            return Err(self.session.fail(true, |_| {
                if sent {
                    "token-max sent more than its result after writing".into()
                } else {
                    "the connection closed after writing, before the audit could be ended".into()
                }
            }));
        }
        match self.session.stop(false).and_then(|s| s.code()) {
            Some(0) => Ok(()),
            // Ctrl-C stopped the client after its report and clean end, which already
            // confirm the write; the signal cut nothing short.
            None if cancelled() => Ok(()),
            status => Err(ScanError::new(format!(
                "{}: token-max failed after writing ({})",
                self.session.destination,
                exit(status)
            ))),
        }
    }
}

/// What a broken stream cut short once a destination was asked to write.
const WRITING: &str = "while it was writing, so it may have changed its settings files; check them and their .token-max.bak backups";

/// A destination's stream that is not what this build writes, and the step it cut short.
fn invalid_stream(error: &wire::Error, status: Option<i32>, doing: Option<&str>) -> String {
    let after = doing.map_or(String::new(), |doing| format!(" {doing}"));
    match error {
        wire::Error::Exceeded(limit) => format!(
            "token-max sent more than {limit}, the most this host reads ({}){after}",
            exit(status)
        ),
        wire::Error::Json(error) => format!(
            "invalid SSH audit stream at line {}, column {} ({}){after}",
            error.line(),
            error.column(),
            exit(status)
        ),
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
    /// How the profiles a report listed pay, which comes first.
    Paid(Paid),
    Request(Request),
    Answer(Answer),
}

/// A destination's side of a walk: its files as it read them, and how to print.
struct Walked {
    owner: Owner,
    style: Style,
    /// What `vibes` has here, from the same scan.
    vibes: Vec<Item>,
    home: Option<String>,
    /// This host's spend every 30 days.
    monthly: f64,
    since: Option<Millis>,
    /// The report lines whose changes the walk wrote here, which can move what `vibes` advises.
    written: Vec<crate::report::Origin>,
}

impl Walked {
    /// `vibes`' advice as it reads after whatever the walk wrote here, and the changes it
    /// offers, checked against the files as they are now, which the walk goes on with.
    fn advise(&mut self) -> Outcome {
        self.vibes = crate::vibes::settled(std::mem::take(&mut self.vibes), &self.written);
        self.owner = crate::vibes::owner(&self.vibes, self.home.clone(), self.monthly);
        self.advice()
    }

    /// `vibes`' advice and the changes this walk offers for it.
    fn advice(&self) -> Outcome {
        Outcome::Vibes {
            since: self.since,
            items: self.vibes.clone(),
            home: self.home.clone(),
            offers: Some(Box::new(self.owner.offers().clone())),
        }
    }
}

/// Writes the coordinator asked for that are not finished, which a closed stdin must not cut
/// short.
static WRITES: AtomicUsize = AtomicUsize::new(0);

/// Run as an SSH audit destination: scan this host, stream what the coordinator's job asks for
/// to stdout and return the exit code. For a walk, then answer the coordinator's reviews,
/// write the changes it confirms, and go on to `vibes`' walk when it asks, until it is done.
///
/// Diagnostics travel in the stream, since the coordinator discards this host's stderr. A
/// report first lists the Claude profiles whose billing picks their main TTL's default and
/// scans once the coordinator says how they pay. The coordinator's request arrives on stdin
/// while this host scans, and the audit stops when stdin closes, which happens when the
/// coordinator or its connection ends, unless it already asked this host to write.
pub fn serve(since: Option<Millis>, scope: Scope) -> u8 {
    let (sender, incoming) = mpsc::channel();
    std::thread::spawn(move || {
        let mut stdin = BufReader::new(io::stdin());
        let mut stream = Stream::new(&mut stdin, Limit::NONE);
        if scope != Scope::Sessions {
            let Ok(paid) = stream.value::<Paid>(Limit::NONE) else {
                std::process::exit(2);
            };
            let _ = sender.send(Incoming::Paid(paid));
        }
        let Ok(request) = stream.value::<Request>(Limit::NONE) else {
            // Not this build's coordinator, or it is gone.
            std::process::exit(2);
        };
        let walk = matches!(request.job, Job::Report { walk: true, .. });
        let _ = sender.send(Incoming::Request(request));
        if walk {
            // Ends when the coordinator is done, or exits once stdin closes.
            loop {
                let Ok(answer) = stream.value::<Answer>(Limit::NONE) else {
                    // Once asked to write, the write finishes even if the coordinator goes:
                    // the walk then finds this channel closed.
                    if WRITES.load(Ordering::SeqCst) > 0 {
                        return;
                    }
                    std::process::exit(2);
                };
                if matches!(answer, Answer::Write) {
                    WRITES.fetch_add(1, Ordering::SeqCst);
                }
                let done = matches!(answer, Answer::Done);
                let _ = sender.send(Incoming::Answer(answer));
                if done {
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
        if scope == (Scope::Report { defaults: true }) {
            discovery.pretend_defaults();
        }
        let home = std::env::home_dir().map(|dir| dir.to_string_lossy().into_owned());
        if scope != Scope::Sessions {
            let roots = discover(&discovery)?.billing_roots();
            let profiles = Outcome::Profiles {
                home: home.clone(),
                roots: roots.clone(),
            };
            wire::write(&mut stdout, &profiles)
                .map_err(|_| ScanError::new("the coordinator is gone; no report produced"))?;
            // The stdin thread keeps the sender until it exits this process.
            let Ok(Incoming::Paid(paid)) = incoming.recv() else {
                unreachable!("the answers come first")
            };
            if paid.profiles.keys().any(|root| !roots.contains(root)) {
                return Err(ScanError::new(
                    "the coordinator answered how a profile pays that this host did not list",
                ));
            }
            discovery.billing = paid.profiles;
        }
        let reach = scope.reach();
        let scan = scan(&ScanOptions {
            discovery,
            since,
            reach,
            reading: None,
        })?;
        // The stdin thread sends the request before anything ends it.
        let Ok(Incoming::Request(request)) = incoming.recv() else {
            unreachable!("the request comes next")
        };
        if matches!(request.job, Job::Sessions) != (scope == Scope::Sessions) {
            return Err(ScanError::new(
                "the coordinator asked for another job than this host scanned for",
            ));
        }
        let (max_cost_per_use_usd, format, walk, advice) = match &request.job {
            Job::Report {
                max_cost_per_use_usd,
                format,
                walk,
                vibes,
            } => (*max_cost_per_use_usd, format, *walk, *vibes),
            Job::Sessions => {
                let sessions = export(&scan, &request.catalog)?;
                return Ok((Outcome::Sessions { since, sessions }, None));
            }
        };
        let Rendered {
            printed:
                Printed {
                    listing,
                    change,
                    totals,
                    vibes,
                },
            fixes,
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
                reading: None,
            },
        )?;
        // A walk of `vibes` alone reads only the files its own changes name.
        let owner = |home: Option<String>| match advice {
            true => crate::vibes::owner(&vibes, home, totals.monthly),
            false => Owner::new(fixes, turned_off, home, totals.monthly),
        };
        let walked = walk.then(|| Walked {
            owner: owner(home.clone()),
            style: format.style(home.clone()),
            vibes: vibes.clone(),
            home: home.clone(),
            monthly: totals.monthly,
            since,
            written: vec![],
        });
        let outcome = match (advice, &walked) {
            (true, Some(walked)) => walked.advice(),
            (true, None) => Outcome::Vibes {
                since,
                items: crate::vibes::resolved(vibes),
                home,
                offers: None,
            },
            (false, walked) => Outcome::Report {
                since,
                listing,
                change,
                totals,
                offers: walked.as_ref().map(|w| Box::new(w.owner.offers().clone())),
                vibes,
            },
        };
        Ok((outcome, walked))
    }))
    .unwrap_or_else(|_| Err(ScanError::new("analysis failed; no report produced")));
    let (outcome, walked) = match result {
        Ok((outcome, walked)) => (outcome, walked),
        Err(error) => (Outcome::Error(error), None),
    };
    let failed = matches!(outcome, Outcome::Error(_));
    if wire::write(&mut stdout, &outcome).is_err() || failed {
        return 2;
    }
    match walked {
        Some(walked) => walk_here(walked, &incoming, &mut stdout),
        None => 0,
    }
}

/// Answer the coordinator's reviews of this host's changes and write each one it confirms,
/// `doctor`'s and then `vibes`' when it asks for them, until it is done.
fn walk_here(
    mut walked: Walked,
    incoming: &mpsc::Receiver<Incoming>,
    stdout: &mut impl io::Write,
) -> u8 {
    // The changes last reviewed, which a confirm writes.
    let mut plan: Option<Plan> = None;
    loop {
        // A coordinator that leaves without saying it is done closes stdin, which exits this
        // process unless a write was under way.
        let Ok(Incoming::Answer(answer)) = incoming.recv() else {
            return 2;
        };
        let write = matches!(answer, Answer::Write);
        let outcome = panic::catch_unwind(AssertUnwindSafe(|| match answer {
            Answer::Done => Ok(None),
            Answer::Review { events } => {
                // Checked against the files as they were read, which the coordinator's report
                // of the answers leaves to this host.
                let reviewed = walked.owner.plan(&events)?;
                let preview = preview(&reviewed, &walked.style);
                plan = Some(reviewed);
                Ok(Some(Outcome::Reviewed { preview }))
            }
            Answer::Write => match plan.take() {
                Some(plan) => write_plan(&plan).map(|written| {
                    walked.written.extend(plan.accepted());
                    Some(Outcome::Wrote { written })
                }),
                None => Err(ScanError::new(
                    "the walk confirmed changes it had not reviewed; no settings were changed",
                )),
            },
            Answer::Vibes => {
                plan = None;
                Ok(Some(walked.advise()))
            }
        }))
        .unwrap_or_else(|_| {
            Err(ScanError::new(if write {
                "token-max failed while applying changes"
            } else {
                "token-max failed while reviewing changes"
            }))
        });
        let outcome = match outcome {
            Ok(None) => return 0,
            Ok(Some(outcome)) => outcome,
            Err(error) => Outcome::Error(error),
        };
        let failed = matches!(outcome, Outcome::Error(_));
        let sent = wire::write(stdout, &outcome);
        if write {
            WRITES.fetch_sub(1, Ordering::SeqCst);
        }
        if sent.is_err() || failed {
            return 2;
        }
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
        // The run line names the binary it runs.
        let ran = |path: &Path| (Some(0), format!("run {}\nran doctor\n", path.display()));
        // Nothing there yet.
        assert_eq!(run(os, "0.0.1"), (Some(0), "copy\n".into()));
        assert_eq!(run("plan9", "0.0.1"), (Some(0), download.clone()));
        // Only on another platform does an installed binary of this version stand in for
        // this build, the first that reports it, even past one that does not.
        let installed = home.join(".cargo/bin/token-max");
        release(&installed, "0.0.1");
        release(&home.join(".local/bin/token-max"), "0.0.2");
        assert_eq!(run(os, "0.0.1"), (Some(0), "copy\n".into()));
        assert_eq!(run("plan9", "0.0.1"), ran(&installed));
        assert_eq!(
            run("plan9", "0.0.2"),
            ran(&home.join(".local/bin/token-max"))
        );
        assert_eq!(run("plan9", "0.0.3"), (Some(0), download.clone()));
        release(&installed, "0.0.0");
        assert_eq!(run(os, "0.0.0"), (Some(0), "copy\n".into()));
        assert_eq!(run("plan9", "0.0.0"), ran(&installed));
        std::fs::remove_file(&installed).unwrap();
        std::fs::remove_file(home.join(".local/bin/token-max")).unwrap();
        // The empty entry in this `PATH` is the current directory, the home here.
        let current = home.join("token-max");
        release(&current, "0.0.1");
        assert_eq!(run("plan9", "0.0.1"), ran(Path::new("./token-max")));
        std::fs::remove_file(&current).unwrap();
        // Cached binaries: the copy on this platform, the release on another.
        script(&cache(home).join("token-max-copy"), r#"echo "copy $*""#);
        release(
            &cache(home).join(format!("token-max-v0.0.1-{}", target())),
            "0.0.1",
        );
        let copy = cache(home).join("token-max-copy");
        assert_eq!(
            run(os, "0.0.1"),
            (Some(0), format!("run {}\ncopy doctor\n", copy.display()))
        );
        // A cached release runs before an installed one of the same version.
        release(&installed, "0.0.1");
        assert_eq!(
            run("plan9", "0.0.1"),
            ran(&cache(home).join(format!("token-max-v0.0.1-{}", target())))
        );
        assert_eq!(run("plan9", "0.0.2"), (Some(0), download));
    }

    #[test]
    fn the_run_line_keeps_a_path_that_echo_would_cut() {
        let workspace = Workspace::new();
        let home = &workspace.home;
        // dash's `echo`, the `sh` of Debian and Ubuntu, stops output at a `\c`.
        let cache = home.join("x\\cy");
        let copy = cache.join("token-max/ssh/token-max-copy");
        script(&copy, r#"echo "copy $*""#);
        let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
        let output = command(home, RUN, &[os, arch, "token-max-copy", "0.0.1", "doctor"])
            .env("XDG_CACHE_HOME", &cache)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("run {}\ncopy doctor\n", copy.display())
        );
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
        let mut hash = crate::safe::hex(&Sha256::digest(std::fs::read(&archive).unwrap()));
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
        assert_eq!(cached(home), [name.as_str()]);
        let (os, arch) = ("plan9", std::env::consts::ARCH);
        assert_eq!(
            sh(home, RUN, &[os, arch, "token-max-copy", "0.0.1", "doctor"]),
            (
                Some(0),
                format!("run {}/{name}\nran doctor\n", cache(home).display())
            )
        );
    }

    #[test]
    fn the_copy_script_caches_only_an_intact_binary_that_runs() {
        let workspace = Workspace::new();
        let home = &workspace.home;
        let binary = home.join("binary");
        release(&binary, "0.0.1");
        let bytes = std::fs::read(&binary).unwrap();
        let hash = crate::safe::hex(&Sha256::digest(&bytes));
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
