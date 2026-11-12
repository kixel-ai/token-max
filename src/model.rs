//! Shared data types. Serialized names match the JSON report.

use crate::collections::IndexMap;
use serde::{Deserialize, Serialize};

/// Milliseconds since the Unix epoch.
pub type Millis = i64;

/// A shared immutable string, for identifiers repeated across many records.
pub type Text = std::sync::Arc<str>;

/// An opaque identifier. Nearly every one is a SHA-256 hex digest, held as its 32 bytes
/// rather than 64 characters; any other text, such as a modeled event's, is kept as is.
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum Id {
    Digest([u8; 32]),
    Text(Box<str>),
}

impl From<&str> for Id {
    fn from(text: &str) -> Self {
        let digit = |b: u8| match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            _ => None,
        };
        let b = text.as_bytes();
        if b.len() == 64 {
            let mut digest = [0; 32];
            if digest.iter_mut().enumerate().all(|(i, out)| {
                match (digit(b[2 * i]), digit(b[2 * i + 1])) {
                    (Some(high), Some(low)) => {
                        *out = high << 4 | low;
                        true
                    }
                    _ => false,
                }
            }) {
                return Id::Digest(digest);
            }
        }
        Id::Text(text.into())
    }
}

impl From<String> for Id {
    fn from(text: String) -> Self {
        Id::from(text.as_str())
    }
}

impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Id::Digest(digest) => digest.iter().try_for_each(|b| write!(f, "{b:02x}")),
            Id::Text(text) => f.write_str(text),
        }
    }
}

impl std::fmt::Debug for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.to_string())
    }
}

impl Serialize for Id {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Id {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Id::from(
            <std::borrow::Cow<'de, str>>::deserialize(deserializer)?.as_ref(),
        ))
    }
}

/// One shared copy of each distinct string and profile set the records of a scan repeat.
#[derive(Default)]
pub struct Interner {
    texts: crate::collections::HashSet<Text>,
    profiles: crate::collections::HashSet<Profiles>,
    tools: crate::collections::HashSet<ServerTools>,
}

impl Interner {
    /// The shared copy of `value`.
    pub fn text(&mut self, value: &str) -> Text {
        if let Some(text) = self.texts.get(value) {
            return text.clone();
        }
        let text = Text::from(value);
        self.texts.insert(text.clone());
        text
    }

    /// Replace `slot` with its shared copy.
    pub fn share(&mut self, slot: &mut Option<Text>) {
        if let Some(text) = slot {
            *text = self.text(text);
        }
    }

    /// The shared copy of a profile set.
    pub fn profiles(&mut self, ids: &[String]) -> Profiles {
        if let Some(profiles) = self.profiles.get(ids) {
            return profiles.clone();
        }
        let profiles = Profiles::from(ids);
        self.profiles.insert(profiles.clone());
        profiles
    }

    /// `profiles` followed by any of `items` it lacks, as `[...new Set([...a, ...b])]` would.
    pub fn union(&mut self, profiles: &Profiles, items: &[String]) -> Profiles {
        if items.iter().all(|item| profiles.contains(item)) {
            return profiles.clone();
        }
        let mut ids = profiles.to_vec();
        for item in items {
            if !ids.contains(item) {
                ids.push(item.clone());
            }
        }
        self.profiles(&ids)
    }

    /// Share every repeated string of a request, keeping its unique identifiers as they are.
    pub fn request(&mut self, r: &mut Request) {
        r.session = self.text(&r.session);
        for slot in [
            &mut r.model,
            &mut r.provider,
            &mut r.source,
            &mut r.trigger,
            &mut r.turn,
            &mut r.version,
            &mut r.client,
            &mut r.cwd,
        ] {
            self.share(slot);
        }
        for p in &mut r.parts {
            for slot in [&mut p.model, &mut p.tier, &mut p.speed, &mut p.geo] {
                self.share(slot);
            }
        }
        if let Some(speed) = &mut r.requested_speed {
            self.share(&mut speed.tier);
            self.share(&mut speed.speed);
        }
        for issue in &mut r.issues {
            *issue = self.text(issue);
        }
        r.profiles = self.profiles(&r.profiles);
        if let Some(tools) = &mut r.server_tools {
            match self.tools.get(tools) {
                Some(shared) => *tools = shared.clone(),
                None => {
                    self.tools.insert(tools.clone());
                }
            }
        }
        r.parts.shrink_to_fit();
        r.locations.shrink_to_fit();
        r.issues.shrink_to_fit();
    }

    /// Share every repeated string of a structural event.
    pub fn event(&mut self, e: &mut StructuralEvent) {
        e.session = self.text(&e.session);
        for slot in [
            &mut e.model,
            &mut e.version,
            &mut e.client,
            &mut e.cwd,
            &mut e.tool,
        ] {
            self.share(slot);
        }
        for origin in &mut e.origins {
            origin.session = self.text(&origin.session);
        }
        e.profiles = self.profiles(&e.profiles);
        e.origins.shrink_to_fit();
    }
}

/// The two locally supported transcript formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Harness {
    Claude,
    Codex,
}

/// A documented Claude cache lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Ttl {
    #[serde(rename = "5m")]
    FiveMinutes,
    #[serde(rename = "1h")]
    OneHour,
}

impl Ttl {
    pub const ALL: [Ttl; 2] = [Ttl::FiveMinutes, Ttl::OneHour];
    pub fn as_str(self) -> &'static str {
        match self {
            Ttl::FiveMinutes => "5m",
            Ttl::OneHour => "1h",
        }
    }
}

/// A value for each TTL, serialized as `{"5m": .., "1h": ..}`.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct PerTtl<T> {
    #[serde(rename = "5m")]
    pub five_minutes: T,
    #[serde(rename = "1h")]
    pub one_hour: T,
}

impl<T> PerTtl<T> {
    pub fn get(&self, ttl: Ttl) -> &T {
        match ttl {
            Ttl::FiveMinutes => &self.five_minutes,
            Ttl::OneHour => &self.one_hour,
        }
    }
    pub fn get_mut(&mut self, ttl: Ttl) -> &mut T {
        match ttl {
            Ttl::FiveMinutes => &mut self.five_minutes,
            Ttl::OneHour => &mut self.one_hour,
        }
    }
}

/// A value for the main conversation and for sub-agents.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PerBucket<T> {
    pub main: T,
    pub subagent: T,
}

impl<T> PerBucket<T> {
    pub fn get(&self, bucket: Bucket) -> &T {
        match bucket {
            Bucket::Main => &self.main,
            Bucket::Subagent => &self.subagent,
        }
    }
    pub fn get_mut(&mut self, bucket: Bucket) -> &mut T {
        match bucket {
            Bucket::Main => &mut self.main,
            Bucket::Subagent => &mut self.subagent,
        }
    }
}

/// The two Claude TTL buckets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Bucket {
    Main,
    Subagent,
}

impl Bucket {
    pub const ALL: [Bucket; 2] = [Bucket::Main, Bucket::Subagent];
    pub fn as_str(self) -> &'static str {
        match self {
            Bucket::Main => "main",
            Bucket::Subagent => "subagent",
        }
    }
}

/// Evidence for a result, separate from whether it recommends a change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Evidence {
    Measured,
    Estimated,
    Guidance,
    Unknown,
}

/// A safe source location without transcript content.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Location {
    #[serde(with = "crate::wire::text")]
    pub file: Text,
    pub line: u64,
}

