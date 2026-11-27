//! `--ssh` audits through a fake `ssh` that runs each destination's command locally.
#![cfg(unix)]

mod support;

use std::io::{BufRead, BufReader};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use token_max::DiscoveryOptions;
use token_max::Env;
use token_max::{
    Action, BackgroundCheck, BackgroundFeature, CheckResult, ContextKind, Harness, Report,
};
use token_max::{AnalyzeOptions, analyze};
use token_max::{PriceOptions, load_prices};
use token_max::{Reach, ScanOptions, scan};

const BIN: &str = env!("CARGO_BIN_EXE_token-max");

/// Destinations under one target scratch directory whose path has a space, removed on drop.
///
/// `ssh` runs the destination's command with `sh`, its home under `homes/` and its `PATH`
/// of `remote/<host>`, where a test can replace a command, then `tools/`. The coordinator
/// copies itself to each destination's cache on its first run there.
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
        // Linux ships sha256sum and macOS shasum; either checks a copy.
        let sums = ["sha256sum", "shasum"];
        for tool in [
            "sh", "uname", "sleep", "cat", "mkdir", "mktemp", "chmod", "mv", "rm",
        ]
        .into_iter()
        .chain(sums)
        {
            let Some(found) = std::env::split_paths(&path)
                .map(|dir| dir.join(tool))
                .find(|file| file.is_file())
            else {
                assert!(sums.contains(&tool), "{tool} is not on PATH");
                continue;
            };
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

    /// A destination with the synthetic transcript, where the coordinator copies itself.
    fn host(&self, name: &str) -> PathBuf {
        self.home(name)
    }

    /// The cache the coordinator copies itself into on `host`.
    fn cache(&self, host: &str) -> PathBuf {
        self.root
            .join("homes")
            .join(host)
            .join(".cache/token-max/ssh")
    }

    /// A destination whose copy of this build is this shell script instead.
    fn fake(&self, host: &str, body: &str) {
        let cache = self.cache(host);
        std::fs::create_dir_all(&cache).unwrap();
        script(&cache.join(copy_name()), body);
    }

    /// A destination whose `uname` names another platform than this one.
    fn elsewhere(&self, host: &str) {
        let (os, arch) = if cfg!(target_os = "linux") {
            ("Darwin", "arm64")
        } else {
            ("Linux", "x86_64")
        };
        self.replace(
            host,
            "uname",
            &format!(r#"case "$1" in -s) echo {os} ;; -m) echo {arch} ;; esac"#),
        );
    }

    fn home(&self, name: &str) -> PathBuf {
        let home = self.root.join("homes").join(name);
        let project = home.join(".claude/projects/my project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::copy(fixture("claude.jsonl"), project.join("main.jsonl")).unwrap();
        // An advisor already set, so its `vibes` card advises nothing and asks nothing.
        std::fs::write(
            home.join(".claude/settings.json"),
            r#"{"promptCacheTtl":"1h","advisorModel":"fable","cleanupPeriodDays":365}"#,
        )
        .unwrap();
        // The token-audit skill installed, so its `vibes` card is quiet too.
        let skill = home.join(".claude/skills/token-audit");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "synthetic\n").unwrap();
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
        support::isolated(&mut command)
            .args([
                name,
                "--prices-file",
                fixture("prices.json").to_str().unwrap(),
            ])
            .args(args)
            .env("HOME", self.root.join("homes/local"))
            .env(
                "PATH",
                format!("{}:{path}", self.root.join("bin").display()),
            )
            .env("TEST_ROOT", &self.root)
            .env("TEST_PATH", self.root.join("tools"))
            .env("TOKEN_MAX_RELEASES", support::current())
            .stdin(Stdio::null());
        command
    }

    fn run(&self, args: &[&str]) -> (i32, String, String) {
        run(&mut self.command(args))
    }

    /// The first line `doctor --ssh-scan` prints: the header naming this build.
    fn header(&self) -> String {
        // Held open: a destination stops when its stdin closes.
        let mut child = support::isolated(&mut Command::new(BIN))
            .args(["doctor", "--ssh-scan"])
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

/// The name the coordinator caches its copy under on each destination.
fn copy_name() -> String {
    let digest = Sha256::digest(std::fs::read(BIN).unwrap());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("token-max-{}", &hex[..16])
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
        opencode_dirs: vec![],
        antigravity_dirs: vec![],
        unreplayed: true,
        t3_dirs: vec![],
        managed_settings_file: format!("{home}/managed-settings.json"),
        codex_system_config_file: format!("{home}/system.toml"),
        codex_requirements_file: format!("{home}/requirements.toml"),
        defaults: false,
        billing: Default::default(),
    };
    analyze(AnalyzeOptions {
        scan: scan(&ScanOptions {
            discovery,
            since,
            reach: Reach::History,
            reading: None,
        })
        .unwrap(),
        catalog: &catalog,
        since,
        now: NOW,
        max_cost_per_use_usd,
        reading: None,
    })
    .unwrap()
}

/// An instant after every fixture request, so analyses don't depend on the wall clock.
const NOW: i64 = 1_796_083_200_000;
/// 2026-11-01T00:00:00Z.
const NOVEMBER: i64 = 1_793_491_200_000;
const MINUTE: i64 = 60_000;

/// One host's part of a report covering several, under `--details`: its title, notes and
/// evidence, from the block its title heads after the shared rows.
fn section<'a>(text: &'a str, host: &str) -> &'a str {
    let heading = format!("\ntoken-max: {host}  ");
    let start = text.find(&heading).unwrap_or_else(|| panic!("{text}")) + heading.len();
    let rest = &text[start..];
    let end = rest.find("\ntoken-max: ").unwrap_or(rest.len());
    &rest[..end]
}

/// What a report says of one host, to compare a report covering several with one run there:
/// that host's rows, by heading, without the tag naming it or the percents, which are of
/// every host's spend; and its evidence. `host` is None for a report of that host alone.
fn view(text: &str, host: Option<&str>) -> String {
    let rows_end = text.find("\n  EVIDENCE\n").unwrap_or(text.len());
    // Each row, and whether lines it sums follow it, which `--details` lists in its place.
    let mut rows: Vec<(String, bool)> = vec![];
    let mut heading = "";
    let mut keep = false;
    // The last top-level row kept, which a member line under it replaces.
    let mut parent: Option<usize> = None;
    for line in text[..rows_end].lines() {
        if line.starts_with("token-max") {
            continue;
        }
        let Some(body) = line.strip_prefix("  ") else {
            continue;
        };
        // A row's members sit under it, two columns in.
        let member = body
            .strip_prefix("  ")
            .filter(|b| b.starts_with(|c: char| c.is_ascii_lowercase()));
        let body = member.unwrap_or(body);
        if body.starts_with(|c: char| c.is_ascii_uppercase())
            && body
                .split("  ")
                .next()
                .unwrap()
                .chars()
                .all(|c| c.is_ascii_uppercase() || c == ' ')
        {
            heading = body.split("  ").next().unwrap();
            continue;
        }
        // A row's detail lines sit under its name.
        if body.starts_with("         ") {
            if keep {
                let words: Vec<&str> = body.split_whitespace().collect();
                rows.last_mut()
                    .unwrap()
                    .0
                    .push_str(&format!("\n  {}", words.join(" ")));
            }
            continue;
        }
        if !body.starts_with(|c: char| c.is_ascii_lowercase()) || body.starts_with("token-max") {
            keep = false;
            continue;
        }
        if let (Some(_), Some(at)) = (member, parent) {
            rows[at].1 = true;
        }
        let mut words: Vec<String> = vec![];
        keep = host.is_none();
        for word in body.split_whitespace() {
            if word.ends_with('%')
                && word[..word.len() - 1]
                    .chars()
                    .all(|c| c.is_ascii_digit() || c == '<')
            {
                continue;
            }
            match host {
                Some(host) if word == format!("[{host}]") => keep = true,
                Some(host) if word.starts_with(&format!("[{host}:")) => {
                    keep = true;
                    words.push(format!("[{}", &word[host.len() + 2..]));
                }
                _ => words.push(word.to_string()),
            }
        }
        if member.is_none() {
            parent = keep.then_some(rows.len());
        }
        if keep {
            rows.push((format!("{heading}: {}", words.join(" ")), false));
        }
    }
    let mut rows: Vec<String> = rows
        .into_iter()
        .filter(|(_, summed)| !summed)
        .map(|(row, _)| row)
        .collect();
    rows.sort();
    let block = match host {
        Some(host) => section(text, host),
        None => text,
    };
    let evidence = block.find("\n  EVIDENCE\n").map_or("", |at| &block[at..]);
    // What the walk leaves to write comes after every host's evidence.
    let evidence = [
        "\n  EDITS  ",
        "\n  STEPS BY HAND",
        "\n  NOT WRITTEN",
        "\n  token-max vibes has ",
    ]
    .iter()
    .filter_map(|end| evidence.find(end))
    .min()
    .map_or(evidence, |at| &evidence[..at]);
    timeless(&format!("{}\n{}", rows.join("\n"), evidence.trim_end()))
}

