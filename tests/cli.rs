//! The built `token-max` binary against synthetic homes under the checkout's `.tmp/`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_token-max");

/// An isolated home with empty Claude and Codex profiles, removed on drop.
struct Workspace {
    home: PathBuf,
    claude: PathBuf,
    codex: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let home = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".tmp")
            .join(format!(
                "cli-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        let claude = home.join(".claude");
        let codex = home.join(".codex");
        std::fs::create_dir_all(claude.join("projects/project")).unwrap();
        std::fs::create_dir_all(codex.join("sessions")).unwrap();
        Self {
            home,
            claude,
            codex,
        }
    }

    fn transcript(&self) -> PathBuf {
        self.claude.join("projects/project/main.jsonl")
    }

    fn fixture(&self) {
        std::fs::copy(fixture("claude.jsonl"), self.transcript()).unwrap();
    }

    fn settings(&self, text: &str) {
        std::fs::write(self.claude.join("settings.json"), text).unwrap();
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("test/fixtures")
        .join(name)
}

fn text(path: &Path) -> String {
    // Windows paths joined from "a/b" keep the slash; the code under test normalizes it.
    let path: std::path::PathBuf = if cfg!(windows) {
        path.components().collect()
    } else {
        path.to_path_buf()
    };
    path.to_string_lossy().into_owned()
}

/// The binary with only this workspace's environment and no inherited settings controls.
fn command(w: &Workspace, args: &[&str]) -> Command {
    let mut command = Command::new(BIN);
    command
        .args(args)
        .env_clear()
        .env("HOME", &w.home)
        // Windows resolves the home from USERPROFILE and ignores HOME.
        .env("USERPROFILE", &w.home)
        .env("TERM", "xterm-256color")
        .stdin(Stdio::null());
    command
}

fn run(command: &mut Command) -> (i32, String, String) {
    let Output {
        status,
        stdout,
        stderr,
    } = command.output().unwrap();
    (
        status.code().unwrap(),
        String::from_utf8(stdout).unwrap(),
        String::from_utf8(stderr).unwrap(),
    )
}

fn prices_args(w: &Workspace) -> Vec<String> {
    vec![
        "--claude-dir".into(),
        text(&w.claude),
        "--prices-file".into(),
        text(&fixture("prices.json")),
    ]
}

fn args<'a>(base: &'a [String], extra: &[&'a str]) -> Vec<&'a str> {
    base.iter()
        .map(String::as_str)
        .chain(extra.iter().copied())
        .collect()
}

#[test]
fn conditional_codex_prices_keep_assumptions_in_both_json_projections() {
    let w = Workspace::new();
    // Today's premium setting must not price a past request with no selection.
    std::fs::write(w.codex.join("config.toml"), "service_tier = 'ultrafast'\n").unwrap();
    let path = w.codex.join("sessions/rollout.jsonl");
    let mut context = json!({"type": "turn_context", "timestamp": "2026-11-01T00:00:00Z",
        "payload": {"turn_id": "turn-a", "model": "gpt-6.1-sol"}});
    let meta = json!({"type": "session_meta", "timestamp": "2026-11-01T00:00:00Z",
        "payload": {"id": "thread-a", "model_provider": "openai"}});
    let usage = json!({"type": "token_usage_record", "timestamp": "2026-11-01T00:00:01Z",
        "payload": {"response_id": "response-a", "turn_id": "turn-a", "usage": {
            "input_tokens": 1000, "output_tokens": 100, "cached_input_tokens": 0,
            "cache_write_input_tokens": 0}}});
    for (tier, expected) in [
        (None, Some(0.003)),
        (Some("auto"), Some(0.003)),
        (Some("fast"), Some(0.006)),
        (Some("flex"), None),
    ] {
        context["payload"]["service_tier"] = json!(tier);
        std::fs::write(&path, format!("{meta}\n{context}\n{usage}\n")).unwrap();
        let base = vec![
            "--codex-dir".into(),
            text(&w.codex),
            "--prices-file".into(),
            text(&fixture("prices.json")),
        ];
        let (status, stdout, stderr) = run(&mut command(&w, &args(&base, &["--json"])));
        assert!((0..=1).contains(&status), "{stderr}");
        let report: Value = serde_json::from_str(&stdout).unwrap();
        let row = &report["historical"][0];
        assert_eq!(row["cost"]["totalUsd"], json!(expected), "{tier:?}");
        assert_eq!(row["conditionalRequests"], u64::from(expected.is_some()));
        assert_eq!(report["scan"]["coverage"]["tierKnown"], 0);
        if expected.is_some() {
            assert!(
                row["cost"]["assumptions"]
                    .to_string()
                    .contains("conditional")
            );
            assert!(
                row["tierSensitivity"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|s| s["tier"] == "flex" && s["cost"]["totalUsd"].is_null())
            );
        }
        let (_, stdout, _) = run(&mut command(&w, &args(&base, &[])));
        assert!(stdout.contains("Conditional Codex pricing"), "{stdout}");
        assert!(stdout.contains("tier sensitivity"), "{stdout}");
    }
}

#[test]
fn json_exit_codes_and_no_partial_findings_or_transcript_content() {
    let w = Workspace::new();
    w.fixture();
    w.settings(r#"{"promptCacheTtl":"5m"}"#);
    let before = std::fs::read(w.transcript()).unwrap();
    let base = prices_args(&w);
    let invoke = || {
        let mut c = command(&w, &args(&base, &["--json"]));
        c.env("CLAUDE_CONFIG_DIR", &w.claude)
            .env("CODEX_HOME", &w.codex);
        run(&mut c)
    };
    let (status, stdout, stderr) = invoke();
    assert_eq!(status, 1, "{stderr}");
    assert_eq!(stderr, "");
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["schemaVersion"], 2);
    assert_eq!(report["scan"]["requests"], 2);
    assert!(!stdout.contains("SYNTHETIC_PRIVATE"));
    assert_eq!(std::fs::read(w.transcript()).unwrap(), before);
    w.settings(r#"{"promptCacheTtl":"1h"}"#);
    assert_eq!(invoke().0, 0);
    std::fs::write(
        w.transcript(),
        r#"{"timestamp":"2020-01-01T00:00:00Z","type":"assistant","private":"SYNTHETIC_PRIVATE_REPLY","message":{"usage":{"input_tokens":"bad"}}}
"#,
    )
    .unwrap();
    let (status, stdout, stderr) = invoke();
    assert_eq!(status, 2);
    assert_eq!(stdout, "");
    assert!(!stderr.contains("SYNTHETIC_PRIVATE") && !stderr.contains("recommended"));
    let diagnostic: Value = serde_json::from_str(&stderr).unwrap();
    assert_eq!(diagnostic["location"]["line"], 1);
}

#[test]
fn a_tool_only_recommendation_sets_exit_one() {
    let w = Workspace::new();
    w.settings(r#"{"promptCacheTtl":"1h","subagentPromptCacheTtl":"1h"}"#);
    let tools: Vec<Value> = ["read", "write"]
        .map(|tool| {
            json!({
                "name": format!("mcp__docs__{tool}"),
                "description": "synthetic schema ".repeat(50),
                "input_schema": {"type": "object"}
            })
        })
        .into();
    let calls: Vec<Value> = (0..20).map(|i| json!({"type": "tool_use", "id": format!("call-{i}"), "name": "mcp__docs__read", "input": {}})).collect();
    let records = [
        json!({"type": "api-request", "id": "api", "sessionId": "session", "timestamp": "2026-11-01T00:00:00Z", "params": {
            "model": "claude-opus-5-5", "system": "synthetic system", "messages": [], "tools": tools
        }}),
        json!({"type": "assistant", "uuid": "block", "sessionId": "session", "timestamp": "2026-11-01T00:00:01Z", "entrypoint": "sdk-ts", "requestRef": "api", "message": {
            "id": "message", "model": "claude-opus-5-5", "content": calls, "usage": {
                "input_tokens": 1000, "output_tokens": 100, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0,
                "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 0}, "service_tier": "standard", "speed": "standard", "inference_geo": "global"
            }
        }}),
    ];
    let lines = records
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(w.transcript(), lines).unwrap();
    let base = prices_args(&w);
    let (status, stdout, stderr) = run(&mut command(
        &w,
        &args(&base, &["--json", "--max-cost-per-use", "0.001"]),
    ));
    assert_eq!(status, 1, "{stderr}\n{stdout}");
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert!(
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["action"] != "change")
    );
    let server = report["context"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "docs")
        .unwrap();
    assert_eq!(server["verdict"], "keep");
    let write = server["details"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "mcp__docs__write")
        .unwrap();
    assert_eq!(write["verdict"], "off");
}

#[test]
fn json_counts_requests_instead_of_listing_them() {
    let w = Workspace::new();
    w.fixture();
    w.settings(r#"{"promptCacheTtl":"5m"}"#);
    let before = std::fs::read(w.transcript()).unwrap();
    let base = prices_args(&w);
    let (status, compact, stderr) = run(&mut command(&w, &args(&base, &["--json"])));
    assert_eq!(status, 1, "{stderr}");
    assert!(stderr.is_empty());
    assert!(!compact.contains("SYNTHETIC_PRIVATE"));
    let compact: Value = serde_json::from_str(&compact).unwrap();
    assert_eq!(compact["schemaVersion"], 2);
    assert_eq!(compact["scan"]["requests"], 2);
    assert_eq!(compact["scan"]["events"], 1);
    assert_eq!(std::fs::read(w.transcript()).unwrap(), before);
    let (_, _, diagnostic) = run(&mut command(&w, &args(&base, &["--json", "--bogus"])));
    assert_eq!(
        serde_json::from_str::<Value>(&diagnostic).unwrap()["error"],
        "invalid arguments; use --help"
    );
}

/// The synthetic fixture rewritten so one recorded summary precedes four large writes.
fn compaction_transcript() -> String {
    let records: Vec<Value> = std::fs::read_to_string(fixture("claude.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let mut api = records[0].clone();
    api["id"] = json!("sample-api");
    let mut message = records[1].clone();
    message["requestRef"] = json!("sample-api");
    message["timestamp"] = json!("2026-11-01T00:00:01Z");
    let base = message["message"]["usage"].clone();
    let usage = |write: i64, input: i64, output: i64| {
        let mut u = base.clone();
        u["input_tokens"] = json!(input);
        u["output_tokens"] = json!(output);
        u["cache_creation_input_tokens"] = json!(write);
        u["cache_creation"] =
            json!({"ephemeral_5m_input_tokens": write, "ephemeral_1h_input_tokens": 0});
        u
    };
    let initial = usage(20000, 0, 10);
    let mut first = initial.clone();
    first["type"] = json!("message");
    let mut summary = usage(0, 300_000, 2000);
    summary["type"] = json!("compaction");
    message["message"]["usage"] = initial.clone();
    message["message"]["usage"]["iterations"] = json!([first, summary]);
    let mut boundary = records.last().unwrap().clone();
    boundary["timestamp"] = json!("2026-11-01T00:00:02Z");
    boundary["requestRef"] = json!("sample-api");
    boundary["compactMetadata"] =
        json!({"trigger": "auto", "preTokens": 300_000, "postTokens": 2000});
    let mut resume = message.clone();
    resume.as_object_mut().unwrap().remove("requestRef");
    resume["uuid"] = json!("sample-resumed-block");
    resume["message"]["id"] = json!("sample-resume");
    resume["timestamp"] = json!("2026-11-01T00:00:03Z");
    resume["message"]["usage"] = initial.clone();
    let mut lines = vec![api, message.clone(), boundary, resume];
    for i in 0..4 {
        let mut r = message.clone();
        r.as_object_mut().unwrap().remove("requestRef");
        r["uuid"] = json!(format!("large-block-{i}"));
        r["message"]["id"] = json!(format!("large-{i}"));
        r["timestamp"] = json!(format!("2026-11-01T0{}:00:00Z", i + 1));
        let mut large = initial.clone();
        large["type"] = json!("message");
        large["cache_creation_input_tokens"] = json!(150_000);
        large["cache_creation"] =
            json!({"ephemeral_5m_input_tokens": 150_000, "ephemeral_1h_input_tokens": 0});
        r["message"]["usage"] = initial.clone();
        r["message"]["usage"]["iterations"] = json!([large]);
        lines.push(r);
    }
    lines.iter().map(|r| format!("{r}\n")).collect()
}

#[test]
fn renders_priced_compaction_windows_alongside_ttl_checks() {
    let w = Workspace::new();
    std::fs::write(w.transcript(), compaction_transcript()).unwrap();
    w.settings(r#"{"promptCacheTtl":"5m","autoCompactWindow":400000}"#);
    let base = prices_args(&w);
    let (status, stdout, stderr) = run(&mut command(&w, &args(&base, &["--json"])));
    assert_eq!(status, 1, "{stderr}");
    let report: Value = serde_json::from_str(&stdout).unwrap();
    let result = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["check"] == "compaction-window")
        .unwrap();
    assert_eq!(result["action"], "change");
    assert_eq!(result["current"], 400_000);
    assert_eq!(result["recommended"], 100_000);
    let (status, stdout, stderr) = run(&mut command(&w, &args(&base, &["--details", "--plain"])));
    assert_eq!(status, 1, "{stderr}");
    assert_eq!(stderr, "");
    for value in [
        "claude-cache-ttl",
        "CHANGE  compaction-window",
        "Current: 400000 tokens",
        "Suggested: 100000 tokens",
        "Conditional TTL baseline",
        "Added compaction",
        "Geometry basis",
        "compaction loses detail",
        result["owner"].as_str().unwrap(),
    ] {
        assert!(stdout.contains(value), "{value}");
    }
    let lower = stdout.to_lowercase();
    for forbidden in ["synthetic_private", "\x1b", "total sav"] {
        assert!(!lower.contains(forbidden), "{forbidden}");
    }
}

/// The offset of an option row's description, as `/^ {2}-.*? {2,}(?=\S)/` matches it.
fn description_column(row: &str) -> Option<usize> {
    let rest = row.strip_prefix("  -")?;
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let run = bytes[i..].iter().take_while(|&&b| b == b' ').count();
        if run >= 2 && i + run < bytes.len() {
            return Some(3 + i + run);
        }
        i += run.max(1);
    }
    None
}

#[test]
fn text_handles_all_clear_empty_windows_missing_histories_offline_prices_and_help() {
    let w = Workspace::new();
    let base = prices_args(&w);
    let invoke = |extra: &[&str]| run(&mut command(&w, &args(&base, extra)));
    let (status, stdout, stderr) = invoke(&[]);
    assert_eq!(status, 0, "{stderr}");
    assert_eq!(stderr, "");
    assert!(stdout.contains("No transcript histories found"));
    assert!(stdout.contains("--claude-dir"));
    w.fixture();
    w.settings(r#"{"promptCacheTtl":"1h"}"#);
    let (status, stdout, stderr) = invoke(&["--plain"]);
    assert_eq!(status, 0, "{stderr}");
    assert!(stdout.contains("No setting changes recommended"));
    assert!(stdout.lines().any(|l| l == "Keep"));
    assert!(!stdout.contains('\x1b') && !stdout.contains('→'));
    assert!(!stdout.contains("SYNTHETIC_PRIVATE"));
    assert_eq!(stderr, "");
    let (status, stdout, stderr) = invoke(&["--since", "2027-01-01"]);
    assert_eq!(status, 0, "{stderr}");
    assert!(stdout.contains("No requests found"));
    assert!(stdout.contains("earlier --since"));
    let cache = text(&fixture("prices.json"));
    let claude = text(&w.claude);
    let (status, stdout, stderr) = run(&mut command(
        &w,
        &[
            "--claude-dir",
            &claude,
            "--offline",
            "--price-cache",
            &cache,
        ],
    ));
    assert_eq!(status, 0, "{stderr}");
    assert!(stdout.contains("Prices: cached from"));
    let absent = text(&w.home.join("absent-prices"));
    let (status, stdout, stderr) = run(&mut command(
        &w,
        &[
            "--claude-dir",
            &claude,
            "--offline",
            "--price-cache",
            &absent,
        ],
    ));
    assert_eq!(status, 2);
    assert_eq!(stdout, "");
    assert!(stderr.contains("fetch prices once"));
    let (status, help, stderr) = invoke(&["--help"]);
    assert_eq!(status, 0, "{stderr}");
    assert!(help.lines().any(|l| l.starts_with("  --since ")));
    assert!(help.lines().any(|l| l.starts_with("  token-max --since ")));
    for row in help.lines().filter(|l| l.starts_with("  -")) {
        assert_eq!(description_column(row), Some(26), "{row}");
    }
    let (status, stdout, stderr) = invoke(&["--bogus"]);
    assert_eq!(status, 2);
    assert_eq!(stdout, "");
    assert_eq!(stderr.to_lowercase().matches("use --help").count(), 1);
    let (status, _, stderr) = invoke(&["--max-cost-per-use", "lots"]);
    assert_eq!(status, 2);
    assert!(stderr.contains("--max-cost-per-use requires a dollar amount"));
    for flag in [
        "--since",
        "--max-cost-per-use",
        "--claude-dir",
        "--codex-dir",
        "--details",
        "--plain",
        "--json",
        "NO_COLOR",
        "Exit 0",
        "Exit 1",
        "Exit 2",
    ] {
        assert!(help.contains(flag), "{flag}");
    }
}

#[test]
fn arguments_follow_strict_parse_args() {
    let w = Workspace::new();
    let base = prices_args(&w);
    let invoke = |extra: &[&str]| run(&mut command(&w, &args(&base, extra)));
    for extra in [
        &["positional"][..],
        &["--json=1"],
        &["--since"],
        &["--since", "-x"],
        &["-x"],
        &["-vx"],
        &["--", "positional"],
        &["--no-json"],
    ] {
        let (status, stdout, stderr) = invoke(extra);
        assert_eq!(status, 2, "{extra:?}");
        assert_eq!(stdout, "");
        assert_eq!(
            stderr, "token-max: invalid arguments; use --help\n",
            "{extra:?}"
        );
    }
    let (status, _, stderr) = invoke(&["--json", "--bogus"]);
    assert_eq!(status, 2);
    assert_eq!(stderr, "{\"error\":\"invalid arguments; use --help\"}\n");
    let (status, stdout, _) = invoke(&["-hv"]);
    assert_eq!(
        (status, stdout.starts_with("token-max: cheaper")),
        (0, true)
    );
    let (status, stdout, _) = invoke(&["-v", "--"]);
    assert_eq!(
        (status, stdout),
        (0, format!("{}\n", env!("CARGO_PKG_VERSION")))
    );
    for (value, message) in [
        ("2026-11", "--since requires YYYY-MM-DD"),
        ("2026-11-01T00:00:00", "--since requires YYYY-MM-DD"),
        ("2026-02-30", "invalid --since date"),
        ("2026-11-01Tnoon Z", "invalid --since date"),
    ] {
        let (status, _, stderr) = invoke(&["--since", value]);
        assert_eq!(status, 2, "{value}");
        assert!(stderr.contains(message), "{value}: {stderr}");
    }
    for value in ["$0.50", "2", "0.5"] {
        assert_ne!(invoke(&["--max-cost-per-use", value]).0, 2, "{value}");
    }
    for value in ["", "1.", ".5", "1e3", "-1"] {
        let (status, _, stderr) = invoke(&[&format!("--max-cost-per-use={value}")]);
        assert_eq!(status, 2, "{value}");
        assert!(stderr.contains("requires a dollar amount"), "{value}");
    }
}

/// Floats print as JavaScript prints them: integers without a fraction, no `1.0`.
#[test]
fn json_numbers_print_like_javascript() {
    let w = Workspace::new();
    w.fixture();
    w.settings(r#"{"promptCacheTtl":"5m"}"#);
    let base = prices_args(&w);
    let (_, stdout, _) = run(&mut command(&w, &args(&base, &["--json"])));
    assert!(stdout.contains(r#""maxCostPerUseUsd":1,"#));
    assert!(!stdout.contains(".0,") && !stdout.contains(".0}"));
    assert!(stdout.ends_with("}\n") && stdout.matches('\n').count() == 1);
}

/// Windows from `--since` replay the same cache comparison as the library.
#[test]
fn explicit_windows_match_the_library_replay() {
    use token_max::controls::Env;
    use token_max::discovery::DiscoveryOptions;
    use token_max::model::{Action, CheckResult};
    use token_max::prices::{PriceOptions, load_prices};
    use token_max::reader::{ScanOptions, scan};
    use token_max::replay::{AnalyzeOptions, analyze};

    const MIN: i64 = 60_000;
    let iso = |minute: i64| {
        jiff::Timestamp::from_millisecond(minute * MIN)
            .unwrap()
            .strftime("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string()
    };
    let common = |kind: &str, minute: i64| {
        json!({
            "version": "2.1.314",
            "entrypoint": "sdk-ts",
            "sessionId": "synthetic",
            "querySource": "sdk",
            "type": kind,
            "timestamp": iso(minute),
        })
    };
    let assistant = |minute: i64, id: &str, read: i64, write: i64| {
        let mut r = common("assistant", minute);
        r["message"] = json!({
            "id": id,
            "content": [{"type": "text", "text": "SYNTHETIC_PRIVATE_WINDOW"}],
            "model": "claude-opus-5-5",
            "usage": {
                "input_tokens": 0,
                "output_tokens": 10,
                "cache_read_input_tokens": read,
                "cache_creation_input_tokens": write,
                "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": write},
                "service_tier": "standard",
                "speed": "standard",
                "inference_geo": "global",
            },
        });
        r
    };
    let attempt = |minute: i64| {
        let mut r = common("api-request", minute);
        r["id"] = json!("attempt");
        r["params"] = json!({"model": "claude-opus-5-5"});
        r
    };
    let catalog = load_prices(&PriceOptions {
        file: Some(fixture("prices.json")),
        cache: PathBuf::new(),
        offline: false,
    })
    .unwrap();
    for mode in ["known", "unsupported", "estimated"] {
        let w = Workspace::new();
        w.settings(r#"{"promptCacheTtl":"5m"}"#);
        let mut records = vec![assistant(0, "first", 0, 100_000)];
        if mode != "known" {
            records.push(attempt(if mode == "estimated" { 10 } else { 17 }));
        }
        records.push(if mode == "estimated" {
            assistant(20, "last", 0, 100_000)
        } else {
            assistant(20, "last", 100_000, 2000)
        });
        let lines: String = records.iter().map(|r| format!("{r}\n")).collect();
        std::fs::write(w.transcript(), lines).unwrap();
        let since = 18 * MIN;
        let selected = scan(&ScanOptions {
            discovery: DiscoveryOptions {
                home: text(&w.home),
                env: Env::default(),
                claude_dirs: vec![],
                codex_dirs: vec![],
                t3_dirs: vec![],
                managed_settings_file: text(&w.home.join("managed-settings.json")),
                codex_system_config_file: text(&w.home.join("system.toml")),
                codex_requirements_file: text(&w.home.join("requirements.toml")),
            },
            since: Some(since),
        })
        .unwrap();
        let report = analyze(AnalyzeOptions {
            scan: selected,
            catalog: &catalog,
            since: Some(since),
            now: 21 * MIN,
            max_cost_per_use_usd: 1.0,
        })
        .unwrap();
        let Some(CheckResult::Ttl(check)) = report.checks.first() else {
            panic!("{mode}: no TTL check");
        };
        let claude = text(&w.claude);
        let prices = text(&fixture("prices.json"));
        let window = iso(18);
        let mut c = command(
            &w,
            &[
                "--claude-dir",
                &claude,
                "--since",
                &window,
                "--prices-file",
                &prices,
                "--json",
            ],
        );
        c.env("CLAUDE_CONFIG_DIR", &w.claude);
        let (status, stdout, stderr) = run(&mut c);
        let expected = i32::from(report.checks.iter().any(|c| c.action() == Action::Change));
        assert_eq!(status, expected, "{mode}: {stderr}");
        let cli: Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(
            numbers(cli["checks"][0]["comparisonCost"].clone()),
            numbers(serde_json::to_value(&check.comparison_cost).unwrap()),
            "{mode}"
        );
        assert!(!stdout.contains("SYNTHETIC_PRIVATE"));
    }
}

/// The published crate carries the license and the semantics document the README links to.
#[test]
fn the_crate_package_ships_its_license_and_docs() {
    let output = Command::new(env!("CARGO"))
        .args(["package", "--list", "--allow-dirty", "--offline"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let list = String::from_utf8(output.stdout).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for file in ["LICENSE", "README.md", "docs/semantics.md"] {
        // Windows lists packaged paths with backslashes.
        assert!(list.lines().any(|l| l.replace('\\', "/") == file), "{file}");
    }
}

/// A relocated binary needs no checkout, resolves relative paths from its working directory and
/// leaves transcripts and settings untouched.
#[test]
fn a_relocated_binary_runs_from_any_directory_with_relative_paths() {
    let w = Workspace::new();
    let consumer = w.home.join("consumer with spaces");
    std::fs::create_dir_all(consumer.join("profile/projects/project")).unwrap();
    // A hard link, unlike a fresh copy, has no write descriptor a concurrently forked test
    // child could still hold when this test executes it.
    let bin = consumer.join("token-max");
    std::fs::hard_link(BIN, &bin).unwrap();
    let transcript = consumer.join("profile/projects/project/main.jsonl");
    let settings = consumer.join("profile/settings.json");
    std::fs::copy(fixture("claude.jsonl"), &transcript).unwrap();
    std::fs::copy(fixture("prices.json"), consumer.join("prices.json")).unwrap();
    let original = std::fs::read(&transcript).unwrap();
    let invoke = |extra: &[&str]| {
        let mut c = Command::new(&bin);
        c.args(extra)
            .current_dir(&consumer)
            .env_clear()
            .env("HOME", &w.home)
            // Windows resolves the home from USERPROFILE and ignores HOME.
            .env("USERPROFILE", &w.home)
            .env("NO_COLOR", "1")
            .stdin(Stdio::null());
        run(&mut c)
    };
    let last_line = |stderr: &str| -> Value {
        serde_json::from_str(stderr.trim().lines().last().unwrap()).unwrap()
    };
    let profile = ["--claude-dir", "profile", "--prices-file", "prices.json"];
    let with = |extra: &'static str| [&profile[..], &[extra][..]].concat();

    let (status, help, stderr) = invoke(&["--help"]);
    assert_eq!(status, 0, "{stderr}");
    assert!(help.contains("Usage: token-max"));
    let (status, version, stderr) = invoke(&["--version"]);
    assert_eq!(status, 0, "{stderr}");
    assert_eq!(version, format!("{}\n", env!("CARGO_PKG_VERSION")));

    std::fs::write(&settings, r#"{"promptCacheTtl":"5m"}"#).unwrap();
    let (status, text, text_err) = invoke(&with("--plain"));
    assert_eq!(status, 1, "{text_err}");
    assert!(
        text.contains("Main-agent prompt cache TTL: 5m -> 1h"),
        "{text}"
    );
    assert!(
        text.contains("Saves $") || text.contains("Saves <$"),
        "{text}"
    );
    let (status, json, json_err) = invoke(&with("--json"));
    assert_eq!(status, 1, "{json_err}");
    let report: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(report["schemaVersion"], 2);
    assert_eq!(report["scan"]["requests"], 2);
    let check = |name: &str| {
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["check"] == name)
            .unwrap_or_else(|| panic!("{name}"))
            .clone()
    };
    let ttl = check("claude-cache-ttl");
    assert_eq!(ttl["action"], "change");
    assert_eq!(ttl["recommended"], "1h");
    assert_eq!(
        check("idle-compaction")["action"],
        "advisory",
        "SDK usage must retain the idle check without recommending a native toggle"
    );
    let unknown = text
        .find("Not enough evidence")
        .unwrap_or_else(|| panic!("{text}"));
    assert!(text[unknown..].contains("Idle compaction"), "{text}");
    let all = format!("{text}{json}{text_err}{json_err}");
    assert!(!all.contains("SYNTHETIC_PRIVATE"));
    assert_eq!(std::fs::read(&transcript).unwrap(), original);
    assert_eq!(
        std::fs::read_to_string(&settings).unwrap(),
        r#"{"promptCacheTtl":"5m"}"#
    );

    std::fs::write(&settings, r#"{"promptCacheTtl":"1h"}"#).unwrap();
    let (status, clear, stderr) = invoke(&with("--json"));
    assert_eq!(status, 0, "{stderr}");
    let clear: Value = serde_json::from_str(&clear).unwrap();
    let checks = clear["checks"].as_array().unwrap();
    assert!(checks.iter().all(|c| c["action"] != "change"));

    let (status, stdout, stderr) = invoke(&["--bogus", "--json"]);
    assert_eq!(status, 2, "{stderr}");
    assert_eq!(stdout, "");
    let error = last_line(&stderr)["error"].as_str().unwrap().to_string();
    assert!(error.contains("invalid arguments"), "{error}");

    std::fs::write(
        &transcript,
        r#"{"timestamp":"2020-01-01T00:00:00Z","type":"assistant","private":"SYNTHETIC_PRIVATE","message":{"usage":{"input_tokens":"bad"}}}
"#,
    )
    .unwrap();
    let (status, stdout, stderr) = invoke(&with("--json"));
    assert_eq!(status, 2, "{stderr}");
    assert_eq!(stdout, "");
    assert_eq!(last_line(&stderr)["location"]["line"], 1);
    assert!(!stderr.contains("SYNTHETIC_PRIVATE"));
}

/// Every JSON number as a float, so `1` and `1.0` compare equal.
fn numbers(value: Value) -> Value {
    match value {
        Value::Number(n) => json!(n.as_f64().unwrap()),
        Value::Array(a) => Value::Array(a.into_iter().map(numbers).collect()),
        Value::Object(o) => Value::Object(o.into_iter().map(|(k, v)| (k, numbers(v))).collect()),
        v => v,
    }
}

// Ported from fast.test.ts: "built CLI warns from current settings alone, remains read-only and skips unreadable lines".
#[test]
fn warns_from_current_fast_settings_alone_stays_read_only_and_skips_unreadable_lines() {
    let w = Workspace::new();
    let settings = w.codex.join("config.toml");
    let config = "model=\"gpt-6.1-sol\"\nservice_tier=\"ultrafast\"";
    std::fs::write(&settings, config).unwrap();
    let base = [
        "--codex-dir".to_string(),
        text(&w.codex),
        "--prices-file".into(),
        text(&fixture("prices.json")),
        "--json".into(),
    ];
    let invoke = || {
        let mut c = command(&w, &args(&base, &[]));
        c.env("CODEX_HOME", &w.codex);
        run(&mut c)
    };
    let (status, stdout, stderr) = invoke();
    assert_eq!(status, 1, "{stderr}");
    let report: Value = serde_json::from_str(&stdout).unwrap();
    let fast = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["check"] == "fast-mode")
        .unwrap();
    assert_eq!(fast["savingsUsd"], Value::Null);
    assert_eq!(std::fs::read_to_string(&settings).unwrap(), config);
    std::fs::write(
        &settings,
        format!("{config}\n[features]\nultrafast_mode=false"),
    )
    .unwrap();
    assert_eq!(invoke().0, 0);
    std::fs::write(
        w.codex.join("sessions/broken.jsonl"),
        "{\"private\":\"SYNTHETIC_PRIVATE\",BROKEN}\n",
    )
    .unwrap();
    let (status, stdout, stderr) = invoke();
    assert_eq!(status, 0, "{stderr}");
    assert!(!format!("{stdout}{stderr}").contains("SYNTHETIC_PRIVATE"));
}

#[cfg(unix)]
mod unix {
    use std::io::Read;
    use std::net::TcpListener;
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::thread::JoinHandle;

    use super::*;

    /// A pseudo-terminal of `columns` cells, passing output bytes through unchanged.
    struct Pty {
        master: OwnedFd,
        slave: OwnedFd,
    }

    impl Pty {
        fn new(columns: u16) -> Self {
            let (mut master, mut slave) = (-1, -1);
            let mut size = libc::winsize {
                ws_row: 24,
                ws_col: columns,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            // SAFETY: the out-pointers are valid and openpty initializes both descriptors.
            let (master, slave) = unsafe {
                assert_eq!(
                    libc::openpty(
                        &mut master,
                        &mut slave,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        // Linux takes a const pointer and macOS a mutable one.
                        &raw mut size,
                    ),
                    0
                );
                (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave))
            };
            use std::os::fd::AsRawFd;
            // SAFETY: the termios is fully written by tcgetattr before use.
            unsafe {
                // Children of parallel tests must not inherit these and hold them open.
                for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
                    assert_eq!(libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC), 0);
                }
                let mut termios: libc::termios = std::mem::zeroed();
                assert_eq!(libc::tcgetattr(slave.as_raw_fd(), &mut termios), 0);
                libc::cfmakeraw(&mut termios);
                assert_eq!(
                    libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &termios),
                    0
                );
            }
            Self { master, slave }
        }

        /// Read everything written until every slave descriptor closes.
        fn drain(self) -> (Stdio, JoinHandle<String>) {
            let Pty { master, slave } = self;
            let reader = std::thread::spawn(move || {
                let mut file = std::fs::File::from(master);
                let mut out = vec![];
                let mut buf = [0; 4096];
                // Linux reports EIO once the last slave descriptor closes.
                while let Ok(n @ 1..) = file.read(&mut buf) {
                    out.extend_from_slice(&buf[..n]);
                }
                String::from_utf8(out).unwrap()
            });
            (Stdio::from(slave), reader)
        }
    }

    /// Run with stdout and stderr on separate terminals of `columns` cells.
    fn tty(command: &mut Command, columns: u16) -> (i32, String, String) {
        let (stdout, out) = Pty::new(columns).drain();
        let (stderr, err) = Pty::new(columns).drain();
        command.stdout(stdout).stderr(stderr);
        let mut child = command.spawn().unwrap();
        // Close this process's copies of the slaves, so readers end when the child exits.
        command.stdout(Stdio::null()).stderr(Stdio::null());
        let status = child.wait().unwrap().code().unwrap();
        (status, out.join().unwrap(), err.join().unwrap())
    }

    #[test]
    fn progress_stays_on_interactive_stderr_and_color_width_and_locale_follow_the_terminal() {
        let w = Workspace::new();
        w.fixture();
        w.settings(r#"{"promptCacheTtl":"5m"}"#);
        let base = prices_args(&w);
        let invoke = |extra: &[&str], env: &[(&str, &str)]| {
            let mut c = command(&w, &args(&base, extra));
            c.envs(env.iter().copied());
            tty(&mut c, 40)
        };
        let (status, stdout, stderr) = invoke(&[], &[]);
        assert_eq!(status, 1, "{stderr}");
        assert!(stdout.contains("\x1b["));
        for phase in [
            "Scanning local histories",
            "Loading public prices",
            "Checking settings",
        ] {
            assert!(stderr.contains(phase), "{phase}");
        }
        for leak in [
            "Scanning",
            "Loading public",
            "Checking settings",
            "SYNTHETIC_PRIVATE",
        ] {
            assert!(!stdout.contains(leak), "{leak}");
        }
        let (status, stdout, stderr) = invoke(&[], &[("NO_COLOR", "1")]);
        assert_eq!(status, 1, "{stderr}");
        assert!(!stdout.contains('\x1b'));
        assert!(stdout.lines().all(|l| l.chars().count() <= 40), "{stdout}");
        let (_, stdout, _) = invoke(&[], &[("LC_ALL", "en_US.UTF-8")]);
        assert!(stdout.contains('→'));
        let (_, stdout, _) = invoke(&[], &[("LC_ALL", "C"), ("LC_CTYPE", "en_US.UTF-8")]);
        assert!(!stdout.contains('→'));
        let (_, help, _) = invoke(&["--help"], &[]);
        assert!(help.lines().any(|l| l.starts_with("  --since ")));
        assert!(help.lines().any(|l| l.starts_with("  token-max --since ")));
        assert!(help.lines().all(|l| l.chars().count() <= 40), "{help}");
        assert!(help.lines().any(|l| l.len() > 26
            && l[..26].bytes().all(|b| b == b' ')
            && l.as_bytes()[26] != b' '));
        let (status, stdout, stderr) = invoke(&["--plain", "--details"], &[]);
        assert_eq!(status, 1, "{stderr}");
        let both = format!("{stdout}{stderr}");
        assert!(!both.contains('\x1b') && !both.contains('→'));
        assert!(stdout.contains("Uncertainty:"));
        let (status, stdout, stderr) = invoke(&["--json", "--details", "--plain"], &[]);
        assert_eq!(status, 1, "{stderr}");
        assert_eq!(stderr, "");
        let report: Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(report["schemaVersion"], 2);
        assert_eq!(report["scan"]["requests"], 2);
        assert_eq!(report["checks"][0]["action"], "change");
        let (_, stdout, stderr) = run(&mut command(&w, &args(&base, &[])));
        assert_eq!(stderr, "");
        assert!(!stdout.contains('\x1b') && !stdout.contains('→'));
        assert!(
            stdout
                .lines()
                .any(|l| l.ends_with(" Set in ~/.claude/settings.json:promptCacheTtl.")),
            "piped paths remain on one copyable line"
        );
        let (_, stdout, stderr) = invoke(&[], &[("TERM", "dumb")]);
        assert_eq!(stderr, "");
        assert!(!stdout.contains('\x1b') && !stdout.contains('→'));
        std::fs::write(
            w.transcript(),
            "{\"private\":\"SYNTHETIC_PRIVATE\",BROKEN}\n",
        )
        .unwrap();
        let (status, stdout, stderr) = invoke(&[], &[]);
        assert_ne!(status, 2);
        let unreadable = stdout.find("1 unreadable transcript").unwrap();
        let rest = stdout[unreadable + "1 unreadable transcript".len()..].trim_start();
        assert!(rest.starts_with("lines in 1 files"), "{stdout}");
        assert!(!format!("{stdout}{stderr}").contains("SYNTHETIC_PRIVATE"));
    }

    fn signal(pid: u32, signal: libc::c_int) {
        // SAFETY: kill only sends a signal to the child process this test spawned.
        assert_eq!(unsafe { libc::kill(pid as libc::pid_t, signal) }, 0);
    }

    /// A price fetch held pending by a local proxy that accepts and never answers.
    #[test]
    fn sigint_and_sigterm_cancel_a_pending_price_fetch_atomically() {
        let w = Workspace::new();
        w.fixture();
        let claude = text(&w.claude);
        let cache = text(&w.home.join("prices.json"));
        for sig in [libc::SIGINT, libc::SIGTERM] {
            for json in [false, true] {
                let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
                let mut args = vec!["--claude-dir", &claude, "--price-cache", &cache];
                if json {
                    args.push("--json");
                }
                let mut c = command(&w, &args);
                c.env(
                    "ALL_PROXY",
                    format!("http://{}", proxy.local_addr().unwrap()),
                )
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
                let child = c.spawn().unwrap();
                // The fetch is pending once the proxy has its connection.
                let (_connection, _) = proxy.accept().unwrap();
                signal(child.id(), sig);
                let output = child.wait_with_output().unwrap();
                let stdout = String::from_utf8(output.stdout).unwrap();
                let stderr = String::from_utf8(output.stderr).unwrap();
                assert_eq!(output.status.code(), Some(2), "{stderr}");
                assert_eq!(stdout, "");
                for leak in ["SYNTHETIC_PRIVATE", "recommended", "schemaVersion"] {
                    assert!(!stderr.contains(leak), "{leak}");
                }
                let message = if json {
                    serde_json::from_str::<Value>(&stderr).unwrap()["error"]
                        .as_str()
                        .unwrap()
                        .to_string()
                } else {
                    stderr
                };
                assert!(
                    message.contains("cancelled; no report produced"),
                    "{message}"
                );
            }
        }
        assert!(!std::fs::read_dir(&w.home).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("prices.json")
        }));
    }
}

#[test]
fn background_reports_keep_project_overrides_and_unresolved_history_separate() {
    let w = Workspace::new();
    w.settings(r#"{"promptSuggestionEnabled":true,"awaySummaryEnabled":true}"#);
    let off = w.home.join("off");
    let on = w.home.join("on");
    for directory in [&off, &on] {
        std::fs::create_dir_all(directory.join(".claude")).unwrap();
        std::fs::create_dir_all(directory.join(".git")).unwrap();
    }
    std::fs::write(off.join(".claude/settings.local.json"), r#"{"promptSuggestionEnabled":false,"awaySummaryEnabled":false,"env":{"CLAUDE_CODE_GOAL_CHECKIN_MINUTES":"0"}}"#).unwrap();
    std::fs::write(on.join(".claude/settings.local.json"), r#"{"env":{"CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION":"1","CLAUDE_CODE_ENABLE_AWAY_SUMMARY":"1","CLAUDE_CODE_GOAL_CHECKIN_MINUTES":"0","SECRET":"SYNTHETIC_PRIVATE_SETTINGS"}}"#).unwrap();
    let template = std::fs::read_to_string(fixture("claude.jsonl")).unwrap();
    for (index, cwd) in [
        Some(off.clone()),
        Some(on.clone()),
        None,
        Some(w.home.join("gone")),
    ]
    .into_iter()
    .enumerate()
    {
        let mut lines = String::new();
        for line in template.lines() {
            let mut row: Value = serde_json::from_str(line).unwrap();
            row["sessionId"] = format!("session-{index}").into();
            row["cwd"] = cwd.as_ref().map(|p| text(p)).into();
            row["entrypoint"] = "cli".into();
            if row["type"] == "api-request" {
                row["querySource"] = "repl_main_thread".into();
            }
            for key in ["id", "uuid", "requestRef"] {
                if let Some(value) = row[key].as_str() {
                    row[key] = format!("{value}-{index}").into();
                }
            }
            if let Some(value) = row["message"]["id"].as_str() {
                row["message"]["id"] = format!("{value}-{index}").into();
            }
            lines += &format!("{row}\n");
        }
        std::fs::write(
            w.claude
                .join(format!("projects/project/main-{index}.jsonl")),
            lines,
        )
        .unwrap();
    }
    let mut args = prices_args(&w);
    args.extend(["--json".into(), "--max-cost-per-use".into(), "0".into()]);
    let (_, stdout, stderr) = run(&mut command(
        &w,
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
    ));
    assert_eq!(stderr, "");
    assert!(!stdout.contains("SYNTHETIC_PRIVATE"));
    let report: Value = serde_json::from_str(&stdout).unwrap();
    let checks: Vec<&Value> = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["check"] == "background-generation")
        .collect();
    let feature = |directory: Option<&Path>, name: &str| {
        *checks
            .iter()
            .find(|c| {
                c["feature"] == name
                    && c["directories"][0]
                        == directory
                            .map(text)
                            .map(Value::String)
                            .unwrap_or(Value::Null)
            })
            .unwrap()
    };
    let disabled = feature(Some(&off), "prompt-suggestions");
    assert_eq!(disabled["current"], false);
    assert_eq!(disabled["action"], "keep");
    assert_eq!(disabled["requests"], 2);
    assert_eq!(
        disabled["offFile"],
        text(&off.join(".claude/settings.local.json"))
    );
    // Uses are not recorded, so a cost above the per-use limit is only advisory.
    let enabled = feature(Some(&on), "prompt-suggestions");
    assert_eq!(enabled["action"], "advisory");
    assert_eq!(enabled["recommended"], Value::Null);
    assert_eq!(enabled["requests"], 2);
    assert_eq!(enabled["generations"], 1);
    let patch: Value =
        serde_json::from_str(&format!("{{{}}}", enabled["off"].as_str().unwrap())).unwrap();
    assert_eq!(
        patch["env"]["CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION"],
        "false"
    );
    let gone = w.home.join("gone");
    for directory in [None, Some(gone.as_path())] {
        let unknown = feature(directory, "prompt-suggestions");
        assert_eq!(unknown["current"], Value::Null);
        assert_eq!(unknown["action"], "advisory");
        assert_eq!(unknown["cost"]["totalUsd"], Value::Null);
        assert_eq!(unknown["coverage"], Value::Null);
    }
    args.retain(|a| a != "--json");
    let (_, stdout, stderr) = run(&mut command(
        &w,
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
    ));
    assert_eq!(stderr, "");
    assert!(!stdout.contains("SYNTHETIC_PRIVATE"));
    assert!(stdout.contains("scope or launch selection is unresolved"));
    assert!(stdout.contains("CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION"));
    assert!(stdout.contains("Background estimates apply current files to history"));
    let consider = &stdout[stdout.find("\nConsider turning off\n").unwrap() + 1..];
    let consider = &consider[..consider.find("\n\n").unwrap()];
    assert!(consider.contains("Prompt suggestions"));
    assert!(consider.contains("its uses are not recorded"));
}

#[test]
fn t3_cli_keeps_native_usage_and_reports_optional_and_auxiliary_gaps() {
    let w = Workspace::new();
    w.fixture();
    let native = std::fs::read_to_string(w.transcript())
        .unwrap()
        .replace("sdk-ts", "t3-code");
    std::fs::write(w.transcript(), native).unwrap();
    let state = w.home.join("selected-t3");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("settings.json"),json!({"idleCompaction":"off","providerInstances":{"claudeAgent":{"driver":"claudeAgent","config":{"homePath":text(&w.claude),"autoCompactWindow":"300000"}},"other":{"driver":"unsupported","config":{"homePath":{"PRIVATE_CONFIG":"not-a-home"}}}}}).to_string()).unwrap();
    let state = text(&state);
    let prices = text(&fixture("prices.json"));
    let invoke = |output| {
        run(&mut command(
            &w,
            &["--t3-dir", &state, "--prices-file", &prices, output],
        ))
    };
    let (status, report, stderr) = invoke("--json");
    assert_ne!(status, 2, "{stderr}");
    assert!(stderr.is_empty());
    let report: Value = serde_json::from_str(&report).unwrap();
    assert_eq!(report["scan"]["requests"], 2);
    assert_eq!(report["scan"]["t3"].as_array().unwrap().len(), 1);
    assert!(
        report["scan"]["t3"][0]["metadata"]
            .as_str()
            .unwrap()
            .contains("unavailable")
    );
    assert!(
        report["scan"]["t3"][0]["exclusions"]
            .to_string()
            .contains("unsupported")
    );
    assert_eq!(
        report["scan"]["t3"][0]["textGeneration"]["selection"],
        json!({"instance": "codex", "model": "gpt-6-luna"})
    );
    assert!(
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["check"] == "t3-idle-compaction"
                && c["currentSettings"]["scenarios"].as_array().unwrap().len() == 3)
    );
    let (_, details, stderr) = invoke("--details");
    assert!(stderr.is_empty());
    assert!(
        details.contains("t3-idle-compaction")
            && details
                .contains("T3 text generation: codex gpt-6-luna (T3 default). Cost unmeasured")
            && details.contains("unavailable")
    );
    let (_, summary, stderr) = run(&mut command(
        &w,
        &["--t3-dir", &state, "--prices-file", &prices],
    ));
    assert!(stderr.is_empty());
    let unmeasured = summary.split("Not enough evidence").nth(1).unwrap();
    assert!(
        unmeasured.contains("T3 text generation: codex gpt-6-luna (T3 default). Cost unmeasured")
            && unmeasured.contains(&format!(
                "Set in ~{0}selected-t3{0}settings.json:textGenerationModelSelection.",
                std::path::MAIN_SEPARATOR
            ))
    );
    for output in [report.to_string(), details, summary] {
        assert!(!output.contains("SYNTHETIC_PRIVATE") && !output.contains("PRIVATE_CONFIG"));
    }
    assert!(!Path::new(&state).join("statev2.sqlite").exists());
}