/// Normalized counts: reads and writes are subsets of input, reasoning of output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input: Option<i64>,
    pub output: Option<i64>,
    pub read: Option<i64>,
    pub write: Option<i64>,
    #[serde(rename = "write5m")]
    pub write5m: Option<i64>,
    #[serde(rename = "write1h")]
    pub write1h: Option<i64>,
    #[serde(rename = "write30m")]
    pub write30m: Option<i64>,
    #[serde(rename = "write12h")]
    pub write12h: Option<i64>,
    pub reasoning: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PartKind {
    Message,
    Compaction,
    Advisor,
    Unknown,
}

/// One non-overlapping billed sampling iteration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BillingPart {
    pub kind: PartKind,
    #[serde(with = "crate::wire::opt_text")]
    pub model: Option<Text>,
    pub usage: Usage,
    #[serde(with = "crate::wire::opt_text")]
    pub tier: Option<Text>,
    #[serde(with = "crate::wire::opt_text")]
    pub speed: Option<Text>,
    #[serde(with = "crate::wire::opt_text")]
    pub geo: Option<Text>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RequestEvidence {
    Measured,
    Estimated,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Main,
    Subagent,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Api {
    Responses,
    Messages,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Accounting {
    Individual,
    LegacyLast,
    LegacyDelta,
}

/// Recorded API-attempt budget; this does not establish execution or billing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attempt {
    pub max_output_tokens: Option<i64>,
}

/// Requested controls never establish the billed response tier.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RequestedSpeed {
    #[serde(with = "crate::wire::opt_text")]
    pub tier: Option<Text>,
    #[serde(with = "crate::wire::opt_text")]
    pub speed: Option<Text>,
}

/// One deduplicated model request, with no message content.
///
/// Strings are shared: a request holds references to values that thousands of requests
/// repeat, such as its model, session and profiles, so a full history fits in memory.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub evidence: RequestEvidence,
    #[serde(with = "crate::wire::text")]
    pub id: Text,
    pub harness: Harness,
    #[serde(with = "crate::wire::text")]
    pub session: Text,
    pub agent: Agent,
    pub time: Millis,
    pub end_time: Millis,
    #[serde(with = "crate::wire::opt_text")]
    pub model: Option<Text>,
    #[serde(with = "crate::wire::opt_text")]
    pub provider: Option<Text>,
    pub api: Api,
    #[serde(with = "crate::wire::opt_text")]
    pub source: Option<Text>,
    #[serde(with = "crate::wire::opt_text")]
    pub trigger: Option<Text>,
    #[serde(with = "crate::wire::opt_text")]
    pub reference: Option<Text>,
    #[serde(with = "crate::wire::opt_text")]
    pub turn: Option<Text>,
    #[serde(with = "crate::wire::opt_text")]
    pub version: Option<Text>,
    #[serde(with = "crate::wire::opt_text")]
    pub client: Option<Text>,
    #[serde(with = "crate::wire::opt_text")]
    pub cwd: Option<Text>,
    pub parts: Vec<BillingPart>,
    pub server_tools: Option<ServerTools>,
    pub profiles: Profiles,
    pub locations: Vec<Location>,
    pub accounting: Accounting,
    pub attempt: Option<Attempt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_speed: Option<RequestedSpeed>,
    #[serde(with = "crate::wire::texts")]
    pub issues: Vec<Text>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t3: Option<Box<T3Request>>,
}

/// A safe location under one conversation alias.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Origin {
    pub session: Text,
    pub location: Location,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EventKind {
    Compaction,
    TurnStart,
    TurnEnd,
    ToolStart,
    ToolEnd,
    Settings,
    Client,
    IdleNotice,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IdleAttribution {
    Unknown,
    Notice,
    T3,
}

/// A scalar settings value as recorded.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Scalar {
    Bool(bool),
    Number(f64),
    Text(String),
}

/// Preceding pricing metadata from the same file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pricing {
    pub model: Option<Text>,
    pub provider: Option<Text>,
    pub tier: Option<Text>,
    pub speed: Option<Text>,
    pub geo: Option<Text>,
}

/// Structural context and timing evidence, never a billed boundary request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuralEvent {
    pub id: Id,
    /// All observed conversation aliases and safe locations for a copied event.
    pub origins: Vec<Origin>,
    pub harness: Harness,
    #[serde(with = "crate::wire::text")]
    pub session: Text,
    pub time: Millis,
    pub kind: EventKind,
    pub profiles: Profiles,
    pub location: Location,
    #[serde(with = "crate::wire::opt_text")]
    pub request: Option<Text>,
    #[serde(with = "crate::wire::opt_text")]
    pub model: Option<Text>,
    #[serde(with = "crate::wire::opt_text")]
    pub version: Option<Text>,
    #[serde(with = "crate::wire::opt_text")]
    pub client: Option<Text>,
    #[serde(with = "crate::wire::opt_text")]
    pub cwd: Option<Text>,
    pub trigger: Option<String>,
    pub precomputed: Option<bool>,
    pub pre_tokens: Option<i64>,
    pub post_tokens: Option<i64>,
    pub duration_ms: Option<i64>,
    #[serde(with = "crate::wire::opt_text")]
    pub tool: Option<Text>,
    pub tool_id: Option<Id>,
    pub settings: Settings,
    pub idle_attribution: IdleAttribution,
    /// Preceding metadata from this file, retained even before the selected window.
    pub pricing: Option<std::sync::Arc<Pricing>>,
    /// Claude Bash call details on its tool start and end, without the command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<ShellCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t3: Option<Box<T3Request>>,
}

/// Recorded settings values by key; most events carry none, so they hold no map.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings(Option<Box<IndexMap<String, Scalar>>>);

impl Settings {
    /// Settings holding `values`.
    pub fn new(values: IndexMap<String, Scalar>) -> Self {
        Self((!values.is_empty()).then(|| Box::new(values)))
    }

    /// The value recorded for `key`.
    pub fn get(&self, key: &str) -> Option<&Scalar> {
        self.0.as_ref()?.get(key)
    }

    /// Every recorded key and value, in recorded order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Scalar)> {
        self.0.iter().flat_map(|values| values.iter())
    }

    /// Whether no value is recorded.
    pub fn is_empty(&self) -> bool {
        self.0.is_none()
    }
}

impl FromIterator<(String, Scalar)> for Settings {
    fn from_iter<I: IntoIterator<Item = (String, Scalar)>>(items: I) -> Self {
        Self::new(items.into_iter().collect())
    }
}

impl From<IndexMap<String, Scalar>> for Settings {
    fn from(values: IndexMap<String, Scalar>) -> Self {
        Self::new(values)
    }
}

impl Serialize for Settings {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(self.iter())
    }
}

impl<'de> Deserialize<'de> for Settings {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        IndexMap::deserialize(deserializer).map(Settings::new)
    }
}

/// The profiles a record belongs to, shared by every record with the same set.
pub type Profiles = std::sync::Arc<[String]>;

/// Server-tool request counts by name, in recorded order. Nearly every request records the
/// same counts, so requests share one copy.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct ServerTools(std::sync::Arc<[(String, i64)]>);

impl ServerTools {
    /// Each tool's name and count, in recorded order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, i64)> {
        self.0.iter().map(|(name, count)| (name.as_str(), *count))
    }
}

impl FromIterator<(String, i64)> for ServerTools {
    fn from_iter<I: IntoIterator<Item = (String, i64)>>(items: I) -> Self {
        Self(items.into_iter().collect())
    }
}

impl Serialize for ServerTools {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(self.iter())
    }
}

impl<'de> Deserialize<'de> for ServerTools {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        IndexMap::<String, i64>::deserialize(deserializer).map(|tools| tools.into_iter().collect())
    }
}

