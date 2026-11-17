//! Audits of selected SSH destinations, each run by this build of `token-max` there.
//!
//! The coordinator runs `token-max doctor --ssh-scan` on each destination through the local OpenSSH
//! client, with its configuration, authentication and host keys. The destination reads its own
//! histories and settings, prices them with the coordinator's catalog, and sends back only its
//! printed report or its session lines (see [`crate::wire`]); its scan never leaves it.
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
use crate::model::Millis;
use crate::reader::{ScanOptions, scan};
use crate::safe::{ScanError, iso, terminal_text};
use crate::sessions::{self, export};
use crate::wire::{self, Header, Job, Outcome, Request, Stream};

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
/// Prints `run` and execs a binary of this build: a copy of the coordinator on its own
/// platform, else this version's release, else an installed `token-max` of this version. An
/// installed development build may be any commit, so on the coordinator's own platform its
/// copy is used instead. Otherwise prints `copy` or `download <target>` and exits.
///
/// The release installers put the command in the first of `$XDG_BIN_HOME`,
/// `$XDG_DATA_HOME/../bin` and `~/.local/bin`, searched first since they are the documented
/// install, and `cargo install` in `~/.cargo/bin`; non-interactive shells often leave these
/// off `PATH`.
const RUN: &str = r#"PATH="$PATH:${XDG_BIN_HOME:-${XDG_DATA_HOME:-$HOME/.local/share}/../bin}:$HOME/.local/bin:${CARGO_HOME:-$HOME/.cargo}/bin"; if [ "$os $arch" = "$1 $2" ]; then same=1 bin=$cache/$3; else same= bin=$cache/token-max-v$4-$arch-$abi; fi; if [ ! -x "$bin" ]; then bin=; if [ -z "$same" ] || [ "$4" != 0.0.0 ]; then found=$(command -v token-max) && [ "$("$found" --version 2>/dev/null)" = "$4" ] && bin=$found; fi; fi; if [ -z "$bin" ]; then if [ -n "$same" ]; then echo copy; else echo download $arch-$abi; fi; exit 0; fi; echo run; shift 4; exec "$bin" "$@""#;

/// Picks a SHA-256 command, since Linux and macOS ship different ones.
const SUM: &str = r#"if command -v sha256sum >/dev/null; then sum=sha256sum; elif command -v shasum >/dev/null; then sum="shasum -a 256"; else exit 5; fi; mkdir -p "$cache" || exit 1; "#;

/// Caches the binary on stdin under the name `$1` once its SHA-256 is `$2` and it runs there.
/// Each step writes beside the cache so the final move is an atomic rename, and the install
/// removes every other binary there, so repeated builds don't pile up.
const COPY: &str = r#"tmp=$(mktemp "$cache/.copy.XXXXXX") || exit 1; clean() { rm -f "$tmp"; }; trap clean EXIT; cat >"$tmp" || exit 1; actual=$($sum <"$tmp") || exit 1; [ "${actual%% *}" = "$2" ] || exit 6; chmod 755 "$tmp" && "$tmp" --version >/dev/null 2>&1 || exit 4; rm -f "$cache"/token-max-*; mv -f "$tmp" "$cache/$1" || exit 1"#;

