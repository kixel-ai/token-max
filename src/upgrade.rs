//! A newer token-max release: finding it, and putting it in place of this binary.
//!
//! Claude Code and Codex change fast enough that an old build's advice goes stale, so
//! `doctor` checks for a newer release before it reads anything and offers to upgrade.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::prices::{Discard, create_temporary, unique};
use crate::version::plain;

/// This build's version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Where the install methods are listed, for a platform with no release.
pub const INSTALL: &str = "https://github.com/kixel-ai/token-max#install";

/// How long the check may hold up a run before it gives up and says so.
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// Caps on a download and on the binary unpacked from it, each many times a release's size,
/// so a bad response fills neither the disk nor the run's time.
const ARCHIVE_LIMIT: u64 = 100 << 20;
const BINARY_LIMIT: u64 = 400 << 20;
/// More than a `.sha256` file's one line.
const CHECKSUM_LIMIT: u64 = 4 << 10;

/// The release archives' target for this build's platform, as `run.sh` names it; none where
/// no release is built.
const TARGET: Option<&str> = if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    Some("x86_64-unknown-linux-musl")
} else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
    Some("aarch64-unknown-linux-musl")
} else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
    Some("x86_64-apple-darwin")
} else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    Some("aarch64-apple-darwin")
} else if cfg!(all(windows, target_arch = "x86_64")) {
    Some("x86_64-pc-windows-msvc")
} else {
    None
};

/// Where releases are published: `TOKEN_MAX_RELEASES`, which tests point at a local copy, or
/// GitHub.
pub fn releases() -> String {
    std::env::var("TOKEN_MAX_RELEASES").unwrap_or_else(|_| crate::ssh::RELEASES.to_string())
}

/// What the check found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Check {
    /// This build is the latest release, or newer.
    Current,
    /// A newer release, by its tag, such as `v0.0.2`.
    Newer(String),
    /// Why the check could not run.
    Failed(String),
}

/// Ask `releases` for its latest release, as `run.sh` does, from GitHub's `/latest` redirect.
pub fn check(releases: &str) -> Check {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_global(Some(CHECK_TIMEOUT))
        .build()
        .into();
    let Ok(response) = agent.head(format!("{releases}/latest")).call() else {
        return Check::Failed(format!("could not reach {releases}"));
    };
    let location = response
        .status()
        .is_redirection()
        .then(|| response.headers().get("location"))
        .flatten()
        .and_then(|value| value.to_str().ok());
    let Some(location) = location else {
        return Check::Failed(format!("{releases} did not name its latest release"));
    };
    newer(location.rsplit('/').next().unwrap_or_default(), VERSION)
}

/// Whether `tag` is a release newer than `version`. GitHub sends `/releases` itself when there
/// is none, and its latest release is never a pre-release, so anything but `v` and three plain
/// numbers is unexpected.
fn newer(tag: &str, version: &str) -> Check {
    let this = plain(version).expect("the package version is plain digits");
    match tag.strip_prefix('v').and_then(plain) {
        Some(latest) if latest > this => Check::Newer(tag.to_string()),
        Some(_) => Check::Current,
        None => Check::Failed("the latest release has an unexpected tag".into()),
    }
}