/// What a Claude Bash call asked for and how it ended, never its command or output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellCall {
    /// Started with `run_in_background`, so the model did not wait on it.
    pub background: bool,
    /// The command sleeps inside a shell loop, polling something.
    pub polls: bool,
    /// The result reports that the call reached its timeout and moved to the background.
    pub timed_out: bool,
}

/// A current window control and the setting that owns it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowControl {
    pub value: Option<i64>,
    pub owner: String,
    pub reason: String,
    pub applicable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimum_claude_patch: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comparison_unknown: Option<String>,
    /// No setting names a window, so Claude Code's tuned window for the model applies.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unset: bool,
}

/// Claude Code's window for one model when no setting names one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TunedWindow {
    /// The trigger in tokens, or None when neither a compaction nor the documented table establishes it.
    pub value: Option<i64>,
    pub reason: String,
}

/// A current TTL control for one bucket.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TtlControl {
    pub value: Option<Ttl>,
    pub owner: String,
    pub reason: String,
}

/// Window controls for the default and per-model overrides.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompactionControls {
    pub default: WindowControl,
    pub models: IndexMap<String, WindowControl>,
    /// `CLAUDE_CODE_DISABLE_1M_CONTEXT=1` holds every model at a 200k window.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub capped: bool,
    /// `CLAUDE_CODE_MAX_CONTEXT_TOKENS` may size a model ID Claude Code does not recognize.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub declared: bool,
    /// Tuned windows by undated model ID, for every model the profile used.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub tuned: IndexMap<String, TunedWindow>,
}

/// Current native idle controls, with ownership and unresolved scopes kept explicit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdleControl {
    pub value: Option<bool>,
    pub owner: String,
    pub threshold: Option<i64>,
    pub threshold_owner: String,
    pub auto_compact: Option<bool>,
    pub reason: String,
    pub resolved: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<PerBucket<TtlControl>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionControls>,
}

/// Current speed preference after known disabling controls, without inferred session overrides.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpeedControl {
    pub value: Option<String>,
    pub owner: String,
    pub replacement: String,
    pub resolved: bool,
    pub reason: String,
}

/// Claude's effective foreground Bash timeout ceiling and the variables that set it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashCapControl {
    /// The larger of `BASH_MAX_TIMEOUT_MS` (default 600000) and `BASH_DEFAULT_TIMEOUT_MS`.
    pub value: Millis,
    /// Where `BASH_MAX_TIMEOUT_MS` is set, or the settings file that would set it.
    pub owner: String,
    /// The effective `BASH_DEFAULT_TIMEOUT_MS` (default 120000).
    pub default_timeout: Millis,
    pub default_owner: String,
}

/// A selected profile's current settings and their owning controls.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub id: String,
    pub harness: Harness,
    pub root: String,
    pub aliases: Vec<String>,
    pub histories: Vec<String>,
    pub settings_file: String,
    pub ttl: PerBucket<TtlControl>,
    pub provider: Option<String>,
    pub settings: IndexMap<String, Scalar>,
    /// MCP server ids the settings file declares, which its own switches can turn off.
    pub mcp_servers: Vec<String>,
    pub compaction: CompactionControls,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle: Option<IdleControl>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<SpeedControl>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bash: Option<BashCapControl>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed_directories: Option<IndexMap<String, SpeedControl>>,
    /// Current project controls observed at recorded working directories.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_directories: Option<IndexMap<String, IdleControl>>,
    /// Background generation features and the controls that switch them.
    pub background: Vec<BackgroundControl>,
    /// Background controls at recorded working directories; an empty key means no directory.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub background_directories: IndexMap<String, Vec<BackgroundControl>>,
    /// Request-specific overrides when session transitions or launch precedence are unresolved.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub background_requests: IndexMap<String, Vec<BackgroundControl>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t3: Option<T3Instance>,
}

/// T3's three idle policies, independent of native Claude's interactive idle setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum T3IdleMode {
    #[serde(rename = "onReturn")]
    OnReturn,
    #[serde(rename = "beforeCacheExpires")]
    BeforeCacheExpires,
    #[serde(rename = "off")]
    Off,
}

impl T3IdleMode {
    /// The setting value accepted by T3.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OnReturn => "onReturn",
            Self::BeforeCacheExpires => "beforeCacheExpires",
            Self::Off => "off",
        }
    }
}

/// An instance's allowlisted current controls; disabled instances retain historical usage.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct T3Instance {
    pub state: String,
    pub instance: String,
    pub enabled: bool,
    pub idle_mode: T3IdleMode,
    pub idle_owner: String,
    pub ownership_resolved: bool,
    pub limitations: Vec<String>,
    /// The instance's environment when it inherits the native settings window, which
    /// discovery resolves at each recorded project like a native profile's.
    #[serde(skip)]
    pub inherited_window: Option<crate::controls::Env>,
}

/// Coverage of one selected T3 state, without aggregate usage or connection credentials.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct T3State {
    pub path: String,
    pub status: String,
    pub exclusions: Vec<String>,
    pub metadata: String,
    pub snapshot_started_at: Option<String>,
    pub snapshot_finished_at: Option<String>,
    pub joined_requests: u64,
    pub ambiguous_requests: u64,
    /// Requests whose T3 or native control owner stays unresolved, counted by cause.
    #[serde(default)]
    pub unresolved: IndexMap<String, u64>,
    /// The model T3 resolves for generated text; absent for a state that was not read.
    pub text_generation: Option<T3TextGeneration>,
}

/// A T3 model selection: the instance or text generator it routes to, and its model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct T3Selection {
    pub instance: String,
    pub model: String,
}

/// T3's current text-generation model; its cost is unmeasured because T3 keeps no native records.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct T3TextGeneration {
    pub selection: T3Selection,
    /// The settings key that changes it.
    pub owner: String,
    /// How the selection resolved: set, T3's default, or a fallback past a disabled selection.
    pub resolution: String,
    /// Narrower settings that currently replace it, keyed by setting name.
    pub overrides: IndexMap<String, T3Selection>,
    /// Projects whose own settings replace any of these selections.
    pub project_overrides: u64,
}

/// Historical projection evidence uniquely linked to a captured native request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct T3Request {
    pub thread: String,
    pub run: String,
    pub attempt: String,
    pub provider_turn: String,
    pub status: String,
    pub completed_at: Option<Millis>,
    pub usage_updated_at: Option<Millis>,
    pub used_tokens: Option<i64>,
    pub cache_expires_at: Option<Millis>,
    pub idle_mode: Option<T3IdleMode>,
    /// Current lifecycle guards, not proof that a lost in-memory timer fired.
    pub lifecycle_eligible: Option<bool>,
}

/// A model request the client makes on its own, outside the turns the user drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackgroundFeature {
    /// Claude's predicted next prompt after each response.
    PromptSuggestions,
    /// Claude's recap after the terminal sits unfocused.
    SessionRecaps,
    /// Claude's turns that check on background work an active goal waits for.
    GoalCheckIns,
    /// Codex's recap while the terminal is unfocused.
    AutoRecaps,
}

/// One background feature's current state and the control that owns it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundControl {
    pub feature: BackgroundFeature,
    /// Current file-based value; null when scope, trust or launch selection is unresolved.
    pub value: Option<bool>,
    /// What the current discovery establishes, without claiming historical state.
    pub reason: String,
    /// The goal check-in interval, when it applies.
    pub minutes: Option<i64>,
    /// Where the effective value comes from.
    pub owner: String,
    /// The setting that turns it off.
    pub off: String,
    /// The file to set it in; null for an environment variable.
    pub off_file: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RootState {
    Selected,
    Excluded,
    Missing,
    Inaccessible,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RootOrigin {
    Default,
    Environment,
    Discovered,
    Explicit,
}

/// Discovery includes exclusions and failures so selection cannot imply full-host coverage.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RootStatus {
    pub harness: Harness,
    pub path: String,
    pub status: RootState,
    pub origin: RootOrigin,
}

