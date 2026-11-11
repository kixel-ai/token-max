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
        "--json-full",
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
        .command(&["--json-full", since[0], since[1]])
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
fn keepalive_retains_costs_unknowns_and_caps_in_both_ssh_schemas() {
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
    let keepalive = |report: &Value| {
        let mut check = report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["check"] == "keepalive")
            .unwrap()
            .clone();
        // Each invocation has its own clock; the replay costs do not depend on it.
        check.as_object_mut().unwrap().remove("until");
        check.as_object_mut().unwrap().remove("monthlySavingsUsd");
        check
    };
    let mut full = None;
    for flag in ["--json-full", "--json"] {
        let args = [flag, "--since", "2026-11-01T00:10:00Z"];
        let (status, stdout, stderr) = hosts.run(&[&["--ssh", "devbox"][..], &args].concat());
        assert_ne!(status, 2, "{stderr}");
        assert!(stderr.is_empty(), "{stderr}");
        assert!(!stdout.contains("SYNTHETIC_PRIVATE"));
        let combined: Value = serde_json::from_str(&stdout).unwrap();
        let remote = &combined["hosts"][0]["report"];
        let check = keepalive(remote);
        assert_eq!(check["current"], Value::Null);
        assert_eq!(check["action"], "advisory");
        assert_eq!(check["savingsUsd"], Value::Null);
        assert_eq!(check["marginUsd"], Value::Null);
        assert_eq!(check["pingInputTokens"], 1000);
        assert!(check["coverage"]["priced"].as_u64().unwrap() > 0);
        assert!(check["unpriced"].as_u64().unwrap() > 0);
        let caps = check["scenarios"].as_array().unwrap();
        assert!(caps.len() >= 2);
        for cap in caps {
            assert!(cap["pingCostUsd"].as_f64().unwrap() > 0.0);
            assert!(
                cap["closedSavingsUsd"].as_f64().unwrap() > cap["savingsUsd"].as_f64().unwrap()
            );
        }
        let (_, stdout, stderr) = run(hosts.command(&args).env("HOME", &home));
        assert!(stderr.is_empty(), "{stderr}");
        let local: Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(check, keepalive(&local));
        if let Some(full) = &full {
            assert_eq!(&check, full);
        } else {
            assert_eq!(
                remote["scan"]["cacheContext"]["requests"]
                    .as_array()
                    .unwrap()
                    .len(),
                2
            );
            full = Some(check);
        }
    }
}