/// A report without what depends on each run's own clock: instants, dates, day counts, and
/// dollar amounts, since a per-30-day rate divides by the window that ends at that clock.
fn timeless(text: &str) -> String {
    let text = text.strip_prefix("token-max  ").unwrap_or(text);
    // The closing hint counts every host's advice, so it is no part of one host's report.
    let text = text
        .find("\n  token-max vibes has ")
        .map_or(text, |at| &text[..at]);
    // A section names its own host as the base of its percents; run there, it is "this host".
    let named;
    // The name is the word before the first "'s", as in "of devbox's spend" or, when part
    // of the spend is unpriced, "of the $5 devbox's priced requests".
    let text = match text.find("each percent is of ").and_then(|at| {
        let from = at + "each percent is of ".len();
        let end = from + text[from..].find("'s ")?;
        let start = text[..end].rfind(' ').map_or(0, |space| space + 1);
        (!text[..end].ends_with("this host")).then_some(&text[start..end])
    }) {
        Some(name) => {
            named = text.replace(&format!(" {name}'s "), " this host's ");
            named.as_str()
        }
        None => text,
    };
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
    let (status, stdout, stderr) = hosts.run(&["--ssh", "devbox", "--details", since[0], since[1]]);
    assert_eq!(status, 0, "{stderr}");
    // Login noise and transcript text stay out of the output.
    assert_eq!(stderr, "");
    assert!(!stdout.contains("SYNTHETIC_PRIVATE"));
    // Each host's evidence follows the rows every host shares.
    for host in ["this host", "devbox"] {
        assert!(section(&stdout, host).contains("; 2 requests;"), "{host}");
    }
    // Every percent is of both hosts' spend together.
    assert!(
        flat(&stdout).contains("each percent is of these hosts' spend"),
        "{stdout}"
    );
    let (_, there, _) = run(hosts
        .command(&["--details", since[0], since[1]])
        .env("HOME", &home));
    assert_eq!(view(&stdout, Some("devbox")), view(&there, None));

    let args = std::fs::read_to_string(hosts.root.join("ssh.args")).unwrap();
    for option in [
        "-T",
        "BatchMode=yes",
        "ControlPersist=no",
        "-- devbox sh -c",
    ] {
        assert!(args.contains(option), "{option}: {args}");
    }

    // With a change to offer on devbox, which exits 1.
    std::fs::write(hosts.settings("devbox"), r#"{"promptCacheTtl":"5m"}"#).unwrap();
    let (status, text, _) = hosts.run(&["--ssh", "devbox"]);
    assert_eq!(status, 1);
    // One total heads the report, beside each host's own section.
    assert!(
        // Each host recorded 1,020 and 1,120 tokens, cache included.
        text.starts_with(
            "token-max: total over 2 hosts  4 requests and 4,280 tokens in 2 sessions, "
        ),
        "{text}"
    );
    // One credit and one list of changes cover both hosts, each row naming its host.
    let flat_text = flat(&text);
    assert!(
        flat_text.contains("Together, these changes save <$0.01/mo, 7% of these hosts' spend."),
        "{text}"
    );
    let (_, credit) = flat_text.split_once("Compared with defaults, ").unwrap();
    assert!(credit.contains("of these hosts' spend."), "{text}");
    let change = text
        .lines()
        .find(|line| line.starts_with("  change "))
        .unwrap();
    assert!(change.contains("Main-agent prompt cache TTL"), "{text}");
    assert!(change.contains(" [devbox] "), "{text}");
    assert!(!text.contains("\ntoken-max: devbox  "), "{text}");
    // The edit to make names the file on its host.
    assert!(
        text.contains("\n  EDITS  devbox:~/.claude/settings.json\n"),
        "{text}"
    );
    assert!(!text.contains("don't add amounts across hosts"), "{text}");
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
    // The unpriced session is extrapolated over from the priced ones, with a margin.
    assert!(check.base.savings_usd.is_some());
    assert!(check.margin_usd.is_some());
    assert!(check.base.margin.is_some());
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
    let remote = view(&stdout, Some("devbox"));
    assert!(remote.contains("keepalive"), "{remote}");
    let (_, there, stderr) = run(hosts.command(&args).env("HOME", &home));
    assert!(stderr.is_empty(), "{stderr}");
    assert_eq!(remote, view(&there, None));
}

#[test]
fn bash_caps_and_waits_keep_their_host_ownership() {
    let hosts = Hosts::new();
    let home = hosts.host("devbox");
    std::fs::write(
        home.join(".claude/settings.json"),
        r#"{"promptCacheTtl":"1h","advisorModel":"fable","cleanupPeriodDays":365,"env":{"BASH_MAX_TIMEOUT_MS":"7200000"}}"#,
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

    let cap = |home: &Path| analysis(home, None, 1.0).bash.remove(0);
    let local = cap(&hosts.root.join("homes/local"));
    let remote = cap(&home);
    assert_eq!(local.current, 600_000);
    assert_eq!(local.waits.calls, 0);
    assert_eq!(remote.current, 7_200_000);
    assert_eq!(remote.suggested, Some(3_300_000));
    let w = &remote.waits;
    assert_eq!((w.calls, w.long, w.timed_out, w.polling), (1, 1, 1, 1));
    assert!(w.miss_estimate.unwrap().usd > 0.0);

    // `doctor` leaves the cap to `vibes`, which it names once for every host.
    let (_, stdout, stderr) = hosts.run(&["--ssh", "devbox", "--details"]);
    assert_eq!(stderr, "");
    assert!(!stdout.contains("BASH_MAX_TIMEOUT_MS"), "{stdout}");
    assert!(
        stdout.ends_with(
            "\n  token-max vibes has 4 pieces of advice the transcripts can't price. Run token-max vibes with the same profile, host, --since and price options to see them.\n"
        ),
        "{stdout}"
    );
    let advice = |args: &[&str]| run(&mut hosts.invoke("vibes", args));
    let (status, stdout, stderr) = advice(&["--ssh", "devbox"]);
    assert_eq!((status, stderr.as_str()), (0, ""), "{stdout}");
    assert!(!stdout.contains("SYNTHETIC_PRIVATE"));
    let cards: Vec<&str> = stdout.split("\n\n").collect();
    assert!(
        cards[0].starts_with("token-max vibes: 4 pieces of advice the transcripts can't price.\n"),
        "{stdout}"
    );
    // Each host's card names its host, its own waits and the edit to make there, in its home.
    let here = flat(cards[1]);
    assert!(here.starts_with("[1/12] Bash timeout cap not measured 10m -> 55m Host this host"));
    assert!(
        here.contains("No foreground Bash wait outlasted the cache."),
        "{here}"
    );
    assert!(
        here.ends_with(
            r#"Change "env": {"BASH_MAX_TIMEOUT_MS": "3300000"} in ~/.claude/settings.json"#
        ),
        "{here}"
    );
    // Each host's memory card follows its cap, then its window card, its edit in that
    // host's home.
    assert!(flat(cards[2]).starts_with("[2/12] Auto memory not measured Host this host"));
    assert!(flat(cards[8]).starts_with("[8/12] Auto memory not measured Host devbox"));
    let window = flat(cards[3]);
    assert!(
        window.starts_with(
            "[3/12] Auto-compact window (claude-opus-5-5) not measured 967k -> 400k Host this host"
        ) && window.ends_with(r#"in ~/.claude/settings.json"#),
        "{window}"
    );
    let window = flat(cards[9]);
    assert!(
        window.starts_with(
            "[9/12] Auto-compact window (claude-opus-5-5) not measured 967k -> 400k Host devbox"
        ) && window.ends_with(r#"in devbox:~/.claude/settings.json"#),
        "{window}"
    );
    // Each host's default has its own card, which advises nothing on the pick.
    assert!(flat(cards[4]).starts_with("[4/12] Main-session default not measured Host this host"));
    assert!(flat(cards[10]).starts_with("[10/12] Main-session default not measured Host devbox"));
    // Each host's advisor card, already set, advises nothing.
    assert!(
        flat(cards[5]).starts_with("[5/12] Advisor model not measured Host this host What"),
        "{stdout}"
    );
    assert!(
        flat(cards[11]).starts_with("[11/12] Advisor model not measured Host devbox What"),
        "{stdout}"
    );
    let there = flat(cards[7]);
    assert!(there.starts_with("[7/12] Bash timeout cap not measured 2h -> 55m Host devbox"));
    assert!(
        there.contains(
            "1 foreground Bash wait outlasted the cache (1 hit the timeout, 1 polled in a loop)"
        ),
        "{there}"
    );
    assert!(
        there.ends_with(
            r#"Change "env": {"BASH_MAX_TIMEOUT_MS": "3300000"} in devbox:~/.claude/settings.json"#
        ),
        "{there}"
    );
    // The destination's card is the one it prints for itself, but for its host's name.
    let (_, alone, stderr) = run(hosts.invoke("vibes", &[]).env("HOME", &home));
    assert_eq!(stderr, "");
    let alone = flat(alone.split("\n\n").nth(1).unwrap());
    assert_eq!(
        there
            .replace("[7/12]", "[1/6]")
            .replace(" Host devbox", "")
            .replace("devbox:~", "~"),
        alone
    );
    // Each host looks for the token-audit skill in its own homes: installed here, the card
    // is quiet; missing on devbox, it names the step there, and doctor counts it.
    assert!(
        flat(cards[6]).starts_with("[6/12] Token audit skill not measured Host this host What")
            && !cards[6].contains("Change"),
        "{stdout}"
    );
    std::fs::remove_dir_all(home.join(".claude/skills")).unwrap();
    let (status, stdout, stderr) = advice(&["--ssh", "devbox"]);
    assert_eq!((status, stderr.as_str()), (0, ""), "{stdout}");
    let cards: Vec<&str> = stdout.split("\n\n").collect();
    assert!(flat(cards[6]).contains("Installed for every Claude Code and Codex profile here"));
    let there = flat(cards[12]);
    assert!(
        there.starts_with("[12/12] Token audit skill not measured Host devbox")
            && there.contains("Not installed for Claude Code in ~/.claude.")
            && there.ends_with("Change by hand, since doctor and vibes write only settings files, not skills: run `token-max skill install`, or `npx skills add kixel-ai/token-max --skill token-audit -g`, then start a new session. Where token-max isn't on the PATH, such as on a host reached with --ssh, run it as `npx @kixel/crabx token-max`."),
        "{there}"
    );
    let (_, stdout, _) = hosts.run(&["--ssh", "devbox"]);
    assert!(
        stdout.contains("token-max vibes has 5 pieces of advice"),
        "{stdout}"
    );
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
    // Its session, task, turn and settings, then its first usage record.
    let usage = &fixture[4];
    let mut records = vec![
        fixture[0].clone(),
        fixture[1].clone(),
        fixture[2].clone(),
        fixture[3].clone(),
        serde_json::json!({
            "type": "world_state", "timestamp": "2026-11-01T00:00:00Z",
            "payload": {"full": true, "state": {"tools": {"mcp__docs": "SYNTHETIC_SHIPPED_SCHEMA"}}}
        }),
        serde_json::json!({
            "type": "response_item", "timestamp": "2026-11-01T00:00:00Z",
            "payload": {"type": "function_call", "name": "mcp__docs__read", "call_id": "read-1", "arguments": "{}"}
        }),
        serde_json::json!({
            "type": "response_item", "timestamp": "2026-11-01T00:00:00Z",
            "payload": {"type": "function_call_output", "call_id": "read-1", "output": "SYNTHETIC_PRIVATE_OUTPUT"}
        }),
        usage.clone(),
    ];
    let mut next = usage.clone();
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

        let (status, stdout, stderr) = hosts.run(&["--ssh", "devbox", "--details"]);
        assert_eq!(status, 0, "{stderr}");
        assert_eq!(stderr, "");
        assert!(!stdout.contains("SYNTHETIC_PRIVATE"));
        // What the remote's server says it does comes back with its report.
        assert!(
            view(&stdout, Some("devbox")).contains("What: SYNTHETIC_SHIPPED_SCHEMA"),
            "{stdout}"
        );
        let (_, there, stderr) = run(hosts.command(&["--details"]).env("HOME", &home));
        assert_eq!(stderr, "");
        assert_eq!(view(&stdout, Some("devbox")), view(&there, None));
        let (_, alone, stderr) = hosts.run(&["--ssh", "devbox", "--details"]);
        assert_eq!(stderr, "");
        assert_eq!(view(&alone, Some("devbox")), view(&there, None));

        // The tier assumptions and the output cap are evidence, which only details show.
        let (_, text, stderr) = hosts.run(&["--ssh", "devbox", "--details"]);
        assert_eq!(stderr, "");
        assert!(!text.contains("SYNTHETIC_PRIVATE"));
        assert!(text.contains("Served-tier scenario:"), "{text}");
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
        claude.background_directories[home.join("workspace").to_str().unwrap()][2].value,
        Some(true)
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
    let (status, stdout, stderr) = hosts.run(&[&["--ssh", "devbox"][..], &args].concat());
    assert_eq!(status, 1, "{stderr}");
    assert_eq!(stderr, "");
    assert!(!stdout.contains("SYNTHETIC_PRIVATE"));
    let (status, there, stderr) = run(hosts.command(&args).env("HOME", &home));
    assert_eq!(status, 1, "{stderr}");
    assert_eq!(stderr, "");
    // The same report, but for its closing edits naming their files after the host, as `scp`
    // would, and its steps by hand naming the host.
    assert_eq!(view(&stdout, Some("devbox")), view(&there, None));
    // What is left to write on one host, from the first of `starts`.
    let closings = |text: &str, starts: &[&str]| {
        let start = starts
            .iter()
            .filter_map(|at| text.find(at))
            .min()
            .unwrap_or_else(|| panic!("{text}"));
        let end = text.find("\n  token-max vibes has ").unwrap_or(text.len());
        text[start..end].trim_end().to_string()
    };
    let devbox = closings(
        &stdout,
        &["\n  EDITS  devbox:", "\n  STEPS BY HAND  devbox\n"],
    );
    assert!(devbox.contains("\n  EDITS  devbox:~"), "{devbox}");
    assert!(devbox.contains("\n  STEPS BY HAND  devbox\n"), "{devbox}");
    let devbox = devbox
        .replace("\n  EDITS  devbox:~", "\n  EDITS  ~")
        .replace("\n  STEPS BY HAND  devbox\n", "\n  STEPS BY HAND\n");
    let there = closings(&there, &["\n  EDITS  ", "\n  STEPS BY HAND\n"]);
    assert_eq!(timeless(&devbox), timeless(&there));
    // Why a cost is unknown is evidence, which only details show.
    let (_, text, stderr) = hosts.run(&["--ssh", "devbox", "--details"]);
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
fn the_coordinator_copies_itself_once_and_replaces_older_copies() {
    let hosts = Hosts::new();
    hosts.host("devbox");
    // An older build's copy, which this one replaces.
    let cache = hosts.cache("devbox");
    std::fs::create_dir_all(&cache).unwrap();
    script(&cache.join("token-max-0000000000000000"), "exit 2");
    // An installed development build may be any commit, so it is not run.
    hosts.replace("devbox", "token-max", "exit 2");
    let copies = || {
        std::fs::read_to_string(hosts.root.join("ssh.args"))
            .unwrap()
            .matches(".copy.XXXXXX")
            .count()
    };
    for _ in 0..2 {
        let (status, stdout, stderr) = hosts.run(&["--ssh", "devbox"]);
        assert_eq!(status, 0, "{stderr}");
        assert!(
            stdout.starts_with("token-max: total over 2 hosts  "),
            "{stdout}"
        );
        assert_eq!(copies(), 1);
    }
    let cached: Vec<_> = std::fs::read_dir(&cache)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(cached, [copy_name()]);
    assert_eq!(
        std::fs::read(cache.join(copy_name())).unwrap(),
        std::fs::read(BIN).unwrap()
    );
}

#[test]
fn another_platform_runs_its_installed_build() {
    let hosts = Hosts::new();
    let home = hosts.home("installed");
    hosts.elsewhere("installed");
    let bin = home.join(".local/bin");
    std::fs::create_dir_all(&bin).unwrap();
    symlink(BIN, bin.join("token-max")).unwrap();
    // An older `cargo install` loses to the installers' copy.
    let cargo = home.join(".cargo/bin");
    std::fs::create_dir_all(&cargo).unwrap();
    script(&cargo.join("token-max"), "exit 2");
    let (status, stdout, stderr) = hosts.run(&["--ssh", "installed"]);
    assert_eq!(status, 0, "{stderr}");
    assert!(
        stdout.starts_with("token-max: total over 2 hosts  "),
        "{stdout}"
    );
    assert!(!hosts.cache("installed").exists());
}

#[test]
fn any_failed_destination_fails_the_whole_report() {
    let hosts = Hosts::new();
    hosts.host("devbox");
    hosts.home("bare");
    hosts.elsewhere("bare");
    hosts.host("windows");
    hosts.replace("windows", "uname", "echo Windows_NT");
    hosts.host("static");
    hosts.replace("static", "chmod", "exit 1");
    hosts.host("damaged");
    for sum in ["sha256sum", "shasum"] {
        hosts.replace("damaged", sum, "echo 00 -");
    }
    hosts.fake("old", "echo 'invalid arguments' >&2; exit 2");
    let header = hosts.header();
    assert!(header.starts_with("{\"tokenMaxScan\":"), "{header}");
    let other = header.replacen("\"tokenMaxScan\":\"", "\"tokenMaxScan\":\"9.9.9+", 1);
    hosts.fake("other", &format!("echo '{other}'; exec sleep 600"));
    hosts.fake(
        "corrupt",
        &format!("echo '{header}'; echo '{{\"scan\":{{\"since\":'"),
    );
    hosts.fake("silent", &format!("echo '{header}'; exit 0"));
    let broken = hosts.host("broken");
    std::fs::write(
        broken.join(".claude/projects/my project/main.jsonl"),
        r#"{"timestamp":"2026-11-02T00:00:00Z","type":"assistant","private":"SYNTHETIC_PRIVATE_REPLY","message":{"usage":{"input_tokens":"bad"}}}
"#,
    )
    .unwrap();
    let version = env!("CARGO_PKG_VERSION");
    let target = if cfg!(target_os = "linux") {
        "aarch64-apple-darwin"
    } else {
        "x86_64-unknown-linux-musl"
    };
    let bare = format!(
        "bare: needs curl and tar to download the token-max {version} release for {target}\n"
    );
    let other = format!(
        "other: the token-max {version} there, {}, is build 9.9.9+",
        hosts.cache("other").join(copy_name()).display()
    );
    for (host, expected) in [
        (
            "unreachable",
            "unreachable: SSH connection failed; check that `ssh unreachable true` works",
        ),
        ("bare", bare.as_str()),
        ("windows", "windows: unsupported platform"),
        (
            "static",
            "static: a copy of this token-max does not run there",
        ),
        (
            "damaged",
            "damaged: the copy of this token-max arrived damaged\n",
        ),
        (
            "old",
            "old: token-max there did not start an SSH audit (exit 2)\n",
        ),
        ("other", other.as_str()),
        ("corrupt", "corrupt: invalid SSH audit stream at line "),
        ("silent", "silent: invalid SSH audit stream at line 2"),
        ("broken", "broken: "),
    ] {
        let (status, stdout, stderr) = hosts.run(&["--ssh", "devbox", "--ssh", host]);
        assert_eq!(status, 2, "{host}: {stderr}");
        assert_eq!(stdout, "", "{host}");
        assert!(
            stderr.starts_with(&format!("token-max: {expected}")),
            "{host}: {stderr}"
        );
        assert!(!stderr.contains("SYNTHETIC_PRIVATE") && !stderr.contains("noise"));
        // The binary that ran is the one to replace.
        if host == "other" {
            assert!(stderr.contains("; run a released token-max here, or delete it there and build this one with `cargo install"), "{stderr}");
        }
    }
    // A failed copy leaves nothing behind.
    for host in ["static", "damaged"] {
        let cache = hosts.cache(host);
        assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 0, "{host}");
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
fn a_destination_that_strays_from_the_protocol_fails_the_report() {
    let hosts = Hosts::new();
    // Destinations have no sed, so the shell replaces the report line with `action`'s.
    let report = |action: &str| {
        format!(
            r#" | while IFS= read -r line; do case "$line" in '{{"report":'*) {action} ;; *) printf '%s\n' "$line" ;; esac; done"#
        )
    };
    // Each runs this build there and then changes what it sends.
    for (host, tamper, expected) in [
        (
            "window",
            report(r#"printf '{"report":{"since":1,%s\n' "${line#*\"since\":*,}""#),
            "window: token-max scanned a different window than requested",
        ),
        (
            "kind",
            report(r#"echo '{"wrote":{"written":[]}}'"#),
            "kind: token-max sent a different result than requested",
        ),
        (
            "trailing",
            "; echo '{}'".to_string(),
            "trailing: token-max sent more than 1 byte after its result",
        ),
        (
            "failing",
            "; exit 3".to_string(),
            "failing: token-max failed after sending its result (exit 3)",
        ),
    ] {
        hosts.host(host);
        hosts.fake(host, &format!("'{BIN}' \"$@\"{tamper}"));
        let (status, stdout, stderr) = hosts.run(&["--ssh", host]);
        assert_eq!(status, 2, "{host}: {stderr}");
        assert_eq!(stdout, "", "{host}");
        assert!(
            stderr.starts_with(&format!("token-max: {expected}")),
            "{host}: {stderr}"
        );
    }
}

#[test]
fn a_destination_that_sends_more_than_a_limit_fails_the_report() {
    let hosts = Hosts::new();
    hosts.host("devbox");
    // A MiB of whitespace, which a value may hold anywhere and costs nothing to parse.
    let pad = hosts.root.join("pad");
    std::fs::write(&pad, vec![b' '; 1 << 20]).unwrap();
    let pads = |mib: u32| {
        format!(
            "i=0; while [ $i -lt {mib} ]; do cat '{}'; i=$((i + 1)); done",
            pad.display()
        )
    };
    let header = hosts.header();
    let profiles = r#"{"profiles":{"home":null,"roots":[]}}"#;
    for (command, host, body, expected) in [
        (
            "doctor",
            "header",
            r#"printf '{"tokenMaxScan":"%01025d' 0; exec sleep 600"#.to_string(),
            "header: token-max sent more than 1 KiB for its header, the most this host reads",
        ),
        (
            "doctor",
            "profiles",
            format!(
                "echo '{header}'; printf '{{\"profiles\":'; {}; exec sleep 600",
                pads(1)
            ),
            "profiles: token-max sent more than 1 MiB for its profiles",
        ),
        (
            "doctor",
            "report",
            format!(
                "echo '{header}'; echo '{profiles}'; printf '{{\"report\":'; {}; exec sleep 600",
                pads(16)
            ),
            "report: token-max sent more than 16 MiB for its report",
        ),
        (
            "doctor",
            "trailing",
            format!("'{BIN}' \"$@\"; echo"),
            "trailing: token-max sent more than 1 byte after its result",
        ),
    ] {
        hosts.host(host);
        hosts.fake(host, &body);
        let (status, stdout, stderr) =
            run(&mut hosts.invoke(command, &["--ssh", "devbox", "--ssh", host]));
        assert_eq!(status, 2, "{host}: {stderr}");
        assert_eq!(stdout, "", "{host}");
        assert!(
            stderr.starts_with(&format!("token-max: {expected}")),
            "{host}: {stderr}"
        );
    }
    // Sessions, which grow with history, may pass a report's limit.
    hosts.host("history");
    hosts.fake(
        "history",
        &format!(
            r#"echo '{header}'; printf '{{"sessions":'; {}; echo '{{"since":null,"sessions":[]}}}}'"#,
            pads(17)
        ),
    );
    let (status, stdout, stderr) =
        run(&mut hosts.invoke("sessions", &["--ssh", "devbox", "--ssh", "history"]));
    assert_eq!(status, 0, "{stderr}");
    assert!(!lines(&stdout).is_empty());
}

#[test]
fn cancelling_stops_and_reaps_every_destination() {
    let hosts = Hosts::new();
    let ready = hosts.root.join("ready");
    let fifo = std::ffi::CString::new(ready.to_str().unwrap()).unwrap();
    // SAFETY: mkfifo creates a named pipe at a path this test owns.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let header = hosts.header();
    hosts.fake(
        "slow",
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
    hosts.fake(
        "slow",
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
    let (status, stdout, stderr) = run_sessions(&["--ssh", "devbox"]);
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
    let (_, windowed, stderr) = run_sessions(&["--ssh", "devbox", "--since", "2026-11-01"]);
    assert_eq!(stderr, "");
    // The window drops devbox's early session; this host's session-a starts inside it.
    let windowed: Vec<_> = lines(&windowed)
        .into_iter()
        .map(|l| (l["host"].clone(), l["session"].clone()))
        .collect();
    assert_eq!(
        windowed,
        [
            (Value::Null, Value::from("session-a")),
            (Value::from("devbox"), Value::from("session-a"))
        ]
    );

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
            &["--ssh", "devbox", "--ssh", "unreachable"][..],
            "unreachable: SSH connection failed",
        ),
        (&["--ssh", "devbox", "--ssh", "broken"], "broken: "),
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

/// A coordinator run on a pseudo-terminal, as a person runs the walk.
struct Terminal {
    master: std::fs::File,
    output: std::sync::Arc<(std::sync::Mutex<Vec<u8>>, std::sync::Condvar)>,
    child: std::process::Child,
    /// How much of the output earlier prompts consumed.
    seen: usize,
}

impl Terminal {
    fn spawn(mut command: Command) -> Self {
        use std::os::fd::FromRawFd;
        let (mut master, mut slave) = (0, 0);
        // SAFETY: openpty writes two new descriptors this test then owns.
        let opened = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(opened, 0);
        // SAFETY: each descriptor is owned once, by the File made from it.
        let (master, slave) = unsafe {
            (
                std::fs::File::from_raw_fd(master),
                std::fs::File::from_raw_fd(slave),
            )
        };
        let child = command
            .stdin(slave.try_clone().unwrap())
            .stdout(slave)
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let output =
            std::sync::Arc::new((std::sync::Mutex::new(vec![]), std::sync::Condvar::new()));
        let mut reader = master.try_clone().unwrap();
        let shared = output.clone();
        std::thread::spawn(move || {
            let mut buffer = [0; 4096];
            // Ends with EIO once the child closes its side.
            while let Ok(n @ 1..) = std::io::Read::read(&mut reader, &mut buffer) {
                shared.0.lock().unwrap().extend_from_slice(&buffer[..n]);
                shared.1.notify_all();
            }
        });
        Self {
            master,
            output,
            child,
            seen: 0,
        }
    }

    /// Wait for `text` after the last text waited for.
    fn wait(&mut self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(60);
        let (lock, ready) = &*self.output;
        let mut output = lock.lock().unwrap();
        loop {
            // Whole, with the terminal's line breaks folded, so `seen` indexes the same text
            // each time; only an incomplete tail can differ, and a match ends before it.
            let printed = String::from_utf8_lossy(&output).replace("\r\n", "\n");
            if let Some(at) = printed[self.seen..].find(text) {
                self.seen += at + text.len();
                return;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "no {text:?} in {}", &printed[self.seen..]);
            output = ready.wait_timeout(output, left).unwrap().0;
        }
    }

    /// Wait for `text`, then type `answer` and Enter.
    fn answer(&mut self, text: &str, answer: &str) {
        self.wait(text);
        std::io::Write::write_all(&mut self.master, format!("{answer}\n").as_bytes()).unwrap();
    }

    /// Wait for the run to end, returning its exit code, everything it printed and stderr.
    fn finish(&mut self) -> (i32, String, String) {
        let mut pipe = self.child.stderr.take().unwrap();
        let stderr = std::thread::spawn(move || {
            let mut stderr = String::new();
            std::io::Read::read_to_string(&mut pipe, &mut stderr).unwrap();
            stderr
        });
        // A run that never ends fails here, with what it printed.
        let deadline = Instant::now() + Duration::from_secs(60);
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                let printed = self.output.0.lock().unwrap().clone();
                panic!("still running: {}", String::from_utf8_lossy(&printed));
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let code = status.code().unwrap();
        // The reader stops once the child's side closes.
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::sync::Arc::strong_count(&self.output) > 1 && Instant::now() < deadline {
            std::thread::yield_now();
        }
        let output = self.output.0.lock().unwrap().clone();
        let output = String::from_utf8_lossy(&output).replace("\r\n", "\n");
        (code, output, stderr.join().unwrap())
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `text` with its wrapping undone: every run of whitespace as one space.
fn flat(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Each walked home's settings: a 1h TTL, and a Bash cap that outlasts it, which `vibes`
/// offers to lower on every host with a saving not measured. The compaction window is at the
/// Kixel's cap and an advisor is set, so their cards ask nothing.
const HOME_SETTINGS: &str = r#"{"promptCacheTtl":"1h","autoCompactWindow":400000,"advisorModel":"fable","cleanupPeriodDays":365,"env":{"BASH_MAX_TIMEOUT_MS":"7200000"}}"#;

/// [`HOME_SETTINGS`] with the cap lowered under the TTL, as accepting its change writes them.
const CAP_LOWERED: &str = "{\n  \"promptCacheTtl\": \"1h\",\n  \"autoCompactWindow\": 400000,\n  \"advisorModel\": \"fable\",\n  \"cleanupPeriodDays\": 365,\n  \"env\": {\n    \"BASH_MAX_TIMEOUT_MS\": \"3300000\"\n  }\n}";

impl Hosts {
    /// `vibes` on this host, devbox and laptop, walked on a terminal after the billing questions.
    fn walk(&self) -> Terminal {
        self.host("devbox");
        self.host("laptop");
        self.walk_ready("vibes")
    }

    /// `command`'s walk over the hosts of [`Hosts::walk`], already set up, each home given
    /// [`HOME_SETTINGS`].
    fn walk_ready(&self, command: &str) -> Terminal {
        for host in ["local", "devbox", "laptop"] {
            std::fs::write(self.settings(host), HOME_SETTINGS).unwrap();
        }
        let mut command = self.invoke(command, &["--plain", "--ssh", "devbox", "--ssh", "laptop"]);
        command.env("NO_COLOR", "1");
        let mut terminal = Terminal::spawn(command);
        // Each host's profile, in the order the hosts were given.
        for profile in ["~/.claude", "devbox:~/.claude", "laptop:~/.claude"] {
            terminal.answer(&format!("How is {profile} paid for?"), "a");
        }
        terminal
    }

    fn settings(&self, host: &str) -> PathBuf {
        self.root
            .join("homes")
            .join(host)
            .join(".claude/settings.json")
    }

    fn backup(&self, host: &str) -> Option<String> {
        std::fs::read_to_string(
            self.root
                .join("homes")
                .join(host)
                .join(".claude/settings.json.token-max.bak"),
        )
        .ok()
    }
}

#[test]
fn the_walk_offers_every_hosts_changes_and_writes_each_on_its_host() {
    let hosts = Hosts::new();
    let mut terminal = hosts.walk();
    // Asked in the order the hosts were given.
    terminal.wait("token-max vibes: 3 pieces of advice the transcripts can't price.");
    terminal.wait("Host    this host");
    terminal.answer("Accept? [y]es, [n]o, [q]uit:", "y");
    terminal.wait("Host    devbox");
    terminal.answer("Accept? [y]es, [n]o, [q]uit:", "n");
    terminal.wait("Host    laptop");
    terminal.answer("Accept? [y]es, [n]o, [q]uit:", "y");
    terminal.wait("laptop:~/.claude/settings.json\n  @@");
    terminal.answer("[n]o:", "y");
    let (code, output, stderr) = terminal.finish();
    assert_eq!(code, 0, "{output}{stderr}");
    // Advice has no report to print again after its questions.
    assert!(!output.contains("token-max: devbox  "), "{output}");
    assert!(flat(&output).contains("Write the 2 accepted changes to ~/.claude/settings.json and laptop:~/.claude/settings.json?"), "{output}");
    assert!(flat(&output).contains("Wrote 1 edit to laptop:~/.claude/settings.json. Backed up the old file to laptop:~/.claude/settings.json.token-max.bak."), "{output}");
    for host in ["local", "laptop"] {
        assert_eq!(
            std::fs::read_to_string(hosts.settings(host)).unwrap(),
            CAP_LOWERED
        );
        assert_eq!(hosts.backup(host).unwrap(), HOME_SETTINGS);
    }
    // Declined, so its file is as it was.
    assert_eq!(
        std::fs::read_to_string(hosts.settings("devbox")).unwrap(),
        HOME_SETTINGS
    );
    assert_eq!(hosts.backup("devbox"), None);
}

#[test]
fn doctor_hands_every_host_to_vibes_from_the_same_scan() {
    let hosts = Hosts::new();
    hosts.host("devbox");
    hosts.host("laptop");
    // Put this build on each destination first, so the walk reaches each of them once.
    let (_, _, stderr) = hosts.run(&["--ssh", "devbox", "--ssh", "laptop"]);
    assert_eq!(stderr, "");
    std::fs::remove_file(hosts.root.join("ssh.args")).unwrap();
    let mut terminal = hosts.walk_ready("doctor");
    // One credit for every host opens the walk, before this host's header.
    terminal.wait("token-max: total over 3 hosts  ");
    terminal.wait("Compared with defaults, ");
    // Nothing measured to change, so doctor's walk ends with one report for every host.
    terminal.wait("Nothing to change on these hosts.");
    terminal.wait("token-max vibes has 3 pieces of advice the transcripts can't price. Show them?");
    terminal.answer("[y]es, [n]o:", "y");
    terminal.wait("token-max vibes: 3 pieces of advice the transcripts can't price.");
    terminal.wait("Host    this host");
    terminal.answer("Accept? [y]es, [n]o, [q]uit:", "n");
    terminal.wait("Host    devbox");
    terminal.answer("Accept? [y]es, [n]o, [q]uit:", "y");
    terminal.wait("Host    laptop");
    terminal.answer("Accept? [y]es, [n]o, [q]uit:", "y");
    terminal.answer("[n]o:", "y");
    let (code, output, stderr) = terminal.finish();
    assert_eq!((code, stderr.as_str()), (0, ""), "{output}");
    assert!(flat(&output).contains("Write the 2 accepted changes to devbox:~/.claude/settings.json and laptop:~/.claude/settings.json?"), "{output}");
    for host in ["devbox", "laptop"] {
        assert!(
            flat(&output).contains(&format!("Wrote 1 edit to {host}:~/.claude/settings.json.")),
            "{output}"
        );
        assert_eq!(
            std::fs::read_to_string(hosts.settings(host)).unwrap(),
            CAP_LOWERED
        );
    }
    assert_eq!(
        std::fs::read_to_string(hosts.settings("local")).unwrap(),
        HOME_SETTINGS
    );
    // One scan for both commands: each destination was reached once, and each host's billing
    // asked once.
    let args = std::fs::read_to_string(hosts.root.join("ssh.args")).unwrap();
    assert_eq!(args.matches("--ssh-scan").count(), 2, "{args}");
    assert_eq!(output.matches("paid for?").count(), 3, "{output}");
    // Each host saves the same with its 1h TTL and counts one setting not measured; the
    // credit adds them, its share of the three hosts' spend together, both where the walk
    // opens and in the one report after it.
    let flat = flat(&output);
    let credit = "Compared with defaults, your settings save <$0.01/mo, 13% of these hosts' spend. 3 more settings differ by amounts not measured. Overlapping settings may save less.";
    assert!(
        flat.contains(&format!(
            "$0.03 at API prices {credit} No changes recommended or worth considering. {credit}"
        )),
        "{output}"
    );
    assert_eq!(flat.matches(credit).count(), 2, "{output}");
    // One line sums a setting kept on every host, each host's history its own.
    let row =
        "keep Main-agent prompt cache TTL 3 kept saves <$0.01/mo 13% [this host, devbox, laptop]";
    assert!(flat.contains(row), "{output}");
}

#[test]
fn one_hosts_walk_opens_with_its_credit() {
    let hosts = Hosts::new();
    std::fs::write(hosts.settings("local"), HOME_SETTINGS).unwrap();
    let mut command = hosts.command(&["--plain"]);
    command.env("NO_COLOR", "1");
    let mut terminal = Terminal::spawn(command);
    terminal.answer("How is ~/.claude paid for?", "a");
    // Nothing to change, yet the header is followed by the credit before the report.
    terminal.wait("token-max  2 requests in 1 session");
    terminal.wait("at API prices\n  Compared with defaults, your settings save <$0.01/mo");
    terminal.answer("can't price. Show it?", "n");
    let (code, output, stderr) = terminal.finish();
    assert_eq!((code, stderr.as_str()), (0, ""), "{output}");
    // The report after the walk keeps its own line.
    let credit = "Compared with defaults, your settings save <$0.01/mo, 13% of this host's spend. 1 more setting differs by an amount not measured.";
    assert_eq!(flat(&output).matches(credit).count(), 2, "{output}");
}

#[test]
fn a_host_that_fails_to_write_stops_the_run_before_later_hosts() {
    let hosts = Hosts::new();
    let mut terminal = hosts.walk();
    for _ in 0..3 {
        terminal.answer("Accept? [y]es, [n]o, [q]uit:", "y");
    }
    terminal.wait("Write the 3 accepted changes");
    // Another tool writes devbox's settings while the confirm waits.
    std::fs::write(hosts.settings("devbox"), "{}").unwrap();
    terminal.answer("[n]o:", "y");
    let (code, output, stderr) = terminal.finish();
    assert_eq!(code, 2, "{output}{stderr}");
    assert!(
        output.contains("Wrote 1 edit to ~/.claude/settings.json."),
        "{output}"
    );
    assert!(!output.contains("Wrote 1 edit to devbox"), "{output}");
    assert!(stderr.contains("devbox: "), "{stderr}");
    assert!(
        flat(&stderr).contains("changed while token-max was running; nothing was written"),
        "{stderr}"
    );
    assert!(
        flat(&stderr).contains("; nothing was written on laptop"),
        "{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(hosts.settings("local")).unwrap(),
        CAP_LOWERED
    );
    assert_eq!(
        std::fs::read_to_string(hosts.settings("devbox")).unwrap(),
        "{}"
    );
    assert_eq!(
        std::fs::read_to_string(hosts.settings("laptop")).unwrap(),
        HOME_SETTINGS
    );
    for host in ["devbox", "laptop"] {
        assert_eq!(hosts.backup(host), None);
    }
}

#[test]
fn a_walk_that_is_not_confirmed_writes_no_host() {
    let hosts = Hosts::new();
    let mut terminal = hosts.walk();
    for _ in 0..3 {
        terminal.answer("Accept? [y]es, [n]o, [q]uit:", "y");
    }
    terminal.answer("[n]o:", "n");
    let (code, output, stderr) = terminal.finish();
    assert_eq!(code, 0, "{output}{stderr}");
    assert!(output.ends_with("Nothing was written.\n"), "{output}");
    for host in ["local", "devbox", "laptop"] {
        assert_eq!(
            std::fs::read_to_string(hosts.settings(host)).unwrap(),
            HOME_SETTINGS
        );
        assert_eq!(hosts.backup(host), None);
    }
    // Each destination exits once its connection closes.
    for host in ["devbox", "laptop"] {
        let pid: i32 = std::fs::read_to_string(hosts.root.join(format!("{host}.pid")))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        // SAFETY: signal 0 only checks whether the process exists.
        while unsafe { libc::kill(pid, 0) } == 0 {
            assert!(Instant::now() < deadline, "{host} still running");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[test]
fn a_failed_write_never_reads_as_nothing_written() {
    let hosts = Hosts::new();
    let mut terminal = hosts.walk();
    for _ in 0..3 {
        terminal.answer("Accept? [y]es, [n]o, [q]uit:", "y");
    }
    terminal.wait("Write the 3 accepted changes");
    // This host's file changes while the confirm waits, so the first write fails.
    std::fs::write(hosts.settings("local"), "{}").unwrap();
    terminal.answer("[n]o:", "y");
    let (code, output, stderr) = terminal.finish();
    assert_eq!(code, 2, "{output}{stderr}");
    let closing = &output[output.rfind("[n]o:").unwrap()..];
    assert!(
        !closing.contains("Wrote") && !closing.contains("Nothing"),
        "{output}"
    );
    assert!(
        flat(&stderr).contains("; nothing was written on devbox, laptop"),
        "{stderr}"
    );
}

#[test]
fn ctrl_c_while_a_destination_writes_stops_waiting_and_starts_no_other_host() {
    let hosts = Hosts::new();
    hosts.host("devbox");
    hosts.host("laptop");
    // devbox's token-max runs behind a relay of its stdin that holds the write back, so the
    // coordinator waits on that write until it is cancelled.
    for name in ["devbox.fifo", "never"] {
        let fifo = std::ffi::CString::new(hosts.root.join(name).to_str().unwrap()).unwrap();
        // SAFETY: mkfifo creates a named pipe at a path this test owns.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    }
    hosts.fake(
        "devbox",
        &format!(
            r#"'{BIN}' "$@" < "$TEST_ROOT/devbox.fifo" &
exec 3>"$TEST_ROOT/devbox.fifo"
while IFS= read -r line; do
  [ "$line" = '"write"' ] && : > "$TEST_ROOT/writing" && read -r never < "$TEST_ROOT/never"
  printf '%s\n' "$line" >&3
done"#
        ),
    );
    let mut terminal = hosts.walk_ready("vibes");
    for _ in 0..3 {
        terminal.answer("Accept? [y]es, [n]o, [q]uit:", "y");
    }
    terminal.answer("[n]o:", "y");
    let writing = hosts.root.join("writing");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !writing.exists() {
        assert!(Instant::now() < deadline, "devbox never started writing");
        std::thread::sleep(Duration::from_millis(20));
    }
    // SAFETY: kill only signals the child this test spawned.
    assert_eq!(
        unsafe { libc::kill(terminal.child.id() as i32, libc::SIGINT) },
        0
    );
    let (code, output, stderr) = terminal.finish();
    assert_eq!(code, 2, "{output}{stderr}");
    assert!(
        flat(&output).contains("Wrote 1 edit to ~/.claude/settings.json."),
        "{output}"
    );
    let stderr = flat(&stderr);
    assert!(
        stderr.contains("devbox: cancelled while it was writing, so it may have changed"),
        "{stderr}"
    );
    assert!(stderr.contains("nothing was written on laptop"), "{stderr}");
    assert_eq!(
        std::fs::read_to_string(hosts.settings("laptop")).unwrap(),
        HOME_SETTINGS
    );
}

#[test]
fn each_destination_prices_its_main_ttl_by_the_answer_given_for_it() {
    let hosts = Hosts::new();
    hosts.host("devbox");
    hosts.host("laptop");
    let mut command = hosts.command(&[
        "--plain",
        "--defaults",
        "--ssh",
        "devbox",
        "--ssh",
        "laptop",
    ]);
    command.env("NO_COLOR", "1");
    let mut terminal = Terminal::spawn(command);
    for (profile, answer) in [
        ("~/.claude", "s"),
        ("devbox:~/.claude", "p"),
        ("laptop:~/.claude", "a"),
    ] {
        terminal.answer(&format!("How is {profile} paid for?"), answer);
    }
    let (code, output, stderr) = terminal.finish();
    assert_eq!(code, 1, "{output}{stderr}");
    // Skipped, a plan's 1h, and an API key's 5m: each host's own answer, not this host's.
    // This host's and laptop's changes share a row, which names each host's own value.
    let text = flat(&output);
    for line in [
        "Prompt cache TTL main (this host) unset -> 1h, main (laptop) 5m (default) -> 1h saves <$0.01/mo + up to <$0.01/mo 9% [this host, laptop]",
        "Main-agent prompt cache TTL 1h (default) saves <$0.01/mo 4% [devbox]",
    ] {
        assert!(text.contains(line), "{line}: {output}");
    }
    // Every setting is its default, so neither the total nor any host compares with them.
    assert!(!output.contains("Compared with defaults"), "{output}");
}

#[test]
fn a_destination_takes_answers_only_for_the_profiles_it_listed() {
    let hosts = Hosts::new();
    hosts.host("devbox");
    // Answers for another root than the one devbox lists, as a coordinator that misread it
    // would send.
    hosts.fake(
        "devbox",
        &format!(
            r#"{{ read -r paid; echo '{{"profiles":{{"/elsewhere":"plan"}}}}'; cat; }} | '{BIN}' "$@""#
        ),
    );
    let (status, stdout, stderr) = hosts.run(&["--ssh", "devbox"]);
    assert_eq!(status, 2, "{stderr}");
    assert_eq!(stdout, "");
    assert!(
        stderr.starts_with(
            "token-max: devbox: the coordinator answered how a profile pays that this host did not list"
        ),
        "{stderr}"
    );
}

#[test]
fn audits_from_every_host_rank_together_with_each_pointer_on_its_host() {
    let hosts = Hosts::new();
    let home = hosts.host("devbox");
    // The destination's session ran in a checkout of the same repository as this host's, by
    // its SSH remote.
    let remote = |dir: &Path, url: &str| {
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(
            dir.join(".git/config"),
            format!("[remote \"origin\"]\n\turl = {url}\n"),
        )
        .unwrap();
    };
    let there = home.join("checkout");
    remote(&there, "git@host:o/r.git");
    let here = hosts.root.join("homes/local/checkout");
    remote(&here, "https://host/o/r");
    let session: String = std::fs::read_to_string(fixture("claude.jsonl"))
        .unwrap()
        .replace("session-a", "session-there")
        .replace("message-", "there-message-")
        .lines()
        .map(|line| {
            let mut record: Value = serde_json::from_str(line).unwrap();
            record["cwd"] = there.to_str().unwrap().into();
            format!("{record}\n")
        })
        .collect();
    std::fs::write(
        home.join(".claude/projects/my project/there.jsonl"),
        session,
    )
    .unwrap();
    let audit = |args: &[&str]| {
        let (status, stdout, stderr) = run(hosts
            .invoke(
                "audit",
                &[&["--since", "2026-11-01", "--ssh", "devbox"], args].concat(),
            )
            .current_dir(&here));
        assert_eq!((status, &*stderr), (0, ""), "{stdout}");
        assert!(!stdout.contains("SYNTHETIC_PRIVATE"), "{stdout}");
        serde_json::from_str::<Value>(&stdout).unwrap()
    };
    let hosted = |found: &Value| -> Vec<(Value, String)> {
        let list = &found["sessions"]["list"];
        found["sessions"]["costliest"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| {
                let at = &list[i.as_u64().unwrap() as usize]["at"];
                (at["host"].clone(), at["file"].as_str().unwrap().to_string())
            })
            .collect()
    };
    // Every host's sessions, in one ranking, each pointer naming its host.
    let all = audit(&["--all"]);
    assert_eq!(all["hosts"], json!([null, "devbox"]));
    assert_eq!(all["totals"]["sessions"], 3);
    let mut found = hosted(&all);
    found.sort_by_key(|(host, file)| (host.to_string(), file.clone()));
    assert_eq!(found.len(), 3);
    assert_eq!(found[0].0, json!("devbox"));
    assert!(found[0].1.ends_with("main.jsonl"), "{found:?}");
    assert!(found[1].1.ends_with("there.jsonl"), "{found:?}");
    assert_eq!(found[2].0, Value::Null);
    // The repository here, matched there by its remote in another form.
    let scoped = audit(&[]);
    assert_eq!(scoped["totals"]["sessions"], 1);
    let found = hosted(&scoped);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0, json!("devbox"));
    assert!(found[0].1.ends_with("there.jsonl"), "{found:?}");

    // A destination may send no more entries than this host asked for.
    let header = hosts.header();
    let shape = r#"{"harness":"claude","session":null,"parent":null,"subagent":false,"cwd":null,"requests":1,"costUsd":1,"complete":true,"skippedLines":0,"inputTokens":0,"cacheReadTokens":0,"cacheHitRatio":null,"peakInputTokens":0,"startInputTokens":null,"compactions":0,"toolCalls":0,"toolErrors":null,"streak":null,"at":{"host":null,"file":"f","line":1},"earlyLines":[],"errorLines":[]}"#;
    hosts.host("flood");
    hosts.fake(
        "flood",
        &format!(
            r#"echo '{header}'; read -r request; echo '{{"audit":{{"since":1793491200000,"audit":{{"totals":{{"sessions":2,"requests":2,"costUsd":2,"incomplete":0}},"cacheMisses":{{"total":{{"events":0,"tokens":0,"costUsd":0,"unpriced":0}},"byCause":{{}},"hotspots":[]}},"toolOutputs":{{"byTool":[],"hotspots":[]}},"sessions":{{"costliest":[{shape},{shape}],"leastCached":[],"mostInput":[],"largestStart":[],"subagents":[],"mostToolCalls":[],"mostToolErrors":[],"longestStreaks":[]}}}}}}}}'; exec sleep 600"#
        ),
    );
    let (status, stdout, stderr) = run(hosts
        .invoke(
            "audit",
            &[
                "--all",
                "--since",
                "2026-11-01",
                "--limit",
                "1",
                "--ssh",
                "flood",
            ],
        )
        .current_dir(&here));
    assert_eq!((status, &*stdout), (2, ""));
    assert_eq!(
        stderr,
        "{\"error\":\"flood: token-max there sent a list longer than the 1 entries asked for\"}\n"
    );
    // A repository with no remote names nothing another host could match.
    std::fs::write(here.join(".git/config"), "[core]\n").unwrap();
    let (status, _, stderr) = run(hosts
        .invoke("audit", &["--ssh", "devbox"])
        .current_dir(&here));
    assert_eq!(status, 2);
    assert!(stderr.contains("has no remote"), "{stderr}");
}