/// File identity and byte boundary frozen before any transcript is parsed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub file: String,
    pub harness: Harness,
    pub profiles: Vec<String>,
    pub bytes: u64,
    pub device: u64,
    pub inode: u64,
    pub deferred_bytes: u64,
    /// Complete lines that were not valid JSON objects, such as truncated or interleaved writes.
    pub skipped_lines: u64,
    pub lines: u64,
}

/// A price may be partially known without pretending the known subtotal is the total.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Cost {
    pub known_usd: f64,
    pub total_usd: Option<f64>,
    pub unknown: Vec<String>,
    pub assumptions: Vec<String>,
}

/// An observed boundary with either linked usage or one explicit summary estimate.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionCharge {
    pub event: Id,
    pub request: Option<Text>,
    pub evidence: Evidence,
    pub cost: Cost,
    pub ttl_cost: PerTtl<Cost>,
    pub assumptions: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EstimateEvidence {
    Estimated,
    Unknown,
}

/// A comparison-only input estimate, with original measured coverage left untouched.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageEstimate {
    pub request: Text,
    pub evidence: EstimateEvidence,
    pub basis: Vec<Text>,
    pub part: Option<BillingPart>,
    pub max_output_tokens: Option<i64>,
    pub reason: &'static str,
    pub assumptions: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum KnownState {
    Known,
    Estimated,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PrefixEvidence {
    Measured,
    Estimated,
    Unknown,
}

/// Cache-history witnesses distinguish measured prefixes from estimates and unresolved cohorts.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CachePredecessor {
    pub request: Text,
    pub part: usize,
    pub time: Millis,
    pub state: KnownState,
    pub prefix_evidence: PrefixEvidence,
    pub cohort_known: bool,
    pub reason: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReplayOutcome {
    Invariant,
    Estimated,
    Unknown,
}

/// Evidence for one selected request's counterfactual cache geometry, independent of prices.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CacheReplayEvidence {
    pub request: Text,
    pub part: usize,
    pub ttl: Ttl,
    pub state: KnownState,
    pub predecessors: Vec<CachePredecessor>,
    pub outcome: ReplayOutcome,
    pub reason: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Change,
    Keep,
    Advisory,
}

/// Request parts priced in both compared scenarios.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Coverage {
    pub priced: u64,
    pub total: u64,
}

/// Shared recommendation metadata; savings are relative to the named baseline.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckBase<V> {
    pub profile: String,
    pub history_profiles: Vec<String>,
    pub evidence: Evidence,
    pub action: Action,
    pub current: Option<V>,
    pub recommended: Option<V>,
    pub owner: String,
    pub since: Option<Millis>,
    pub until: Option<Millis>,
    pub window_basis: String,
    pub baseline: String,
    pub requests: u64,
    /// Request parts priced in both compared scenarios; null when the check is not a per-part replay.
    pub coverage: Option<Coverage>,
    pub savings_usd: Option<f64>,
    pub monthly_savings_usd: Option<f64>,
    pub break_even: Option<String>,
    pub assumptions: Vec<String>,
    pub uncertainty: Vec<String>,
    pub sources: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WindowBasisKind {
    Explicit,
    TtlTransition,
    Available,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnlinkedSensitivity {
    pub no_billed_attempt: PerTtl<Cost>,
    pub cold_cache: PerTtl<Cost>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TtlSensitivity {
    pub cold_prefix1h_usd: Option<f64>,
    pub cold_prefix1h_comparison_usd: Option<f64>,
    pub warm_reuse_break_even_fraction: Option<f64>,
    pub unlinked: Option<UnlinkedSensitivity>,
}

/// A TTL comparison preserves cache and fixed costs separately.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TtlCheck {
    #[serde(flatten)]
    pub base: CheckBase<Ttl>,
    pub window_basis_kind: WindowBasisKind,
    pub bucket: Bucket,
    /// How much less 1h costs than 5m on the parts both replays price; negative when 5m is cheaper.
    pub one_hour_savings_usd: f64,
    pub cost: PerTtl<Cost>,
    pub summary_cost: PerTtl<Cost>,
    /// Cache charges used to compare TTLs; ordinary input/output are fixed.
    pub comparison_cost: PerTtl<Cost>,
    /// Ordinary input, output, server fees and proxies held identical in both scenarios.
    pub fixed_cost: Cost,
    pub usage_estimates: Vec<UsageEstimate>,
    pub cache_evidence: Vec<CacheReplayEvidence>,
    pub sensitivity: TtlSensitivity,
}

/// An observed compaction event and the request that resumed after it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CompactionBasis {
    pub event: Id,
    pub request: Text,
}

/// One extra modeled compaction and the observations supporting its geometry.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AddedCompaction {
    pub request: Text,
    pub input_tokens: i64,
    pub summary_tokens: i64,
    pub prefix_tokens: i64,
    pub basis: Vec<CompactionBasis>,
    pub summary_cost: Cost,
    pub prefix_cost: Cost,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowSensitivity {
    pub larger_summary_savings_usd: Option<f64>,
    pub cold_summary_savings_usd: Option<f64>,
    pub reread_break_even_usd: Option<f64>,
}

/// One candidate window, net of added summaries and subsequent prefix charges.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowScenario {
    pub window: i64,
    pub cost: Cost,
    pub baseline_comparison_cost: Cost,
    pub comparison_cost: Cost,
    pub savings_usd: Option<f64>,
    /// Changed request parts priced under both windows.
    pub coverage: Coverage,
    pub added: Vec<AddedCompaction>,
    pub affected_requests: u64,
    pub cache_evidence: Vec<CacheReplayEvidence>,
    pub sensitivity: WindowSensitivity,
}

/// A compaction check runs after the TTL choices and retains observed boundaries.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionCheck {
    #[serde(flatten)]
    pub base: CheckBase<i64>,
    /// Always "all".
    pub bucket: &'static str,
    pub model: Option<Text>,
    pub models: Vec<Option<Text>>,
    /// No setting names the window, so `current` is Claude Code's tuned window for the model.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub unset: bool,
    pub baseline_cost: Cost,
    pub observed_summary_cost: Cost,
    pub ttls: PerBucket<Option<Ttl>>,
    pub scenarios: Vec<WindowScenario>,
    pub usage_estimates: Vec<UsageEstimate>,
    pub cache_evidence: Vec<CacheReplayEvidence>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservedPremium {
    pub premium_parts: u64,
    pub standard_parts: u64,
    pub unknown_parts: u64,
    pub premium: Cost,
    pub monthly_premium_usd: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentPremium {
    pub premium: Cost,
    pub priced_parts: u64,
    pub unknown_parts: u64,
}

/// Rate-only premium comparisons keep current-setting estimates separate from observed response premiums.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeedCheck {
    #[serde(flatten)]
    pub base: CheckBase<String>,
    /// Always "all".
    pub bucket: &'static str,
    pub replacement: String,
    pub reason: String,
    pub observed: ObservedPremium,
    pub current_estimate: CurrentPremium,
    pub usage_estimates: Vec<UsageEstimate>,
}

