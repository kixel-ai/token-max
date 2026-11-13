//! max.kixel.ai's run script (`run.sh`, or `run.ps1` on Windows) against fixture releases on a
//! local server.

// Only the platforms the scripts have a release for.
#![cfg(any(
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    target_os = "macos",
    windows
))]

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const TARGET: &str = "x86_64-unknown-linux-musl";
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const TARGET: &str = "aarch64-unknown-linux-musl";
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const TARGET: &str = "aarch64-apple-darwin";
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
const TARGET: &str = "x86_64-apple-darwin";
#[cfg(windows)]
const TARGET: &str = "x86_64-pc-windows-msvc";

/// A releases directory served over HTTP, with GitHub's `/latest` redirect, and the cache the
/// script fills. Removed on drop.
struct Releases {
    root: PathBuf,
    url: String,
    server: Arc<Mutex<Server>>,
}

struct Server {
    latest: Option<String>,
    /// Every requested path, in order.
    log: Vec<String>,
}

impl Releases {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "run-script-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("home")).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/releases", listener.local_addr().unwrap());
        let server = Arc::new(Mutex::new(Server {
            latest: None,
            log: Vec::new(),
        }));
        let (files, state, base) = (root.join("files"), server.clone(), url.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                serve(stream.unwrap(), &files, &state, &base);
            }
        });
        Self { root, url, server }
    }

    /// Publish `tag` as the latest release, with a checksum file that lies when `corrupt`.
    fn publish(&self, tag: &str, corrupt: bool) {
        let stage = self.root.join("stage").join(tag);
        let dir = self.root.join("files/releases/download").join(tag);
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let name = archive(&stage, &dir);
        let bytes = std::fs::read(dir.join(&name)).unwrap();
        let mut hash: String = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if corrupt {
            hash.replace_range(..1, if hash.starts_with('0') { "1" } else { "0" });
        }
        std::fs::write(
            dir.join(format!("{name}.sha256")),
            format!("{hash}  {name}\n"),
        )
        .unwrap();
        self.server.lock().unwrap().latest = Some(tag.into());
    }

    /// Requests for release assets so far.
    fn downloads(&self) -> usize {
        let log = &self.server.lock().unwrap().log;
        log.iter()
            .filter(|path| path.contains("/download/"))
            .count()
    }

    fn cache(&self) -> PathBuf {
        self.root.join("cache/token-max/run")
    }

    /// The names in the run cache.
    fn cached(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.cache())
            .map(|dir| {
                dir.map(|entry| entry.unwrap().file_name().into_string().unwrap())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    /// Run the script as the documented one-liner would, passing `--help` on to doctor.
    fn run(&self) -> (i32, String, String) {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("site/public");
        let mut command = if cfg!(windows) {
            let mut command = Command::new("powershell");
            command
                .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass"])
                .arg("-Command")
                .arg("& ([scriptblock]::Create([IO.File]::ReadAllText($env:RUN_SCRIPT))) --help; exit $LASTEXITCODE")
                .env("RUN_SCRIPT", script.join("run.ps1"))
                .stdin(Stdio::null());
            command
        } else {
            // `curl ... | sh -s -- --help`: the script arrives on stdin.
            let mut command = Command::new("sh");
            command.args(["-s", "--", "--help"]).stdin(Stdio::piped());
            command
        };
        command
            .env("TOKEN_MAX_RELEASES", &self.url)
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("HOME", self.root.join("home"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(&std::fs::read(script.join("run.sh")).unwrap())
                .unwrap();
        }
        let Output {
            status,
            stdout,
            stderr,
        } = child.wait_with_output().unwrap();
        (
            status.code().unwrap(),
            String::from_utf8(stdout).unwrap(),
            String::from_utf8(stderr).unwrap(),
        )
    }
}

impl Drop for Releases {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Packs a release binary for this target from `stage` into `dir`, as cargo-dist lays it out,
/// and returns the archive's name.
#[cfg(unix)]
fn archive(stage: &Path, dir: &Path) -> String {
    use std::os::unix::fs::PermissionsExt;
    // A stand-in that shows what it was run with, so the archive stays small.
    let folder = format!("token-max-{TARGET}");
    let bin = stage.join(&folder).join("token-max");
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    std::fs::write(&bin, "#!/bin/sh\necho \"ran $*\"\n").unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let name = format!("{folder}.tar.xz");
    let status = Command::new("tar")
        .arg("-cJf")
        .arg(dir.join(&name))
        .arg("-C")
        .arg(stage)
        .arg(&folder)
        .status()
        .unwrap();
    assert!(status.success());
    name
}

#[cfg(windows)]
fn archive(stage: &Path, dir: &Path) -> String {
    // The real binary: a stand-in would need its own executable.
    std::fs::copy(env!("CARGO_BIN_EXE_token-max"), stage.join("token-max.exe")).unwrap();
    let name = format!("token-max-{TARGET}.zip");
    let status = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command"])
        .arg("Compress-Archive -LiteralPath $env:FROM -DestinationPath $env:TO")
        .env("FROM", stage.join("token-max.exe"))
        .env("TO", dir.join(&name))
        .status()
        .unwrap();
    assert!(status.success());
    name
}

/// What doctor prints when the script hands it `--help`.
fn expected() -> String {
    if cfg!(windows) {
        let output = Command::new(env!("CARGO_BIN_EXE_token-max"))
            .args(["doctor", "--help"])
            .output()
            .unwrap();
        String::from_utf8(output.stdout).unwrap()
    } else {
        "ran doctor --help\n".into()
    }
}

/// Answers one request: GitHub's `/latest` redirect, a file, or 404.
fn serve(stream: TcpStream, files: &Path, state: &Mutex<Server>, base: &str) {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let mut header = String::new();
    while reader.read_line(&mut header).unwrap() > 2 {
        header.clear();
    }
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next().unwrap(), parts.next().unwrap().to_string());
    let latest = {
        let mut state = state.lock().unwrap();
        state.log.push(path.clone());
        state.latest.clone()
    };
    let (status, location, body) = if path == "/releases/latest" {
        // GitHub sends /releases itself when there is no release.
        let location = match latest {
            Some(tag) => format!("{base}/tag/{tag}"),
            None => base.to_string(),
        };
        ("302 Found", Some(location), Vec::new())
    } else {
        match std::fs::read(files.join(path.trim_start_matches('/'))) {
            Ok(body) => ("200 OK", None, body),
            Err(_) => ("404 Not Found", None, Vec::new()),
        }
    };
    let mut out = &stream;
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if let Some(location) = location {
        head.push_str(&format!("Location: {location}\r\n"));
    }
    head.push_str("\r\n");
    out.write_all(head.as_bytes()).unwrap();
    if method != "HEAD" {
        out.write_all(&body).unwrap();
    }
}

/// Runs the script and checks that it ran doctor with the arguments it was given.
fn runs_doctor(releases: &Releases) {
    let (code, stdout, stderr) = releases.run();
    assert_eq!(
        (code, stdout.as_str()),
        (0, expected().as_str()),
        "{stderr}"
    );
}

fn exe(tag: &str) -> String {
    format!("token-max-{tag}-{TARGET}{}", std::env::consts::EXE_SUFFIX)
}

#[test]
fn runs_the_latest_release_and_reuses_it_until_a_newer_one_lands_beside_it() {
    let releases = Releases::new();
    releases.publish("v1.0.0", false);
    runs_doctor(&releases);
    assert_eq!(releases.downloads(), 2);
    assert_eq!(releases.cached(), [exe("v1.0.0")]);

    // Still the latest: only the redirect is fetched.
    runs_doctor(&releases);
    assert_eq!(releases.downloads(), 2);

    releases.publish("v1.0.1", false);
    runs_doctor(&releases);
    assert_eq!(releases.downloads(), 4);
    // Kept, since another run may be about to start it.
    assert_eq!(releases.cached(), [exe("v1.0.0"), exe("v1.0.1")]);
}

#[test]
fn keeps_other_platforms_binaries_in_a_shared_cache_apart() {
    let releases = Releases::new();
    releases.publish("v1.0.0", false);
    // Another platform's binary for the same release, from a home shared with it.
    let other = format!(
        "token-max-v1.0.0-other-target{}",
        std::env::consts::EXE_SUFFIX
    );
    std::fs::create_dir_all(releases.cache()).unwrap();
    std::fs::write(releases.cache().join(&other), "not this platform's").unwrap();
    runs_doctor(&releases);
    assert_eq!(releases.downloads(), 2);
    let mut both = vec![exe("v1.0.0"), other];
    both.sort();
    assert_eq!(releases.cached(), both);
}

#[test]
fn refuses_an_archive_that_does_not_match_its_checksum() {
    let releases = Releases::new();
    releases.publish("v1.0.0", true);
    let (code, stdout, stderr) = releases.run();
    assert_ne!(code, 0);
    assert_eq!(stdout, "");
    assert!(
        stderr.contains("does not match its checksum; nothing was run"),
        "{stderr}"
    );
    assert_eq!(releases.cached(), Vec::<String>::new());
}

#[test]
fn fails_when_there_is_no_release() {
    let releases = Releases::new();
    let (code, stdout, stderr) = releases.run();
    assert_ne!(code, 0);
    assert_eq!(stdout, "");
    assert!(stderr.contains("has no release yet"), "{stderr}");
    assert_eq!(releases.downloads(), 0);
}
