//! Synthetic requests and checkout-local scratch profiles for unit tests.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use indexmap::IndexMap;

use crate::model::*;
use crate::prices::{Catalog, PriceOptions, load_prices};

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
        harness: Harness::Claude,
        session: "session".into(),
        agent: Agent::Main,
        time: 0,
        end_time: 0,
        model: Some("claude-opus-5-5".into()),
        provider: Some("anthropic".into()),
        api: Api::Messages,
        source: Some("sdk".into()),
        trigger: None,
        reference: None,
        turn: None,
        version: Some("2.1.314".into()),
        client: Some("sdk-ts".into()),
        cwd: None,
        parts: vec![message_part(claude_usage(ClaudeUsage::default()))],
        server_tools: Some(IndexMap::new()),
        profiles: vec!["profile".into()],
        locations: vec![Location {
            file: "synthetic".into(),
            line: 1,
        }],
        accounting: Accounting::Individual,
        attempt: None,
        requested_speed: None,
        issues: vec![],
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