/// What a background feature cost, estimated from the turns where it would have run.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundCheck {
    /// Current and recommended values: true is on.
    #[serde(flatten)]
    pub base: CheckBase<bool>,
    pub feature: BackgroundFeature,
    /// Recorded directories whose requests this control applies to; null means absent.
    pub directories: Vec<Option<Text>>,
    /// Current ownership and any unresolved scope or selection.
    pub reason: String,
    /// The setting that turns it off.
    pub off: String,
    /// The file to set it in; null for an environment variable.
    pub off_file: Option<String>,
    /// Turns where it would have run; null when they cannot be found in transcripts.
    pub generations: Option<u64>,
    /// Estimated cost of its requests over the window.
    pub cost: Cost,
    /// Later input costs its cache reads avoided, including recaps and cache rewrites.
    pub lost_warming: Cost,
}

/// Foreground Bash calls that held the cache idle, and what their misses cost.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BashWaits {
    /// Foreground Bash calls with a recorded result.
    pub calls: u64,
    /// Those whose wait reached the cache TTL their request wrote.
    pub long: u64,
    /// Long waits that reached their timeout.
    pub timed_out: u64,
    /// Long waits that slept in a polling loop.
    pub polling: u64,
    /// Rewriting the prefix the next request could have read, priced at write minus read rates.
    pub miss_cost: Cost,
}

/// Whether the foreground Bash timeout cap outlasts the cache TTL.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BashCapCheck {
    #[serde(flatten)]
    pub base: CheckBase<Millis>,
    /// Always "all".
    pub bucket: &'static str,
    /// The main-agent TTL the cap is compared with; None when unresolved.
    pub ttl: Option<Ttl>,
    pub ttl_basis: String,
    pub replacement: String,
    pub waits: BashWaits,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PeriodStatus {
    Eligible,
    Ineligible,
    Unknown,
    Censored,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReturnTiming {
    BeforeExpiry,
    AfterExpiry,
    NearExpiry,
    Final,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PeriodOutcome {
    Applied,
    NotApplied,
    Unobserved,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeriodCalibration {
    pub events: Vec<Id>,
    pub summary_tokens: i64,
    pub prefix_tokens: i64,
}

/// One request gap or final observation period, without an assumed session end.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdlePeriod {
    pub request: Text,
    pub resume: Option<Text>,
    pub session: Text,
    pub context_tokens: Option<i64>,
    pub gap_ms: i64,
    pub status: PeriodStatus,
    pub reason: &'static str,
    pub confirmed: bool,
    pub return_timing: ReturnTiming,
    pub outcome: PeriodOutcome,
    pub event: Option<Id>,
    pub version: Option<Text>,
    pub client: Option<Text>,
    pub flag_state: Option<&'static str>,
    pub activation: Option<f64>,
    pub activation_basis: std::borrow::Cow<'static, str>,
    pub summary_cost: Cost,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calibration: Option<PeriodCalibration>,
}

/// A value for idle compaction on and off.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct OnOff<T> {
    pub on: T,
    pub off: T,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ScenarioWindow {
    pub owner: String,
    pub models: Vec<Option<Text>>,
    pub value: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextDelta {
    pub tokens: Option<i64>,
    pub periods: u64,
    pub delta_usd: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivationCohort {
    pub version: Text,
    pub client: Text,
    pub flag_state: &'static str,
    pub applied: u64,
    pub eligible: u64,
    pub fraction: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdleSensitivity {
    pub closed_delta_usd: Option<f64>,
    pub open_delta_usd: Option<f64>,
    pub larger_summary_delta_usd: Option<f64>,
    pub late_trigger_delta_usd: Option<f64>,
    pub lifetime_break_even_fraction: Option<f64>,
    pub reread_break_even_usd_per_resumed_summary: Option<f64>,
}

/// An aggregate idle policy comparison at one named TTL/window configuration.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdleScenario {
    pub baseline: String,
    pub applicable: bool,
    pub ttls: PerBucket<Option<Ttl>>,
    pub windows: Vec<ScenarioWindow>,
    pub periods: Vec<IdlePeriod>,
    pub cost: OnOff<Cost>,
    /// Only changed charges, relative to the shared recorded-request baseline.
    pub adjustment: OnOff<Cost>,
    pub delta_usd: Option<f64>,
    pub monthly_delta_usd: Option<f64>,
    pub by_context: Vec<ContextDelta>,
    pub activation_cohorts: Vec<ActivationCohort>,
    pub sensitivity: IdleSensitivity,
    pub unknown: Vec<String>,
}

/// Idle on/off runs after the TTL and window decisions, with a current-policy comparison.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdleCheck {
    #[serde(flatten)]
    pub base: CheckBase<bool>,
    /// Always "main".
    pub bucket: &'static str,
    pub threshold: Option<i64>,
    pub current_settings: IdleScenario,
    pub after_recommendations: IdleScenario,
    pub usage_estimates: Vec<UsageEstimate>,
}

/// The changed cost of one T3 idle mode at a named TTL/window baseline.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct T3IdleScenario {
    pub mode: T3IdleMode,
    pub cost: Cost,
    pub summaries: u64,
    pub final_summaries: u64,
}

/// One T3 comparison preserves the conditional native TTL/window baseline.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct T3IdleComparison {
    pub baseline: String,
    pub ttl: Option<Ttl>,
    pub scenarios: Vec<T3IdleScenario>,
    /// Shared eligibility; summary cost and calibration describe beforeCacheExpires only.
    pub periods: Vec<IdlePeriod>,
}

/// T3 idle modes use observed expiry and lifecycle evidence, independently of native interactive eligibility.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct T3IdleCheck {
    #[serde(flatten)]
    pub base: CheckBase<T3IdleMode>,
    pub current_settings: T3IdleComparison,
    pub after_recommendations: T3IdleComparison,
}

/// Pinging every interval for up to one cap, against leaving sessions idle.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeepaliveScenario {
    /// How long after the last request pings continue.
    pub cap_ms: Millis,
    /// Pings in gaps that ended with a return, and in final periods that never did.
    pub pings: u64,
    pub final_pings: u64,
    /// Returns that would have read a warm cache instead of rewriting it.
    pub rescued: u64,
    pub ping_cost_usd: f64,
    /// Recorded-work savings, including later context and modeled compactions, before ping costs.
    pub avoided_usd: f64,
    /// Avoided minus ping cost, relative to leaving sessions idle.
    pub savings_usd: f64,
    /// The same, if every final period closed right after its last request.
    pub closed_savings_usd: f64,
}

/// Capped cache keepalive pings, against leaving sessions idle and idle compaction.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeepaliveCheck {
    #[serde(flatten)]
    pub base: CheckBase<String>,
    /// Always "main".
    pub bucket: &'static str,
    /// The cache lifetime pings must beat; null when unknown.
    pub lifetime_ms: Option<Millis>,
    pub interval_ms: Option<Millis>,
    /// Median incremental prompt input and recorded reply output per ping.
    pub ping_input_tokens: Option<i64>,
    pub ping_output_tokens: Option<i64>,
    /// Gaps longer than the interval that ended with a return.
    pub returns: u64,
    /// Sessions' last requests, after which pings run the full cap.
    pub finals: u64,
    /// Periods in sessions whose complete policy comparison could not be priced.
    pub unpriced: u64,
    /// Idle compaction's estimated saving where recommended; null when unknown or unsupported.
    pub idle_compaction_savings_usd: Option<f64>,
    /// The best cap's saving minus the best alternative's; negative when pinging loses.
    pub margin_usd: Option<f64>,
    /// One-ping and recorded rescue-threshold caps, shortest first.
    pub scenarios: Vec<KeepaliveScenario>,
}

