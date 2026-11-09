//! `--ssh` audits through a fake `ssh` that runs each destination's command locally.
#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_token-max");

/// Destinations under one checkout-local directory whose path has a space, removed on drop.
///
/// `ssh` runs the destination's command with `sh`, its home under `homes/` and its `PATH`
/// starting with `remote/<host>`, which links the real binary unless a test replaces it.
struct Hosts {
    root: PathBuf,
}

impl Hosts {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".tmp")
            .join(format!(
                "ssh test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        std::fs::create_dir_all(root.join("bin")).unwrap();
        script(
            &root.join("bin/ssh"),
            r#"echo "$*" >> "$TEST_ROOT/ssh.args"
while [ "$1" != "--" ]; do shift; done
host=$2
echo "login noise from $host" >&2
[ "$host" = unreachable ] && exit 255
echo $$ > "$TEST_ROOT/$host.pid"
HOME="$TEST_ROOT/homes/$host" PATH="$TEST_ROOT/remote/$host:$TEST_PATH" exec sh -c "$3""#,
        );
        let hosts = Self { root };
        hosts.home("local");
        hosts
    }

    /// A destination with the real binary installed and the synthetic transcript.
    fn host(&self, name: &str) -> PathBuf {
        let bin = self.root.join("remote").join(name);
        std::fs::create_dir_all(&bin).unwrap();
        symlink(BIN, bin.join("token-max")).unwrap();
        self.home(name)
    }

    fn home(&self, name: &str) -> PathBuf {
        let home = self.root.join("homes").join(name);
        let project = home.join(".claude/projects/my project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::copy(fixture("claude.jsonl"), project.join("main.jsonl")).unwrap();
        home
    }

    /// A destination whose `name` command is this shell script instead.
    fn replace(&self, host: &str, name: &str, body: &str) {
        let bin = self.root.join("remote").join(host);
        std::fs::create_dir_all(&bin).unwrap();
        let _ = std::fs::remove_file(bin.join(name));
        script(&bin.join(name), body);
    }

    /// The coordinator, with this host's home as `local` and only the test's environment.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(BIN);
        let path = std::env::var("PATH").unwrap();
        command
            .args(["--prices-file", fixture("prices.json").to_str().unwrap()])
            .args(args)
            .env_clear()
            .env("HOME", self.root.join("homes/local"))
            .env(
                "PATH",
                format!("{}:{path}", self.root.join("bin").display()),
            )
            .env("TEST_ROOT", &self.root)
            .env("TEST_PATH", path)
            .stdin(Stdio::null());
        command
    }

    fn run(&self, args: &[&str]) -> (i32, String, String) {
        run(&mut self.command(args))
    }

    /// The first line `--ssh-scan` prints: the header naming this build.
    fn header(&self) -> String {
        // Held open: a destination stops when its stdin closes.
        let mut child = Command::new(BIN)
            .arg("--ssh-scan")
            .env_clear()
            .env("HOME", self.root.join("homes/empty"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        drop(child.stdin.take());
        child.wait().unwrap();
        line.trim_end().to_string()
    }
}

impl Drop for Hosts {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("test/fixtures")
        .join(name)
}

fn run(command: &mut Command) -> (i32, String, String) {
    let output = command.output().unwrap();
    (
        output.status.code().unwrap(),
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

/// A report's scan without the instants that differ between runs.
fn timeless(report: &Value) -> Value {
    let mut scan = report["scan"].clone();
    scan["snapshotStartedAt"] = Value::Null;
    scan["snapshotFinishedAt"] = Value::Null;
    scan
}

#[test]
fn a_remote_report_matches_running_there() {
    let hosts = Hosts::new();
    let home = hosts.host("devbox");
    let since = ["--since", "2026-11-01"];
    let (status, stdout, stderr) = hosts.run(&[
        "--ssh",
        "devbox",
        "--include-local",
        "--json",
        since[0],
        since[1],
    ]);
    assert_eq!(status, 0, "{stderr}");
    // Login noise and transcript text stay out of the output.
    assert_eq!(stderr, "");
    assert!(!stdout.contains("SYNTHETIC_PRIVATE"));
    let combined: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(combined["schemaVersion"], 1);
    let reports = combined["hosts"].as_array().unwrap();
    assert_eq!(reports.len(), 2);
    assert_eq!(reports[0]["ssh"], Value::Null);
    assert_eq!(reports[1]["ssh"], "devbox");
    let (_, there, _) = run(hosts
        .command(&["--json", since[0], since[1]])
        .env("HOME", &home));
    let there: Value = serde_json::from_str(&there).unwrap();
    assert_eq!(timeless(&reports[1]["report"]), timeless(&there));
    let file = reports[1]["report"]["scan"]["snapshots"][0]["file"]
        .as_str()
        .unwrap();
    assert!(file.starts_with(home.to_str().unwrap()), "{file}");

    let args = std::fs::read_to_string(hosts.root.join("ssh.args")).unwrap();
    for option in [
        "-T",
        "BatchMode=yes",
        "ControlPersist=no",
        "-- devbox sh -c",
    ] {
        assert!(args.contains(option), "{option}: {args}");
    }

    let (status, text, _) = hosts.run(&["--ssh", "devbox", "--include-local"]);
    assert_eq!(status, 0);
    assert!(text.starts_with("token-max: this host\n"), "{text}");
    assert!(text.contains("\ntoken-max: devbox\n"), "{text}");
    assert!(text.contains("  claude ~/.claude\n"), "{text}");
    assert!(text.contains("don't add amounts across hosts"), "{text}");
    let (_, alone, _) = hosts.run(&["--ssh", "devbox"]);
    assert!(alone.starts_with("token-max: devbox\n"), "{alone}");
    assert!(!alone.contains("this host") && !alone.contains("across hosts"));
}

#[test]
fn bash_caps_and_waits_keep_their_host_ownership() {
    let hosts = Hosts::new();
    let home = hosts.host("devbox");
    std::fs::write(
        home.join(".claude/settings.json"),
        r#"{"env":{"BASH_MAX_TIMEOUT_MS":"7200000"}}"#,
    )
    .unwrap();
    let transcript = home.join(".claude/projects/my project/main.jsonl");
    let mut records: Vec<Value> = std::fs::read_to_string(&transcript)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    records[1]["message"]["content"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "type": "tool_use", "id": "wait", "name": "Bash",
            "input": {"command": "until test -f SYNTHETIC_PRIVATE; do sleep 30; done"}
        }));
    records.insert(3, serde_json::json!({
        "type": "user", "sessionId": "session-a", "timestamp": "2026-11-01T00:06:00Z",
        "message": {"content": [{"type": "tool_result", "tool_use_id": "wait", "content": "SYNTHETIC_PRIVATE_OUTPUT"}]},
        "toolUseResult": {"timedOutAfterMs": 360000}
    }));
    let lines: String = records.iter().map(|r| format!("{r}\n")).collect();
    std::fs::write(&transcript, lines).unwrap();

    let (status, stdout, stderr) = hosts.run(&["--ssh", "devbox", "--include-local", "--json"]);
    assert_eq!(status, 1, "{stderr}");
    assert_eq!(stderr, "");
    assert!(!stdout.contains("SYNTHETIC_PRIVATE"));
    let combined: Value = serde_json::from_str(&stdout).unwrap();
    let reports = combined["hosts"].as_array().unwrap();
    let cap = |report: &Value| -> Value {
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["check"] == "bash-timeout-cap")
            .unwrap()
            .clone()
    };
    let local = cap(&reports[0]["report"]);
    let remote = cap(&reports[1]["report"]);
    assert_eq!(local["current"], 600000);
    assert_eq!(local["action"], "keep");
    assert_eq!(local["waits"]["calls"], 0);
    assert_eq!(remote["current"], 7200000);
    assert_eq!(remote["recommended"], 3300000);
    assert_eq!(remote["action"], "change");
    for field in ["calls", "long", "timedOut", "polling"] {
        assert_eq!(remote["waits"][field], 1, "{field}");
    }
    assert!(remote["waits"]["missCost"]["totalUsd"].as_f64().unwrap() > 0.0);
    let (status, there, stderr) = run(hosts.command(&["--json"]).env("HOME", &home));
    assert_eq!(status, 1, "{stderr}");
    let there: Value = serde_json::from_str(&there).unwrap();
    assert_eq!(timeless(&reports[1]["report"]), timeless(&there));
    let there = cap(&there);
    assert_eq!(remote["waits"], there["waits"]);
    assert_eq!(remote["recommended"], there["recommended"]);
}

#[test]
fn any_failed_destination_fails_the_whole_report() {
    let hosts = Hosts::new();
    hosts.host("devbox");
    hosts.home("bare");
    hosts.host("windows");
    hosts.replace("windows", "uname", "echo Windows_NT");
    hosts.replace("old", "token-max", "echo 'invalid arguments' >&2; exit 2");
    let header = hosts.header();
    assert!(header.starts_with("{\"tokenMaxScan\":"), "{header}");
    let other = header.replacen("\"tokenMaxScan\":\"", "\"tokenMaxScan\":\"9.9.9+", 1);
    hosts.replace(
        "other",
        "token-max",
        &format!("echo '{other}'; exec sleep 600"),
    );
    hosts.replace(
        "corrupt",
        "token-max",
        &format!("echo '{header}'; echo '{{\"scan\":{{\"since\":'"),
    );
    hosts.replace("silent", "token-max", &format!("echo '{header}'; exit 0"));
    let broken = hosts.host("broken");
    std::fs::write(
        broken.join(".claude/projects/my project/main.jsonl"),
        r#"{"timestamp":"2026-11-02T00:00:00Z","type":"assistant","private":"SYNTHETIC_PRIVATE_REPLY","message":{"usage":{"input_tokens":"bad"}}}
"#,
    )
    .unwrap();
    for (host, expected) in [
        (
            "unreachable",
            "unreachable: SSH connection failed; check that `ssh unreachable true` works",
        ),
        (
            "bare",
            "bare: token-max is not installed on PATH or in ~/.cargo/bin; install it with `cargo install --locked",
        ),
        ("windows", "windows: unsupported platform"),
        (
            "old",
            "old: token-max there did not start an SSH audit (exit 2); install the same version with `cargo install",
        ),
        ("other", "other: runs token-max 9.9.9+"),
        ("corrupt", "corrupt: invalid SSH audit stream at line "),
        ("silent", "silent: invalid SSH audit stream at line 2"),
        ("broken", "broken: "),
    ] {
        let (status, stdout, stderr) =
            hosts.run(&["--ssh", "devbox", "--ssh", host, "--include-local"]);
        assert_eq!(status, 2, "{host}: {stderr}");
        assert_eq!(stdout, "", "{host}");
        assert!(
            stderr.starts_with(&format!("token-max: {expected}")),
            "{host}: {stderr}"
        );
        assert!(!stderr.contains("SYNTHETIC_PRIVATE") && !stderr.contains("noise"));
    }
    let (status, _, stderr) = hosts.run(&["--ssh", "broken", "--json"]);
    assert_eq!(status, 2);
    let diagnostic: Value = serde_json::from_str(&stderr).unwrap();
    assert_eq!(diagnostic["location"]["line"], 1);
    // A mismatched destination is stopped, not left scanning.
    let pid: i32 = std::fs::read_to_string(hosts.root.join("other.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // SAFETY: signal 0 only checks whether the process exists.
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
}

#[test]
fn cancelling_stops_and_reaps_every_destination() {
    let hosts = Hosts::new();
    let ready = hosts.root.join("ready");
    let fifo = std::ffi::CString::new(ready.to_str().unwrap()).unwrap();
    // SAFETY: mkfifo creates a named pipe at a path this test owns.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let header = hosts.header();
    hosts.replace(
        "slow",
        "token-max",
        &format!("echo '{header}'; echo > \"$TEST_ROOT/ready\"; exec sleep 600"),
    );
    for signal in [libc::SIGINT, libc::SIGTERM] {
        let child = hosts
            .command(&["--ssh", "slow"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // The destination is mid-scan once it opens the pipe.
        std::fs::read(&ready).unwrap();
        // SAFETY: kill only signals the child this test spawned.
        assert_eq!(unsafe { libc::kill(child.id() as i32, signal) }, 0);
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(output.stdout, b"");
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .contains("cancelled")
        );
        let pid: i32 = std::fs::read_to_string(hosts.root.join("slow.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // SAFETY: signal 0 only checks whether the process exists.
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            -1,
            "destination still running"
        );
    }
}

#[test]
fn destinations_and_directory_flags_are_unambiguous() {
    let hosts = Hosts::new();
    for (args, message) in [
        (
            &["--ssh=-oProxyCommand=x"][..],
            "--ssh requires a destination",
        ),
        (&["--ssh", "two words"], "--ssh requires a destination"),
        (&["--ssh", "a", "--ssh", "a"], "--ssh a is selected twice"),
        (&["--include-local"], "--include-local requires --ssh"),
        (&["--ssh", "a", "--claude-dir", "x"], "add --include-local"),
        (&["--ssh-scan", "--json"], "invalid arguments"),
        (&["--ssh", "-x"], "invalid arguments"),
    ] {
        let (status, stdout, stderr) = hosts.run(args);
        assert_eq!(status, 2, "{args:?}");
        assert_eq!(stdout, "");
        assert!(stderr.contains(message), "{args:?}: {stderr}");
    }
    assert!(!hosts.root.join("ssh.args").exists());
}
