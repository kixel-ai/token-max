//! Synthetic requests and checkout-local scratch profiles for unit tests.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::collections::IndexMap;

use crate::controls::Env;
use crate::discovery::DiscoveryOptions;
use crate::model::*;
use crate::prices::{Catalog, PriceOptions, load_prices};
use crate::reader::ScanOptions;

/// Adjustable synthetic Claude usage; defaults to a 1000-token 5m write and 10 output tokens.
#[derive(Clone, Copy, Debug)]
pub struct ClaudeUsage {
    pub write: i64,
    pub read: i64,
    pub output: i64,
    pub ttl: Ttl,
    pub input: i64,
}

impl Default for ClaudeUsage {
    fn default() -> Self {
        Self {
            write: 1000,
            read: 0,
            output: 10,
            ttl: Ttl::FiveMinutes,
            input: 0,
        }
    }
}

/// Complete synthetic Claude usage, normalized like a recorded `usage` object.
pub fn claude_usage(options: ClaudeUsage) -> Usage {
    let ClaudeUsage {
        write,
        read,
        output,
        ttl,
        input,
    } = options;
    Usage {
        input: Some(input + read + write),
        output: Some(output),
        read: Some(read),
        write: Some(write),
        write5m: Some(if ttl == Ttl::FiveMinutes { write } else { 0 }),
        write1h: Some(if ttl == Ttl::OneHour { write } else { 0 }),
        write30m: None,
        write12h: None,
        reasoning: None,
    }
}

/// One synthetic message part priced as standard global Opus.
pub fn message_part(usage: Usage) -> BillingPart {
    BillingPart {
        kind: PartKind::Message,
        model: Some("claude-opus-5-5".into()),
        usage,
        tier: Some("standard".into()),
        speed: Some("standard".into()),
        geo: Some("global".into()),
    }
}

/// One synthetic normalized request without reading a real session; override fields with `..make_request()`.
pub fn make_request() -> Request {
    Request {
        evidence: RequestEvidence::Measured,
        id: "request".into(),
        time: 0,
        end_time: 0,
        reference: None,
        turn: None,
        parts: vec![message_part(claude_usage(ClaudeUsage::default()))],
        locations: vec![Location {
            file: "synthetic".into(),
            line: 1,
        }],
        t3: None,
        meta: RequestMeta {
            harness: Harness::Claude,
            session: "session".into(),
            agent: Agent::Main,
            model: Some("claude-opus-5-5".into()),
            provider: Some("anthropic".into()),
            api: Api::Messages,
            source: Some("sdk".into()),
            trigger: None,
            version: Some("2.1.314".into()),
            client: Some("sdk-ts".into()),
            cwd: None,
            effort: None,
            server_tools: Some(Default::default()),
            profiles: vec!["profile".into()].into(),
            accounting: Accounting::Individual,
            attempt: None,
            requested_speed: None,
            issues: vec![],
        }
        .into(),
    }
}

/// Deterministic synthetic public prices, read without using the network.
pub fn prices() -> Catalog {
    load_prices(&PriceOptions {
        file: Some(fixture_path("prices.json")),
        cache: Path::new(env!("CARGO_MANIFEST_DIR")).join(".tmp/unused-cache.json"),
        offline: false,
    })
    .unwrap()
}

/// A checked-in synthetic fixture under `test/fixtures`.
pub fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("test/fixtures")
        .join(name)
}

/// An isolated home with empty Claude and Codex profiles under the checkout's `.tmp/`.
pub struct Workspace {
    pub home: PathBuf,
    pub claude: PathBuf,
    pub codex: PathBuf,
}