/// How this binary can be upgraded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Install {
    /// Put the release for `target` in place of the binary at `exe`.
    Replace { exe: PathBuf, target: &'static str },
    /// A package manager owns the binary, and this command upgrades it.
    Manager(&'static str),
    /// Why the binary cannot be replaced here, such as an unwritable directory, so it must be
    /// installed again.
    Reinstall(String),
}

/// How the running binary can be upgraded.
pub fn install() -> Install {
    match Paths::here() {
        Ok(paths) => paths.install(),
        Err(_) => Install::Reinstall("this binary cannot find its own path".into()),
    }
}

/// Where the binary is installed, from the paths that decide who owns it.
#[derive(Clone, Debug)]
pub struct Paths {
    /// The running binary, its symlinks resolved.
    pub exe: PathBuf,
    /// `$CARGO_HOME`, or `~/.cargo`, whose `bin` holds what `cargo install` and
    /// `cargo binstall` put there.
    pub cargo: Option<PathBuf>,
}

impl Paths {
    /// This process's binary and Cargo home.
    pub fn here() -> io::Result<Self> {
        let exe = std::env::current_exe()?.canonicalize()?;
        let cargo = std::env::var_os("CARGO_HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::home_dir().map(|home| home.join(".cargo")));
        Ok(Self { exe, cargo })
    }

    /// How the binary here can be upgraded.
    pub fn install(self) -> Install {
        if self.exe.starts_with("/nix/store") {
            return Install::Manager("nix run github:kixel-ai/token-max -- doctor");
        }
        let cargo_bin = self.cargo.map(|home| home.join("bin"));
        // Canonical, as `exe` is, so a symlinked Cargo home still matches.
        let cargo_bin = cargo_bin.map(|bin| bin.canonicalize().unwrap_or(bin));
        if cargo_bin.is_some_and(|bin| self.exe.parent() == Some(bin.as_path())) {
            return Install::Manager(
                "cargo binstall token-max or cargo install --locked token-max",
            );
        }
        let Some(target) = TARGET else {
            return Install::Reinstall("no release is built for this platform".into());
        };
        match self.exe.parent() {
            Some(dir) if crate::doctor::writable(dir) => Install::Replace {
                exe: self.exe,
                target,
            },
            dir => {
                let dir = dir.unwrap_or(&self.exe).to_string_lossy();
                Install::Reinstall(format!("this user cannot write to {dir}"))
            }
        }
    }
}

/// The release `tag` from `releases`, checked against its published checksum and that it runs
/// here as that version, put in place of `exe` in one rename.
pub fn replace(releases: &str, tag: &str, exe: &Path, target: &str) -> Result<(), String> {
    let dir = exe.parent().ok_or("this binary has no directory")?;
    let (archive, member) = if cfg!(windows) {
        (
            format!("token-max-{target}.zip"),
            "token-max.exe".to_string(),
        )
    } else {
        (
            format!("token-max-{target}.tar.xz"),
            format!("token-max-{target}/token-max"),
        )
    };
    let tar = tar()?;
    // Beside the binary, so the final move is a rename on the same filesystem; registered, so
    // Ctrl-C deletes them too.
    let stem = format!(".token-max-{}", unique());
    let packed = dir.join(format!("{stem}.download"));
    let _packed = Discard(&packed);
    let unpacked = dir.join(format!("{stem}{}", std::env::consts::EXE_SUFFIX));
    let _unpacked = Discard(&unpacked);
    let cannot_write = |error: io::Error| format!("cannot write to {}: {error}", dir.display());

    let url = format!("{releases}/download/{tag}/{archive}");
    let not_downloaded = || format!("could not download {archive}");
    let mut response = ureq::get(&url).call().map_err(|_| not_downloaded())?;
    let mut file = create_temporary(&packed).map_err(cannot_write)?;
    let mut hasher = Sha256::new();
    let mut body = response
        .body_mut()
        .with_config()
        .limit(ARCHIVE_LIMIT)
        .reader();
    let mut buf = vec![0; 64 << 10];
    loop {
        let n = body.read(&mut buf).map_err(|_| not_downloaded())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n]).map_err(cannot_write)?;
    }
    drop(file);
    let actual = crate::safe::hex(&hasher.finalize());
    let sum = format!("{url}.sha256");
    let expected = ureq::get(&sum)
        .call()
        .and_then(|mut r| {
            r.body_mut()
                .with_config()
                .limit(CHECKSUM_LIMIT)
                .read_to_string()
        })
        .map_err(|_| format!("could not download {archive}.sha256"))?;
    let expected = expected.split_whitespace().next().unwrap_or_default();
    if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{archive}.sha256 holds no SHA-256"));
    }
    if !expected.eq_ignore_ascii_case(&actual) {
        return Err(format!("{archive} does not match its checksum"));
    }

    unpack(&tar, &packed, &member, &unpacked).map_err(|error| match error {
        Unpack::Write(error) => cannot_write(error),
        Unpack::Failed => format!("could not unpack {archive}"),
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(exe).map_or(0o755, |m| m.permissions().mode()) & 0o7777;
        fs::set_permissions(&unpacked, fs::Permissions::from_mode(mode)).map_err(cannot_write)?;
    }
    let version = Command::new(&unpacked)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok());
    if version.as_deref().map(str::trim) != Some(tag.trim_start_matches('v')) {
        return Err(format!("the {tag} binary does not run here as {tag}"));
    }
    swap(&unpacked, exe).map_err(|error| format!("cannot replace {}: {error}", exe.display()))
}

/// The `tar` that unpacks a release: Windows' own, which reads `.zip`, rather than whichever a
/// toolchain put first on `PATH`.
fn tar() -> Result<PathBuf, String> {
    if cfg!(windows) {
        let root = std::env::var_os("SystemRoot").ok_or("SystemRoot is not set")?;
        let tar = Path::new(&root).join("System32").join("tar.exe");
        if !tar.is_file() {
            return Err("needs Windows' tar.exe, in Windows 10 and later".into());
        }
        Ok(tar)
    } else {
        Ok(PathBuf::from("tar"))
    }
}

