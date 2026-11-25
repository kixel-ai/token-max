//! The built `token-max` binary against synthetic homes under Cargo's target scratch directory.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use support::native_path;

use serde_json::{Value, json};
use token_max::{Action, BackgroundFeature, CheckResult, Usd, Verdict};

const BIN: &str = env!("CARGO_BIN_EXE_token-max");

/// An isolated home with empty Claude and Codex profiles, removed on drop.
///
/// It lives under the target directory, which `cargo package` skips: on Windows, walking a home
/// another test is deleting fails the package listing. Unlike the system temp directory, it
/// shares a volume with the binary a test hard-links.
struct Workspace {
    home: PathBuf,
    claude: PathBuf,
    codex: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let home = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
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

    /// The fixture moved so its history starts `days` before now. A report's window ends
    /// now, so a monthly figure from the fixture as written shrinks as the days pass.
    #[cfg(unix)]
    fn recent_fixture(&self, days: i64) {
        let text = std::fs::read_to_string(fixture("claude.jsonl")).unwrap();
        let lines: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let at = |v: &Value| v["timestamp"].as_str()?.parse::<jiff::Timestamp>().ok();
        let first = lines.iter().filter_map(at).min().unwrap();
        let start = jiff::Timestamp::now()
            .checked_sub(jiff::SignedDuration::from_hours(24 * days))
            .unwrap();
        let shift = start.duration_since(first);
        let moved: Vec<String> = lines
            .into_iter()
            .map(|mut line| {
                if let Some(t) = at(&line) {
                    let t = t.checked_add(shift).unwrap();
                    line["timestamp"] = json!(t.strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string());
                }
                line.to_string()
            })
            .collect();
        std::fs::write(self.transcript(), moved.join("\n") + "\n").unwrap();
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

/// The binary with only this workspace's environment and no inherited settings controls.
fn command(w: &Workspace, args: &[&str]) -> Command {
    command_at(Path::new(BIN), w, args)
}

/// The binary at `bin`, as [`command`] runs the built one.
fn command_at(bin: &Path, w: &Workspace, args: &[&str]) -> Command {
    let mut command = Command::new(bin);
    support::isolated(&mut command)
        .args(args)
        .env("HOME", &w.home)
        // Windows resolves the home from USERPROFILE and ignores HOME.
        .env("USERPROFILE", &w.home)
        .env("TERM", "xterm-256color")
        .env("TOKEN_MAX_RELEASES", support::current())
        .stdin(Stdio::null());
    command
}

/// Where a plain report line's profile tag and setting start, after the columns two or
/// more spaces separate; None when the last column is a figure or percent.
fn setting_start(text: &str) -> Option<usize> {
    let mut columns: Vec<(usize, &str)> = vec![];
    let mut start = None;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == ' ' && chars.peek().is_some_and(|(_, next)| *next == ' ') {
            if let Some(s) = start.take() {
                columns.push((s, &text[s..i]));
            }
            while chars.peek().is_some_and(|(_, next)| *next == ' ') {
                chars.next();
            }
        } else if start.is_none() && c != ' ' {
            start = Some(i);
        }
    }
    if let Some(s) = start {
        columns.push((s, &text[s..]));
    }
    let (at, last) = *columns.last().filter(|_| columns.len() > 1)?;
    let figure = last.ends_with('%')
        || last.ends_with("/mo")
        || last.starts_with('$')
        || last.starts_with("~$")
        || last.starts_with("at least ")
        || matches!(last, "unknown" | "not measured" | "saving not measured");
    if figure {
        return None;
    }
    match columns.iter().rev().nth(1) {
        Some((tag, text)) if text.starts_with('[') && columns.len() > 2 => Some(*tag),
        _ => Some(at),
    }
}

/// A plain report's line whose heading starts with `head`: its verdict word, heading
/// (name, value, figure and percent) with padding collapsed, and the evidence under it,
/// which only details show. The evidence cards after the lines are not searched.
fn report_line(text: &str, head: &str) -> (String, String, String) {
    // Every line but the header sits inside the report's two-cell margin.
    let lines: Vec<&str> = text
        .lines()
        .map(|l| l.strip_prefix("  ").unwrap_or(l))
        .take_while(|l| *l != "EVIDENCE")
        .collect();
    lines
        .iter()
        .enumerate()
        .find_map(|(i, line)| {
            let (word, _) = line.split_once(' ')?;
            if !["change", "consider", "keep", "unknown", "info"].contains(&word) {
                return None;
            }
            let rest = line.get(9..)?;
            // The heading ends where the setting starts.
            let end = setting_start(rest).unwrap_or(rest.len());
            let heading = rest[..end].split_whitespace().collect::<Vec<_>>().join(" ");
            let under = |n: usize| {
                lines
                    .get(i + n)
                    .and_then(|l| l.strip_prefix("         "))
                    .unwrap_or_default()
            };
            // Details open each line with what it does, then its evidence.
            let evidence = match under(1).starts_with("What: ") {
                true => under(2),
                false => under(1),
            };
            heading
                .starts_with(head)
                .then(|| (word.to_string(), heading, evidence.to_string()))
        })
        .unwrap_or_else(|| panic!("no line starts with {head:?} in {text}"))
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
        native_path(&w.claude),
        "--prices-file".into(),
        native_path(fixture("prices.json")),
    ]
}

/// An instant after every fixture request, so analyses don't depend on the wall clock.
const NOW: i64 = 1_796_083_200_000;

/// The analysis the binary runs on this workspace, through the library, with `select`
/// choosing the profiles as the binary's directory flags would.
fn analysis(
    w: &Workspace,
    select: impl FnOnce(&mut token_max::DiscoveryOptions),
    max_cost_per_use_usd: f64,
) -> token_max::Report {
    use token_max::DiscoveryOptions;
    use token_max::Env;
    use token_max::{AnalyzeOptions, analyze};
    use token_max::{PriceOptions, load_prices};
    use token_max::{Reach, ScanOptions, scan};

    let mut discovery = DiscoveryOptions {
        home: native_path(&w.home),
        env: Env::default(),
        claude_dirs: vec![],
        codex_dirs: vec![],
        t3_dirs: vec![],
        managed_settings_file: native_path(w.home.join("managed-settings.json")),
        codex_system_config_file: native_path(w.home.join("system.toml")),
        codex_requirements_file: native_path(w.home.join("requirements.toml")),
        defaults: false,
        billing: Default::default(),
    };
    select(&mut discovery);
    let catalog = load_prices(&PriceOptions {
        file: Some(fixture("prices.json")),
        cache: PathBuf::new(),
        offline: false,
    })
    .unwrap();
    analyze(AnalyzeOptions {
        scan: scan(&ScanOptions {
            discovery,
            since: None,
            reach: Reach::History,
            reading: None,
        })
        .unwrap(),
        catalog: &catalog,
        since: None,
        now: NOW,
        max_cost_per_use_usd,
    })
    .unwrap()
}

/// The first check `find` matches, which the test expects to exist.
fn check(report: &token_max::Report, find: impl Fn(&CheckResult) -> bool) -> &CheckResult {
    report.checks.iter().find(|c| find(c)).expect("the check")
}

/// `token-max doctor` with `base` and `extra` options.
fn args<'a>(base: &'a [String], extra: &[&'a str]) -> Vec<&'a str> {
    ["doctor"]
        .into_iter()
        .chain(base.iter().map(String::as_str))
        .chain(extra.iter().copied())
        .collect()
}

#[test]
fn conditional_codex_prices_keep_assumptions_in_the_analysis_and_text() {
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
            native_path(&w.codex),
            "--prices-file".into(),
            native_path(fixture("prices.json")),
        ];
        let report = analysis(&w, |d| d.codex_dirs = vec![native_path(&w.codex)], 1.0);
        let row = &report.historical[0];
        assert_eq!(row.cost.total_usd.map(Usd::to_f64), expected, "{tier:?}");
        assert_eq!(row.conditional_requests, u64::from(expected.is_some()));
        assert_eq!(report.scan.coverage.tier_known, 0);
        if expected.is_some() {
            assert!(
                row.cost
                    .assumptions
                    .iter()
                    .any(|a| a.contains("conditional"))
            );
            assert!(
                row.tier_sensitivity
                    .iter()
                    .any(|s| s.tier == "flex" && s.cost.total_usd.is_none())
            );
        }
        // The default report points to details, which show the assumption.
        let (status, stdout, stderr) = run(&mut command(&w, &args(&base, &[])));
        assert!((0..=1).contains(&status), "{stderr}");
        if expected.is_some() {
            assert!(
                stdout.contains("--details shows each tier's cost"),
                "{stdout}"
            );
        }
        let (status, stdout, stderr) = run(&mut command(&w, &args(&base, &["--details"])));
        assert!((0..=1).contains(&status), "{stderr}");
        assert!(
            stdout.contains("Served-tier scenario: flex: unknown"),
            "{stdout}"
        );
        assert!(stdout.contains("tier sensitivity"), "{stdout}");
    }
}

