//! Synthetic requests and isolated scratch profiles for unit tests.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::collections::IndexMap;

use crate::controls::Env;
use crate::discovery::DiscoveryOptions;
use crate::model::*;
use crate::prices::{Catalog, PriceOptions, load_prices};
use crate::reader::ScanOptions;

mod paths;
pub use paths::native_path;

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
        }]
        .into(),
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
        cache: std::env::temp_dir().join("token-max-unused-cache.json"),
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

/// An isolated home with empty Claude and Codex profiles in the system temp directory,
/// outside any checkout or real home whose settings discovery would read as a project's.
pub struct Workspace {
    pub home: PathBuf,
    pub claude: PathBuf,
    pub codex: PathBuf,
}

impl Workspace {
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        // create_dir fails on an existing path, so a planted symlink is never followed; a
        // name a killed run left behind is skipped.
        let home = loop {
            let home = std::env::temp_dir().join(format!(
                "token-max-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&home) {
                Ok(()) => break home,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => panic!("{}: {e}", home.display()),
            }
        };
        // Tests compare discovered paths, normalized as discovery does: symlinks such as
        // macOS's /var resolve, and Windows' verbatim prefix is stripped.
        let home = PathBuf::from(crate::discovery::realpath(home.to_str().unwrap()).unwrap());
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
                defaults: false,
                billing: Default::default(),
            },
            since: None,
            reach: crate::reader::Reach::History,
            reading: None,
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

/// One synthetic Claude transcript of `profile` whose unreadable lines are `lines`, which
/// leave `unmatched` tool results without a parsed call.
pub fn make_snapshot(profile: &str, lines: &[&str], unmatched: u64) -> Snapshot {
    let mut unreadable = crate::unreadable::Unreadable::default();
    for line in lines {
        unreadable.observe(line.as_bytes());
    }
    unreadable.unmatched_results = unmatched;
    Snapshot {
        file: "synthetic".into(),
        harness: Harness::Claude,
        profiles: vec![profile.into()],
        bytes: 0,
        device: 0,
        inode: 0,
        deferred_bytes: 0,
        skipped_lines: lines.len() as u64,
        unreadable,
        lines: 0,
    }
}

/// A cache TTL check for `profile`'s `bucket` that recommends `recommended` with `action`.
pub fn make_ttl_check(profile: &str, bucket: Bucket, action: Action, recommended: Ttl) -> TtlCheck {
    TtlCheck {
        base: CheckBase {
            profile: profile.into(),
            profiles: vec![profile.into()],
            history_profiles: vec![],
            evidence: Evidence::Estimated,
            action,
            current: None,
            recommended: Some(recommended),
            owner: String::new(),
            since: None,
            until: None,
            window_basis: String::new(),
            baseline: String::new(),
            requests: 0,
            coverage: None,
            savings_usd: None,
            margin: None,
            monthly_savings_usd: None,
            break_even: None,
            assumptions: vec![],
            uncertainty: vec![],
            sources: vec![],
            upgrade: None,
        },
        window_basis_kind: WindowBasisKind::Available,
        bucket,
        one_hour_savings_usd: 0.0,
        comparison_estimate: PerTtl::default(),
        cost: Default::default(),
        summary_cost: Default::default(),
        comparison_cost: Default::default(),
        fixed_cost: crate::prices::zero_cost(),
        usage_estimates: vec![],
        cache_evidence: vec![],
        sensitivity: TtlSensitivity {
            cold_prefix1h_usd: None,
            cold_prefix1h_comparison_usd: None,
            unlinked: None,
        },
    }
}

/// One synthetic Claude profile rooted at `root`, with explicit 5m TTLs and no speed or idle controls.
pub fn make_profile(id: &str, root: &str) -> Profile {
    let ttl = || TtlControl {
        value: Some(Ttl::FiveMinutes),
        owner: format!("{root}/settings.json:promptCacheTtl"),
        reason: "explicit setting".into(),
        varies: false,
    };
    Profile {
        agents: Default::default(),
        instruction_budget_file: None,
        instruction_budget_unresolved: None,
        outranking_files: vec![],
        ttl_directories: IndexMap::default(),
        claude_config: None,
        updater: None,
        codex_system_config: None,
        switched_off: vec![],
        pretended_on: vec![],
        policy_off: vec![],
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
        billing: None,
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
            percent: None,
            layers: None,
        },
        idle: None,
        speed: None,
        bash: None,
        retention: None,
        turn_end: None,
        effort: None,
        speed_directories: None,
        idle_directories: None,
        background: vec![],
        background_directories: IndexMap::default(),
        background_requests: IndexMap::default(),
        memory: None,
        inbound: None,
        plugins: None,
        t3: None,
    }
}