/// Setting-specific evidence accompanies one common recommendation model.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "check")]
pub enum CheckResult {
    #[serde(rename = "claude-cache-ttl")]
    Ttl(TtlCheck),
    #[serde(rename = "compaction-window")]
    Compaction(CompactionCheck),
    #[serde(rename = "idle-compaction")]
    Idle(IdleCheck),
    #[serde(rename = "t3-idle-compaction")]
    T3Idle(T3IdleCheck),
    #[serde(rename = "fast-mode")]
    Speed(SpeedCheck),
    #[serde(rename = "background-generation")]
    Background(BackgroundCheck),
    #[serde(rename = "bash-timeout-cap")]
    BashCap(BashCapCheck),
    #[serde(rename = "keepalive")]
    Keepalive(KeepaliveCheck),
}

impl CheckResult {
    /// The recommendation shared by every check.
    pub fn action(&self) -> Action {
        match self {
            CheckResult::Ttl(c) => c.base.action,
            CheckResult::Compaction(c) => c.base.action,
            CheckResult::Idle(c) => c.base.action,
            CheckResult::T3Idle(c) => c.base.action,
            CheckResult::Speed(c) => c.base.action,
            CheckResult::Background(c) => c.base.action,
            CheckResult::BashCap(c) => c.base.action,
            CheckResult::Keepalive(c) => c.base.action,
        }
    }
}

// ---------------------------------------------------------------------------
// Exposure: recurring context components and how they were used.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Category {
    ToolSchema,
    SkillListing,
    SkillBody,
    McpDiscovery,
    McpSchema,
    McpResult,
    Instructions,
    Agents,
    Hooks,
    PluginInstructions,
    AppInstructions,
    ManagedInstructions,
    Other,
}

impl Category {
    /// The serialized name, such as "skill-listing".
    pub fn as_str(self) -> &'static str {
        match self {
            Category::ToolSchema => "tool-schema",
            Category::SkillListing => "skill-listing",
            Category::SkillBody => "skill-body",
            Category::McpDiscovery => "mcp-discovery",
            Category::McpSchema => "mcp-schema",
            Category::McpResult => "mcp-result",
            Category::Instructions => "instructions",
            Category::Agents => "agents",
            Category::Hooks => "hooks",
            Category::PluginInstructions => "plugin-instructions",
            Category::AppInstructions => "app-instructions",
            Category::ManagedInstructions => "managed-instructions",
            Category::Other => "other",
        }
    }
}

/// Measured size of one component.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Size {
    pub characters: Option<i64>,
    pub recorded_tokens: Option<i64>,
    pub fingerprint: Id,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Owner {
    Skills,
    Context,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Component,
    Aggregate,
}

/// One revision of one historical component, with no retained content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Component {
    #[serde(flatten)]
    pub size: Size,
    /// The revision, sharing the copy that indexes it.
    #[serde(with = "crate::wire::text")]
    pub id: Text,
    pub key: Id,
    /// Readable name: a tool, skill, server or file path, never content.
    pub name: String,
    pub category: Category,
    pub owner: Owner,
    pub group: Option<String>,
    /// One of a few fixed descriptions, shared across components.
    #[serde(with = "crate::wire::text")]
    pub provenance: Text,
    pub first_seen: Millis,
    pub current: Current,
    pub scope: Scope,
    pub members: Vec<String>,
    pub targets: Vec<String>,
    /// Absolute SKILL.md identity resolved from a rendered Codex catalog, without filesystem reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_path: Option<String>,
    /// Recorded instruction text size, without retaining its content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction_size: Option<InstructionSize>,
}

/// UTF-8 bytes and lines of recorded instruction text, including rendered wrappers.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct InstructionSize {
    pub bytes: usize,
    pub lines: usize,
}