#[test]
fn exit_codes_and_no_partial_findings_or_transcript_content() {
    let w = Workspace::new();
    w.fixture();
    w.settings(r#"{"promptCacheTtl":"5m"}"#);
    let before = std::fs::read(w.transcript()).unwrap();
    let base = prices_args(&w);
    let invoke = || {
        let mut c = command(&w, &args(&base, &[]));
        c.env("CLAUDE_CONFIG_DIR", &w.claude)
            .env("CODEX_HOME", &w.codex);
        run(&mut c)
    };
    let (status, stdout, stderr) = invoke();
    assert_eq!(status, 1, "{stderr}");
    assert_eq!(stderr, "");
    assert!(
        stdout.starts_with("token-max  2 requests in 1 session from "),
        "{stdout}"
    );
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
    assert!(
        stderr.starts_with("token-max: ") && stderr.ends_with(":1\n"),
        "{stderr}"
    );
}

#[test]
fn vibes_prints_advice_with_no_verdict_or_exit_code_and_only_takes_its_options() {
    let w = Workspace::new();
    w.fixture();
    // A TTL doctor recommends raising, and a cap past the margin under the one set now.
    const SETTINGS: &str = r#"{"promptCacheTtl":"5m","cleanupPeriodDays":365,"env":{"BASH_MAX_TIMEOUT_MS":"7200000"}}"#;
    w.settings(SETTINGS);
    let base = prices_args(&w);
    let invoke = |name: &str, extra: &[&str]| {
        let mut all = vec![name];
        all.extend(base.iter().map(String::as_str));
        all.extend(extra);
        run(&mut command(&w, &all))
    };
    // Doctor's measured change sets its exit code; the cap is no line of its report, which
    // only names the command that has it.
    let (status, report, stderr) = invoke("doctor", &["--details"]);
    assert_eq!((status, stderr.as_str()), (1, ""), "{report}");
    assert!(!report.contains("Bash timeout cap") && !report.contains("BASH_MAX_TIMEOUT_MS"));
    assert!(!report.contains("WARNINGS"), "{report}");
    assert!(
        report.ends_with(
            "\n\n  token-max vibes has 3 pieces of advice the transcripts can't price. Run token-max vibes with the same profile, host, --since and price options to see them.\n"
        ),
        "{report}"
    );
    // The same scan's advice: cited cards, each change named and not made, exit 0.
    let (status, stdout, stderr) = invoke("vibes", &[]);
    assert_eq!((status, stderr.as_str()), (0, ""), "{stdout}");
    let (cards, advisor) = stdout.split_at(
        stdout
            .find("\n  [5/5] Advisor model  not measured\n")
            .unwrap_or_else(|| panic!("{stdout}")),
    );
    assert_eq!(
        cards,
        format!(
            "token-max vibes: 3 pieces of advice the transcripts can't price.\n  None of it is measured, so none of it is a verdict. Each card names whose advice it is.\n\n  [1/5] Bash timeout cap  not measured\n        2h -> 4m\n        What    The longest Claude waits on a foreground Bash command before Claude Code moves it to the background.\n        Advice  A foreground wait at or past the 5m cache TTL, the current main-agent cache TTL setting, lets the cache expire before Claude replies, and a 4m cap leaves room under it. No foreground Bash wait outlasted the cache. What a lower cap saves is not measured, and the 4m margin is a guess. A command past the cap is not killed: it keeps running in the background, where Claude checks on it in later turns. Some commands need longer in the foreground; keep the cap if yours do. Project and local settings, which override it, are not read.\n        Source  The maintainer's pick of a margin under the cache TTL. Claude Code's docs: https://code.claude.com/docs/en/env-vars https://code.claude.com/docs/en/tools-reference https://code.claude.com/docs/en/prompt-caching\n        Change  \"env\": {{\"BASH_MAX_TIMEOUT_MS\": \"240000\"}} in ~{0}.claude{0}settings.json\n\n  [2/5] Auto memory  not measured\n        What    Auto memory lets Claude write notes for itself and loads them into every session.\n        Advice  0 of 1 main session (0%) in the window ran in a repository with a CLAUDE.md or AGENTS.md, counting as without 1 session whose transcript recorded loading none and whose directory is gone or unrecorded. Outside a repository, memory may be the only place a fact is kept, so turning it off is not advised. doctor's line could not price it over the window (never read back or written).\n        Source  The maintainer's pick: a lasting fact about a project belongs in its repository. How memory works: https://code.claude.com/docs/en/memory#auto-memory (2026-11-11)\n\n  [3/5] Auto-compact window (claude-opus-5-5)  not measured\n        967k -> 400k\n        What    How large a conversation grows before it is automatically summarized and continued from the summary.\n        Advice  No setting names it, so Claude Code compacts this model near 967k, its window for the model. The maintainer would cap it at 400k for better output, not a smaller bill: a smaller window compacts sooner, so the model works from a shorter context. Anthropic's docs say accuracy and recall degrade as a context grows. doctor's auto-compact window line keeps it on cost. Its replay did not price a 400k window, so what the cap costs is not measured. The cap is advice beside that line, not its verdict.\n        Source  The maintainer's pick, for output quality rather than cost. Anthropic's docs on context rot: https://platform.claude.com/docs/en/build-with-claude/context-windows\n        Change  \"modelSettings\": {{\"claude-opus-5-5\": {{\"autoCompactWindow\": 400000}}}} in ~{0}.claude{0}settings.json\n\n  [4/5] Main-session default  not measured\n        What    The model and reasoning effort a main session starts with.\n        Advice  Defaults to claude-opus-5-5 at medium, the maintainer's pick. Ran claude-opus-5-5 at an unrecorded effort 2 times, $0.01.\n        Source  The maintainer's pick of a default. Anthropic's guidance: https://claude.dev/blog/which-claude-model-should-you-build-with/ https://code.claude.com/docs/en/model-config#choose-an-effort-level\n",
            std::path::MAIN_SEPARATOR
        )
    );
    // The fixture's Opus main gets a Fable advisor, one rank up.
    assert!(
        advisor.starts_with("\n  [5/5] Advisor model  not measured\n        none -> fable\n")
            && advisor.contains("Fable ranks one above claude-opus-5-5, the main-session default (the latest recorded main-session model).")
            && advisor.ends_with(&format!(
                "        Change  \"advisorModel\": \"fable\" in ~{0}.claude{0}settings.json\n",
                std::path::MAIN_SEPARATOR
            )),
        "{advisor}"
    );
    assert_eq!(
        std::fs::read_to_string(w.claude.join("settings.json")).unwrap(),
        SETTINGS
    );
    // Settings pretended away are not the ones vibes reads, so --defaults names no count.
    let (_, report, _) = invoke("doctor", &["--defaults"]);
    assert!(!report.contains("token-max vibes"), "{report}");
    // A cap already at the margin, a window at the maintainer's and an advisor already set
    // keep their cards, and doctor has nothing to name.
    w.settings(
        r#"{"promptCacheTtl":"1h","autoCompactWindow":400000,"advisorModel":"fable","cleanupPeriodDays":365,"env":{"BASH_MAX_TIMEOUT_MS":"3300000"}}"#,
    );
    let (status, report, _) = invoke("doctor", &[]);
    assert_eq!(status, 0, "{report}");
    assert!(!report.contains("token-max vibes"), "{report}");
    let (status, stdout, _) = invoke("vibes", &[]);
    assert_eq!(status, 0);
    // The heading counts advice as doctor does, so the card that advises nothing is not in it.
    assert!(
        stdout.starts_with("token-max vibes: no advice for these settings.\n\n  [1/5] "),
        "{stdout}"
    );
    assert!(
        stdout.contains("\n  [1/5] Bash timeout cap  not measured\n        What    ")
            && stdout.contains("\n  [3/5] Auto-compact window (claude-opus-5-5)  not measured\n        What    ")
            && stdout.contains("\n  [5/5] Advisor model  not measured\n        What    ")
            && stdout.contains("Set to 400k. That is at or under the 400k window the maintainer caps it at, so there is nothing to change.")
            && stdout.contains("Capped at 55m, the margin suggested under the 1h cache, the current main-agent cache TTL setting.")
            && !stdout.contains("Change"),
        "{stdout}"
    );
    // Advice has no verdict to tune, evidence to expand or defaults to pretend.
    for extra in [
        &["--details"][..],
        &["--defaults"],
        &["--max-cost-per-use", "2"],
        &["--ssh-scan"],
        &["-v"],
        &["--bogus"],
    ] {
        let (status, stdout, stderr) = invoke("vibes", extra);
        assert_eq!((status, stdout.as_str()), (2, ""), "{extra:?}");
        assert_eq!(
            stderr, "token-max: invalid arguments; use token-max vibes --help\n",
            "{extra:?}"
        );
    }
    let (status, help, _) = invoke("vibes", &["--help"]);
    assert_eq!(status, 0);
    assert!(help.starts_with("token-max vibes: advice the transcripts can't price"));
    let (_, top, _) = run(&mut command(&w, &["--help"]));
    assert!(
        top.contains("\n  vibes     Advice the transcripts can't price"),
        "{top}"
    );
    // A failed scan is the one way it exits nonzero.
    std::fs::write(w.transcript(), "{\"type\":\"assistant\",\"timestamp\":\"2020-01-01T00:00:00Z\",\"message\":{\"usage\":{\"input_tokens\":\"bad\"}}}\n").unwrap();
    let (status, stdout, stderr) = invoke("vibes", &[]);
    assert_eq!((status, stdout.as_str()), (2, ""), "{stderr}");
}

#[test]
fn vibes_advises_memory_off_when_most_sessions_ran_in_a_repository() {
    let w = Workspace::new();
    // A checkout whose root holds an AGENTS.md, and the fixture's session in a directory
    // under it. Its own .git ends the walk up before this repository's.
    let repo = w.home.join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("AGENTS.md"), "synthetic\n").unwrap();
    // The fixture's session in `cwd`, after an attachment recording the files it loaded.
    let session = |cwd: &Path, loaded: &[&Path]| {
        let files: Vec<Value> = loaded
            .iter()
            .map(|p| {
                // Claude Code types the user's own file apart from the project's.
                let kind = if p.starts_with(&w.claude) {
                    "User"
                } else {
                    "Project"
                };
                json!({"path": p.to_string_lossy(), "content": "synthetic", "type": kind})
            })
            .collect();
        let attachment = json!({
            "type": "attachment", "sessionId": "session-a", "uuid": "attachment-a",
            "timestamp": "2026-11-01T00:00:00Z", "cwd": cwd.to_string_lossy(),
            "attachment": {"type": "instructions", "files": files},
        });
        let mut text = format!("{attachment}\n");
        for line in std::fs::read_to_string(fixture("claude.jsonl"))
            .unwrap()
            .lines()
        {
            let mut record: Value = serde_json::from_str(line).unwrap();
            record["cwd"] = json!(cwd.to_string_lossy());
            text += &format!("{record}\n");
        }
        std::fs::write(w.transcript(), text).unwrap();
    };
    session(&repo.join("src"), &[]);
    // Memory is on by default; a 55m cap under a 1h cache, a window at the maintainer's cap,
    // a year of transcripts and an advisor already set leave the other cards quiet.
    w.settings(
        r#"{"promptCacheTtl":"1h","autoCompactWindow":400000,"advisorModel":"fable","cleanupPeriodDays":365,"env":{"BASH_MAX_TIMEOUT_MS":"3300000"}}"#,
    );
    let base = prices_args(&w);
    let invoke = |name: &str| {
        let mut all = vec![name];
        all.extend(base.iter().map(String::as_str));
        run(&mut command(&w, &all))
    };
    // doctor's measured line gives no verdict here, so vibes has the one piece of advice.
    let (_, report, _) = invoke("doctor");
    assert!(
        report.contains("token-max vibes has 1 piece of advice"),
        "{report}"
    );
    let (status, stdout, stderr) = invoke("vibes");
    assert_eq!((status, stderr.as_str()), (0, ""), "{stdout}");
    let card = &stdout[stdout.find("Auto memory").expect(&stdout)..];
    assert!(
        card.starts_with("Auto memory  not measured\n        on -> off\n"),
        "{stdout}"
    );
    assert!(
        card.contains("Advice  1 of 1 main session (100%) in the window ran in a repository with a CLAUDE.md or AGENTS.md. A lasting fact about a project belongs in that file"),
        "{stdout}"
    );
    let sep = std::path::MAIN_SEPARATOR;
    assert!(
        card.contains(&format!(
            "        Change  \"autoMemoryEnabled\": false in ~{sep}.claude{sep}settings.json\n"
        )),
        "{stdout}"
    );
    // Without the file, the session ran where memory may be the only place for a fact.
    std::fs::remove_file(repo.join("AGENTS.md")).unwrap();
    let (status, stdout, _) = invoke("vibes");
    assert_eq!(status, 0);
    assert!(
        stdout.starts_with("token-max vibes: no advice for these settings."),
        "{stdout}"
    );
    assert!(
        stdout.contains("0 of 1 main session (0%)") && !stdout.contains("on -> off"),
        "{stdout}"
    );
    // A session whose directory is gone counts where its transcript recorded loading a
    // project file from it, and not where it recorded only the user's own, though that sits
    // above every directory in the home.
    let gone = w.home.join("gone");
    let user = w.claude.join("CLAUDE.md");
    session(&gone.join("src"), &[&user, &gone.join("AGENTS.md")]);
    let (_, stdout, _) = invoke("vibes");
    assert!(
        stdout.contains("1 of 1 main session (100%)") && stdout.contains("on -> off"),
        "{stdout}"
    );
    session(&gone.join("src"), &[&user]);
    let (_, stdout, _) = invoke("vibes");
    assert!(
        stdout.contains("0 of 1 main session (0%) in the window ran in a repository with a CLAUDE.md or AGENTS.md, counting as without 1 session whose transcript recorded loading none"),
        "{stdout}"
    );
}

