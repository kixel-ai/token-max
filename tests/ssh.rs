//! `--ssh` audits through a fake `ssh` that runs each destination's command locally.
#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use token_max::controls::Env;
use token_max::discovery::DiscoveryOptions;
use token_max::model::{
    Action, BackgroundCheck, BackgroundFeature, CheckResult, ContextKind, Harness, Report,
};
use token_max::prices::{PriceOptions, load_prices};
use token_max::reader::{ScanOptions, scan};
use token_max::replay::{AnalyzeOptions, analyze};

const BIN: &str = env!("CARGO_BIN_EXE_token-max");

/// Destinations under one target scratch directory whose path has a space, removed on drop.
///
/// `ssh` runs the destination's command with `sh`, its home under `homes/` and its `PATH`
/// of `remote/<host>`, which links the real binary unless a test replaces it, then `tools/`.
struct Hosts {
    root: PathBuf,
}

impl Hosts {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "ssh test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("bin")).unwrap();
        // Destinations get only the commands their scripts run, so a token-max the developer
        // installed can't stand in for the one under test.
        let tools = root.join("tools");
        std::fs::create_dir_all(&tools).unwrap();
        let path = std::env::var_os("PATH").unwrap();
        for tool in ["sh", "uname", "sleep"] {
            let found = std::env::split_paths(&path)
                .map(|dir| dir.join(tool))
                .find(|file| file.is_file())
                .unwrap_or_else(|| panic!("{tool} is not on PATH"));
            let found = std::env::current_dir().unwrap().join(found);
            symlink(found, tools.join(tool)).unwrap();
        }
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
        std::fs::write(
            home.join(".claude/settings.json"),
            r#"{"promptCacheTtl":"1h"}"#,
        )
        .unwrap();
        home
    }

    /// A destination whose `name` command is this shell script instead.
    fn replace(&self, host: &str, name: &str, body: &str) {
        let bin = self.root.join("remote").join(host);
        std::fs::create_dir_all(&bin).unwrap();
        let _ = std::fs::remove_file(bin.join(name));
        script(&bin.join(name), body);
    }

    /// The coordinator's `doctor`, with this host's home as `local` and only the test's environment.
    fn command(&self, args: &[&str]) -> Command {
        self.invoke("doctor", args)
    }

    /// The coordinator running `name`, as [`Hosts::command`] runs `doctor`.
    fn invoke(&self, name: &str, args: &[&str]) -> Command {
        let mut command = Command::new(BIN);
        let path = std::env::var("PATH").unwrap();
        command
            .args([
                name,
                "--prices-file",
                fixture("prices.json").to_str().unwrap(),
            ])
            .args(args)
            .env_clear()
            .env("HOME", self.root.join("homes/local"))
            .env(
                "PATH",
                format!("{}:{path}", self.root.join("bin").display()),
            )
            .env("TEST_ROOT", &self.root)
            .env("TEST_PATH", self.root.join("tools"))
            .stdin(Stdio::null());
        command
    }

    fn run(&self, args: &[&str]) -> (i32, String, String) {
        run(&mut self.command(args))
    }

    /// The first line `doctor --ssh-scan` prints: the header naming this build.
    fn header(&self) -> String {
        // Held open: a destination stops when its stdin closes.
        let mut child = Command::new(BIN)
            .args(["doctor", "--ssh-scan"])
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

/// The analysis a destination runs on its own home, through the library.
fn analysis(home: &Path, since: Option<i64>, max_cost_per_use_usd: f64) -> Report {
    let home = home.to_str().unwrap();
    let catalog = load_prices(&PriceOptions {
        file: Some(fixture("prices.json")),
        cache: PathBuf::new(),
        offline: false,
    })
    .unwrap();
    let discovery = DiscoveryOptions {
        home: home.into(),
        env: Env::default(),
        claude_dirs: vec![],
        codex_dirs: vec![],
        t3_dirs: vec![],
        managed_settings_file: format!("{home}/managed-settings.json"),
        codex_system_config_file: format!("{home}/system.toml"),
        codex_requirements_file: format!("{home}/requirements.toml"),
    };
    analyze(AnalyzeOptions {
        scan: scan(&ScanOptions { discovery, since }).unwrap(),
        catalog: &catalog,
        since,
        now: NOW,
        max_cost_per_use_usd,
    })
    .unwrap()
}

/// An instant after every fixture request, so analyses don't depend on the wall clock.
const NOW: i64 = 1_796_083_200_000;
/// 2026-11-01T00:00:00Z.
const NOVEMBER: i64 = 1_793_491_200_000;
const MINUTE: i64 = 60_000;

/// One host's section of a report covering several, without its heading.
fn section<'a>(text: &'a str, host: &str) -> &'a str {
    let heading = format!("token-max: {host}\n");
    let start = text.find(&heading).unwrap_or_else(|| panic!("{text}")) + heading.len();
    let rest = &text[start..];
    let end = ["\ntoken-max: ", "\nEach host is analyzed on its own."]
        .iter()
        .filter_map(|marker| rest.find(marker))
        .min()
        .unwrap_or(rest.len());
    &rest[..end]
}

