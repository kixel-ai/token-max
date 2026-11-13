//! Audits of selected SSH destinations, each run by the `token-max` installed there.
//!
//! The coordinator runs `token-max doctor --ssh-scan` on each destination through the local OpenSSH
//! client, with its configuration, authentication and host keys. The destination reads its own
//! histories and settings, prices them with the coordinator's catalog, and sends back only its
//! printed report or its session lines (see [`crate::wire`]); its scan never leaves it.

use std::io::{self, BufReader, BufWriter};
use std::panic::{self, AssertUnwindSafe};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::{Mutex, PoisonError, mpsc};

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

/// The command a destination needs to match this build.
fn install() -> String {
    // Until the first release every build is 0.0.0, so only a fresh install of the same
    // source matches, on both hosts.
    match env!("CARGO_PKG_VERSION") {
        "0.0.0" => {
            "`cargo install --locked --git https://github.com/kixel-ai/token-max` on both hosts"
                .into()
        }
        version => format!("`cargo install --locked token-max --version {version}` there"),
    }
}

/// Runs on the destination through its login shell, which may not be POSIX: only `sh` reads
/// the script, and the script holds no single quotes or backslashes. `cargo install` puts the
/// command in `~/.cargo/bin`, which non-interactive shells often leave off `PATH`.
const SCRIPT: &str = r#"case "$(uname -s)" in Linux|Darwin) ;; *) exit 3 ;; esac; PATH="$PATH:${CARGO_HOME:-$HOME/.cargo}/bin"; command -v token-max >/dev/null || exit 127; exec token-max "$@""#;

/// Exit statuses the script and OpenSSH report before a stream starts.
const UNSUPPORTED: i32 = 3;
const NOT_INSTALLED: i32 = 127;
const SSH_FAILED: i32 = 255;

fn remote_command(since: Option<Millis>) -> String {
    let mut command = format!("sh -c '{SCRIPT}' sh doctor --ssh-scan");
    if let Some(since) = since {
        command.push_str(&format!(" --since '{}'", iso(since)));
    }
    command
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

/// Audit one destination with its installed `token-max`, failing on any error there.
///
/// The destination scans while this host loads prices; once `request` delivers the catalog and
/// job, it analyzes and sends back what the job asks for. A destination that fails first is
/// reported at once, without waiting for the request.
pub fn audit(
    destination: &str,
    since: Option<Millis>,
    request: mpsc::Receiver<Request>,
) -> Result<Reply, ScanError> {
    let mut command = Command::new("ssh");
    command
        .args(["-T", "-x", "-a"])
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
        .arg(remote_command(since))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Login scripts may print anything; failures are reported from exit statuses.
        .stderr(Stdio::null());
    let (mut session, stdout) = {
        let mut children = children();
        if children.cancelled {
            return Err(ScanError::new("cancelled; no report produced"));
        }
        let mut child = command
            .spawn()
            .map_err(|_| ScanError::new("cannot run ssh; SSH audits need an OpenSSH client"))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("piped stdout");
        children.next += 1;
        let id = children.next;
        children.running.push((id, child));
        (
            Session {
                destination: destination.to_string(),
                id,
                stdin,
            },
            stdout,
        )
    };
    let mut stream = Stream::new(BufReader::new(stdout));
    let header = match stream.header() {
        Ok(header) => header,
        Err(error) => {
            // Anything but an empty stdout is not a token-max stream, which may never end.
            return Err(session.fail(!error.is_eof(), |status| match status {
                Some(SSH_FAILED) => format!(
                    "SSH connection failed; check that `ssh {destination} true` works without a prompt"
                ),
                Some(NOT_INSTALLED) => format!(
                    "token-max is not installed on PATH or in ~/.cargo/bin; install it with {}",
                    install()
                ),
                Some(UNSUPPORTED) => {
                    "unsupported platform; SSH audits support Linux and macOS destinations".into()
                }
                status => format!(
                    "token-max there did not start an SSH audit ({}); install the same version with {}",
                    exit(status),
                    install()
                ),
            }));
        }
    };
    let local = wire::build();
    if header.token_max_scan != local {
        let remote = terminal_text(&header.token_max_scan);
        return Err(session.fail(true, |_| {
            format!(
                "runs token-max {remote} but this is {local}; install the same version with {}",
                install()
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
pub fn serve(since: Option<Millis>) -> u8 {
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
        let discovery = DiscoveryOptions::host()?;
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
                // Its changes are made there, so its report lists them.
                changes: true,
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