#[test]
fn vibes_suggests_a_year_of_transcripts_unless_claude_already_keeps_one() {
    let w = Workspace::new();
    w.fixture();
    // The transcript's size, as the card rounds it.
    assert_eq!(std::fs::metadata(w.transcript()).unwrap().len(), 2252);
    let base = prices_args(&w);
    let cards = |settings: &str| {
        w.settings(settings);
        let mut all = vec!["vibes"];
        all.extend(base.iter().map(String::as_str));
        let (status, stdout, stderr) = run(&mut command(&w, &all));
        assert_eq!((status, stderr.as_str()), (0, ""), "{stdout}");
        stdout
    };
    let file = format!("~{0}.claude{0}settings.json", std::path::MAIN_SEPARATOR);
    // Unset is Claude Code's 30 days; the card counts the disk the transcripts read take.
    let unset = cards("{}");
    assert!(
        unset.contains("  [2/6] Transcript retention  not measured\n        30 days -> 365 days\n        What    How long Claude Code keeps session transcripts before its startup sweep deletes them.\n        Advice  Claude Code deletes eligible transcripts after 30 days, so token-max has less history to replay. The transcripts token-max read take "),
        "{unset}"
    );
    assert!(
        unset.contains(&format!("The setting only stops future deletion: history already deleted is gone. Not set; it would go in {file}:cleanupPeriodDays.")),
        "{unset}"
    );
    assert!(
        unset.contains(&format!(
            "        Change  \"cleanupPeriodDays\": 365 in {file}\n"
        )),
        "{unset}"
    );
    assert!(unset.contains("take 2.3 kB today."), "{unset}");
    // A shorter period set in the file is edited where it is.
    let short = cards(r#"{"cleanupPeriodDays":90}"#);
    assert!(short.contains("        90 days -> 365 days\n"), "{short}");
    assert!(
        short.contains(&format!("Set in {file}:cleanupPeriodDays.")),
        "{short}"
    );
    // A year or more, or a value Claude Code rejects, advises nothing.
    for settings in [
        r#"{"cleanupPeriodDays":365}"#,
        r#"{"cleanupPeriodDays":3650}"#,
        r#"{"cleanupPeriodDays":0}"#,
        r#"{"cleanupPeriodDays":"90"}"#,
    ] {
        assert!(
            !cards(settings).contains("Transcript retention"),
            "{settings}"
        );
    }
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
        &args(&base, &["--max-cost-per-use", "0.001"]),
    ));
    assert_eq!(status, 1, "{stderr}\n{stdout}");
    assert!(stdout.contains("mcp__docs__write"), "{stdout}");
    let report = analysis(&w, |d| d.claude_dirs = vec![native_path(&w.claude)], 0.001);
    assert!(report.checks.iter().all(|c| c.action() != Action::Change));
    let server = report.context.iter().find(|c| c.name == "docs").unwrap();
    assert_eq!(server.verdict, Some(Verdict::Keep));
    let write = server
        .details
        .iter()
        .find(|c| c.name == "mcp__docs__write")
        .unwrap();
    assert_eq!(write.verdict, Some(Verdict::Off));
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
    let report = analysis(&w, |d| d.claude_dirs = vec![native_path(&w.claude)], 1.0);
    let CheckResult::Compaction(result) =
        check(&report, |c| matches!(c, CheckResult::Compaction(_)))
    else {
        unreachable!()
    };
    assert_eq!(result.base.action, Action::Change);
    assert_eq!(result.base.current, Some(400_000));
    assert_eq!(result.base.recommended, Some(100_000));
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
        &result.base.owner,
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
    assert!(stdout.contains("\n  No changes recommended"));
    assert!(stdout.lines().any(|l| l.starts_with("  keep     ")));
    assert!(!stdout.contains('\x1b') && !stdout.contains('→'));
    assert!(!stdout.contains("SYNTHETIC_PRIVATE"));
    assert_eq!(stderr, "");
    let (status, stdout, stderr) = invoke(&["--since", "2027-01-01"]);
    assert_eq!(status, 0, "{stderr}");
    assert!(stdout.contains("No requests found"));
    assert!(stdout.contains("earlier --since"));
    let cache = native_path(fixture("prices.json"));
    let claude = native_path(&w.claude);
    let (status, stdout, stderr) = run(&mut command(
        &w,
        &[
            "doctor",
            "--claude-dir",
            &claude,
            "--offline",
            "--price-cache",
            &cache,
        ],
    ));
    assert_eq!(status, 0, "{stderr}");
    assert!(stdout.contains("Prices: cached from"));
    let absent = native_path(w.home.join("absent-prices"));
    let (status, stdout, stderr) = run(&mut command(
        &w,
        &[
            "doctor",
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
    assert!(
        help.lines()
            .any(|l| l.starts_with("  token-max doctor --since "))
    );
    for row in help.lines().filter(|l| l.starts_with("  -")) {
        assert_eq!(description_column(row), Some(26), "{row}");
    }
    let (status, stdout, stderr) = invoke(&["--bogus"]);
    assert_eq!(status, 2);
    assert_eq!(stdout, "");
    assert_eq!(
        stderr,
        "token-max: invalid arguments; use token-max doctor --help\n"
    );
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
        "NO_COLOR",
        "on a terminal: exit 0",
        "Otherwise: exit 0",
        "--version",
    ] {
        assert!(help.contains(flag), "{flag}");
    }
    assert!(!help.contains("--json"));
}

#[test]
fn arguments_follow_strict_parse_args() {
    let w = Workspace::new();
    let base = prices_args(&w);
    let invoke = |extra: &[&str]| run(&mut command(&w, &args(&base, extra)));
    for extra in [
        &["positional"][..],
        &["--json"],
        &["--plain=1"],
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
            stderr, "token-max: invalid arguments; use token-max doctor --help\n",
            "{extra:?}"
        );
    }
    // The version and the command list belong to the bare command.
    let top = |args: &[&str]| run(&mut command(&w, args));
    let (status, stdout, _) = top(&["-hv"]);
    assert_eq!(
        (status, stdout.starts_with("token-max: agent settings")),
        (0, true)
    );
    let (status, stdout, _) = top(&["-v", "--"]);
    assert_eq!(
        (status, stdout),
        (0, format!("{}\n", env!("CARGO_PKG_VERSION")))
    );
    assert_eq!(
        invoke(&["-v"]),
        (0, format!("{}\n", env!("CARGO_PKG_VERSION")), String::new())
    );
    let (status, stdout, stderr) = top(&[]);
    assert_eq!((status, &*stdout), (2, ""));
    assert!(stderr.starts_with("token-max: agent settings") && stderr.contains("\n  doctor "));
    let (status, _, stderr) = top(&["--since", "2026-11-01"]);
    assert_eq!(status, 2);
    assert_eq!(
        stderr,
        "token-max: options follow a command, such as token-max doctor --since 2026-11-01; use --help\n"
    );
    for args in [&["report"][..], &["--bogus"], &["-x"]] {
        let (status, _, stderr) = top(args);
        assert_eq!(status, 2, "{args:?}");
        assert_eq!(
            stderr, "token-max: invalid arguments; use --help\n",
            "{args:?}"
        );
    }
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

#[test]
fn sessions_prints_one_json_line_per_session_and_only_takes_its_options() {
    let w = Workspace::new();
    w.fixture();
    let before = std::fs::read(w.transcript()).unwrap();
    let base = prices_args(&w);
    let invoke = |extra: &[&str]| {
        let mut all = vec!["sessions"];
        all.extend(base.iter().map(String::as_str).chain(extra.iter().copied()));
        run(&mut command(&w, &all))
    };
    let (status, stdout, stderr) = invoke(&[]);
    assert_eq!((status, &*stderr), (0, ""));
    assert!(!stdout.contains("SYNTHETIC_PRIVATE"));
    assert_eq!(std::fs::read(w.transcript()).unwrap(), before);
    let lines: Vec<Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["harness"], "claude");
    assert_eq!(lines[0]["session"], "session-a");
    assert_eq!(lines[0]["files"], json!([native_path(w.transcript())]));
    assert!(lines[0]["costUsd"].as_f64().unwrap() > 0.0);
    // This host, and no directory to find a checkout in.
    for field in ["host", "cwd", "repositories", "repositoriesSource"] {
        assert_eq!(lines[0].get(field), Some(&Value::Null), "{field}");
    }
    // After the fixture's requests, nothing is selected.
    assert_eq!(
        invoke(&["--since", "2030-01-01"]),
        (0, String::new(), String::new())
    );
    for extra in [
        &["--json"][..],
        &["--details"],
        &["--max-cost-per-use", "1"],
        &["-v"],
    ] {
        let (status, stdout, stderr) = invoke(extra);
        assert_eq!((status, &*stdout), (2, ""), "{extra:?}");
        assert_eq!(
            stderr,
            "{\"error\":\"invalid arguments; use token-max sessions --help\"}\n"
        );
    }
    let (status, help, _) = invoke(&["--help"]);
    assert!(status == 0 && help.starts_with("token-max sessions:"));
    let (_, help, _) = run(&mut command(&w, &["--help"]));
    assert!(help.contains("\n  sessions "), "{help}");
}

