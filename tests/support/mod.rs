//! Fixture releases served over HTTP as GitHub serves them, for the run script and doctor's
//! release check, so no test reaches GitHub.
#![allow(dead_code, reason = "each test binary uses part of it")]

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use sha2::{Digest, Sha256};

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub const TARGET: &str = "x86_64-unknown-linux-musl";
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
pub const TARGET: &str = "aarch64-unknown-linux-musl";
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub const TARGET: &str = "aarch64-apple-darwin";
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
pub const TARGET: &str = "x86_64-apple-darwin";
#[cfg(windows)]
pub const TARGET: &str = "x86_64-pc-windows-msvc";

/// `command` without this process's environment, except what Windows needs to run one:
/// without `SystemRoot`, Winsock cannot open a socket.
pub fn isolated(command: &mut Command) -> &mut Command {
    command.env_clear();
    if let Some(root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", root);
    }
    command
}

/// Releases whose latest is this build, which every doctor run checks against unless a test
/// sets its own.
pub fn current() -> &'static str {
    static CURRENT: OnceLock<Releases> = OnceLock::new();
    &CURRENT
        .get_or_init(|| {
            let releases = Releases::new();
            releases.latest(&format!("v{}", env!("CARGO_PKG_VERSION")));
            releases
        })
        .url
}

/// A releases directory served over HTTP, with GitHub's `/latest` redirect. Removed on drop.
pub struct Releases {
    pub root: PathBuf,
    /// The `/releases` URL, as `TOKEN_MAX_RELEASES` takes it.
    pub url: String,
    /// What a Windows release packs as `token-max.exe`: this build, unless a test swaps in a
    /// stand-in. Elsewhere a release packs a shell script.
    pub binary: PathBuf,
    server: Arc<Mutex<Server>>,
}

struct Server {
    latest: Option<String>,
    /// Every requested path, in order.
    log: Vec<String>,
}

impl Releases {
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "releases-{}-{}",
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
        Self {
            root,
            url,
            binary: env!("CARGO_BIN_EXE_token-max").into(),
            server,
        }
    }

    /// Make `tag` the latest release without publishing its archive.
    pub fn latest(&self, tag: &str) {
        self.server.lock().unwrap().latest = Some(tag.into());
    }

    /// Publish `tag` as the latest release, with a checksum file that lies when `corrupt`.
    /// Only the platforms with a release have an archive to publish.
    #[cfg(any(
        all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        target_os = "macos",
        windows
    ))]
    pub fn publish(&self, tag: &str, corrupt: bool) {
        let stage = self.root.join("stage").join(tag);
        let dir = self.root.join("files/releases/download").join(tag);
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let name = archive(&stage, &dir, tag.trim_start_matches('v'), &self.binary);
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
        self.latest(tag);
    }

    /// Requests for release assets so far.
    pub fn downloads(&self) -> usize {
        let log = &self.server.lock().unwrap().log;
        log.iter()
            .filter(|path| path.contains("/download/"))
            .count()
    }

    /// Requests for the latest release so far.
    pub fn checks(&self) -> usize {
        let log = &self.server.lock().unwrap().log;
        log.iter().filter(|path| path.ends_with("/latest")).count()
    }
}

impl Drop for Releases {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Packs a release binary for this target from `stage` into `dir`, as cargo-dist lays it out,
/// and returns the archive's name.
#[cfg(any(
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    target_os = "macos"
))]
fn archive(stage: &Path, dir: &Path, version: &str, _binary: &Path) -> String {
    use std::os::unix::fs::PermissionsExt;
    // A stand-in that reports its version and shows what else it was run with, so the archive
    // stays small.
    let folder = format!("token-max-{TARGET}");
    let bin = stage.join(&folder).join("token-max");
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\nif [ \"$*\" = --version ]; then echo {version}; else echo \"ran $*\"; fi\n"
        ),
    )
    .unwrap();
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
fn archive(stage: &Path, dir: &Path, _version: &str, binary: &Path) -> String {
    // A stand-in is an executable of its own, so it reports whatever version its test gives it.
    std::fs::copy(binary, stage.join("token-max.exe")).unwrap();
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

/// Answers one request: GitHub's `/latest` redirect, a file, or 404.
fn serve(stream: TcpStream, files: &Path, state: &Mutex<Server>, base: &str) {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let mut header = String::new();
    while reader.read_line(&mut header).is_ok_and(|n| n > 2) {
        header.clear();
    }
    let mut parts = line.split_whitespace();
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else {
        return;
    };
    let path = path.to_string();
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
    // A client may hang up first, as a cancelled run does.
    let _ = out.write_all(head.as_bytes());
    if method != "HEAD" {
        let _ = out.write_all(&body);
    }
}