/// Whether a historical component is still sent; scans never resolve it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Current {
    Unresolved,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InvocationKind {
    SkillRequest,
    SkillReference,
    SkillCall,
    SkillLoad,
    McpCall,
    McpResource,
    McpPrompt,
    ScriptReference,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InvocationStatus {
    Requested,
    Called,
    Completed,
    Failed,
    Reference,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Via {
    Typed,
    Mention,
    Wrapper,
    Tool,
    Completion,
    Script,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IdentityBasis {
    Recorded,
    ContentOccurrence,
}

/// Actual calls, requested skills and weak script references are different evidence.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Invocation {
    pub id: String,
    pub target: String,
    pub group: Option<String>,
    pub kind: InvocationKind,
    pub status: InvocationStatus,
    pub via: Via,
    pub session: String,
    pub sessions: Vec<String>,
    pub identity_basis: IdentityBasis,
    /// Opaque Codex user-turn boundary, recorded or inferred from native user-message aliases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<String>,
    pub harness: Harness,
    pub client: Option<String>,
    pub version: Option<String>,
    pub time: Millis,
    pub profiles: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Linkage {
    Wire,
    Chronological,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TextCoverage {
    CompleteText,
    Partial,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct References {
    pub base: Option<Id>,
    pub keep: Option<i64>,
    #[serde(with = "crate::wire::ids")]
    pub tail: Ids,
    pub items: Option<Ids>,
}

/// A shared list of identifiers that keeps a prefix of an earlier list instead of copying it.
///
/// Consecutive requests usually carry nearly the same components, and each request's
/// message digests keep every digest of the one before it, so flat lists would grow
/// quadratically with conversation length.
#[derive(Clone)]
pub struct Ids(std::sync::Arc<IdsNode>);

struct IdsNode {
    /// The earlier list whose first `keep` identifiers lead this one.
    base: Option<Ids>,
    keep: usize,
    tail: std::sync::Arc<[Text]>,
    len: usize,
    /// Nodes walked to read the whole list, kept at most `len` so reading stays linear.
    depth: usize,
    /// Hashes of the kept prefix and the whole list, independent of its segmentation.
    prefix_hash: u64,
    hash: u64,
}

fn append_hash(mut hash: u64, ids: &[Text]) -> u64 {
    use std::hash::{Hash, Hasher};
    for id in ids {
        let mut element = std::collections::hash_map::DefaultHasher::new();
        id.hash(&mut element);
        hash = hash
            .wrapping_mul(0x517c_c1b7_2722_0a95)
            .wrapping_add(element.finish());
    }
    hash
}

impl Ids {
    /// The first `keep` identifiers of `base` (which must hold that many), then `tail`.
    pub fn new(base: Option<(&Ids, usize)>, tail: std::sync::Arc<[Text]>) -> Self {
        let (mut base, keep) = match base {
            Some((b, keep)) if keep > 0 => (Some(b), keep),
            _ => (None, 0),
        };
        assert!(base.is_none_or(|b| keep <= b.len()), "id prefix too long");
        // Skip ancestors whose own tail this list does not keep.
        while let Some(b) = base.filter(|b| keep <= b.0.keep) {
            base = b.0.base.as_ref();
        }
        let len = keep + tail.len();
        let depth = base.map_or(0, |b| b.0.depth) + 1;
        if let Some(b) = base
            && depth > len
        {
            // A long chain of short tails: copy instead, which amortizes to one copy per
            // `len` lists.
            let tail: std::sync::Arc<[Text]> =
                b.iter().take(keep).chain(tail.iter()).cloned().collect();
            return Self::new(None, tail);
        }
        let prefix_hash = base.map_or(0, |base| {
            if keep == base.len() {
                base.0.hash
            } else {
                append_hash(base.0.prefix_hash, &base.0.tail[..keep - base.0.keep])
            }
        });
        let hash = append_hash(prefix_hash, &tail);
        Ids(std::sync::Arc::new(IdsNode {
            base: base.cloned(),
            keep,
            tail,
            len,
            depth,
            prefix_hash,
            hash,
        }))
    }
    /// `ids`, sharing the prefix it has in common with `like`.
    pub fn sharing(like: Option<&Ids>, ids: Vec<Text>) -> Self {
        let keep = like.map_or(0, |like| {
            like.iter().zip(&ids).take_while(|(a, b)| a == b).count()
        });
        let tail = ids[keep..].iter().cloned().collect();
        Self::new(like.map(|like| (like, keep)), tail)
    }
    pub fn len(&self) -> usize {
        self.0.len
    }
    pub fn is_empty(&self) -> bool {
        self.0.len == 0
    }
    /// The identifiers in order.
    pub fn iter(&self) -> impl Iterator<Item = &Text> {
        self.iter_from(0)
    }
    /// The identifiers from index `start` on, without walking the shared prefix before it.
    pub fn iter_from(&self, start: usize) -> impl Iterator<Item = &Text> {
        let mut segments = Vec::new();
        let mut node = &*self.0;
        let mut limit = node.len;
        while limit > start {
            let from = node.keep.max(start);
            if limit > from {
                segments.push(&node.tail[from - node.keep..limit - node.keep]);
            }
            limit = limit.min(node.keep);
            match &node.base {
                Some(b) if limit > start => node = &b.0,
                _ => break,
            }
        }
        segments.into_iter().rev().flatten()
    }
    /// The list whose first identifiers lead this one, and how many it keeps.
    pub fn base(&self) -> Option<(&Ids, usize)> {
        self.0.base.as_ref().map(|b| (b, self.0.keep))
    }
    /// The digests this list adds after the ones it keeps from its base.
    pub fn tail(&self) -> &[Text] {
        &self.0.tail
    }
    /// An identity for this shared list, stable while it is alive.
    pub fn address(&self) -> usize {
        std::sync::Arc::as_ptr(&self.0) as usize
    }
}

impl From<Vec<Text>> for Ids {
    fn from(ids: Vec<Text>) -> Self {
        Self::new(None, ids.into())
    }
}

impl FromIterator<Text> for Ids {
    fn from_iter<I: IntoIterator<Item = Text>>(iter: I) -> Self {
        Self::new(None, iter.into_iter().collect())
    }
}

impl Drop for IdsNode {
    // Release long chains iteratively instead of recursing once per list.
    fn drop(&mut self) {
        let mut next = self.base.take();
        while let Some(Ids(node)) = next {
            next = match std::sync::Arc::try_unwrap(node) {
                Ok(mut node) => node.base.take(),
                Err(_) => None,
            };
        }
    }
}

impl PartialEq for Ids {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
            || (self.len() == other.len() && self.iter().eq(other.iter()))
    }
}

impl Eq for Ids {}

impl std::hash::Hash for Ids {
    // Shared prefixes retain their content hash, so growing histories do not get
    // rehashed, and changing a middle component does not collide with every revision.
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.len().hash(state);
        self.0.hash.hash(state);
    }
}

impl std::fmt::Debug for Ids {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// A request-local view of historical state, including unresolved visibility.
///
/// Serialized by [`crate::wire`], which shares views and result totals once per SSH stream.
#[derive(Clone, Debug, PartialEq)]
pub struct Exposure {
    pub request: Text,
    /// Identical views are shared, since consecutive requests usually carry the same context.
    pub view: std::sync::Arc<ExposureView>,
    /// Tool output the conversation held when the request was sent.
    pub results: Option<std::sync::Arc<ResultTotals>>,
}

/// Characters of tool output a conversation holds, by tool, since its last compaction.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultTotals {
    /// Changes at every compaction and transcript, so equal epochs mean one unbroken conversation.
    pub epoch: u64,
    /// Each tool's total characters, and whether any of its output was an image or other non-text input.
    pub tools: Vec<(Text, i64, bool)>,
}

/// The request-independent part of an exposure.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExposureView {
    #[serde(with = "crate::wire::ids")]
    pub components: Ids,
    #[serde(with = "crate::wire::ids")]
    pub candidates: Ids,
    #[serde(with = "crate::wire::ids")]
    pub removed: Ids,
    #[serde(with = "crate::wire::ids")]
    pub inherited: Ids,
    #[serde(with = "crate::wire::ids")]
    pub deferred: Ids,
    pub linkage: Linkage,
    pub coverage: TextCoverage,
    pub references: Option<References>,
    pub complete_characters: Option<i64>,
    /// How many leading components are the recorded tools and system prompt, in cache-prefix order.
    pub prefix: usize,
    #[serde(with = "crate::wire::opt_text")]
    pub wire_model: Option<Text>,
    /// Views repeat a few fixed reasons, so they share one copy of each list.
    pub reasons: std::sync::Arc<[String]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visible {
    Partial,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Observed {
    Observed,
    Unknown,
}

/// Coverage is scoped to a concrete harness, client, version, component and window.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Visibility {
    pub harness: Harness,
    #[serde(with = "crate::wire::opt_text")]
    pub client: Option<Text>,
    #[serde(with = "crate::wire::opt_text")]
    pub version: Option<Text>,
    pub profiles: Profiles,
    #[serde(with = "crate::wire::opt_text")]
    pub component: Option<Text>,
    pub since: Millis,
    pub until: Millis,
    pub exposure: Visible,
    pub direct_calls: Observed,
    pub nested_calls: Visible,
    pub typed_skills: Observed,
    /// Always false.
    pub automatic_unused: bool,
    /// The same reasons for every row, so rows share one list.
    pub reasons: std::sync::Arc<[String]>,
}

/// How often one tool was called within the window, per profile set.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCalls {
    pub name: String,
    pub profiles: Vec<String>,
    pub calls: u64,
    pub last_used: Millis,
}

/// How much output one tool returned within the window, per profile set.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResults {
    pub name: String,
    pub profiles: Vec<String>,
    pub results: u64,
    /// Characters of the largest text result.
    pub largest_characters: i64,
}

/// Parser output shared by later skills and context checks.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExposureScan {
    pub components: Vec<Component>,
    pub requests: Vec<Exposure>,
    pub invocations: Vec<Invocation>,
    pub visibility: Vec<Visibility>,
    /// Selected-window calls to each tool, deduplicated by call id.
    pub tool_calls: Vec<ToolCalls>,
    /// Selected-window output of each tool, deduplicated by call id.
    pub tool_results: Vec<ToolResults>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CalibrationSample {
    pub request: Text,
    pub characters: i64,
    pub tokens: i64,
}

/// Character estimates are calibrated only within a matching model and client cohort.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Calibration {
    pub cohort: String,
    pub samples: Vec<CalibrationSample>,
    pub tokens_per_character: f64,
    pub range: (f64, f64),
    pub assumptions: Vec<String>,
}

// ---------------------------------------------------------------------------
// Context costs.

/// A settings.json value a control sets or appends.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(untagged)]
pub enum ControlValue {
    Text(String),
    Bool(bool),
    Skill { path: String, enabled: bool },
}

/// One settings.json change: set `path` to `value`, or add `value` to the list at `path`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct Control {
    pub path: Vec<String>,
    pub value: ControlValue,
    pub append: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContextKind {
    Tool,
    McpServer,
    McpTool,
    McpDiscovery,
    Skill,
    Instructions,
    System,
    /// A tool's output, carried in the conversation after the call that returned it.
    ToolOutput,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Off,
    Keep,
}

/// Observations for a Codex catalog member; counts are evidence events, not successful executions.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogSkill {
    pub path: Option<String>,
    pub requested: u64,
    pub loaded: u64,
    /// Successful skill execution is not observed in the supported Codex records.
    pub successful_invocations: Option<u64>,
    /// Name-only requests that could reach multiple catalog paths, excluded from uses.
    pub ambiguous_requests: u64,
    /// Same-named entries in the current catalog, including itself; one when not flagged.
    pub duplicate_entries: u64,
    /// Preferred copy to retain when the latest catalog has duplicates.
    pub retained_path: Option<String>,
}