/// One synthetic enabled T3 Claude instance rooted at `root`, with resolved settings and
/// ownership, so its lines join the native profile's that share its settings files.
pub fn make_t3_profile(id: &str, root: &str) -> Profile {
    let mut p = make_profile(id, root);
    p.t3 = Some(T3Instance {
        state: "/state".into(),
        instance: id.into(),
        enabled: true,
        idle_mode: T3IdleMode::OnReturn,
        idle_owner: "/state/settings.json:idleCompaction".into(),
        settings_unresolved: false,
        limitations: vec![],
        env: None,
        inherited_window: true,
        environment: vec![],
        retained_since: None,
    });
    p
}

/// One setting's line in a plain-text report, with its evidence under details.
#[derive(Clone, Debug, PartialEq)]
pub struct ReportLine {
    /// Whether it is listed among the changes, before everything else.
    pub change: bool,
    /// The verdict word, such as "change".
    pub mark: String,
    /// The name, value, figure and percent, with column padding collapsed to single spaces.
    pub head: String,
    /// The profile, when several were read, then the setting that changes it.
    pub setting: String,
    /// The line under it naming its default and what the current value saves against it;
    /// empty when the setting is at its default.
    pub default: String,
    /// The line after any default advising a harness upgrade; empty when there is none.
    pub upgrade: String,
    /// What the setting or feature does, the first line after any default in a details
    /// report; empty otherwise.
    pub what: String,
    /// The line after that in a details report; empty otherwise.
    pub evidence: String,
}

/// A line's columns, which two or more spaces separate, with where each starts.
fn columns(text: &str) -> Vec<(usize, &str)> {
    let mut columns = vec![];
    let mut start = None;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let gap = c == ' ' && chars.peek().is_some_and(|(_, next)| *next == ' ');
        if gap {
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
    columns
}

/// Where a report line's profile tag and setting start, after its name, value, figure and
/// percent; None when it names neither.
fn setting_start(text: &str) -> Option<usize> {
    let columns = columns(text);
    let figure = |c: &str| {
        c.ends_with('%')
            || c.ends_with("/mo")
            || c.starts_with('$')
            || c.starts_with("~$")
            || c.starts_with("at least ")
            || matches!(c, "unknown" | "not measured" | "saving not measured")
    };
    let (at, last) = *columns.last().filter(|_| columns.len() > 1)?;
    if figure(last) {
        return None;
    }
    // A profile tag before the setting belongs with it.
    match columns.iter().rev().nth(1) {
        Some((tag, text)) if text.starts_with('[') && columns.len() > 2 => Some(*tag),
        _ => Some(at),
    }
}

/// Text with runs of spaces collapsed to one.
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The setting lines of a plain, unwrapped report, in order.
pub fn report_lines(text: &str) -> Vec<ReportLine> {
    let mut group = None;
    let mut rows = vec![];
    // Every line but the header sits inside the report's margin.
    let lines: Vec<&str> = text
        .lines()
        .map(|l| l.strip_prefix("  ").unwrap_or(l))
        .collect();
    for (i, line) in lines.iter().enumerate() {
        if line.starts_with("CHANGES  ") {
            group = Some(true);
            continue;
        }
        if line.starts_with("EVERYTHING ELSE  ") {
            group = Some(false);
            continue;
        }
        // The evidence cards follow the lines.
        if line.starts_with("EVIDENCE") {
            break;
        }
        let word = line.split(' ').next().unwrap_or_default();
        let Some(change) = group else { continue };
        if !["change", "consider", "keep", "unknown", "info"].contains(&word) || line.len() < 9 {
            continue;
        }
        let rest = &line[9..];
        let (head, setting) = match setting_start(rest) {
            Some(at) => (&rest[..at], &rest[at..]),
            None => (rest, ""),
        };
        let under = |n: usize| {
            lines
                .get(i + n)
                .and_then(|l| l.strip_prefix("         "))
                .unwrap_or_default()
        };
        let default = Some(under(1))
            .filter(|l| l.starts_with("default "))
            .unwrap_or_default();
        let skip = usize::from(!default.is_empty());
        let upgrade = Some(under(1 + skip))
            .filter(|l| !l.starts_with("What: "))
            .unwrap_or_default();
        let skip = skip + usize::from(!upgrade.is_empty());
        let what = under(1 + skip).strip_prefix("What: ").unwrap_or_default();
        let evidence = if what.is_empty() { "" } else { under(2 + skip) };
        rows.push(ReportLine {
            change,
            mark: word.into(),
            head: collapse(head),
            setting: collapse(setting),
            default: default.into(),
            upgrade: upgrade.into(),
            what: what.into(),
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