impl Workspace {
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let home = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".tmp")
            .join(format!(
                "test-{}-{}",
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
    /// Copy a checked-in synthetic transcript into this workspace.
    pub fn fixture(&self, destination: impl AsRef<Path>, name: &str) {
        std::fs::copy(fixture_path(name), destination).unwrap();
    }
    /// The managed settings file discovery reads in this workspace.
    pub fn managed_settings_file(&self) -> PathBuf {
        self.home.join("managed-settings.json")
    }
    /// Discover only this workspace's profiles, with no environment and no time window.
    pub fn scan_options(&self) -> ScanOptions {
        let text = |path: PathBuf| path.to_string_lossy().into_owned();
        ScanOptions {
            discovery: DiscoveryOptions {
                home: text(self.home.clone()),
                env: Env::default(),
                claude_dirs: vec![],
                codex_dirs: vec![],
                t3_dirs: vec![],
                managed_settings_file: text(self.managed_settings_file()),
                codex_system_config_file: text(self.home.join("system.toml")),
                codex_requirements_file: text(self.home.join("requirements.toml")),
            },
            since: None,
        }
    }
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

/// One synthetic Claude profile rooted at `root`, with explicit 5m TTLs and no speed or idle controls.
pub fn make_profile(id: &str, root: &str) -> Profile {
    let ttl = || TtlControl {
        value: Some(Ttl::FiveMinutes),
        owner: format!("{root}/settings.json:promptCacheTtl"),
        reason: "explicit setting".into(),
    };
    Profile {
        agents: Default::default(),
        instruction_budget_file: None,
        instruction_budget_unresolved: None,
        id: id.into(),
        harness: Harness::Claude,
        root: root.into(),
        aliases: vec![],
        histories: vec![format!("{root}/projects")],
        settings_file: format!("{root}/settings.json"),
        ttl: PerBucket {
            main: ttl(),
            subagent: ttl(),
        },
        provider: None,
        settings: IndexMap::default(),
        mcp_servers: vec![],
        compaction: CompactionControls {
            default: WindowControl {
                value: None,
                owner: format!("{root}/settings.json:autoCompactWindow"),
                reason: "no explicit window".into(),
                applicable: true,
                minimum_claude_patch: None,
                comparison_unknown: None,
                unset: false,
                setting: None,
            },
            models: IndexMap::default(),
            capped: false,
            declared: None,
            tuned: IndexMap::default(),
            contexts: IndexMap::default(),
        },
        idle: None,
        speed: None,
        bash: None,
        speed_directories: None,
        idle_directories: None,
        background: vec![],
        background_directories: IndexMap::default(),
        background_requests: IndexMap::default(),
        memory: None,
        t3: None,
    }
}

/// One setting's two lines in a plain-text default report.
#[derive(Clone, Debug, PartialEq)]
pub struct ReportLine {
    /// The section's kind of claim: "MEASURED", "OFFICIAL" or "OPINION".
    pub claim: String,
    /// The verdict word, such as "change".
    pub mark: String,
    /// The heading line, with its column padding collapsed to single spaces.
    pub head: String,
    pub evidence: String,
}

/// The setting lines of a plain, unwrapped default report.
pub fn report_lines(text: &str) -> Vec<ReportLine> {
    let mut claim = String::new();
    let mut rows = vec![];
    let lines: Vec<&str> = text.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if let Some((title, _)) = line.split_once("  ")
            && ["MEASURED", "OFFICIAL", "OPINION"].contains(&title)
        {
            claim = title.into();
            continue;
        }
        let word = line.split(' ').next().unwrap_or_default();
        let evidence = lines.get(i + 1).and_then(|l| l.strip_prefix("         "));
        if claim.is_empty()
            || !["change", "consider", "keep", "unknown", "info"].contains(&word)
            || line.len() < 9
        {
            continue;
        }
        let Some(evidence) = evidence else { continue };
        rows.push(ReportLine {
            claim: claim.clone(),
            mark: word.into(),
            head: line[9..].split_whitespace().collect::<Vec<_>>().join(" "),
            evidence: evidence.into(),
        });
    }
    rows
}

/// The report line whose heading starts with `head`.
pub fn report_line(text: &str, head: &str) -> ReportLine {
    let rows = report_lines(text);
    rows.iter()
        .find(|r| r.head.starts_with(head))
        .cloned()
        .unwrap_or_else(|| panic!("no line starts with {head:?} in {rows:#?}"))
}