/// Floats print in shortest form: integers without a fraction, no `1.0`.
#[test]
fn session_numbers_print_like_javascript() {
    let w = Workspace::new();
    // An unpriced model costs a whole 0 dollars.
    let lines = std::fs::read_to_string(fixture("claude.jsonl"))
        .unwrap()
        .replace("claude-opus-5-5", "synthetic-unpriced-model");
    std::fs::write(w.transcript(), lines).unwrap();
    let base = prices_args(&w);
    let mut all = vec!["sessions"];
    all.extend(base.iter().map(String::as_str));
    let (status, stdout, stderr) = run(&mut command(&w, &all));
    assert_eq!(status, 0, "{stderr}");
    assert!(stdout.contains(r#""costUsd":0,"#), "{stdout}");
    assert!(
        !stdout.contains(".0,") && !stdout.contains(".0}"),
        "{stdout}"
    );
    assert!(stdout.ends_with("}\n") && stdout.matches('\n').count() == 1);
}

/// Windows from `--since` replay the same cache comparison as the library.
#[test]
fn explicit_windows_match_the_library_replay() {
    use token_max::DiscoveryOptions;
    use token_max::Env;
    use token_max::{Action, CheckResult};
    use token_max::{AnalyzeOptions, analyze};
    use token_max::{PriceOptions, load_prices};
    use token_max::{Reach, ScanOptions, scan};
    use token_max::{ReportOptions, format_report};

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
                home: native_path(&w.home),
                env: Env::default(),
                claude_dirs: vec![],
                codex_dirs: vec![],
                t3_dirs: vec![],
                managed_settings_file: native_path(w.home.join("managed-settings.json")),
                codex_system_config_file: native_path(w.home.join("system.toml")),
                codex_requirements_file: native_path(w.home.join("requirements.toml")),
                defaults: false,
                billing: Default::default(),
            },
            since: Some(since),
            reach: Reach::History,
            reading: None,
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
        assert!(
            matches!(report.checks.first(), Some(CheckResult::Ttl(_))),
            "{mode}: no TTL check"
        );
        let claude = native_path(&w.claude);
        let prices = native_path(fixture("prices.json"));
        let window = iso(18);
        let mut c = command(
            &w,
            &[
                "doctor",
                "--claude-dir",
                &claude,
                "--since",
                &window,
                "--prices-file",
                &prices,
                "--details",
            ],
        );
        c.env("CLAUDE_CONFIG_DIR", &w.claude);
        let (status, stdout, stderr) = run(&mut c);
        let expected = i32::from(report.checks.iter().any(|c| c.action() == Action::Change));
        assert_eq!(status, expected, "{mode}: {stderr}");
        // The binary prints the library's window-total comparison for this check.
        let line = |text: &str| {
            text.lines()
                .find(|l| l.contains("Cache comparison"))
                .map(str::to_owned)
        };
        let library = format_report(
            &report,
            &token_max::fixes(&report, false),
            ReportOptions {
                details: true,
                ..ReportOptions::default()
            },
        )
        .text();
        assert!(line(&library).is_some(), "{mode}: {library}");
        assert_eq!(line(&stdout), line(&library), "{mode}");
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
        support::isolated(&mut c)
            .args(extra)
            .current_dir(&consumer)
            .env("HOME", &w.home)
            // Windows resolves the home from USERPROFILE and ignores HOME.
            .env("USERPROFILE", &w.home)
            .env("NO_COLOR", "1")
            .env("TOKEN_MAX_RELEASES", support::current())
            .stdin(Stdio::null());
        run(&mut c)
    };
    let profile = ["--claude-dir", "profile", "--prices-file", "prices.json"];
    let with = |extra: &'static str| [&["doctor"][..], &profile[..], &[extra][..]].concat();

    let (status, help, stderr) = invoke(&["--help"]);
    assert_eq!(status, 0, "{stderr}");
    assert!(help.contains("Usage: token-max"));
    let (status, version, stderr) = invoke(&["--version"]);
    assert_eq!(status, 0, "{stderr}");
    assert_eq!(version, format!("{}\n", env!("CARGO_PKG_VERSION")));

    std::fs::write(&settings, r#"{"promptCacheTtl":"5m"}"#).unwrap();
    let (status, text, text_err) = invoke(&with("--plain"));
    assert_eq!(status, 1, "{text_err}");
    let (mark, head, _) = report_line(&text, "Main-agent prompt cache TTL 5m -> 1h");
    assert_eq!(mark, "change");
    assert!(
        head.contains("saves $") || head.contains("saves <$"),
        "{text}"
    );
    assert!(
        text.starts_with("token-max  2 requests in 1 session from "),
        "{text}"
    );
    assert_eq!(report_line(&text, "Idle compaction").0, "unknown", "{text}");
    let all = format!("{text}{text_err}");
    assert!(!all.contains("SYNTHETIC_PRIVATE"));
    assert_eq!(std::fs::read(&transcript).unwrap(), original);
    assert_eq!(
        std::fs::read_to_string(&settings).unwrap(),
        r#"{"promptCacheTtl":"5m"}"#
    );

    std::fs::write(&settings, r#"{"promptCacheTtl":"1h"}"#).unwrap();
    let (status, clear, stderr) = invoke(&with("--plain"));
    assert_eq!(status, 0, "{stderr}");
    assert!(clear.contains("\n  No changes recommended"), "{clear}");

    let (status, stdout, stderr) = invoke(&["--bogus"]);
    assert_eq!(status, 2, "{stderr}");
    assert_eq!(stdout, "");
    assert!(stderr.contains("invalid arguments"), "{stderr}");

    std::fs::write(
        &transcript,
        r#"{"timestamp":"2020-01-01T00:00:00Z","type":"assistant","private":"SYNTHETIC_PRIVATE","message":{"usage":{"input_tokens":"bad"}}}
"#,
    )
    .unwrap();
    let (status, stdout, stderr) = invoke(&with("--plain"));
    assert_eq!(status, 2, "{stderr}");
    assert_eq!(stdout, "");
    assert!(stderr.ends_with(":1\n"), "{stderr}");
    assert!(!stderr.contains("SYNTHETIC_PRIVATE"));
}

/// Piped, a newer release is a line on stderr naming how to upgrade, and the run goes on with
/// this build and its exit code; offline, nothing is asked.
#[test]
// Only a platform with a release has one to offer.
#[cfg(any(
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    target_os = "macos",
    windows
))]
fn without_a_terminal_a_newer_release_or_a_failed_check_is_one_line_on_stderr() {
    let w = Workspace::new();
    w.fixture();
    w.settings(r#"{"promptCacheTtl":"5m"}"#);
    let base = prices_args(&w);
    let doctor = args(&base, &["--plain"]);
    let (status, report, stderr) = run(&mut command(&w, &doctor));
    assert_eq!((status, stderr.as_str()), (1, ""));
    let releases = support::Releases::new();
    releases.latest("v99.0.0");
    let mut c = command(&w, &doctor);
    c.env("TOKEN_MAX_RELEASES", &releases.url);
    let ours = env!("CARGO_PKG_VERSION");
    let stale =
        format!("token-max: v99.0.0 is out; this is v{ours}, and its advice may be out of date.");
    assert_eq!(
        run(&mut c),
        (
            1,
            report.clone(),
            format!("{stale} Run token-max doctor on a terminal to upgrade.\n")
        )
    );
    assert_eq!(releases.checks(), 1);
    // Only `doctor` checks: `vibes` asks nothing of the release server.
    let mut vibes = doctor.clone();
    vibes[0] = "vibes";
    let mut c = command(&w, &vibes);
    c.env("TOKEN_MAX_RELEASES", &releases.url);
    let (status, _, stderr) = run(&mut c);
    assert_eq!((status, stderr.as_str(), releases.checks()), (0, "", 1));
    // A binary Cargo installed is Cargo's to upgrade.
    let cargo = w.home.join(".cargo/bin");
    std::fs::create_dir_all(&cargo).unwrap();
    let bin = cargo.join(format!("token-max{}", std::env::consts::EXE_SUFFIX));
    std::fs::hard_link(BIN, &bin).unwrap();
    let mut c = command_at(&bin, &w, &doctor);
    c.env("TOKEN_MAX_RELEASES", &releases.url);
    assert_eq!(
        run(&mut c),
        (
            1,
            report.clone(),
            format!(
                "{stale} Upgrade with cargo binstall token-max or cargo install --locked token-max.\n"
            )
        )
    );
    // A run without a terminal never probes the directory, which writes beside the binary, so
    // even one this user cannot write only hears that a terminal run upgrades.
    #[cfg(unix)]
    // SAFETY: geteuid has no preconditions. Root can write anything.
    if unsafe { libc::geteuid() } != 0 {
        use std::os::unix::fs::PermissionsExt;
        let locked = w.home.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        let bin = locked.join("token-max");
        std::fs::hard_link(BIN, &bin).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
        let mut c = command_at(&bin, &w, &doctor);
        c.env("TOKEN_MAX_RELEASES", &releases.url);
        let ran = run(&mut c);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            ran,
            (
                1,
                report.clone(),
                format!("{stale} Run token-max doctor on a terminal to upgrade.\n")
            )
        );
    }
    // Offline asks nothing.
    let checked = releases.checks();
    let mut c = command(&w, &args(&base, &["--plain", "--offline"]));
    c.env("TOKEN_MAX_RELEASES", &releases.url);
    assert_eq!(run(&mut c), (1, report.clone(), String::new()));
    assert_eq!(releases.checks(), checked);
    // Unreachable releases are one line, and the run goes on.
    let mut c = command(&w, &doctor);
    let nowhere = "http://127.0.0.1:1/releases";
    c.env("TOKEN_MAX_RELEASES", nowhere);
    let (status, stdout, stderr) = run(&mut c);
    assert_eq!((status, stdout), (1, report));
    // The cause is the platform's own wording.
    let failed =
        format!("token-max: could not check for a newer release: could not reach {nowhere}: ");
    assert!(stderr.starts_with(&failed), "{stderr}");
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
}