/// What one piece of always-sent context cost over the window, and how to turn it off.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextCost {
    pub harness: Harness,
    pub profiles: Vec<String>,
    pub kind: ContextKind,
    pub name: String,
    /// The settings change that removes it, or null without a resolved control.
    pub control: Option<Control>,
    /// Selected requests that carried it.
    pub requests: u64,
    /// How many of those requests could be priced.
    pub priced: u64,
    /// Priced requests whose effective Codex tier is unrecorded.
    pub conditional_requests: u64,
    /// Requests with absent or conflicting Codex tiers, including unpriced requests.
    pub tier_unknown_requests: u64,
    /// Pricing assumptions, including unresolved tier sensitivity.
    pub assumptions: Vec<String>,
    /// Tokens it took in the most recent request that carried it.
    pub tokens: Option<i64>,
    /// API-price equivalent over the priced requests only.
    pub cost_usd: f64,
    /// costUsd scaled to every request that carried it; null when none could be priced.
    pub estimated_usd: Option<f64>,
    /// Observed uses in the window; null when uses are not observable.
    pub calls: Option<u64>,
    pub last_used: Option<Millis>,
    /// Whether the profile's most recent request still carried it.
    pub current: bool,
    /// estimatedUsd divided by uses, counting no use as one; null without both.
    pub cost_per_use_usd: Option<f64>,
    /// "off" above the cost-per-use limit; null when unpriced, unswitchable,
    /// no longer sent, tier-sensitive, or covered by a server-off verdict.
    pub verdict: Option<Verdict>,
    /// Characters of the largest single result, for tool output.
    pub largest_characters: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill: Option<CatalogSkill>,
    /// MCP tool and discovery costs already included in this server's total.
    pub details: Vec<ContextCost>,
    /// Size of the most recent recorded instruction revision.
    pub instruction_size: Option<InstructionSize>,
}

// ---------------------------------------------------------------------------
// Scan and report.

/// Pre-window Claude metadata supplies cache evidence, never selected billing or coverage.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CacheContext {
    pub requests: Vec<Request>,
    pub boundaries: Vec<StructuralEvent>,
    /// Earlier user turns, which count toward per-session thresholds.
    pub prompts: Vec<StructuralEvent>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanCoverage {
    pub requests: u64,
    pub source_known: u64,
    pub tier_known: u64,
    pub missing_usage: u64,
    pub deferred_files: u64,
    /// Unreadable complete lines skipped; their usage counts as missing.
    pub skipped_lines: u64,
    pub skipped_files: u64,
    pub issues: Vec<String>,
}

/// The reusable parser output for downstream checks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Scan {
    pub snapshot_started_at: String,
    pub snapshot_finished_at: String,
    /// Explicit billing selection bound, inherited by checks and never expanded.
    pub since: Option<Millis>,
    pub roots: Vec<RootStatus>,
    pub profiles: Vec<Profile>,
    pub snapshots: Vec<Snapshot>,
    pub requests: Vec<Request>,
    pub events: Vec<StructuralEvent>,
    /// Historical component counts and invocation evidence, without content.
    pub exposure: ExposureScan,
    pub cache_context: CacheContext,
    pub coverage: ScanCoverage,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub t3: Vec<T3State>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PriceSource {
    pub source: String,
    pub fetched_at: Option<String>,
    pub offline: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoricalCost {
    pub bucket: String,
    pub profiles: Vec<String>,
    pub requests: u64,
    /// Requests with a complete numeric API equivalent, including conditional estimates.
    pub priced_requests: u64,
    /// Priced requests that required a service-tier assumption.
    pub conditional_requests: u64,
    /// Alternative served tiers for missing-tier parts only; these are scenarios, not bounds.
    pub tier_sensitivity: Vec<TierScenario>,
    /// Always "estimated".
    pub evidence: &'static str,
    pub cost: Cost,
}

/// One hypothetical served tier, preserving every recorded effective tier.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TierScenario {
    pub tier: &'static str,
    pub cost: Cost,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ExposureSummary {
    pub calibration: Vec<Calibration>,
    pub assumptions: Vec<String>,
}

/// One host's analysis, which `--json` and the text report each present.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    /// Always 2.
    pub schema_version: u32,
    pub generated_at: String,
    pub scan: Scan,
    pub prices: PriceSource,
    pub historical: Vec<HistoricalCost>,
    pub compactions: Vec<CompactionCharge>,
    pub exposure: ExposureSummary,
    /// What each piece of always-sent context cost, most expensive first.
    pub context: Vec<ContextCost>,
    /// A piece that cost more than this per use is recommended off.
    pub max_cost_per_use_usd: f64,
    pub checks: Vec<CheckResult>,
    pub assumptions: Vec<String>,
}

#[cfg(test)]
mod ids_tests {
    use super::{Ids, Text};
    use crate::collections::HashSet;

    #[test]
    fn branches_keep_their_flat_values_after_ancestors_are_dropped() {
        let texts = |values: &[&str]| values.iter().copied().map(Text::from).collect::<Vec<_>>();
        let first = Ids::from(texts(&["a", "b", "c", "d"]));
        let second = Ids::new(Some((&first, 2)), texts(&["e", "f"]).into());
        let third = Ids::new(Some((&second, 3)), texts(&["g"]).into());
        let branch = Ids::new(Some((&third, 1)), texts(&["h"]).into());
        let last = Ids::sharing(Some(&branch), texts(&["a", "h", "i"]));
        drop((first, second, third, branch));
        let flat = texts(&["a", "h", "i"]);
        for start in 0..=flat.len() + 1 {
            assert_eq!(
                last.iter_from(start).cloned().collect::<Vec<_>>(),
                flat[start.min(flat.len())..]
            );
        }
        assert_eq!(
            serde_json::to_value(&last).unwrap(),
            serde_json::to_value(&flat).unwrap()
        );
        assert!(HashSet::from_iter([Ids::from(flat)]).contains(&last));
        assert_ne!(last, Ids::from(texts(&["a", "different", "i"])));
    }

    #[test]
    fn content_hashes_match_across_prefix_branches_and_distinguish_middle_revisions() {
        use std::hash::{Hash, Hasher};
        let hash = |ids: &Ids| {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            ids.hash(&mut hasher);
            hasher.finish()
        };
        let flat =
            |values: &[&str]| Ids::from(values.iter().map(|&s| Text::from(s)).collect::<Vec<_>>());
        let first = flat(&["a", "b", "c", "d"]);
        let branch = Ids::new(Some((&first, 2)), vec!["e".into(), "f".into()].into());
        let end = Ids::new(Some((&branch, 3)), vec!["g".into()].into());
        let same = flat(&["a", "b", "e", "g"]);
        assert_eq!(end, same);
        assert_eq!(hash(&end), hash(&same));
        assert_ne!(hash(&end), hash(&flat(&["a", "b", "different", "g"])));
        assert_eq!(hash(&Ids::new(Some((&end, 4)), vec![].into())), hash(&same));
    }

    #[test]
    fn long_growing_lists_serialize_and_drop_without_recursion() {
        let mut ids = Ids::from(vec![]);
        for n in 0..20_000 {
            ids = Ids::new(
                Some((&ids, ids.len())),
                vec![Text::from(n.to_string())].into(),
            );
        }
        assert_eq!(ids.len(), 20_000);
        let json = serde_json::to_value(&ids).unwrap();
        assert_eq!(json[0], "0");
        assert_eq!(json[19_999], "19999");
        drop(ids);
    }
}