/// A report without what depends on each run's own clock: instants, dates, day counts, and
/// dollar amounts, since a per-30-day rate divides by the window that ends at that clock.
fn timeless(text: &str) -> String {
    let text = text.strip_prefix("token-max\n").unwrap_or(text);
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let digits = |at: usize| b[at..].iter().take_while(|c| c.is_ascii_digit()).count();
    while i < b.len() {
        let date = i + 10 <= b.len()
            && digits(i) == 4
            && b[i + 4] == b'-'
            && digits(i + 5) == 2
            && b[i + 7] == b'-'
            && digits(i + 8) == 2;
        if date {
            i += 10;
            if b.get(i) == Some(&b'T') {
                i += 1 + b[i + 1..]
                    .iter()
                    .take_while(|c| c.is_ascii_digit() || matches!(c, b':' | b'.' | b'Z'))
                    .count();
            }
            out.push_str("<date>");
        } else if b[i] == b'$' {
            i += 1 + b[i + 1..]
                .iter()
                .take_while(|c| c.is_ascii_digit() || matches!(c, b'.' | b','))
                .count();
            out.push_str("$<usd>");
        } else if b[i].is_ascii_digit() && text[i + digits(i)..].starts_with(" day") {
            i += digits(i);
            out.push_str("<n>");
        } else {
            let c = text[i..].chars().next().unwrap();
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

fn background(report: &Report) -> Vec<&BackgroundCheck> {
    report
        .checks
        .iter()
        .filter_map(|c| match c {
            CheckResult::Background(b) => Some(b),
            _ => None,
        })
        .collect()
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
        "--details",
        since[0],
        since[1],
    ]);
    assert_eq!(status, 0, "{stderr}");
    // Login noise and transcript text stay out of the output.
    assert_eq!(stderr, "");
    assert!(!stdout.contains("SYNTHETIC_PRIVATE"));
    for host in ["this host", "devbox"] {
        assert!(section(&stdout, host).contains("; 2 requests;"), "{host}");
    }
    let (_, there, _) = run(hosts
        .command(&["--details", since[0], since[1]])
        .env("HOME", &home));
    assert_eq!(timeless(section(&stdout, "devbox")), timeless(&there));

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
    assert_eq!(
        timeless(section(&alone, "devbox")),
        timeless(section(&text, "devbox"))
    );
}

#[test]
fn keepalive_retains_costs_unknowns_and_caps_over_ssh() {
    let hosts = Hosts::new();
    let home = hosts.host("devbox");
    let fixture: Vec<Value> = std::fs::read_to_string(fixture("claude.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let mut records = vec![];
    for (i, (session, time, read, write)) in [
        ("sample", "00:00:00", 19_000, 1000),
        ("sample", "00:01:00", 20_000, 1500),
        ("main", "00:10:00", 100_000, 1000),
        ("main", "00:40:00", 100_000, 1000),
        ("main", "02:40:00", 0, 101_000),
        ("unpriced", "00:20:00", 0, 1000),
    ]
    .into_iter()
    .enumerate()
    {
        let mut r = fixture[1].clone();
        r.as_object_mut().unwrap().remove("requestRef");
        r["sessionId"] = session.into();
        r["timestamp"] = format!("2026-11-01T{time}Z").into();
        r["uuid"] = format!("block-{i}").into();
        r["message"]["id"] = format!("message-{i}").into();
        let u = &mut r["message"]["usage"];
        u["output_tokens"] = 500.into();
        u["cache_read_input_tokens"] = read.into();
        u["cache_creation_input_tokens"] = write.into();
        u["cache_creation"]["ephemeral_5m_input_tokens"] = 0.into();
        u["cache_creation"]["ephemeral_1h_input_tokens"] = write.into();
        if session == "unpriced" {
            r["message"]["model"] = "unknown-model".into();
        }
        records.push(r);
    }
    std::fs::write(
        home.join(".claude/projects/my project/main.jsonl"),
        records.iter().map(|r| format!("{r}\n")).collect::<String>(),
    )
    .unwrap();
    let report = analysis(&home, Some(NOVEMBER + 10 * MINUTE), 1.0);
    assert_eq!(report.scan.cache_context.requests.len(), 2);
    let check = report
        .checks
        .iter()
        .find_map(|c| match c {
            CheckResult::Keepalive(k) => Some(k),
            _ => None,
        })
        .unwrap();
    assert_eq!(check.base.current, None);
    assert_eq!(check.base.action, Action::Advisory);
    assert_eq!(check.base.savings_usd, None);
    assert_eq!(check.margin_usd, None);
    assert_eq!(check.ping_input_tokens, Some(1000));
    assert!(check.base.coverage.unwrap().priced > 0);
    assert!(check.unpriced > 0);
    assert!(check.scenarios.len() >= 2);
    for cap in &check.scenarios {
        assert!(cap.ping_cost_usd > 0.0);
        assert!(cap.closed_savings_usd > cap.savings_usd);
    }

    let args = ["--details", "--since", "2026-11-01T00:10:00Z"];
    let (status, stdout, stderr) = hosts.run(&[&["--ssh", "devbox"][..], &args].concat());
    assert_ne!(status, 2, "{stderr}");
    assert!(stderr.is_empty(), "{stderr}");
    assert!(!stdout.contains("SYNTHETIC_PRIVATE"));
    let remote = section(&stdout, "devbox");
    assert!(remote.contains("keepalive"), "{remote}");
    let (_, there, stderr) = run(hosts.command(&args).env("HOME", &home));
    assert!(stderr.is_empty(), "{stderr}");
    assert_eq!(timeless(remote), timeless(&there));
}

#[test]
fn bash_caps_and_waits_keep_their_host_ownership() {
    let hosts = Hosts::new();
    let home = hosts.host("devbox");
    std::fs::write(
        home.join(".claude/settings.json"),
        r#"{"promptCacheTtl":"1h","env":{"BASH_MAX_TIMEOUT_MS":"7200000"}}"#,
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

    let cap = |home: &Path| {
        analysis(home, None, 1.0)
            .checks
            .into_iter()
            .find_map(|c| match c {
                CheckResult::BashCap(b) => Some(b),
                _ => None,
            })
            .unwrap()
    };
    let local = cap(&hosts.root.join("homes/local"));
    let remote = cap(&home);
    assert_eq!(local.base.current, Some(600_000));
    assert_eq!(local.base.action, Action::Keep);
    assert_eq!(local.waits.calls, 0);
    assert_eq!(remote.base.current, Some(7_200_000));
    assert_eq!(remote.base.recommended, Some(3_300_000));
    assert_eq!(remote.base.action, Action::Change);
    let w = &remote.waits;
    assert_eq!((w.calls, w.long, w.timed_out, w.polling), (1, 1, 1, 1));
    assert!(w.miss_cost.total_usd.unwrap().to_f64() > 0.0);

    let (status, stdout, stderr) = hosts.run(&["--ssh", "devbox", "--include-local", "--details"]);
    assert_eq!(status, 1, "{stderr}");
    assert_eq!(stderr, "");
    assert!(!stdout.contains("SYNTHETIC_PRIVATE"));
    assert!(
        section(&stdout, "devbox").contains("BASH_MAX_TIMEOUT_MS"),
        "{stdout}"
    );
    // The destination's change is listed as an edit to make there, in its own home.
    assert!(
        section(&stdout, "devbox").contains(
            "  Bash timeout cap: 2h -> 55m (Measured)\n    In ~/.claude/settings.json:\n      \"env\": {\"BASH_MAX_TIMEOUT_MS\": \"3300000\"}\n"
        ),
        "{stdout}"
    );
    let (status, there, stderr) = run(hosts.command(&["--details"]).env("HOME", &home));
    assert_eq!(status, 1, "{stderr}");
    assert_eq!(timeless(section(&stdout, "devbox")), timeless(&there));
}

#[test]
fn codex_context_and_output_keep_their_remote_controls() {
    let hosts = Hosts::new();
    let home = hosts.host("devbox");
    let codex = home.join(".codex");
    std::fs::create_dir_all(codex.join("sessions")).unwrap();
    std::fs::write(
        codex.join("config.toml"),
        "tool_output_token_limit = 7000\n[mcp_servers.docs]\nurl = 'https://synthetic.invalid/mcp'\n",
    )
    .unwrap();
    let fixture: Vec<Value> = std::fs::read_to_string(fixture("codex.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let mut records = vec![
        fixture[0].clone(),
        fixture[1].clone(),
        serde_json::json!({
            "type": "world_state", "timestamp": "2026-11-01T00:00:00Z",
            "payload": {"full": true, "state": {"tools": {"mcp__docs": "SYNTHETIC_PRIVATE_SCHEMA"}}}
        }),
        serde_json::json!({
            "type": "response_item", "timestamp": "2026-11-01T00:00:00Z",
            "payload": {"type": "function_call", "name": "mcp__docs__read", "call_id": "read-1", "arguments": "{}"}
        }),
        serde_json::json!({
            "type": "response_item", "timestamp": "2026-11-01T00:00:00Z",
            "payload": {"type": "function_call_output", "call_id": "read-1", "output": "SYNTHETIC_PRIVATE_OUTPUT"}
        }),
        fixture[2].clone(),
    ];
    let mut next = fixture[2].clone();
    next["timestamp"] = "2026-11-01T00:00:02Z".into();
    next["payload"]["response_id"] = "response-b".into();
    records.push(next.clone());
    records.push(serde_json::json!({
        "type": "compacted", "timestamp": "2026-11-01T00:00:03Z", "payload": {}
    }));
    next["timestamp"] = "2026-11-01T00:00:04Z".into();
    next["payload"]["response_id"] = "response-c".into();
    records.push(next);
    let local = analysis(&hosts.root.join("homes/local"), None, 1.0);
    assert!(local.context.is_empty());
    for priceable in [false, true] {
        // Unknown offsets across cache categories remain unknown even with a tier estimate.
        // A cache-free request prices every possible offset at the same ordinary rate.
        if priceable {
            for record in &mut records {
                if record["type"] == "token_usage_record" {
                    record["payload"]["usage"]["cached_input_tokens"] = 0.into();
                    record["payload"]["usage"]["cache_write_input_tokens"] = 0.into();
                }
            }
        }
        let lines: String = records.iter().map(|r| format!("{r}\n")).collect();
        std::fs::write(codex.join("sessions/rollout.jsonl"), lines).unwrap();

        let remote = analysis(&home, None, 1.0);
        let server = remote
            .context
            .iter()
            .find(|r| r.kind == ContextKind::McpServer)
            .unwrap();
        assert_eq!(server.harness, Harness::Codex);
        assert_eq!(server.name, "docs");
        assert_eq!(
            server.control.as_ref().unwrap().path,
            ["mcp_servers", "docs", "enabled"]
        );
        assert_eq!(server.estimated_usd.is_some(), priceable, "{server:?}");
        assert_eq!(server.conditional_requests, server.priced);
        assert!(server.assumptions.iter().any(|a| a.contains("conditional")));
        assert_eq!(server.verdict, None);
        // The remote keeps the same two recorded Claude tiers as this host.
        // Adding Codex estimates must not increase that billing-evidence coverage.
        assert_eq!(
            remote.scan.coverage.tier_known,
            local.scan.coverage.tier_known
        );
        let output = remote
            .context
            .iter()
            .find(|r| r.kind == ContextKind::ToolOutput)
            .unwrap();
        assert_eq!(output.name, "mcp__docs");
        // Both requests retain the result; compaction removes it before the third.
        assert_eq!(output.requests, 2);
        assert_eq!(output.calls, Some(1));
        assert_eq!(
            output.largest_characters,
            Some("SYNTHETIC_PRIVATE_OUTPUT".len() as i64)
        );
        assert_eq!(output.priced, if priceable { 2 } else { 0 });
        assert_eq!(output.estimated_usd.is_some(), priceable);

        let (status, stdout, stderr) =
            hosts.run(&["--ssh", "devbox", "--include-local", "--details"]);
        assert_eq!(status, 0, "{stderr}");
        assert_eq!(stderr, "");
        assert!(!stdout.contains("SYNTHETIC_PRIVATE"));
        let (_, there, stderr) = run(hosts.command(&["--details"]).env("HOME", &home));
        assert_eq!(stderr, "");
        assert_eq!(timeless(section(&stdout, "devbox")), timeless(&there));
        let (_, alone, stderr) = hosts.run(&["--ssh", "devbox", "--details"]);
        assert_eq!(stderr, "");
        assert_eq!(timeless(section(&alone, "devbox")), timeless(&there));

        let (_, text, stderr) = hosts.run(&["--ssh", "devbox"]);
        assert_eq!(stderr, "");
        assert!(!text.contains("SYNTHETIC_PRIVATE"));
        assert!(text.contains("Conditional Codex pricing"), "{text}");
        if priceable {
            assert!(
                text.contains("tier sensitivity could reverse keep or off"),
                "{text}"
            );
        }
        assert!(
            text.contains("tool_output_token_limit is 7,000 tokens"),
            "{text}"
        );
    }
}

#[test]
fn background_controls_and_estimates_keep_their_remote_history() {
    let hosts = Hosts::new();
    let home = hosts.host("devbox");
    let local = hosts.root.join("homes/local");
    std::fs::write(
        local.join(".claude/settings.json"),
        r#"{"promptSuggestionEnabled":false,"awaySummaryEnabled":false,"env":{"CLAUDE_CODE_GOAL_CHECKIN_MINUTES":"0"}}"#,
    )
    .unwrap();
    std::fs::write(
        home.join(".claude/settings.json"),
        r#"{"promptCacheTtl":"5m","promptSuggestionEnabled":false,"env":{"CLAUDE_CODE_ENABLE_AWAY_SUMMARY":"0","CLAUDE_CODE_GOAL_CHECKIN_MINUTES":"0"}}"#,
    )
    .unwrap();
    for (root, enabled) in [(&local, false), (&home, true)] {
        let workspace = root.join("workspace");
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        std::fs::create_dir_all(workspace.join(".claude")).unwrap();
        std::fs::create_dir_all(workspace.join(".codex")).unwrap();
        std::fs::write(
            workspace.join(".claude/settings.json"),
            serde_json::json!({
                "promptSuggestionEnabled": enabled,
                "env": {"CLAUDE_CODE_ENABLE_AWAY_SUMMARY": if enabled {"1"} else {"0"},
                        "CLAUDE_CODE_GOAL_CHECKIN_MINUTES": if enabled {"7"} else {"0"},
                        "SECRET": "SYNTHETIC_PRIVATE_SETTINGS"}
            })
            .to_string(),
        )
        .unwrap();
        let codex = root.join(".codex");
        std::fs::create_dir_all(codex.join("sessions")).unwrap();
        let history: String = std::fs::read_to_string(fixture("codex.jsonl"))
            .unwrap()
            .lines()
            .map(|line| {
                let mut record: Value = serde_json::from_str(line).unwrap();
                if record["type"] == "session_meta" || record["type"] == "turn_context" {
                    record["payload"]["cwd"] = workspace.to_str().unwrap().into();
                }
                record["timestamp"] = record["timestamp"]
                    .as_str()
                    .unwrap()
                    .replace("T00:00:", "T00:02:")
                    .into();
                format!("{record}\n")
            })
            .collect();
        std::fs::write(codex.join("sessions/rollout.jsonl"), history).unwrap();
        std::fs::write(
            codex.join("config.toml"),
            format!(
                "[tui]\nauto_recap = {}\n[projects.{}]\ntrust_level = 'trusted'\n",
                !enabled,
                serde_json::to_string(&workspace).unwrap()
            ),
        )
        .unwrap();
        std::fs::write(
            workspace.join(".codex/config.toml"),
            format!("[tui]\nauto_recap = {enabled}\n"),
        )
        .unwrap();
        let file = root.join(".claude/projects/my project/main.jsonl");
        let history: String = std::fs::read_to_string(&file)
            .unwrap()
            .lines()
            .map(|line| {
                let mut record: Value = serde_json::from_str(line).unwrap();
                record["cwd"] = workspace.to_str().unwrap().into();
                format!("{record}\n")
            })
            .collect();
        std::fs::write(file, history).unwrap();
    }
    let response: Value = std::fs::read_to_string(fixture("claude.jsonl"))
        .unwrap()
        .lines()
        .nth(2)
        .map(|line| serde_json::from_str(line).unwrap())
        .unwrap();
    let mut records = vec![];
    for minute in [0, 1, 2, 10] {
        let time = format!("2026-11-01T00:{minute:02}:00Z");
        records.push(serde_json::json!({
            "type": "user", "sessionId": "session-a", "uuid": format!("user-{minute}"),
            "timestamp": time, "entrypoint": "cli", "cwd": home.join("workspace"),
            "message": {"content": "SYNTHETIC_PRIVATE_PROMPT"}
        }));
        let mut r = response.clone();
        r["timestamp"] = time.into();
        r["entrypoint"] = "cli".into();
        r["cwd"] = home.join("workspace").to_str().unwrap().into();
        r["message"]["id"] = format!("response-{minute}").into();
        r["message"]["usage"]["cache_read_input_tokens"] = 1000.into();
        r["message"]["content"] = serde_json::json!([
            {"type": "text", "text": "SYNTHETIC_PRIVATE_REPLY"}
        ]);
        records.push(r.clone());
        r["timestamp"] = format!("2026-11-01T00:{minute:02}:01Z").into();
        records.push(r);
    }
    let lines: String = records.iter().map(|r| format!("{r}\n")).collect();
    std::fs::write(home.join(".claude/projects/my project/main.jsonl"), lines).unwrap();

    // Two earlier turns establish eligibility for the first selected turn's recap.
    let since = Some(NOVEMBER + 2 * MINUTE);
    let local = analysis(&local, since, 0.000001);
    let remote = analysis(&home, since, 0.000001);
    assert_eq!(background(&local).len(), 4);
    assert!(
        background(&local)
            .iter()
            .all(|c| c.base.current == Some(false) && c.base.action == Action::Keep),
        "{:?}",
        background(&local)
    );
    let checks = background(&remote);
    assert_eq!(checks.len(), 4);
    assert!(checks.iter().all(|c| c.base.current == Some(true)));
    let feature = |name| *checks.iter().find(|c| c.feature == name).unwrap();
    let settings = home.join("workspace/.claude/settings.json");
    let suggestions = feature(BackgroundFeature::PromptSuggestions);
    assert_eq!(suggestions.generations, Some(2));
    assert_eq!(suggestions.base.action, Action::Advisory);
    assert!(suggestions.cost.total_usd.unwrap().to_f64() > 0.0);
    assert_eq!(suggestions.off, "\"promptSuggestionEnabled\": false");
    assert_eq!(suggestions.off_file.as_deref(), settings.to_str());
    let recaps = feature(BackgroundFeature::SessionRecaps);
    assert_eq!(recaps.generations, Some(1));
    assert!(recaps.cost.total_usd.unwrap().to_f64() > 0.0);
    assert!(recaps.lost_warming.total_usd.unwrap().to_f64() > 0.0);
    assert_eq!(
        recaps.off,
        "\"env\": {\"CLAUDE_CODE_ENABLE_AWAY_SUMMARY\": \"0\"}"
    );
    assert_eq!(recaps.off_file.as_deref(), settings.to_str());
    for name in [
        BackgroundFeature::GoalCheckIns,
        BackgroundFeature::AutoRecaps,
    ] {
        let c = feature(name);
        assert_eq!(c.base.action, Action::Advisory);
        assert_eq!(c.generations, None);
        assert_eq!(c.cost.total_usd, None);
    }
    let claude = remote
        .scan
        .profiles
        .iter()
        .find(|p| p.harness == Harness::Claude)
        .unwrap();
    assert_eq!(
        claude.background_directories[home.join("workspace").to_str().unwrap()][2].minutes,
        Some(7)
    );
    assert_eq!(remote.scan.cache_context.prompts.len(), 2);
    assert_eq!(remote.scan.cache_context.requests.len(), 2);

    let args = [
        "--details",
        "--since",
        "2026-11-01T00:02:00Z",
        "--max-cost-per-use",
        "0.000001",
    ];
    let (status, stdout, stderr) =
        hosts.run(&[&["--ssh", "devbox", "--include-local"][..], &args].concat());
    assert_eq!(status, 1, "{stderr}");
    assert_eq!(stderr, "");
    assert!(!stdout.contains("SYNTHETIC_PRIVATE"));
    let (status, there, stderr) = run(hosts.command(&args).env("HOME", &home));
    assert_eq!(status, 1, "{stderr}");
    assert_eq!(stderr, "");
    assert_eq!(timeless(section(&stdout, "devbox")), timeless(&there));
    let (_, text, stderr) = hosts.run(&["--ssh", "devbox"]);
    assert_eq!(stderr, "");
    assert!(!text.contains("SYNTHETIC_PRIVATE"));
    for label in [
        "Prompt suggestions",
        "Session recaps",
        "Goal check-ins",
        "Codex automatic recaps",
    ] {
        assert!(text.contains(label), "{text}");
    }
    assert!(text.contains("Not enough evidence: cost unknown"), "{text}");
    assert!(
        text.contains("tui.auto_recap = false in ~/workspace/.codex/config.toml"),
        "{text}"
    );
}

#[test]
fn the_release_installers_directory_is_searched() {
    let hosts = Hosts::new();
    let home = hosts.home("installed");
    let bin = home.join(".local/bin");
    std::fs::create_dir_all(&bin).unwrap();
    symlink(BIN, bin.join("token-max")).unwrap();
    // An older `cargo install` loses to the installers' copy.
    let cargo = home.join(".cargo/bin");
    std::fs::create_dir_all(&cargo).unwrap();
    script(&cargo.join("token-max"), "exit 2");
    let (status, stdout, stderr) = hosts.run(&["--ssh", "installed"]);
    assert_eq!(status, 0, "{stderr}");
    assert!(stdout.starts_with("token-max: installed\n"), "{stdout}");
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
    // The hint names the command that installs this build there.
    let install = match env!("CARGO_PKG_VERSION") {
        "0.0.0" => {
            "`cargo install --locked --git https://github.com/kixel-ai/token-max` on both hosts"
                .to_string()
        }
        version => format!(
            "`curl -LsSf https://max.kixel.ai/install.sh | sh` on both hosts, or `cargo install --locked token-max --version {version}` there"
        ),
    };
    let bare = format!(
        "bare: token-max is not installed on PATH, in the release installers' directory or in ~/.cargo/bin; install it with {install}\n"
    );
    let old = format!(
        "old: token-max there did not start an SSH audit (exit 2); install the same version with {install}\n"
    );
    for (host, expected) in [
        (
            "unreachable",
            "unreachable: SSH connection failed; check that `ssh unreachable true` works",
        ),
        ("bare", bare.as_str()),
        ("windows", "windows: unsupported platform"),
        ("old", old.as_str()),
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
    let (status, _, stderr) = hosts.run(&["--ssh", "broken"]);
    assert_eq!(status, 2);
    assert!(stderr.ends_with(":1\n"), "{stderr}");
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
fn prices_and_destinations_stop_the_run_whichever_fails_first() {
    let hosts = Hosts::new();
    let mkfifo = |name: &str| {
        let path = hosts.root.join(name);
        let fifo = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        // SAFETY: mkfifo creates a named pipe at a path this test owns.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        path
    };
    // Reading a pipe waits for a writer: this one never gets one.
    let stalled = mkfifo("stalled.json");
    // The slow destination writes invalid prices here once it is scanning.
    let invalid = mkfifo("invalid.json");
    let header = hosts.header();
    hosts.replace(
        "slow",
        "token-max",
        &format!("echo '{header}'; printf '{{' > \"$TEST_ROOT/invalid.json\"; exec sleep 600"),
    );
    // The real binary, which starts its audit and then fails to read this transcript.
    let broken = hosts.host("broken");
    std::fs::write(
        broken.join(".claude/projects/my project/main.jsonl"),
        r#"{"timestamp":"2026-11-02T00:00:00Z","type":"assistant","message":{"usage":{"input_tokens":"bad"}}}
"#,
    )
    .unwrap();
    for (destination, prices, expected) in [
        (
            "unreachable",
            &stalled,
            "token-max: unreachable: SSH connection failed",
        ),
        ("broken", &stalled, "token-max: broken: "),
        ("slow", &invalid, "token-max: invalid public price catalog"),
    ] {
        let mut child = hosts
            .command(&[
                "--ssh",
                destination,
                "--prices-file",
                prices.to_str().unwrap(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("{destination}: still waiting after its first failure");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let output = child.wait_with_output().unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(output.status.code(), Some(2), "{destination}: {stderr}");
        assert_eq!(output.stdout, b"", "{destination}");
        assert!(stderr.starts_with(expected), "{destination}: {stderr}");
    }
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
        (&["--ssh-scan", "--plain"], "invalid arguments"),
        (&["--ssh", "-x"], "invalid arguments"),
    ] {
        let (status, stdout, stderr) = hosts.run(args);
        assert_eq!(status, 2, "{args:?}");
        assert_eq!(stdout, "");
        assert!(stderr.contains(message), "{args:?}: {stderr}");
    }
    assert!(!hosts.root.join("ssh.args").exists());
}

/// Each line of a session export, parsed.
fn lines(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn sessions_from_every_host_interleave_by_first_request() {
    let hosts = Hosts::new();
    let home = hosts.host("devbox");
    // An earlier session on the destination, run in a checkout whose remote holds a credential.
    let checkout = home.join("checkout");
    std::fs::create_dir_all(checkout.join(".git")).unwrap();
    std::fs::write(
        checkout.join(".git/config"),
        "[remote \"origin\"]\n\turl = https://user:SYNTHETIC_PRIVATE_TOKEN@host/o/r.git\n",
    )
    .unwrap();
    let early: String = std::fs::read_to_string(fixture("claude.jsonl"))
        .unwrap()
        .replace("session-a", "session-early")
        .replace("2026-11-01", "2026-10-31")
        .replace("message-", "early-message-")
        .lines()
        .map(|line| {
            let mut record: Value = serde_json::from_str(line).unwrap();
            record["cwd"] = checkout.to_str().unwrap().into();
            format!("{record}\n")
        })
        .collect();
    std::fs::write(home.join(".claude/projects/my project/early.jsonl"), early).unwrap();

    let run_sessions = |args: &[&str]| run(&mut hosts.invoke("sessions", args));
    let (status, stdout, stderr) = run_sessions(&["--ssh", "devbox", "--include-local"]);
    assert_eq!(status, 0, "{stderr}");
    assert_eq!(stderr, "");
    assert!(!stdout.contains("SYNTHETIC_PRIVATE"), "{stdout}");
    let found = lines(&stdout);
    let order: Vec<_> = found
        .iter()
        .map(|l| (l["host"].as_str(), l["session"].as_str().unwrap()))
        .collect();
    // A tie keeps the order the hosts were selected in.
    assert_eq!(
        order,
        [
            (Some("devbox"), "session-early"),
            (None, "session-a"),
            (Some("devbox"), "session-a"),
        ]
    );
    assert_eq!(found[0]["repositories"], json!(["https://host/o/r.git"]));
    assert_eq!(found[0]["repositoriesSource"], "checkout");

    // The destination's lines are what it prints itself, under its name.
    let (status, there, stderr) = run(hosts.invoke("sessions", &[]).env("HOME", &home));
    assert_eq!(status, 0, "{stderr}");
    let remote: Vec<Value> = found
        .into_iter()
        .filter(|l| l["host"] == "devbox")
        .map(|mut l| {
            l["host"] = Value::Null;
            l
        })
        .collect();
    assert_eq!(remote, lines(&there));
    let (_, alone, _) = run_sessions(&["--ssh", "devbox"]);
    assert_eq!(
        lines(&alone),
        lines(&stdout)
            .into_iter()
            .filter(|l| l["host"] == "devbox")
            .collect::<Vec<_>>()
    );
    let (_, windowed, stderr) = run_sessions(&["--ssh", "devbox", "--since", "2026-11-01"]);
    assert_eq!(stderr, "");
    let windowed = lines(&windowed);
    assert_eq!(windowed.len(), 1);
    assert_eq!(windowed[0]["session"], "session-a");

    let args = std::fs::read_to_string(hosts.root.join("ssh.args")).unwrap();
    assert!(args.contains("-- devbox sh -c"), "{args}");
}

#[test]
fn a_failed_host_fails_the_whole_session_export() {
    let hosts = Hosts::new();
    hosts.host("devbox");
    let broken = hosts.host("broken");
    std::fs::write(
        broken.join(".claude/projects/my project/main.jsonl"),
        r#"{"timestamp":"2026-11-02T00:00:00Z","type":"assistant","private":"SYNTHETIC_PRIVATE_REPLY","message":{"usage":{"input_tokens":"bad"}}}
"#,
    )
    .unwrap();
    for (args, expected) in [
        (
            &["--ssh", "devbox", "--ssh", "unreachable", "--include-local"][..],
            "unreachable: SSH connection failed",
        ),
        (&["--ssh", "devbox", "--ssh", "broken"], "broken: "),
        (
            &["--ssh", "devbox", "--claude-dir", "x"],
            "--claude-dir, --codex-dir and --t3-dir select local profiles",
        ),
        (&["--include-local"], "--include-local requires --ssh"),
        (&["--ssh", "devbox", "--ssh-scan"], "invalid arguments"),
    ] {
        let (status, stdout, stderr) = run(&mut hosts.invoke("sessions", args));
        assert_eq!((status, &*stdout), (2, ""), "{args:?}: {stderr}");
        let error: Value = serde_json::from_str(&stderr).unwrap();
        assert!(
            error["error"].as_str().unwrap().starts_with(expected),
            "{args:?}: {stderr}"
        );
        assert!(!stderr.contains("SYNTHETIC_PRIVATE") && !stderr.contains("noise"));
    }
}