#[test]
fn compact_ssh_reports_use_the_same_contract_as_local_reports() {
    let hosts = Hosts::new();
    let home = hosts.host("devbox");
    let (status, stdout, stderr) = hosts.run(&["--ssh", "devbox", "--include-local", "--json"]);
    assert_eq!(status, 0, "{stderr}");
    assert!(stderr.is_empty());
    assert!(!stdout.contains("SYNTHETIC_PRIVATE"));
    let combined: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(combined["schemaVersion"], 2);
    let reports = combined["hosts"].as_array().unwrap();
    assert_eq!(reports.len(), 2);
    assert_eq!(reports[0]["ssh"], Value::Null);
    assert_eq!(reports[1]["ssh"], "devbox");
    for host in reports {
        assert_eq!(host["report"]["schemaVersion"], 2);
        assert_eq!(host["report"]["scan"]["requests"], 2);
    }
    let (_, there, stderr) = run(hosts.command(&["--json"]).env("HOME", &home));
    assert!(stderr.is_empty());
    let there: Value = serde_json::from_str(&there).unwrap();
    assert_eq!(timeless(&reports[1]["report"]), timeless(&there));
    for key in ["context", "historical"] {
        assert_eq!(reports[1]["report"][key], there[key], "{key}");
    }
    let remote = reports[1]["report"]["checks"].as_array().unwrap();
    let local = there["checks"].as_array().unwrap();
    assert_eq!(remote.len(), local.len());
    for (remote, local) in remote.iter().zip(local) {
        // Each run has its own observation end; compare costs and decisions without
        // the monthly extrapolation that uses that end.
        for key in [
            "check",
            "action",
            "owner",
            "recommended",
            "cost",
            "coverage",
            "comparisonCost",
        ] {
            assert_eq!(remote[key], local[key], "{key}");
        }
    }
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

    let (status, stdout, stderr) =
        hosts.run(&["--ssh", "devbox", "--include-local", "--json-full"]);
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
    let (status, there, stderr) = run(hosts.command(&["--json-full"]).env("HOME", &home));
    assert_eq!(status, 1, "{stderr}");
    let there: Value = serde_json::from_str(&there).unwrap();
    assert_eq!(timeless(&reports[1]["report"]), timeless(&there));
    let there = cap(&there);
    assert_eq!(remote["waits"], there["waits"]);
    assert_eq!(remote["recommended"], there["recommended"]);
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

        let (status, stdout, stderr) =
            hosts.run(&["--ssh", "devbox", "--include-local", "--json-full"]);
        assert_eq!(status, 0, "{stderr}");
        assert_eq!(stderr, "");
        assert!(!stdout.contains("SYNTHETIC_PRIVATE"));
        let combined: Value = serde_json::from_str(&stdout).unwrap();
        let local = &combined["hosts"][0]["report"];
        let remote = &combined["hosts"][1]["report"];
        assert!(local["context"].as_array().unwrap().is_empty());
        let rows = remote["context"].as_array().unwrap();
        let server = rows.iter().find(|r| r["kind"] == "mcp-server").unwrap();
        assert_eq!(server["harness"], "codex");
        assert_eq!(server["name"], "docs");
        assert_eq!(
            server["control"]["path"],
            serde_json::json!(["mcp_servers", "docs", "enabled"])
        );
        assert_eq!(
            server["estimatedUsd"].as_f64().is_some(),
            priceable,
            "{server}"
        );
        assert_eq!(server["conditionalRequests"], server["priced"]);
        assert!(server["assumptions"].to_string().contains("conditional"));
        assert_eq!(server["verdict"], Value::Null);
        // The remote keeps the same two recorded Claude tiers as this host.
        // Adding Codex estimates must not increase that billing-evidence coverage.
        assert_eq!(
            remote["scan"]["coverage"]["tierKnown"],
            local["scan"]["coverage"]["tierKnown"]
        );
        assert!(
            remote["scan"]["requests"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|r| r["harness"] == "codex")
                .all(|r| r["parts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|p| p["tier"].is_null()))
        );
        let output = rows.iter().find(|r| r["kind"] == "tool-output").unwrap();
        assert_eq!(output["name"], "mcp__docs");
        // Both requests retain the result; compaction removes it before the third.
        assert_eq!(output["requests"], 2);
        assert_eq!(output["calls"], 1);
        assert_eq!(
            output["largestCharacters"],
            "SYNTHETIC_PRIVATE_OUTPUT".len()
        );
        assert_eq!(output["priced"], if priceable { 2 } else { 0 });
        assert_eq!(output["estimatedUsd"].as_f64().is_some(), priceable);
        let (_, there, stderr) = run(hosts.command(&["--json-full"]).env("HOME", &home));
        assert_eq!(stderr, "");
        let there: Value = serde_json::from_str(&there).unwrap();
        assert_eq!(timeless(remote), timeless(&there));
        assert_eq!(remote["context"], there["context"]);
        assert_eq!(remote["historical"], there["historical"]);
        let (_, compact, stderr) = hosts.run(&["--ssh", "devbox", "--json"]);
        assert_eq!(stderr, "");
        let compact: Value = serde_json::from_str(&compact).unwrap();
        assert_eq!(
            compact["hosts"][0]["report"]["historical"],
            remote["historical"]
        );
        assert_eq!(compact["hosts"][0]["report"]["context"], remote["context"]);

        let (_, text, stderr) = hosts.run(&["--ssh", "devbox"]);
        assert_eq!(stderr, "");
        assert!(!text.contains("SYNTHETIC_PRIVATE"));
        assert!(text.contains("Conditional Codex pricing"), "{text}");
        if priceable {
            assert!(text.contains("keep/off unresolved"), "{text}");
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
    let args = [
        "--json-full",
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
    let combined: Value = serde_json::from_str(&stdout).unwrap();
    let local = &combined["hosts"][0]["report"];
    let remote = &combined["hosts"][1]["report"];
    let background = |report: &Value| -> Vec<Value> {
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["check"] == "background-generation")
            .cloned()
            .map(|mut c| {
                c.as_object_mut().unwrap().remove("until");
                c.as_object_mut().unwrap().remove("monthlySavingsUsd");
                c
            })
            .collect()
    };
    assert_eq!(background(local).len(), 4);
    assert!(
        background(local)
            .iter()
            .all(|c| c["current"] == false && c["action"] == "keep"),
        "{:?}",
        background(local)
    );
    let checks = background(remote);
    assert_eq!(checks.len(), 4);
    assert!(checks.iter().all(|c| c["current"] == true));
    let feature = |name: &str| checks.iter().find(|c| c["feature"] == name).unwrap();
    let suggestions = feature("prompt-suggestions");
    assert_eq!(suggestions["generations"], 2);
    assert_eq!(suggestions["action"], "change");
    assert!(suggestions["cost"]["totalUsd"].as_f64().unwrap() > 0.0);
    assert_eq!(suggestions["off"], "\"promptSuggestionEnabled\": false");
    assert_eq!(
        suggestions["offFile"],
        home.join("workspace/.claude/settings.json")
            .to_str()
            .unwrap()
    );
    let recaps = feature("session-recaps");
    assert_eq!(recaps["generations"], 1);
    assert!(recaps["cost"]["totalUsd"].as_f64().unwrap() > 0.0);
    assert!(recaps["lostWarming"]["totalUsd"].as_f64().unwrap() > 0.0);
    assert_eq!(
        recaps["off"],
        "\"env\": {\"CLAUDE_CODE_ENABLE_AWAY_SUMMARY\": \"0\"}"
    );
    assert_eq!(
        recaps["offFile"],
        home.join("workspace/.claude/settings.json")
            .to_str()
            .unwrap()
    );
    for name in ["goal-check-ins", "auto-recaps"] {
        let c = feature(name);
        assert_eq!(c["action"], "advisory");
        assert_eq!(c["generations"], Value::Null);
        assert_eq!(c["cost"]["totalUsd"], Value::Null);
    }
    let profiles = remote["scan"]["profiles"].as_array().unwrap();
    let claude = profiles.iter().find(|p| p["harness"] == "claude").unwrap();
    assert_eq!(
        claude["backgroundDirectories"][home.join("workspace").to_str().unwrap()][2]["minutes"],
        7
    );
    assert_eq!(
        remote["scan"]["cacheContext"]["prompts"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        remote["scan"]["cacheContext"]["requests"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let (status, there, stderr) = run(hosts.command(&args).env("HOME", &home));
    assert_eq!(status, 1, "{stderr}");
    assert_eq!(stderr, "");
    let there: Value = serde_json::from_str(&there).unwrap();
    assert_eq!(timeless(remote), timeless(&there));
    assert_eq!(checks, background(&there));
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
    assert!(text.contains("Cost unknown"), "{text}");
    assert!(
        text.contains("tui.auto_recap = false in ~/workspace/.codex/config.toml"),
        "{text}"
    );
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
    let (status, _, stderr) = hosts.run(&["--ssh", "broken", "--json-full"]);
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