// Built CLI warns from current settings alone, remains read-only and skips unreadable lines.
#[test]
fn a_fast_setting_with_no_recorded_usage_has_no_verdict_and_skips_unreadable_lines() {
    let w = Workspace::new();
    let settings = w.codex.join("config.toml");
    let config = "model=\"gpt-6.1-sol\"\nservice_tier=\"ultrafast\"";
    std::fs::write(&settings, config).unwrap();
    let base = [
        "--codex-dir".to_string(),
        native_path(&w.codex),
        "--prices-file".into(),
        native_path(fixture("prices.json")),
    ];
    let invoke = || {
        let mut c = command(&w, &args(&base, &[]));
        c.env("CODEX_HOME", &w.codex);
        run(&mut c)
    };
    // No recorded usage measures a premium, so the setting alone is no change.
    let (status, stdout, stderr) = invoke();
    assert_eq!(status, 0, "{stderr}");
    assert_eq!(report_line(&stdout, "Fast mode").0, "unknown", "{stdout}");
    let report = analysis(&w, |d| d.codex_dirs = vec![native_path(&w.codex)], 1.0);
    let fast = check(&report, |c| matches!(c, CheckResult::Speed(_)));
    let CheckResult::Speed(fast) = fast else {
        unreachable!()
    };
    assert_eq!(fast.base.savings_usd, None);
    assert_eq!(std::fs::read_to_string(&settings).unwrap(), config);
    std::fs::write(
        &settings,
        format!("{config}\n[features]\nultrafast_mode=false"),
    )
    .unwrap();
    let (status, stdout, _) = invoke();
    assert_eq!(status, 0);
    assert_eq!(report_line(&stdout, "Fast mode").0, "keep", "{stdout}");
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
    use std::io::{Read, Write};
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
        assert!(
            help.lines()
                .any(|l| l.starts_with("  token-max doctor --since "))
        );
        assert!(help.lines().all(|l| l.chars().count() <= 40), "{help}");
        assert!(help.lines().any(|l| l.len() > 26
            && l[..26].bytes().all(|b| b == b' ')
            && l.as_bytes()[26] != b' '));
        let (status, stdout, stderr) = invoke(&["--plain", "--details"], &[]);
        assert_eq!(status, 1, "{stderr}");
        let both = format!("{stdout}{stderr}");
        assert!(!both.contains('\x1b') && !both.contains('→'));
        assert!(stdout.contains("Uncertainty:"));
        // Piped evidence is not wrapped, so the path in it stays whole.
        let (_, stdout, stderr) = run(&mut command(&w, &args(&base, &["--details"])));
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
        // Each wrapped row carries its own styles, so compare the words without them.
        let mut plain = String::new();
        for (i, part) in stdout.split('\x1b').enumerate() {
            let skip = if i == 0 {
                0
            } else {
                part.find('m').map_or(0, |m| m + 1)
            };
            plain.push_str(&part[skip..]);
        }
        let stdout = plain;
        let unreadable = stdout.find("1 unreadable transcript").unwrap();
        let rest = stdout[unreadable + "1 unreadable transcript".len()..]
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(rest.starts_with("lines in 1 files"), "{stdout}");
        assert!(!format!("{stdout}{stderr}").contains("SYNTHETIC_PRIVATE"));
    }

    /// Replaces each dollar figure followed by `/mo` with `$N/mo`.
    fn per_month_masked(text: &str) -> String {
        let mut out = String::new();
        let mut rest = text;
        while let Some(at) = rest.find('$') {
            out.push_str(&rest[..at]);
            let after = &rest[at + 1..];
            let digits = after.len()
                - after
                    .trim_start_matches(|c: char| c.is_ascii_digit() || c == ',' || c == '.')
                    .len();
            if digits > 0 && after[digits..].starts_with("/mo") {
                out.push_str("$N");
            } else {
                out.push_str(&rest[at..at + 1 + digits]);
            }
            rest = &after[digits..];
        }
        out.push_str(rest);
        out
    }

    #[test]
    fn doctor_on_a_terminal_writes_only_the_changes_answered_yes() {
        // A 5m TTL to raise, and a Bash cap that outlasts even 1h, which is `vibes`' to lower.
        const SETTINGS: &str = "{\n  \"promptCacheTtl\": \"5m\",\n  \"env\": {\n    \"BASH_MAX_TIMEOUT_MS\": \"7200000\"\n  },\n  \"cleanupPeriodDays\": 365\n}\n";
        let w = Workspace::new();
        // A fixed window keeps the walk's dollar figures the same on any day.
        w.recent_fixture(15);
        w.settings(SETTINGS);
        // Prices ten thousand times the fixture's, so the TTL saves enough to be asked: the
        // walk leaves out a change saving under $1/mo.
        let mut prices: Value =
            serde_json::from_str(&std::fs::read_to_string(fixture("prices.json")).unwrap())
                .unwrap();
        fn scale(value: &mut Value) {
            match value {
                Value::Object(map) => {
                    for (key, value) in map.iter_mut() {
                        match value {
                            Value::Number(n) if key != "size" => {
                                *value = json!(n.as_f64().unwrap() * 1e4);
                            }
                            _ => scale(value),
                        }
                    }
                }
                Value::Array(items) => items.iter_mut().for_each(scale),
                _ => {}
            }
        }
        for provider in prices.as_object_mut().unwrap().values_mut() {
            for model in provider["models"].as_object_mut().unwrap().values_mut() {
                scale(&mut model["cost"]);
            }
        }
        let costly = w.home.join("costly-prices.json");
        std::fs::write(&costly, prices.to_string()).unwrap();
        let base = [
            "--claude-dir".to_string(),
            native_path(&w.claude),
            "--prices-file".into(),
            native_path(&costly),
        ];
        let mut c = command(&w, &args(&base, &["--plain"]));
        let input = Pty::new(80);
        let (stdout, out) = Pty::new(80).drain();
        let (stderr, err) = Pty::new(80).drain();
        let Pty { master, slave } = input;
        c.stdin(Stdio::from(slave)).stdout(stdout).stderr(stderr);
        let mut child = c.spawn().unwrap();
        c.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Answers wait in the terminal until each prompt reads one; the terminal stays
        // open until the child exits, as a person's would.
        let mut keyboard = std::fs::File::from(master);
        // A plan pays, then yes to the TTL and to writing it; then yes to seeing `vibes`'
        // advice, yes to its Bash cap, no to its window and its advisor, and yes to writing
        // the cap.
        keyboard.write_all(b"p\ny\ny\ny\ny\nn\nn\ny\n").unwrap();
        let status = child.wait().unwrap().code().unwrap();
        let (stdout, stderr) = (out.join().unwrap(), err.join().unwrap());
        assert_eq!(status, 0, "{stderr}");
        // The window ends now, so a per-month figure follows the clock; its percent of
        // the window's spend does not.
        let stdout = per_month_masked(&stdout);
        // The walk comes first: a measured saving before a change whose saving was not
        // measured.
        let at = |text: &str| {
            stdout
                .find(text)
                .unwrap_or_else(|| panic!("{text:?} in {stdout}"))
        };
        // The opening names what the scan will read and how to add a host, then how the
        // profile pays is asked before the scan, then the header names what was read, then
        // the walk asks its questions.
        assert!(stdout.starts_with("\n  token-max "), "{stdout}");
        let found = at("\n  Claude Code  ~/.claude  1 transcript  ");
        let hint = at("\n  + other machines: token-max doctor --ssh <host>\n\n  How is ");
        assert!(found < hint, "{stdout}");
        // The answer echoes on the input terminal, so a separate stdout shows only the
        // blank line that ends the questions.
        at(
            "\n  How is ~/.claude paid for? [p]lan, [a]PI key or usage credits, [s]kip: \ntoken-max  2 requests in 1 session from ",
        );
        let headline = at(
            "\n\n  1 question, biggest saving first.\n  Nothing is written until you confirm the list at the end.\n",
        );
        let ttl = at("\n  [1/1] Main-agent prompt cache TTL ");
        let report = at("\n  1 change recommended.\n");
        assert!(headline < ttl && ttl < report, "{stdout}");
        // Each card shows the saving, the change and its verdict, then what the setting does
        // and why; the window ends now, so its day count and the wrapping follow the clock.
        let words = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
        let entry = words(&stdout[ttl..at("\n  Profiles: ")]);
        assert!(
            entry.starts_with("[1/1] Main-agent prompt cache TTL saves $N/mo 13% 5m -> 1h Recommended What How long the main conversation's prompt stays cached between requests: a 1h cache costs more to write than 5m but survives longer pauses. Why Measured on 2 requests over "),
            "{entry}"
        );
        assert!(
            entry.ends_with(" days. Replayed at 5m and 1h, assuming the same sessions; 1h costs less. While it is unset, Claude Code gives the main conversation 1h on a Claude plan, which you answered pays for this profile; requests past the plan's included usage are paid with usage credits and get 5m. Accept? [y]es, [n]o, [q]uit: Accepted so far: about $N/mo, 13% of this host's spend."),
            "{entry}"
        );
        // Then the report, its accepted change first, and one question to write it.
        let accepted =
            at("\n  ACCEPTED  biggest saving first\n  change   Main-agent prompt cache TTL");
        // The measured report has no line for the cap, whose saving is a guess.
        assert!(
            !stdout[..at("token-max vibes")].contains("Bash timeout cap"),
            "{stdout}"
        );
        // The confirm shows the file's diff before its one question.
        let diff = at(
            "\n  ~/.claude/settings.json\n  @@ -1,3 +1,3 @@\n    {\n  -   \"promptCacheTtl\": \"5m\",\n  +   \"promptCacheTtl\": \"1h\",\n      \"env\": {\n",
        );
        let backup = at(
            "\n  Backs up ~/.claude/settings.json to ~/.claude/settings.json.token-max.bak\n  first.\n",
        );
        let confirm =
            at("\n  Write the 1 accepted change to ~/.claude/settings.json? [y]es, [n]o: ");
        let wrote = at(
            "\n  Wrote 1 edit to ~/.claude/settings.json. Backed up the old file to\n  ~/.claude/settings.json.token-max.bak.\n",
        );
        assert!(
            report < accepted && accepted < diff && diff < backup && backup < confirm,
            "{stdout}"
        );
        // Once the walk ends, doctor offers the advice it can't measure, from the same scan:
        // nothing is asked or scanned again.
        let offer = at(
            "\n  token-max vibes has 3 pieces of advice the transcripts can't price. Show them?\n  [y]es, [n]o: ",
        );
        let advice = at("\ntoken-max vibes: 3 pieces of advice the transcripts can't price.\n");
        let card = at("\n  [1/5] Bash timeout cap ");
        let window = at("\n  [3/5] Auto-compact window (claude-opus-5-5) ");
        assert_eq!(
            words(&stdout[advice..card]),
            "token-max vibes: 3 pieces of advice the transcripts can't price. None of it is measured, so none of it is a verdict. Each card names whose advice it is. Nothing is written until you confirm the list at the end."
        );
        assert!(
            confirm < wrote && wrote < offer && offer < advice && advice < card && card < window
        );
        assert_eq!(stdout.matches("How is ~/.claude paid for?").count(), 1);
        // The card has no verdict: the change, what was measured, whose advice it is and
        // the one edit a yes makes.
        let again = stdout[card..]
            .find("\n  ~/.claude/settings.json\n  @@")
            .map(|at| card + at)
            .unwrap_or_else(|| panic!("{stdout}"));
        // The memory card that follows advises nothing here, so it asks nothing.
        let memory = at("\n  [2/5] Auto memory ");
        assert!(card < memory && memory < window, "{stdout}");
        let entry = words(&stdout[card..memory]);
        assert!(
            entry.starts_with("[1/5] Bash timeout cap not measured 2h -> 4m What The longest Claude waits on a foreground Bash command before Claude Code moves it to the background. Advice A foreground wait at or past the 5m cache TTL, the current main-agent cache TTL setting, lets the cache expire before Claude replies, and a 4m cap leaves room under it. No foreground Bash wait outlasted the cache. What a lower cap saves is not measured, and the 4m margin is a guess."),
            "{entry}"
        );
        assert!(
            entry.ends_with(r#"Source The maintainer's pick of a margin under the cache TTL. Claude Code's docs: https://code.claude.com/docs/en/env-vars https://code.claude.com/docs/en/tools-reference https://code.claude.com/docs/en/prompt-caching Change "env": {"BASH_MAX_TIMEOUT_MS": "240000"} in ~/.claude/settings.json Accept? [y]es, [n]o, [q]uit:"#),
            "{entry}"
        );
        assert!(!entry.contains("Recommended") && !entry.contains("Worth considering"));
        // The window card names doctor's verdict beside its own cap, which is declined.
        // The card on the default advises nothing, so it asks nothing before the diff.
        let next = at("\n  [4/5] Main-session default ");
        let advisor = at("\n  [5/5] Advisor model ");
        assert!(
            window < next && next < advisor && advisor < again,
            "{stdout}"
        );
        let entry = words(&stdout[window..next]);
        assert!(
            entry.starts_with(
                "[3/5] Auto-compact window (claude-opus-5-5) not measured 967k -> 400k What "
            ),
            "{entry}"
        );
        assert!(
            entry.contains("doctor's auto-compact window line keeps it on cost.")
                && entry.ends_with(r#"Change "modelSettings": {"claude-opus-5-5": {"autoCompactWindow": 400000}} in ~/.claude/settings.json Accept? [y]es, [n]o, [q]uit:"#),
            "{entry}"
        );
        // The advisor card asks too, and its no writes nothing.
        assert!(
            words(&stdout[advisor..again]).ends_with(r#"Change "advisorModel": "fable" in ~/.claude/settings.json Accept? [y]es, [n]o, [q]uit:"#),
            "{stdout}"
        );
        // The card is from the scan before doctor wrote 1h, so its cap is for the 5m cache
        // set then; the next run moves it to 55m.
        // Its edit is checked against the file as doctor just wrote it, and confirmed alone.
        let tail = &stdout[again..];
        assert!(
            tail.contains("  @@ -3,3 +3,3 @@\n      \"env\": {\n  -     \"BASH_MAX_TIMEOUT_MS\": \"7200000\"\n  +     \"BASH_MAX_TIMEOUT_MS\": \"240000\"\n"),
            "{tail}"
        );
        assert!(
            tail.contains(
                "\n  Write the 1 accepted change to ~/.claude/settings.json? [y]es, [n]o: "
            ),
            "{tail}"
        );
        assert!(
            tail.contains("\n  Wrote 1 edit to ~/.claude/settings.json."),
            "{tail}"
        );
        let raised = SETTINGS.replace("\"5m\"", "\"1h\"");
        assert!(
            stdout.ends_with("~/.claude/settings.json.token-max.bak.\n"),
            "{stdout}"
        );
        // Each write backs up the file as it was just before it.
        assert_eq!(
            std::fs::read_to_string(w.claude.join("settings.json.token-max.bak")).unwrap(),
            raised
        );
        assert_eq!(
            std::fs::read_to_string(w.claude.join("settings.json")).unwrap(),
            raised.replace("7200000", "240000")
        );
        drop(keyboard);
    }

    /// The binary hard-linked into its own directory, so an upgrade replaces only the link.
    fn installed(w: &Workspace) -> PathBuf {
        let dir = w.home.join("bin");
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("token-max");
        std::fs::hard_link(BIN, &bin).unwrap();
        bin
    }

    /// Run `command` with stdin, stdout and stderr on terminals, typing `keys`.
    fn typed(command: &mut Command, keys: &[u8]) -> (i32, String, String) {
        let Pty { master, slave } = Pty::new(80);
        let (stdout, out) = Pty::new(80).drain();
        let (stderr, err) = Pty::new(80).drain();
        command
            .stdin(Stdio::from(slave))
            .stdout(stdout)
            .stderr(stderr);
        let mut child = command.spawn().unwrap();
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut keyboard = std::fs::File::from(master);
        keyboard.write_all(keys).unwrap();
        let status = child.wait().unwrap().code().unwrap();
        drop(keyboard);
        (status, out.join().unwrap(), err.join().unwrap())
    }

    /// Every name in `dir`.
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    // Only a platform with a release has one to offer.
    #[cfg(any(
        all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        target_os = "macos"
    ))]
    fn a_newer_release_is_offered_first_and_a_yes_replaces_the_binary_and_reruns() {
        let w = Workspace::new();
        w.fixture();
        let bin = installed(&w);
        let releases = support::Releases::new();
        releases.publish("v99.0.0", false);
        let base = prices_args(&w);
        let doctor = args(&base, &["--plain"]);
        let mut c = command_at(&bin, &w, &doctor);
        // The upgrade unpacks with the host's tar.
        c.env("TOKEN_MAX_RELEASES", &releases.url)
            .env("PATH", std::env::var_os("PATH").unwrap());
        let (status, stdout, stderr) = typed(&mut c, b"y\n");
        assert_eq!(status, 0, "{stderr}");
        let ours = env!("CARGO_PKG_VERSION");
        // Before the opening, then the release's own run with the same arguments. The answer
        // echoes on the input terminal, so a separate stdout shows none.
        assert_eq!(
            stdout,
            format!(
                "\n  token-max v99.0.0 is out; this is v{ours}, and its advice may be out of date.\n  Upgrade to v99.0.0 and rerun? [y]es, [n]o:   Downloading v99.0.0...\n  Upgraded to v99.0.0; rerunning.\nran {}\n",
                doctor.join(" ")
            )
        );
        assert_eq!(stderr, "");
        assert!(
            std::fs::read_to_string(&bin)
                .unwrap()
                .starts_with("#!/bin/sh\n")
        );
        // Executable as the binary it replaced was, and nothing left beside it.
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&bin).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode,
            std::fs::metadata(BIN).unwrap().permissions().mode() & 0o777
        );
        assert_eq!(names(bin.parent().unwrap()), ["token-max"]);
    }

    #[test]
    // Only a platform with a release has one to offer.
    #[cfg(any(
        all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        target_os = "macos"
    ))]
    fn a_declined_or_failed_upgrade_runs_this_build_and_leaves_it_in_place() {
        let w = Workspace::new();
        w.fixture();
        let bin = installed(&w);
        let releases = support::Releases::new();
        // The checksum file lies, so only a yes downloads anything and nothing is replaced.
        releases.publish("v99.0.0", true);
        let base = prices_args(&w);
        let doctor = args(&base, &["--plain"]);
        let ours = env!("CARGO_PKG_VERSION");
        let offer = format!(
            "\n  token-max v99.0.0 is out; this is v{ours}, and its advice may be out of date.\n  Upgrade to v99.0.0 and rerun? [y]es, [n]o: "
        );
        let (code, _, stderr) = typed(&mut command_at(&bin, &w, &doctor), b"s\nq\nn\n");
        assert_ne!(code, 2, "{stderr}");
        let opening = format!("\n  token-max {ours}\n");
        let words = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
        let failed = format!(
            "Downloading v99.0.0... Could not upgrade: token-max-{}.tar.xz does not match its checksum. Continuing with v{ours}.",
            support::TARGET
        );
        for (keys, said) in [
            // An unknown answer asks again; then no, skip how the profile pays, quit the walk and decline the advice.
            (
                &b"maybe\nn\ns\nq\nn\n"[..],
                "Upgrade to v99.0.0 and rerun? [y]es, [n]o:".to_string(),
            ),
            (&b"y\ns\nq\nn\n"[..], failed),
        ] {
            let before = releases.downloads();
            let mut c = command_at(&bin, &w, &doctor);
            c.env("TOKEN_MAX_RELEASES", &releases.url)
                .env("PATH", std::env::var_os("PATH").unwrap());
            let (status, stdout, stderr) = typed(&mut c, keys);
            // The exit code a run with no newer release gets.
            assert_eq!(status, code, "{stderr}");
            let (before_opening, _) = stdout.split_once(&opening).expect(&stdout);
            assert!(before_opening.starts_with(&offer), "{stdout}");
            assert_eq!(words(&before_opening[offer.len()..]), said, "{stdout}");
            assert!(stdout.contains("How is ~/.claude paid for?"), "{stdout}");
            assert_eq!(
                releases.downloads() - before,
                if keys[0] == b'y' { 2 } else { 0 }
            );
            assert_eq!(std::fs::read(&bin).unwrap(), std::fs::read(BIN).unwrap());
            assert_eq!(names(bin.parent().unwrap()), ["token-max"]);
        }
        // Reporting as a default user would, it changes nothing, so it only says how.
        let before = releases.downloads();
        let mut c = command_at(&bin, &w, &args(&base, &["--plain", "--defaults"]));
        c.env("TOKEN_MAX_RELEASES", &releases.url);
        let (status, stdout, stderr) = typed(&mut c, b"s\n");
        assert_ne!(status, 2, "{stderr}");
        let (before_opening, _) = stdout.split_once(&opening).expect(&stdout);
        assert_eq!(
            words(before_opening),
            format!(
                "token-max v99.0.0 is out; this is v{ours}, and its advice may be out of date. Run token-max doctor without --defaults to upgrade."
            )
        );
        assert_eq!(releases.downloads(), before);
        // A check that cannot run is one diagnostic line on stderr, and the run goes on.
        let mut c = command_at(&bin, &w, &doctor);
        let nowhere = "http://127.0.0.1:1/releases";
        c.env("TOKEN_MAX_RELEASES", nowhere);
        let (status, stdout, stderr) = typed(&mut c, b"s\nq\nn\n");
        assert_eq!(status, code, "{stderr}");
        assert!(stdout.starts_with(&opening), "{stdout}");
        let failed =
            format!("token-max: could not check for a newer release: could not reach {nowhere}: ");
        assert!(words(&stderr).contains(&words(&failed)), "{stderr}");
        // A directory this user cannot write keeps its binary, and gets the install link
        // instead of the question.
        // SAFETY: geteuid has no preconditions. Root can write anything.
        if unsafe { libc::geteuid() } != 0 {
            use std::os::unix::fs::PermissionsExt;
            let locked = w.home.join("locked");
            std::fs::create_dir_all(&locked).unwrap();
            let bin = locked.join("token-max");
            std::fs::hard_link(BIN, &bin).unwrap();
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
            let mut c = command_at(&bin, &w, &doctor);
            c.env("TOKEN_MAX_RELEASES", &releases.url);
            let (status, stdout, stderr) = typed(&mut c, b"s\nq\nn\n");
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(status, code, "{stderr}");
            let (before_opening, _) = stdout.split_once(&opening).expect(&stdout);
            // The line wraps inside the path, so compare without whitespace.
            let bare = |text: &str| text.split_whitespace().collect::<String>();
            let said = format!(
                "token-max v99.0.0 is out; this is v{ours}, and its advice may be out of date. This user cannot write to {}, so install it again as https://github.com/kixel-ai/token-max#install describes.",
                locked.canonicalize().unwrap().display()
            );
            assert_eq!(bare(before_opening), bare(&said));
            assert_eq!(releases.downloads(), before);
            assert_eq!(names(&locked), ["token-max"]);
        }
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
        let claude = native_path(&w.claude);
        let cache = native_path(w.home.join("prices.json"));
        for sig in [libc::SIGINT, libc::SIGTERM] {
            {
                let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
                let args = ["doctor", "--claude-dir", &claude, "--price-cache", &cache];
                let mut c = command(&w, &args);
                c.env(
                    "ALL_PROXY",
                    format!("http://{}", proxy.local_addr().unwrap()),
                )
                // The release check goes straight to its local server, so the proxy holds only the
                // price fetch.
                .env("NO_PROXY", "127.0.0.1")
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
                for leak in ["SYNTHETIC_PRIVATE", "recommended"] {
                    assert!(!stderr.contains(leak), "{leak}");
                }
                assert_eq!(stderr, "token-max: cancelled; no report produced\n");
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
            row["cwd"] = cwd.as_ref().map(native_path).into();
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
    let report = analysis(&w, |d| d.claude_dirs = vec![native_path(&w.claude)], 0.0);
    let feature = |directory: Option<&Path>, name: BackgroundFeature| {
        report
            .checks
            .iter()
            .find_map(|c| match c {
                CheckResult::Background(b)
                    if b.feature == name
                        && b.directories[0].as_deref() == directory.map(native_path).as_deref() =>
                {
                    Some(b)
                }
                _ => None,
            })
            .unwrap()
    };
    let disabled = feature(Some(&off), BackgroundFeature::PromptSuggestions);
    assert_eq!(disabled.base.current, Some(false));
    assert_eq!(disabled.base.action, Action::Keep);
    assert_eq!(disabled.base.requests, 2);
    assert_eq!(
        disabled.off_file.as_deref(),
        Some(&*native_path(off.join(".claude/settings.local.json")))
    );
    // Uses are not recorded, so a cost above the per-use limit is only advisory.
    let enabled = feature(Some(&on), BackgroundFeature::PromptSuggestions);
    assert_eq!(enabled.base.action, Action::Advisory);
    assert_eq!(enabled.base.recommended, None);
    assert_eq!(enabled.base.requests, 2);
    assert_eq!(enabled.generations, Some(1));
    let patch: Value = serde_json::from_str(&format!("{{{}}}", enabled.off)).unwrap();
    assert_eq!(
        patch["env"]["CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION"],
        "false"
    );
    let gone = w.home.join("gone");
    for directory in [None, Some(gone.as_path())] {
        let unknown = feature(directory, BackgroundFeature::PromptSuggestions);
        assert_eq!(unknown.base.current, None);
        assert_eq!(unknown.base.action, Action::Advisory);
        assert_eq!(unknown.cost.total_usd, None);
        assert_eq!(unknown.base.coverage, None);
    }
    assert!(!format!("{report:?}").contains("SYNTHETIC_PRIVATE"));
    let mut args = vec!["doctor".to_string()];
    args.extend(prices_args(&w));
    args.extend(["--max-cost-per-use".into(), "0".into(), "--details".into()]);
    let (_, stdout, stderr) = run(&mut command(
        &w,
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
    ));
    assert_eq!(stderr, "");
    assert!(!stdout.contains("SYNTHETIC_PRIVATE"));
    assert!(stdout.contains("scope or launch selection is unresolved"));
    assert!(stdout.contains("CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION"));
    assert!(stdout.contains("Background estimates apply current files to history"));
    let (mark, _, evidence) = report_line(&stdout, "Prompt suggestions");
    assert_eq!(mark, "consider");
    assert!(evidence.contains("its uses are not recorded"));
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
    std::fs::write(state.join("settings.json"),json!({"idleCompaction":"off","providerInstances":{"claudeAgent":{"driver":"claudeAgent","config":{"homePath":native_path(&w.claude),"autoCompactWindow":"300000"}},"other":{"driver":"unsupported","config":{"homePath":{"PRIVATE_CONFIG":"not-a-home"}}}}}).to_string()).unwrap();
    let state = native_path(&state);
    let prices = native_path(fixture("prices.json"));
    let invoke = |output| {
        run(&mut command(
            &w,
            &[
                "doctor",
                "--t3-dir",
                &state,
                "--prices-file",
                &prices,
                output,
            ],
        ))
    };
    let report = analysis(&w, |d| d.t3_dirs = vec![state.clone()], 1.0);
    assert_eq!(report.scan.requests.len(), 2);
    assert_eq!(report.scan.t3.len(), 1);
    let t3 = &report.scan.t3[0];
    assert!(t3.metadata.contains("unavailable"));
    assert!(t3.exclusions.iter().any(|e| e.contains("unsupported")));
    let selection = &t3.text_generation.as_ref().unwrap().selection;
    assert_eq!(
        (&*selection.instance, &*selection.model),
        ("codex", "gpt-6-luna")
    );
    assert!(report.checks.iter().any(|c| matches!(c,
        CheckResult::T3Idle(c) if c.current_settings.scenarios.len() == 3)));
    let (status, details, stderr) = invoke("--details");
    assert_ne!(status, 2, "{stderr}");
    assert!(stderr.is_empty());
    assert!(
        details.contains("t3-idle-compaction")
            && details
                .contains("T3 text generation: codex gpt-6-luna (T3 default). Cost unmeasured")
            && details.contains("unavailable")
    );
    let (_, summary, stderr) = run(&mut command(
        &w,
        &["doctor", "--t3-dir", &state, "--prices-file", &prices],
    ));
    assert!(stderr.is_empty());
    let (mark, head, _) = report_line(&summary, "T3 text generation");
    assert_eq!(
        (mark.as_str(), head.as_str()),
        ("unknown", "T3 text generation codex gpt-6-luna")
    );
    // Why it is unmeasured is evidence, which only details show.
    let (_, _, unmeasured) = report_line(&details, "T3 text generation");
    assert!(
        unmeasured.contains("ephemeral sessions that keep no native records (T3 default).")
            && unmeasured.contains(&format!(
                "Set in ~{0}selected-t3{0}settings.json:textGenerationModelSelection.",
                std::path::MAIN_SEPARATOR
            ))
    );
    for output in [format!("{report:?}"), details, summary] {
        assert!(!output.contains("SYNTHETIC_PRIVATE") && !output.contains("PRIVATE_CONFIG"));
    }
    assert!(!Path::new(&state).join("statev2.sqlite").exists());
}

#[test]
fn defaults_report_pretends_the_settings_files_away_and_writes_nothing() {
    let w = Workspace::new();
    // The user turned WebFetch off after its first request, Glob after its last, and
    // WebSearch before any; the project turned NotebookEdit off after its first.
    let settings =
        r#"{"promptCacheTtl":"1h","permissions":{"deny":["WebFetch","WebSearch","Glob"]}}"#;
    w.settings(settings);
    let repo = w.home.join("repo");
    std::fs::create_dir_all(repo.join(".claude")).unwrap();
    std::fs::write(
        repo.join(".claude/settings.local.json"),
        r#"{"permissions":{"deny":["NotebookEdit"]}}"#,
    )
    .unwrap();
    let tool = |name: &str| json!({"name": name, "description": "synthetic schema ".repeat(50), "input_schema": {"type": "object"}});
    let mut records = vec![];
    for i in 0..3 {
        let mut tools = vec![tool("Bash")];
        match i {
            0 => tools.extend([tool("WebFetch"), tool("NotebookEdit")]),
            2 => tools.push(tool("Glob")),
            _ => {}
        }
        let input = 1000 * tools.len();
        records.push(json!({"type": "api-request", "id": format!("api-{i}"), "cwd": native_path(&repo), "sessionId": format!("session-{i}"), "timestamp": format!("2026-11-0{}T00:00:00Z", i + 1), "params": {
            "model": "claude-opus-5-5", "system": "synthetic system", "messages": [], "tools": tools
        }}));
        records.push(json!({"type": "assistant", "uuid": format!("block-{i}"), "cwd": native_path(&repo), "sessionId": format!("session-{i}"), "timestamp": format!("2026-11-0{}T00:00:01Z", i + 1), "entrypoint": "sdk-ts", "requestRef": format!("api-{i}"), "message": {
            "id": format!("message-{i}"), "model": "claude-opus-5-5", "content": [{"type": "tool_use", "id": format!("call-{i}"), "name": "Bash", "input": {}}], "usage": {
                "input_tokens": input, "output_tokens": 100, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0,
                "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 0}, "service_tier": "standard", "speed": "standard", "inference_geo": "global"
            }
        }}));
    }
    let lines: Vec<String> = records.iter().map(Value::to_string).collect();
    std::fs::write(w.transcript(), lines.join("\n") + "\n").unwrap();
    let base = prices_args(&w);
    let report = |extra: &[&str]| {
        let mut c = command(&w, &args(&base, extra));
        // A setting variable in the shell is pretended away with the files.
        c.env("CLAUDE_CODE_PROMPT_CACHE_TTL", "1h");
        run(&mut c)
    };

    let (_, normal, stderr) = report(&["--details"]);
    assert!(
        normal.contains("Compared with defaults"),
        "{stderr}{normal}"
    );
    let (word, heading, _) = report_line(&normal, "WebFetch tool");
    assert_eq!(
        (word.as_str(), heading.as_str()),
        ("info", "WebFetch tool off")
    );

    let (status, stdout, stderr) = report(&["--details", "--defaults"]);
    assert_eq!(status, 1, "{stderr}{stdout}");
    assert!(stdout.contains("(--defaults): every user, project and local settings file"));
    assert!(!stdout.contains("Compared with defaults"), "{stdout}");
    assert!(
        stdout.contains("From the defaults, these changes save"),
        "{stdout}"
    );
    // Carried by one of three requests and never used: on, priced over all three.
    let (word, heading, evidence) = report_line(&stdout, "WebFetch tool");
    assert_eq!(word, "change", "{heading}");
    assert!(
        heading.starts_with("WebFetch tool on -> off saves"),
        "{heading}"
    );
    assert!(
        evidence.contains("so 1 of this line's 3 requests carried it"),
        "{evidence}"
    );
    // Turned off in a project file, or after the latest request: scaled the same way.
    // Glob, first sent mid-window, can't be proven unused.
    for (name, verdict) in [("NotebookEdit tool", "change"), ("Glob tool", "consider")] {
        let (word, _, evidence) = report_line(&stdout, name);
        assert_eq!(word, verdict, "{name}");
        assert!(
            evidence.contains("so 1 of this line's 3 requests carried it"),
            "{evidence}"
        );
    }
    // Never carried in the window: on, at a cost no transcript shows.
    let (word, heading, evidence) = report_line(&stdout, "WebSearch tool");
    assert_eq!(
        (word.as_str(), heading.as_str()),
        ("unknown", "WebSearch tool on costs unknown")
    );
    assert!(
        evidence.contains("turns it off, so no request"),
        "{evidence}"
    );
    // The file's and the shell's 1h are both pretended away.
    let (_, heading, _) = report_line(&stdout, "Main-agent prompt cache TTL");
    assert!(!heading.contains(" 1h"), "{heading}");
    assert_eq!(
        std::fs::read_to_string(w.claude.join("settings.json")).unwrap(),
        settings
    );

    // The cost while it was sent, scaled from its one request to all three.
    let fetch = |defaults: bool| {
        let report = analysis(
            &w,
            |d| {
                d.claude_dirs = vec![native_path(&w.claude)];
                if defaults {
                    d.pretend_defaults();
                }
            },
            1.0,
        );
        let c = report
            .context
            .iter()
            .find(|c| c.name == "WebFetch")
            .unwrap();
        (
            c.requests,
            c.current,
            c.pretended_requests,
            c.estimated_usd.unwrap(),
            c.verdict,
        )
    };
    let (requests, current, pretended, sent, verdict) = fetch(false);
    assert_eq!(
        (requests, current, pretended, verdict),
        (1, false, None, None)
    );
    let (requests, current, pretended, on, verdict) = fetch(true);
    assert_eq!(
        (requests, current, pretended, verdict),
        (1, true, Some(3), Some(Verdict::Off))
    );
    assert!((on - 3.0 * sent).abs() < 1e-12, "{on} {sent}");

    // Policy keeps a feature off, though the latest request carried it.
    std::fs::write(
        w.home.join("managed-settings.json"),
        r#"{"permissions":{"deny":["Glob"]}}"#,
    )
    .unwrap();
    let report = analysis(
        &w,
        |d| {
            d.claude_dirs = vec![native_path(&w.claude)];
            d.pretend_defaults();
        },
        1.0,
    );
    let glob = report.context.iter().find(|c| c.name == "Glob").unwrap();
    assert_eq!(
        (glob.current, glob.verdict, glob.pretended_requests),
        (false, None, None)
    );

    // The flag only shapes doctor's report.
    let (status, _, stderr) = run(&mut command(&w, &["sessions", "--defaults"]));
    assert_eq!(status, 2, "{stderr}");
}