enum Unpack {
    Write(io::Error),
    Failed,
}

/// Write `member` of `archive` to `to`, through `tar` so no extractor is built in.
fn unpack(tar: &Path, archive: &Path, member: &str, to: &Path) -> Result<(), Unpack> {
    let mut file = create_temporary(to).map_err(Unpack::Write)?;
    let mut child = Command::new(tar)
        .arg("-xOf")
        .arg(archive)
        .arg(member)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| Unpack::Failed)?;
    let stdout = child.stdout.take().expect("piped");
    let copied = io::copy(&mut stdout.take(BINARY_LIMIT + 1), &mut file);
    let status = match copied {
        Ok(n) if n <= BINARY_LIMIT => child.wait().map_err(|_| Unpack::Failed)?,
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(match copied {
                Err(error) if error.kind() != io::ErrorKind::BrokenPipe => Unpack::Write(error),
                _ => Unpack::Failed,
            });
        }
    };
    if !status.success() {
        return Err(Unpack::Failed);
    }
    file.sync_all().map_err(Unpack::Write)
}

/// Put `new` at `exe` in one rename. Windows cannot replace a running binary, but can rename
/// it, so there the old one moves aside first and is deleted on a later run.
fn swap(new: &Path, exe: &Path) -> io::Result<()> {
    if cfg!(windows) {
        let old = aside(exe);
        match fs::remove_file(&old) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        fs::rename(exe, &old)?;
        fs::rename(new, exe).inspect_err(|_| {
            let _ = fs::rename(&old, exe);
        })
    } else {
        fs::rename(new, exe)
    }
}

/// Where Windows moves the binary an upgrade replaced.
fn aside(exe: &Path) -> PathBuf {
    let mut name = exe.as_os_str().to_owned();
    name.push(".old");
    PathBuf::from(name)
}

/// Delete the binary an earlier upgrade moved aside on Windows. It may still be running, as
/// the process that upgraded and waits on this one is, so a failure waits for a later run.
pub fn tidy(exe: &Path) {
    if cfg!(windows) {
        let _ = fs::remove_file(aside(exe));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_plain_higher_tag_is_newer() {
        assert_eq!(newer("v0.0.2", "0.0.1"), Check::Newer("v0.0.2".into()));
        assert_eq!(newer("v0.1.0", "0.0.9"), Check::Newer("v0.1.0".into()));
        assert_eq!(newer("v1.0.0", "0.99.99"), Check::Newer("v1.0.0".into()));
        assert_eq!(newer("v0.0.1", "0.0.1"), Check::Current);
        // A development build ahead of the latest release.
        assert_eq!(newer("v0.0.1", "0.0.2"), Check::Current);
        for tag in ["releases", "0.0.2", "v0.0.2-rc.1", "v0.0", "v", ""] {
            assert!(matches!(newer(tag, "0.0.1"), Check::Failed(_)), "{tag:?}");
        }
        assert!(plain(VERSION).is_some());
    }

    #[test]
    fn package_managers_keep_their_binaries() {
        let paths = |exe: &str, cargo: Option<&str>| Paths {
            exe: exe.into(),
            cargo: cargo.map(PathBuf::from),
        };
        assert!(matches!(
            paths("/nix/store/abc-token-max/bin/token-max", None).install(),
            Install::Manager(command) if command.starts_with("nix run ")
        ));
        // Paths that don't exist stay as given, so these compare without a filesystem.
        assert!(matches!(
            paths("/home/u/.cargo/bin/token-max", Some("/home/u/.cargo")).install(),
            Install::Manager(command) if command.contains("cargo install --locked token-max")
        ));
        let Some(target) = TARGET else {
            let elsewhere = paths("/home/u/.local/bin/token-max", None).install();
            assert!(matches!(elsewhere, Install::Reinstall(_)));
            return;
        };
        let w = crate::test_support::Workspace::new();
        let exe = w.home.join("token-max");
        let cargo = w.home.join(".cargo");
        let mine = paths(exe.to_str().unwrap(), cargo.to_str()).install();
        assert_eq!(
            mine,
            Install::Replace {
                exe: exe.clone(),
                target
            }
        );
        let missing = w.home.join("missing/token-max");
        assert_eq!(
            paths(missing.to_str().unwrap(), None).install(),
            Install::Reinstall(format!(
                "this user cannot write to {}",
                w.home.join("missing").display()
            ))
        );
    }
}