/// Caches release `$1` from the releases at `$2` for the destination's platform, as the run
/// script names it, once its archive matches the published checksum and it runs there.
const DOWNLOAD: &str = r#"for tool in curl tar; do command -v $tool >/dev/null || exit 7; done; tmp=$(mktemp -d "$cache/.download.XXXXXX") || exit 1; clean() { rm -rf "$tmp"; }; trap clean EXIT; target=$arch-$abi; archive=token-max-$target.tar.xz; curl -LsSf -o "$tmp/$archive" "$2/download/v$1/$archive" && curl -LsSf -o "$tmp/sum" "$2/download/v$1/$archive.sha256" || exit 8; read -r expected rest <"$tmp/sum"; actual=$($sum <"$tmp/$archive") || exit 1; [ -n "$expected" ] && [ "${actual%% *}" = "$expected" ] || exit 6; tar -xf "$tmp/$archive" -C "$tmp" || exit 9; bin=$tmp/token-max-$target/token-max; [ "$("$bin" --version 2>/dev/null)" = "$1" ] || exit 4; rm -f "$cache"/token-max-*; mv -f "$bin" "$cache/token-max-v$1-$target" || exit 1"#;

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
    /// The destination's outcome, which may come before the request when it fails.
    Outcome(Result<Outcome, serde_json::Error>),
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
/// hold more.
fn found(stdout: &mut BufReader<ChildStdout>) -> Option<Found> {
    let mut line = String::new();
    stdout.take(64).read_line(&mut line).ok()?;
    match line.strip_suffix('\n')? {
        "run" => Some(Found::Run),
        "copy" => Some(Found::Copy),
        other => {
            let target = other.strip_prefix("download ")?;
            TARGETS
                .iter()
                .find(|t| **t == target)
                .map(|t| Found::Download(t))
        }
    }
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
            Found::Copy => format!(
                "a copy of this token-max does not run there; copy a statically linked build, such as one for {}-unknown-linux-musl, or run a release",
                std::env::consts::ARCH
            ),
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
            Some(Found::Run) => break (session, stdout),
            // Only once: a binary just put there that the script still cannot find means the
            // cache does not keep it.
            Some(found) if !provisioned => found,
            Some(_) => {
                return Err(session.fail(true, |_| {
                    "token-max was put in the cache there but is still missing; check that ~/.cache/token-max/ssh there keeps its files".into()
                }));
            }
            None => {
                return Err(session.fail(true, |status| match status {
                    Some(SSH_FAILED) => connection_failed(destination),
                    Some(UNSUPPORTED) => {
                        "unsupported platform; SSH audits support Linux and macOS destinations on x86_64 or arm64".into()
                    }
                    status => format!("could not look for token-max there ({})", exit(status)),
                }));
            }
        };
        session.stop(true);
        if let Found::Download(target) = found
            && VERSION == "0.0.0"
        {
            return Err(ScanError::new(format!(
                "{destination}: runs on {target}, and this development build for {} {} has no release to download; build it with {BUILD}",
                std::env::consts::OS,
                std::env::consts::ARCH,
            )));
        }
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
                status => format!("token-max there did not start an SSH audit ({})", exit(status)),
            }));
        }
    };
    let local = wire::build();
    if header.token_max_scan != local {
        let remote = terminal_text(&header.token_max_scan);
        return Err(session.fail(true, |_| {
            format!(
                "the token-max installed there is build {remote} but this is {local}; build this one there with {BUILD}"
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
        let _ = steps.send(Step::Outcome(stream.outcome()));
    });
    // Whether the job asked for sessions, once it is sent.
    let mut requested = None;
    let outcome = loop {
        match arrived.recv() {
            Ok(Step::Request(request)) => {
                requested = Some(matches!(request.job, Job::Sessions));
                if let Some(stdin) = session.stdin.as_mut() {
                    // A destination that already failed has stopped reading; its outcome says why.
                    let _ = wire::write(&mut BufWriter::new(stdin), &request);
                }
            }
            Ok(Step::Outcome(outcome)) => break outcome,
            Ok(Step::Cancelled) | Err(_) => {
                return Err(ScanError::new("cancelled; no report produced"));
            }
        }
    };
    let (scanned, reply) = match outcome {
        Ok(Outcome::Report {
            since,
            text,
            change,
        }) => (
            since,
            Reply::Report(Rendered {
                text,
                change,
                fixes: vec![],
                listing: None,
                turned_off: vec![],
            }),
        ),
        Ok(Outcome::Sessions { since, sessions }) => (since, Reply::Sessions(sessions)),
        Ok(Outcome::Error { message, location }) => {
            session.stop(true);
            return Err(ScanError {
                message: format!("{destination}: {}", terminal_text(&message)),
                location,
            });
        }
        Err(error) => {
            let (line, column) = (error.line(), error.column());
            return Err(session.fail(true, |status| match status {
                Some(SSH_FAILED) => "SSH connection lost during the audit".into(),
                _ => format!(
                    "invalid SSH audit stream at line {line}, column {column} ({})",
                    exit(status)
                ),
            }));
        }
    };
    let status = session.stop(false).and_then(|s| s.code());
    if status != Some(0) {
        return Err(ScanError::new(format!(
            "{destination}: token-max failed after sending its result ({})",
            exit(status)
        )));
    }
    if scanned != since {
        return Err(ScanError::new(format!(
            "{destination}: token-max scanned a different window than requested"
        )));
    }
    match requested {
        None => Err(ScanError::new(format!(
            "{destination}: token-max sent its result before its request"
        ))),
        Some(sessions) if sessions != matches!(reply, Reply::Sessions(_)) => Err(ScanError::new(
            format!("{destination}: token-max sent a different result than requested"),
        )),
        Some(_) => Ok(reply),
    }
}

fn exit(status: Option<i32>) -> String {
    match status {
        Some(code) => format!("exit {code}"),
        None => "terminated".into(),
    }
}

/// Run as an SSH audit destination: scan this host, stream what the coordinator's job asks for
/// to stdout and return the exit code.
///
/// Diagnostics travel in the stream, since the coordinator discards this host's stderr. The
/// coordinator's request arrives on stdin while this host scans, and the audit stops when stdin
/// closes, which happens when the coordinator or its connection ends.
pub fn serve(since: Option<Millis>, defaults: bool) -> u8 {
    let (sender, requests) = mpsc::channel();
    std::thread::spawn(move || {
        let mut stdin = BufReader::new(io::stdin());
        match Stream::new(&mut stdin).request() {
            Ok(request) => {
                let _ = sender.send(request);
            }
            // Not this build's coordinator, or it is gone.
            Err(_) => std::process::exit(2),
        }
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
        let request: Request = requests.recv().expect("the request or an exit");
        let (max_cost_per_use_usd, format) = match &request.job {
            Job::Report {
                max_cost_per_use_usd,
                format,
            } => (*max_cost_per_use_usd, format),
            Job::Sessions => {
                let sessions = export(&scan, &request.catalog)?;
                return Ok(Outcome::Sessions { since, sessions });
            }
        };
        let home = std::env::home_dir().map(|dir| dir.to_string_lossy().into_owned());
        let Rendered { text, change, .. } = render(
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
        Ok(Outcome::Report {
            since,
            text,
            change,
        })
    }))
    .unwrap_or_else(|_| Err(ScanError::new("analysis failed; no report produced")));
    let (outcome, code) = match result {
        Ok(outcome) => (outcome, 0),
        Err(error) => (
            Outcome::Error {
                message: error.message,
                location: error.location,
            },
            2,
        ),
    };
    match wire::write(&mut stdout, &outcome) {
        Ok(()) => code,
        Err(_) => 2,
    }
}
