//! max.kixel.me's run script (`run.sh`, or `run.ps1` on Windows) against fixture releases on a
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

mod support;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use support::{Releases, TARGET};

/// The cache the script fills, under each fixture's root.
impl Releases {
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
