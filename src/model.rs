//! Shared data types. Types the SSH stream carries derive serde; their names are its wire names.

use crate::collections::{HashSet, IndexMap};
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

/// One shared copy of each distinct string and profile set the records of a scan repeat.
#[derive(Default)]
pub struct Interner {
    texts: crate::collections::HashSet<Text>,
    profiles: crate::collections::HashSet<Profiles>,
    tools: crate::collections::HashSet<ServerTools>,
    metas: crate::collections::HashSet<std::sync::Arc<RequestMeta>>,
    event_metas: crate::collections::HashSet<std::sync::Arc<EventMeta>>,
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

    /// Share every repeated string of a request, keeping its unique identifiers as they are,
    /// and the fields it records alike with other requests.
    pub fn request(&mut self, r: &mut Request) {
        let meta = std::sync::Arc::make_mut(&mut r.meta);
        meta.session = self.text(&meta.session);
        for slot in [
            &mut meta.model,
            &mut meta.provider,
            &mut meta.source,
            &mut meta.trigger,
            &mut meta.version,
            &mut meta.client,
            &mut meta.cwd,
            &mut meta.effort,
        ] {
            self.share(slot);
        }
        if let Some(speed) = &mut meta.requested_speed {
            self.share(&mut speed.tier);
            self.share(&mut speed.speed);
        }
        for issue in &mut meta.issues {
            *issue = self.text(issue);
        }
        meta.issues.shrink_to_fit();
        meta.profiles = self.profiles(&meta.profiles);
        if let Some(tools) = &mut meta.server_tools {
            match self.tools.get(tools) {
                Some(shared) => *tools = shared.clone(),
                None => {
                    self.tools.insert(tools.clone());
                }
            }
        }
        self.share(&mut r.turn);
        for p in &mut r.parts {
            for slot in [&mut p.model, &mut p.tier, &mut p.speed, &mut p.geo] {
                self.share(slot);
            }
        }
        self.meta(&mut r.meta);
        r.parts.shrink_to_fit();
        r.locations.shrink_to_fit();
    }

    /// Replace `meta` with the shared copy of its value; its strings must already be shared.
    pub fn meta(&mut self, meta: &mut std::sync::Arc<RequestMeta>) {
        match self.metas.get(&**meta) {
            Some(shared) => {
                if !std::sync::Arc::ptr_eq(shared, meta) {
                    *meta = shared.clone();
                }
            }
            None => {
                std::sync::Arc::make_mut(meta).issues.shrink_to_fit();
                self.metas.insert(meta.clone());
            }
        }
    }

    /// Share every repeated string of a structural event, and the fields it records alike
    /// with other events.
    pub fn event(&mut self, e: &mut StructuralEvent) {
        let meta = std::sync::Arc::make_mut(&mut e.meta);
        meta.session = self.text(&meta.session);
        for slot in [
            &mut meta.model,
            &mut meta.version,
            &mut meta.client,
            &mut meta.cwd,
            &mut meta.tool,
        ] {
            self.share(slot);
        }
        meta.profiles = self.profiles(&meta.profiles);
        for origin in &mut e.origins {
            origin.session = self.text(&origin.session);
        }
        e.origins.shrink_to_fit();
        self.event_meta(&mut e.meta);
    }

    /// Replace `meta` with the shared copy of its value; its strings must already be shared.
    pub fn event_meta(&mut self, meta: &mut std::sync::Arc<EventMeta>) {
        match self.event_metas.get(&**meta) {
            Some(shared) => {
                if !std::sync::Arc::ptr_eq(shared, meta) {
                    *meta = shared.clone();
                }
            }
            None => {
                self.event_metas.insert(meta.clone());
            }
        }
    }
}

/// The two locally supported transcript formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Harness {
    Claude,
    Codex,
}

/// A documented Claude cache lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
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
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PerTtl<T> {
    pub five_minutes: T,
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
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Evidence {
    Measured,
    Estimated,
    Guidance,
    Unknown,
}

/// A safe source location without transcript content.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Location {
    pub file: Text,
    pub line: u64,
}

/// Normalized counts: reads and writes are subsets of input, reasoning of output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PartKind {
    Message,
    Compaction,
    Advisor,
    Unknown,
}

/// One non-overlapping billed sampling iteration.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BillingPart {
    pub kind: PartKind,
    pub model: Option<Text>,
    pub usage: Usage,
    pub tier: Option<Text>,
    pub speed: Option<Text>,
    pub geo: Option<Text>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RequestEvidence {
    Measured,
    Estimated,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Main,
    Subagent,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Api {
    Responses,
    Messages,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Accounting {
    Individual,
    LegacyLast,
    LegacyDelta,
}

/// Recorded API-attempt budget; this does not establish execution or billing.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Attempt {
    pub max_output_tokens: Option<i64>,
}

/// Requested controls never establish the billed response tier.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct RequestedSpeed {
    pub tier: Option<Text>,
    pub speed: Option<Text>,
}

/// One deduplicated model request, with no message content.
///
/// A history holds a million requests, so each keeps only what differs per request and
/// shares the rest, such as its session, model and profiles, with every request recorded
/// alike. Shared fields read and write through `Deref`; a write copies them first.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub evidence: RequestEvidence,
    pub id: Text,
    pub time: Millis,
    pub end_time: Millis,
    pub reference: Option<Text>,
    pub turn: Option<Text>,
    pub parts: Vec<BillingPart>,
    pub locations: Few<Location>,
    /// Shared with every request the same T3 provider turn ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub t3: Option<std::sync::Arc<T3Request>>,
    #[serde(flatten)]
    pub meta: std::sync::Arc<RequestMeta>,
}

/// What requests recorded alike share: everything but their identity, time, usage and location.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestMeta {
    pub harness: Harness,
    pub session: Text,
    pub agent: Agent,
    pub model: Option<Text>,
    pub provider: Option<Text>,
    pub api: Api,
    pub source: Option<Text>,
    pub trigger: Option<Text>,
    pub version: Option<Text>,
    pub client: Option<Text>,
    pub cwd: Option<Text>,
    /// The recorded reasoning effort, such as `high`.
    pub effort: Option<Text>,
    pub server_tools: Option<ServerTools>,
    pub profiles: Profiles,
    pub accounting: Accounting,
    pub attempt: Option<Attempt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_speed: Option<RequestedSpeed>,
    pub issues: Vec<Text>,
}

impl std::ops::Deref for Request {
    type Target = RequestMeta;
    fn deref(&self) -> &RequestMeta {
        &self.meta
    }
}

impl std::ops::DerefMut for Request {
    /// The shared fields, copied first unless this request holds the only reference.
    fn deref_mut(&mut self) -> &mut RequestMeta {
        std::sync::Arc::make_mut(&mut self.meta)
    }
}

/// A safe location under one conversation alias.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Origin {
    pub session: Text,
    pub location: Location,
}

/// A short list, such as an event's origins or a request's locations. Nearly every one
/// holds exactly one item, which it keeps inline rather than in an allocation of its own.
#[derive(Clone, Debug, Default)]
pub enum Few<T> {
    #[default]
    None,
    One(T),
    Many(Vec<T>),
}

/// An event's origins.
pub type Origins = Few<Origin>;

impl<T> Few<T> {
    pub fn push(&mut self, item: T) {
        *self = match std::mem::take(self) {
            Few::None => Few::One(item),
            Few::One(first) => Few::Many(vec![first, item]),
            Few::Many(mut all) => {
                all.push(item);
                Few::Many(all)
            }
        };
    }

    /// Move every item of `other` to the end of this list.
    pub fn append(&mut self, other: &mut Self) {
        self.extend(std::mem::take(other));
    }

    /// Hold a single item inline, and release any spare capacity.
    pub fn shrink_to_fit(&mut self) {
        if let Few::Many(all) = self {
            match all.len() {
                0 => *self = Few::None,
                1 => *self = Few::One(all.pop().expect("one item")),
                _ => all.shrink_to_fit(),
            }
        }
    }
}

impl<T> std::ops::Deref for Few<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        match self {
            Few::None => &[],
            Few::One(item) => std::slice::from_ref(item),
            Few::Many(all) => all,
        }
    }
}

impl<T> std::ops::DerefMut for Few<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        match self {
            Few::None => &mut [],
            Few::One(item) => std::slice::from_mut(item),
            Few::Many(all) => all,
        }
    }
}

/// Equal when they list the same items, however they hold them.
impl<T: PartialEq> PartialEq for Few<T> {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl<T> From<Vec<T>> for Few<T> {
    fn from(all: Vec<T>) -> Self {
        let mut few = Few::Many(all);
        few.shrink_to_fit();
        few
    }
}

impl<T> FromIterator<T> for Few<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        iter.into_iter().collect::<Vec<_>>().into()
    }
}

impl<T> Extend<T> for Few<T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        for item in iter {
            self.push(item);
        }
    }
}

impl<T> IntoIterator for Few<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;
    fn into_iter(self) -> Self::IntoIter {
        match self {
            Few::None => vec![],
            Few::One(item) => vec![item],
            Few::Many(all) => all,
        }
        .into_iter()
    }
}

impl<'a, T> IntoIterator for &'a Few<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a, T> IntoIterator for &'a mut Few<T> {
    type Item = &'a mut T;
    type IntoIter = std::slice::IterMut<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

impl<T: Serialize> Serialize for Few<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        (**self).serialize(serializer)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum IdleAttribution {
    Unknown,
    Notice,
    T3,
}

/// A scalar settings value as recorded.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Scalar {
    Bool(bool),
    Number(f64),
    Text(String),
}

/// Preceding pricing metadata from the same file.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct Pricing {
    pub model: Option<Text>,
    pub provider: Option<Text>,
    pub tier: Option<Text>,
    pub speed: Option<Text>,
    pub geo: Option<Text>,
}

/// Structural context and timing evidence, never a billed boundary request.
///
/// Like a request, an event keeps only what differs per event and shares the rest with
/// every event recorded alike, through `Deref`; a write copies the shared fields first.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuralEvent {
    pub id: Id,
    /// All observed conversation aliases and safe locations for a copied event.
    pub origins: Origins,
    pub time: Millis,
    pub kind: EventKind,
    pub location: Location,
    pub request: Option<Text>,
    pub tool_id: Option<Id>,
    pub settings: Settings,
    pub idle_attribution: IdleAttribution,
    /// Claude Bash call details on its tool start and end, without the command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<ShellCall>,
    #[serde(flatten)]
    pub meta: std::sync::Arc<EventMeta>,
}

/// What events recorded alike share: their file's context and any rare per-kind details.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventMeta {
    pub harness: Harness,
    pub session: Text,
    pub profiles: Profiles,
    pub model: Option<Text>,
    pub version: Option<Text>,
    pub client: Option<Text>,
    pub cwd: Option<Text>,
    pub trigger: Option<String>,
    pub precomputed: Option<bool>,
    pub pre_tokens: Option<i64>,
    pub post_tokens: Option<i64>,
    pub duration_ms: Option<i64>,
    pub tool: Option<Text>,
    /// Preceding metadata from this file, retained even before the selected window.
    pub pricing: Option<std::sync::Arc<Pricing>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub t3: Option<std::sync::Arc<T3Request>>,
}

impl std::ops::Deref for StructuralEvent {
    type Target = EventMeta;
    fn deref(&self) -> &EventMeta {
        &self.meta
    }
}

impl std::ops::DerefMut for StructuralEvent {
    /// The shared fields, copied first unless this event holds the only reference.
    fn deref_mut(&mut self) -> &mut EventMeta {
        std::sync::Arc::make_mut(&mut self.meta)
    }
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

/// What a Claude Bash call asked for and how it ended, never its command or output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
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
#[derive(Clone, Debug, PartialEq, Serialize)]
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
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub unset: bool,
    /// The window a setting names when the model's context window caps it; `value` is the cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub setting: Option<i64>,
}

/// Claude Code's window for one model when no setting names one.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TunedWindow {
    /// The trigger in tokens, or None when neither a compaction nor the documented table establishes it.
    pub value: Option<i64>,
    pub reason: String,
}

/// A current TTL control for one bucket.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TtlControl {
    pub value: Option<Ttl>,
    pub owner: String,
    pub reason: String,
}

/// Window controls for the default and per-model overrides.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CompactionControls {
    pub default: WindowControl,
    pub models: IndexMap<String, WindowControl>,
    /// `CLAUDE_CODE_DISABLE_1M_CONTEXT=1` holds every model at a 200k window.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub capped: bool,
    /// The positive `CLAUDE_CODE_MAX_CONTEXT_TOKENS`, which may size a model ID Claude Code does not recognize.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub declared: Option<u64>,
    /// Tuned windows by undated model ID, for every model the profile used.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub tuned: IndexMap<String, TunedWindow>,
    /// Context windows by undated model ID, for each used model Claude Code holds below the 1M
    /// maximum; any window larger than its model's runs at this size.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub contexts: IndexMap<String, i64>,
}

/// Current native idle controls, with ownership and unresolved scopes kept explicit.
#[derive(Clone, Debug, PartialEq, Serialize)]
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
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SpeedControl {
    pub value: Option<String>,
    pub owner: String,
    pub replacement: String,
    pub resolved: bool,
    pub reason: String,
}

/// Claude's effective foreground Bash timeout ceiling and the variables that set it.
#[derive(Clone, Debug, PartialEq, Serialize)]
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
#[derive(Clone, Debug, PartialEq, Serialize)]
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
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub background_directories: IndexMap<String, Vec<BackgroundControl>>,
    /// Request-specific overrides when session transitions or launch precedence are unresolved,
    /// by request id; requests resolved alike share one list.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub background_requests: IndexMap<Text, std::sync::Arc<[BackgroundControl]>>,
    /// Auto memory's current setting.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemoryControl>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub t3: Option<T3Instance>,
    /// Current controls that choose subagent models and efforts.
    pub agents: AgentControls,
    /// The file that sets Codex's `project_doc_max_bytes` in `settings`, when one does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instruction_budget_file: Option<String>,
    /// Why Codex's `project_doc_max_bytes` may differ from the system and user value
    /// in `settings`, such as a project or named profile that sets it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instruction_budget_unresolved: Option<String>,
    /// Settings files that outrank `settings_file` for some recorded sessions, such as a
    /// project's, a managed one or a named Codex profile, so a switch they set overrides
    /// the same switch in `settings_file`. Only files that hold settings are listed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub outranking_files: Vec<String>,
}

/// A setting's current value and the file or variable that sets it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Owned {
    pub value: String,
    pub owner: String,
}

/// The current settings that choose subagent models and efforts, which history does not record.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentControls {
    /// Claude's `CLAUDE_CODE_SUBAGENT_MODEL` or Codex's `agents.default_subagent_model`.
    pub model: Option<Owned>,
    /// Claude's `CLAUDE_CODE_SUBAGENT_EFFORT` or Codex's `agents.default_subagent_reasoning_effort`.
    pub effort: Option<Owned>,
    /// Claude's `CLAUDE_CODE_SUBAGENT_MODEL_FORCE`, which drops call and definition models.
    pub force: bool,
    /// Claude's `CLAUDE_CODE_DISABLE_EXPLORE_INHERIT_CAP`, which lets Explore inherit any main model.
    pub explore_uncapped: bool,
    /// Codex's `review_model`.
    pub review: Option<Owned>,
    /// Codex's configured session `model`, which a review without `review_model` runs on.
    pub session: Option<Owned>,
    /// Agent definition files: Claude `agents/*.md`, Codex agent role files.
    pub definitions: Vec<AgentDefinition>,
    /// Codex roles a config layer declares with a description but no role file, by name, to
    /// the config file declaring them. They run on the default subagent model or the parent's.
    pub declared: IndexMap<String, String>,
    /// Why these controls may not be the ones a run used, such as a project of unknown trust.
    pub unresolved: Option<String>,
    /// The controls at each recorded subagent working directory whose project settings or
    /// agent files change them; other directories use these.
    pub directories: IndexMap<String, AgentControls>,
}

/// One agent definition file's model and effort, never its prompt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentDefinition {
    pub name: String,
    pub file: String,
    /// The project directory whose agents it defines; None for the profile's own agents.
    pub project: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// When the file last changed, so runs from before a change are not attributed to it.
    pub modified: Option<Millis>,
}

/// T3's three idle policies, independent of native Claude's interactive idle setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct T3Instance {
    pub state: String,
    pub instance: String,
    pub enabled: bool,
    pub idle_mode: T3IdleMode,
    pub idle_owner: String,
    pub ownership_resolved: bool,
    /// Whether launch arguments, a secret environment value or machine policy could
    /// override native settings in ways discovery does not resolve.
    pub settings_unresolved: bool,
    pub limitations: Vec<String>,
    /// The instance's environment when it inherits the native settings window, which
    /// discovery resolves at each recorded project like a native profile's.
    #[serde(skip)]
    pub inherited_window: Option<crate::controls::Env>,
}

/// Coverage of one selected T3 state, without aggregate usage or connection credentials.
#[derive(Clone, Debug, PartialEq, Serialize)]
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
    pub unresolved: IndexMap<String, u64>,
    /// Requests every T3 owner of which was removed from settings, counted by instance; billed but in no current profile's checks.
    pub removed: IndexMap<String, u64>,
    /// Every instance the settings name, which tells a removed owner from one not selected.
    #[serde(skip)]
    pub(crate) instances: HashSet<String>,
    /// The model T3 resolves for generated text; absent for a state that was not read.
    pub text_generation: Option<T3TextGeneration>,
}

/// A T3 model selection: the instance or text generator it routes to, and its model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct T3Selection {
    pub instance: String,
    pub model: String,
}

/// T3's current text-generation model; its cost is unmeasured because T3 keeps no native records.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
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
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct T3Request {
    pub thread: Id,
    pub run: Id,
    pub attempt: Id,
    pub provider_turn: Id,
    pub status: &'static str,
    pub completed_at: Option<Millis>,
    pub usage_updated_at: Option<Millis>,
    pub used_tokens: Option<i64>,
    pub cache_expires_at: Option<Millis>,
    pub idle_mode: Option<T3IdleMode>,
    /// Current lifecycle guards, not proof that a lost in-memory timer fired.
    pub lifecycle_eligible: Option<bool>,
}

/// A model request the client makes on its own, outside the turns the user drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
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

/// Whether auto memory is on in current settings, and the setting that turns it off.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryControl {
    pub value: bool,
    /// Where the effective value comes from.
    pub owner: String,
    /// The setting that turns it off.
    pub off: String,
    /// The file to set it in; null for an environment variable.
    pub off_file: Option<String>,
    /// The working directory it was resolved at, the latest session's; null when
    /// only profile-wide settings were read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
    /// Why a setting that may apply could not be resolved, such as an unrecorded
    /// project trust or profile selection; null when it resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unresolved: Option<String>,
    /// Files whose memory settings may override `offFile` but could not be resolved.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub overrides: Vec<String>,
    /// Whether Codex's memory store exists, so memories were generated at some point.
    pub stored: bool,
}

/// One background feature's current state and the control that owns it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RootState {
    Selected,
    Excluded,
    Missing,
    Inaccessible,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RootOrigin {
    Default,
    Environment,
    Discovered,
    Explicit,
}

/// Discovery includes exclusions and failures so selection cannot imply full-host coverage.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RootStatus {
    pub harness: Harness,
    pub path: String,
    pub status: RootState,
    pub origin: RootOrigin,
}

/// File identity and byte boundary frozen before any transcript is parsed.
#[derive(Clone, Debug, PartialEq, Serialize)]
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

/// One subagent transcript's type and what its spawn requested, never its task.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Subagent {
    pub harness: Harness,
    pub session: Text,
    /// The conversation that spawned it, when recorded.
    pub parent: Option<Text>,
    /// Claude's agent type; Codex's agent role (`default` when unnamed) or other subagent source.
    pub kind: String,
    /// Whether Claude recorded it as built in; None when unrecorded.
    pub built_in: Option<bool>,
    /// A Claude fork, which always runs on its parent's model.
    pub fork: bool,
    /// A Codex subagent source such as `review` or `compact`, which no agent role defines,
    /// even when a role has the same name.
    pub native: bool,
    /// The model the spawning call requested, when recorded.
    pub model: Option<String>,
    /// The effort the spawning call requested, when recorded.
    pub effort: Option<String>,
}

/// Dollars as a whole number of 2^-64 dollar steps, so totals are exact and come out the
/// same whatever order their parts are added in, unlike f64 sums.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Usd(i128);

impl Usd {
    /// No dollars.
    pub const ZERO: Self = Self(0);
    /// Steps per dollar: a power of two, so converting scales an f64 exactly.
    const STEPS: f64 = 18_446_744_073_709_551_616.0;

    /// Round `usd` to the nearest step.
    ///
    /// Panics on a non-finite amount or one of at least 2^63 dollars either way, which no
    /// validated price produces.
    pub fn from_f64(usd: f64) -> Self {
        let steps = (usd * Self::STEPS).round();
        assert!(
            steps.is_finite() && steps.abs() < 2f64.powi(127),
            "dollar amount out of range: {usd}"
        );
        Self(steps as i128)
    }

    /// The nearest f64, for printing and comparing.
    pub fn to_f64(self) -> f64 {
        self.0 as f64 / Self::STEPS
    }

    /// `self` times `factor`, rounded to a step.
    ///
    /// Scaling does not distribute over addition, so scale each charge before summing, never
    /// a total, or the result depends on how the total was grouped.
    pub fn scale(self, factor: f64) -> Self {
        if factor == 1.0 {
            self
        } else {
            Self::from_f64(self.to_f64() * factor)
        }
    }

    /// The magnitude.
    pub fn abs(self) -> Self {
        Self(self.0.checked_abs().expect("dollar total overflow"))
    }

    /// `self` times `n`, saturating instead of overflowing.
    pub fn saturating_mul(self, n: u64) -> Self {
        Self(self.0.saturating_mul(i128::from(n)))
    }
}

// Overflow panics in release builds too, rather than wrapping into a wrong total.
impl std::ops::Add for Usd {
    type Output = Self;
    fn add(self, other: Self) -> Self {
        Self(self.0.checked_add(other.0).expect("dollar total overflow"))
    }
}

impl std::ops::Sub for Usd {
    type Output = Self;
    fn sub(self, other: Self) -> Self {
        Self(self.0.checked_sub(other.0).expect("dollar total overflow"))
    }
}

impl std::ops::Neg for Usd {
    type Output = Self;
    fn neg(self) -> Self {
        Self(self.0.checked_neg().expect("dollar total overflow"))
    }
}

impl std::ops::AddAssign for Usd {
    fn add_assign(&mut self, other: Self) {
        *self = *self + other;
    }
}

impl std::ops::SubAssign for Usd {
    fn sub_assign(&mut self, other: Self) {
        *self = *self - other;
    }
}

impl std::iter::Sum for Usd {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::ZERO, |a, b| a + b)
    }
}

impl Serialize for Usd {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_f64(self.to_f64())
    }
}

/// A price may be partially known without pretending the known subtotal is the total.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Cost {
    pub known_usd: Usd,
    pub total_usd: Option<Usd>,
    pub unknown: Vec<String>,
    pub assumptions: Vec<String>,
}

/// An observed boundary with either linked usage or one explicit summary estimate.
#[derive(Clone, Debug, PartialEq)]
pub struct CompactionCharge {
    pub event: Id,
    pub request: Option<Text>,
    pub evidence: Evidence,
    pub cost: Cost,
    pub ttl_cost: PerTtl<Cost>,
    pub assumptions: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EstimateEvidence {
    Estimated,
    Unknown,
}

/// A comparison-only input estimate, with original measured coverage left untouched.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageEstimate {
    pub request: Text,
    pub evidence: EstimateEvidence,
    pub basis: Vec<Text>,
    pub part: Option<BillingPart>,
    pub max_output_tokens: Option<i64>,
    pub reason: &'static str,
    pub assumptions: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KnownState {
    Known,
    Estimated,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PrefixEvidence {
    Measured,
    Estimated,
    Unknown,
}

/// Cache-history witnesses distinguish measured prefixes from estimates and unresolved cohorts.
#[derive(Clone, Debug, PartialEq)]
pub struct CachePredecessor {
    pub request: Text,
    pub part: usize,
    pub time: Millis,
    pub state: KnownState,
    pub prefix_evidence: PrefixEvidence,
    pub cohort_known: bool,
    pub reason: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ReplayOutcome {
    Invariant,
    Estimated,
    Unknown,
}

/// Evidence for one selected request's counterfactual cache geometry, independent of prices.
#[derive(Clone, Debug, PartialEq)]
pub struct CacheReplayEvidence {
    pub request: Text,
    pub part: usize,
    pub ttl: Ttl,
    pub state: KnownState,
    pub predecessors: Vec<CachePredecessor>,
    pub outcome: ReplayOutcome,
    pub reason: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    Change,
    Keep,
    Advisory,
}

/// Request parts priced in both compared scenarios.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Coverage {
    pub priced: u64,
    pub total: u64,
}

/// Shared recommendation metadata; savings are relative to the named baseline.
#[derive(Clone, Debug, PartialEq)]
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
    /// Request parts priced in both compared scenarios; None when the check is not a per-part replay.
    pub coverage: Option<Coverage>,
    pub savings_usd: Option<f64>,
    pub monthly_savings_usd: Option<f64>,
    pub break_even: Option<String>,
    pub assumptions: Vec<String>,
    pub uncertainty: Vec<String>,
    pub sources: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WindowBasisKind {
    Explicit,
    TtlTransition,
    Available,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UnlinkedSensitivity {
    pub no_billed_attempt: PerTtl<Cost>,
    pub cold_cache: PerTtl<Cost>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TtlSensitivity {
    pub cold_prefix1h_usd: Option<f64>,
    pub cold_prefix1h_comparison_usd: Option<f64>,
    pub warm_reuse_break_even_fraction: Option<f64>,
    pub unlinked: Option<UnlinkedSensitivity>,
}

/// A TTL comparison preserves cache and fixed costs separately.
#[derive(Clone, Debug, PartialEq)]
pub struct TtlCheck {
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
#[derive(Clone, Debug, PartialEq)]
pub struct CompactionBasis {
    pub event: Id,
    pub request: Text,
}

/// One extra modeled compaction and the observations supporting its geometry.
#[derive(Clone, Debug, PartialEq)]
pub struct AddedCompaction {
    pub request: Text,
    pub input_tokens: i64,
    pub summary_tokens: i64,
    pub prefix_tokens: i64,
    pub basis: Vec<CompactionBasis>,
    pub summary_cost: Cost,
    pub prefix_cost: Cost,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WindowSensitivity {
    pub larger_summary_savings_usd: Option<f64>,
    pub cold_summary_savings_usd: Option<f64>,
    pub reread_break_even_usd: Option<f64>,
}

/// One candidate window, net of added summaries and subsequent prefix charges.
#[derive(Clone, Debug, PartialEq)]
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
#[derive(Clone, Debug, PartialEq)]
pub struct CompactionCheck {
    pub base: CheckBase<i64>,
    /// Always "all".
    pub bucket: &'static str,
    pub model: Option<Text>,
    pub models: Vec<Option<Text>>,
    /// No setting names the window, so `current` is Claude Code's tuned window for the model.
    pub unset: bool,
    /// The window a setting names when the model's context window caps it; `current` is the cap.
    pub setting: Option<i64>,
    pub baseline_cost: Cost,
    pub observed_summary_cost: Cost,
    pub ttls: PerBucket<Option<Ttl>>,
    pub scenarios: Vec<WindowScenario>,
    pub usage_estimates: Vec<UsageEstimate>,
    pub cache_evidence: Vec<CacheReplayEvidence>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ObservedPremium {
    pub premium_parts: u64,
    pub standard_parts: u64,
    pub unknown_parts: u64,
    pub premium: Cost,
    pub monthly_premium_usd: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CurrentPremium {
    pub premium: Cost,
    pub priced_parts: u64,
    pub unknown_parts: u64,
}

/// Rate-only premium comparisons keep current-setting estimates separate from observed response premiums.
#[derive(Clone, Debug, PartialEq)]
pub struct SpeedCheck {
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
#[derive(Clone, Debug, PartialEq)]
pub struct BackgroundCheck {
    /// Current and recommended values: true is on.
    pub base: CheckBase<bool>,
    pub feature: BackgroundFeature,
    /// Recorded directories whose requests this control applies to; None means absent.
    pub directories: Vec<Option<Text>>,
    /// Current ownership and any unresolved scope or selection.
    pub reason: String,
    /// The setting that turns it off.
    pub off: String,
    /// The file to set it in; None for an environment variable.
    pub off_file: Option<String>,
    /// Turns where it would have run; None when they cannot be found in transcripts.
    pub generations: Option<u64>,
    /// Estimated cost of its requests over the window.
    pub cost: Cost,
    /// Later input costs its cache reads avoided, including recaps and cache rewrites.
    pub lost_warming: Cost,
}

/// Auto memory's cost over the window. It is advised off whatever it cost, since
/// notes the agent writes for itself are not reviewed like checked-in instructions.
#[derive(Clone, Debug, PartialEq)]
pub struct MemoryCheck {
    /// Current and recommended values: true is on.
    pub base: CheckBase<bool>,
    pub harness: Harness,
    /// The setting that turns it off.
    pub off: String,
    /// The file to set it in; None for an environment variable.
    pub off_file: Option<String>,
    /// Loaded memory and the memory files read back, priced like other context; None when unpriced.
    pub context_usd: Option<f64>,
    /// Memory writes at the output price, estimated from their characters; None when unpriced.
    pub writes_usd: Option<f64>,
    pub reads: u64,
    pub writes: u64,
    /// Why whether it is on could not be resolved, leaving the current value unknown.
    pub unresolved: Option<String>,
    /// Files whose memory settings may override `off_file`, to turn it off in too.
    pub overrides: Vec<String>,
    /// Whether Codex may have extracted or consolidated memories in the window, which
    /// is never measured, so the total is a lower bound.
    pub unmeasured_generation: bool,
}

/// Foreground Bash calls that held the cache idle, and what their misses cost.
#[derive(Clone, Debug, PartialEq)]
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
#[derive(Clone, Debug, PartialEq)]
pub struct BashCapCheck {
    pub base: CheckBase<Millis>,
    /// Always "all".
    pub bucket: &'static str,
    /// The main-agent TTL the cap is compared with; None when unresolved.
    pub ttl: Option<Ttl>,
    pub ttl_basis: String,
    pub replacement: String,
    pub waits: BashWaits,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PeriodStatus {
    Eligible,
    Ineligible,
    Unknown,
    Censored,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ReturnTiming {
    BeforeExpiry,
    AfterExpiry,
    NearExpiry,
    Final,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PeriodOutcome {
    Applied,
    NotApplied,
    Unobserved,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PeriodCalibration {
    /// The calibrating summaries' events, shared by every period one cohort calibrates.
    pub events: std::sync::Arc<[Id]>,
    pub summary_tokens: i64,
    pub prefix_tokens: i64,
}

/// One request gap or final observation period, without an assumed session end.
#[derive(Clone, Debug, PartialEq)]
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
    pub calibration: Option<PeriodCalibration>,
}

/// A value for idle compaction on and off.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OnOff<T> {
    pub on: T,
    pub off: T,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScenarioWindow {
    pub owner: String,
    pub models: Vec<Option<Text>>,
    pub value: Option<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ContextDelta {
    pub tokens: Option<i64>,
    pub periods: u64,
    pub delta_usd: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ActivationCohort {
    pub version: Text,
    pub client: Text,
    pub flag_state: &'static str,
    pub applied: u64,
    pub eligible: u64,
    pub fraction: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct IdleSensitivity {
    pub closed_delta_usd: Option<f64>,
    pub open_delta_usd: Option<f64>,
    pub larger_summary_delta_usd: Option<f64>,
    pub late_trigger_delta_usd: Option<f64>,
    pub lifetime_break_even_fraction: Option<f64>,
    pub reread_break_even_usd_per_resumed_summary: Option<f64>,
}

/// An aggregate idle policy comparison at one named TTL/window configuration.
#[derive(Clone, Debug, PartialEq)]
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
#[derive(Clone, Debug, PartialEq)]
pub struct IdleCheck {
    pub base: CheckBase<bool>,
    /// Always "main".
    pub bucket: &'static str,
    pub threshold: Option<i64>,
    pub current_settings: IdleScenario,
    pub after_recommendations: IdleScenario,
    pub usage_estimates: Vec<UsageEstimate>,
}

/// The changed cost of one T3 idle mode at a named TTL/window baseline.
#[derive(Clone, Debug, PartialEq)]
pub struct T3IdleScenario {
    pub mode: T3IdleMode,
    pub cost: Cost,
    pub summaries: u64,
    pub final_summaries: u64,
}

/// One T3 comparison preserves the conditional native TTL/window baseline.
#[derive(Clone, Debug, PartialEq)]
pub struct T3IdleComparison {
    pub baseline: String,
    pub ttl: Option<Ttl>,
    pub scenarios: Vec<T3IdleScenario>,
    /// Shared eligibility; summary cost and calibration describe beforeCacheExpires only.
    pub periods: Vec<IdlePeriod>,
}

/// T3 idle modes use observed expiry and lifecycle evidence, independently of native interactive eligibility.
#[derive(Clone, Debug, PartialEq)]
pub struct T3IdleCheck {
    pub base: CheckBase<T3IdleMode>,
    pub current_settings: T3IdleComparison,
    pub after_recommendations: T3IdleComparison,
}

/// Pinging every interval for up to one cap, against leaving sessions idle.
#[derive(Clone, Debug, PartialEq)]
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
#[derive(Clone, Debug, PartialEq)]
pub struct KeepaliveCheck {
    pub base: CheckBase<String>,
    /// Always "main".
    pub bucket: &'static str,
    /// The cache lifetime pings must beat; None when unknown.
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
    /// Idle compaction's estimated saving where recommended; None when unknown or unsupported.
    pub idle_compaction_savings_usd: Option<f64>,
    /// The best cap's saving minus the best alternative's; negative when pinging loses.
    pub margin_usd: Option<f64>,
    /// One-ping and recorded rescue-threshold caps, shortest first.
    pub scenarios: Vec<KeepaliveScenario>,
}

/// Setting-specific evidence accompanies one common recommendation model.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum CheckResult {
    Ttl(TtlCheck),
    Compaction(CompactionCheck),
    Idle(IdleCheck),
    T3Idle(T3IdleCheck),
    Speed(SpeedCheck),
    Background(BackgroundCheck),
    BashCap(BashCapCheck),
    Keepalive(KeepaliveCheck),
    Memory(MemoryCheck),
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
            CheckResult::Memory(c) => c.base.action,
        }
    }
}

// ---------------------------------------------------------------------------
// Exposure: recurring context components and how they were used.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
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
    /// Auto memory the harness loads on its own: Claude's MEMORY.md and system
    /// prompt section, or Codex's memory summary.
    Memory,
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
            Category::Memory => "memory",
            Category::Other => "other",
        }
    }
}

/// Measured size of one component.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Size {
    pub characters: Option<i64>,
    pub recorded_tokens: Option<i64>,
    pub fingerprint: Id,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Owner {
    Skills,
    Context,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Component,
    Aggregate,
}

/// One revision of one historical component, with no retained content.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Component {
    #[serde(flatten)]
    pub size: Size,
    /// The revision, sharing the copy that indexes it.
    pub id: Text,
    pub key: Id,
    /// Readable name: a tool, skill, server or file path, never content.
    pub name: String,
    pub category: Category,
    pub owner: Owner,
    pub group: Option<String>,
    /// One of a few fixed descriptions, shared across components.
    pub provenance: Text,
    pub first_seen: Millis,
    pub current: Current,
    pub scope: Scope,
    pub members: Vec<String>,
    pub targets: Vec<String>,
    /// Absolute SKILL.md identity resolved from a rendered Codex catalog, without filesystem reads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill_path: Option<String>,
    /// Recorded instruction text size, without retaining its content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instruction_size: Option<InstructionSize>,
}

/// UTF-8 bytes and lines of recorded instruction text, including rendered wrappers.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct InstructionSize {
    pub bytes: usize,
    pub lines: usize,
}

/// Whether a historical component is still sent; scans never resolve it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Current {
    Unresolved,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum InvocationStatus {
    Requested,
    Called,
    Completed,
    Failed,
    Reference,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Via {
    Typed,
    Mention,
    Wrapper,
    Tool,
    Completion,
    Script,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum IdentityBasis {
    Recorded,
    ContentOccurrence,
}

/// Actual calls, requested skills and weak script references are different evidence.
#[derive(Clone, Debug, PartialEq, Serialize)]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn: Option<String>,
    pub harness: Harness,
    pub client: Option<String>,
    pub version: Option<String>,
    pub time: Millis,
    pub profiles: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Linkage {
    Wire,
    Chronological,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TextCoverage {
    CompleteText,
    Partial,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct References {
    pub base: Option<Id>,
    pub keep: Option<i64>,
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

impl Serialize for Ids {
    /// A flat list, however its chain is shared in memory.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.iter())
    }
}

/// A request-local view of historical state, including unresolved visibility.
#[derive(Clone, Debug, PartialEq)]
pub struct Exposure {
    pub request: Text,
    /// Identical views are shared, since consecutive requests usually carry the same context.
    pub view: std::sync::Arc<ExposureView>,
    /// Tool output the conversation held when the request was sent.
    pub results: Option<std::sync::Arc<ResultTotals>>,
}

impl Serialize for Exposure {
    /// The request and its view's fields, without the tool-output totals.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Flat<'a> {
            request: &'a str,
            #[serde(flatten)]
            view: &'a ExposureView,
        }
        Flat {
            request: &self.request,
            view: &self.view,
        }
        .serialize(serializer)
    }
}

/// Characters of tool output a conversation holds, by tool, since its last compaction.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ResultTotals {
    /// Changes at every compaction and transcript, so equal epochs mean one unbroken conversation.
    pub epoch: u64,
    /// Each tool's total characters, and whether any of its output was an image or other non-text input.
    pub tools: Vec<(Text, i64, bool)>,
}

/// The request-independent part of an exposure.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExposureView {
    pub components: Ids,
    pub candidates: Ids,
    pub removed: Ids,
    pub inherited: Ids,
    pub deferred: Ids,
    pub linkage: Linkage,
    pub coverage: TextCoverage,
    pub references: Option<References>,
    pub complete_characters: Option<i64>,
    /// How many leading components are the recorded tools and system prompt, in cache-prefix order.
    pub prefix: usize,
    pub wire_model: Option<Text>,
    /// Views repeat a few fixed reasons, so they share one copy of each list.
    pub reasons: std::sync::Arc<[String]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Visible {
    Partial,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Observed {
    Observed,
    Unknown,
}

/// Coverage is scoped to a concrete harness, client, version, component and window.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Visibility {
    pub harness: Harness,
    pub client: Option<Text>,
    pub version: Option<Text>,
    pub profiles: Profiles,
    pub component: Option<Text>,
    pub since: Millis,
    pub until: Millis,
    pub exposure: Visible,
    pub direct_calls: Observed,
    pub nested_calls: Visible,
    pub typed_skills: Observed,
    /// The same reasons for every row, so rows share one list.
    pub reasons: std::sync::Arc<[String]>,
}

/// How often one tool was called within the window, per profile set.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCalls {
    pub name: String,
    pub profiles: Vec<String>,
    pub calls: u64,
    pub last_used: Millis,
}

/// How much output one tool returned within the window, per profile set.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResults {
    pub name: String,
    pub profiles: Vec<String>,
    pub results: u64,
    /// Characters of the largest text result.
    pub largest_characters: i64,
}

/// Parser output shared by later skills and context checks.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
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
    /// Selected-window reads and writes of auto memory, deduplicated by call id.
    pub memory: Vec<MemoryUse>,
}

/// Whether the agent read or wrote auto memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryAccess {
    Read,
    Write,
}

/// One read or write of auto memory by the agent.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryUse {
    pub harness: Harness,
    pub access: MemoryAccess,
    pub profiles: Vec<String>,
    pub time: Millis,
    /// Characters of the tool input that wrote it, which the model generated as output.
    pub characters: Option<i64>,
    /// The request whose reply made the call, when its record was found.
    pub request: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CalibrationSample {
    pub request: Text,
    pub characters: i64,
    pub tokens: i64,
}

/// Character estimates are calibrated only within a matching model and client cohort.
#[derive(Clone, Debug, PartialEq)]
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
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ControlValue {
    Text(String),
    Bool(bool),
    Skill { path: String, enabled: bool },
}

/// One settings.json change: set `path` to `value`, or add `value` to the list at `path`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Control {
    pub path: Vec<String>,
    pub value: ControlValue,
    pub append: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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
    /// Auto memory: its loaded context and the memory files the agent read.
    Memory,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Verdict {
    /// Never used across a window that would have recorded a use: a recommended change.
    Off,
    /// Worth considering off, an advisory: it cost more than the limit per use, or no
    /// use was recorded but the window cannot rule one out.
    Consider,
    Keep,
}

/// Observations for a Codex catalog member; counts are evidence events, not successful executions.
#[derive(Clone, Debug, Default, PartialEq)]
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
#[derive(Clone, Debug, PartialEq)]
pub struct ContextCost {
    pub harness: Harness,
    pub profiles: Vec<String>,
    pub kind: ContextKind,
    pub name: String,
    /// The settings change that removes it, or None without a resolved control.
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
    pub cost_usd: Usd,
    /// cost_usd scaled to every request that carried it; None when none could be priced.
    pub estimated_usd: Option<f64>,
    /// Observed uses in the window; None when uses are not observable.
    pub calls: Option<u64>,
    pub last_used: Option<Millis>,
    /// Whether the profile's most recent request still carried it.
    pub current: bool,
    /// estimated_usd divided by uses; None without a price or a recorded use.
    pub cost_per_use_usd: Option<f64>,
    /// "off" when never used, "consider" above the cost-per-use limit or when
    /// nonuse is unproven, else "keep"; None when unpriced, unswitchable, no
    /// longer sent, tier-sensitive, or covered by a server-off verdict.
    pub verdict: Option<Verdict>,
    /// Characters of the largest single result, for tool output.
    pub largest_characters: Option<i64>,
    pub skill: Option<CatalogSkill>,
    /// MCP tool and discovery costs already included in this server's total.
    pub details: Vec<ContextCost>,
    /// Size of the most recent recorded instruction revision.
    pub instruction_size: Option<InstructionSize>,
}

// ---------------------------------------------------------------------------
// Subagent costs.

/// Where a subagent run's model came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ModelSource {
    /// The spawning call's `model`.
    Call,
    /// The agent definition or role file, or a built-in agent's own definition.
    Definition,
    /// A setting for every subagent without a definition model.
    Setting,
    /// The parent conversation's model.
    Inherited,
    /// History and current settings cannot settle it.
    Unknown,
}

/// How many requests or runs share one value.
#[derive(Clone, Debug, PartialEq)]
pub struct Share {
    pub name: String,
    pub count: u64,
}

/// How many runs took their model from one source.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceShare {
    pub source: ModelSource,
    /// The setting, file or rule behind it, or why it is unknown.
    pub detail: String,
    pub runs: u64,
}

/// What one subagent type or agent role cost and ran on; evidence, never a recommendation.
#[derive(Clone, Debug, PartialEq)]
pub struct SubagentCost {
    pub harness: Harness,
    pub profiles: Vec<String>,
    /// Claude's agent type, or Codex's agent role or subagent source; `unrecorded` when no record names it.
    pub kind: String,
    /// Whether `kind` is a Codex subagent source, such as `review`, rather than an agent role.
    pub native: bool,
    /// Subagent transcripts of this type in the window.
    pub runs: u64,
    pub requests: u64,
    /// Requests with a complete price.
    pub priced: u64,
    /// The API-price equivalent of its recorded requests.
    pub cost: Cost,
    /// Requests by recorded model, most first.
    pub models: Vec<Share>,
    /// Requests by recorded effort, most first; `unrecorded` when none was recorded.
    pub efforts: Vec<Share>,
    /// Runs by where their model came from, most first.
    pub sources: Vec<SourceShare>,
    /// The provider's cheapest current model in the price catalog, when it has one.
    pub cheapest: Option<String>,
    /// The same recorded tokens priced on `cheapest`, assuming the same turns; None when
    /// every request already ran on it or no cheapest model is known.
    pub reprice: Option<Cost>,
    /// The settings that change its model.
    pub model_setting: String,
    /// The settings that change its effort; a lower effort changes output tokens, so its cost is unmeasured.
    pub effort_setting: String,
}

// ---------------------------------------------------------------------------
// Scan and report.

/// Pre-window metadata supplies cache and spawn evidence, never selected billing or coverage.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct CacheContext {
    pub requests: Vec<Request>,
    /// Pre-window Codex requests, without usage, that start a transcript or change its model,
    /// so a subagent's start and its parent's model at spawn are known. Claude's earlier
    /// requests are in `requests`.
    pub codex: Vec<Request>,
    pub boundaries: Vec<StructuralEvent>,
    /// Earlier user turns, which count toward per-session thresholds.
    pub prompts: Vec<StructuralEvent>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
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
#[derive(Clone, Debug, PartialEq, Serialize)]
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
    /// Each subagent transcript's type and spawn request, by session.
    pub subagents: Vec<Subagent>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub t3: Vec<T3State>,
    /// Native session identities, which only the local session export prints.
    #[serde(skip)]
    pub sessions: crate::sessions::Sessions,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PriceSource {
    pub source: String,
    pub fetched_at: Option<String>,
    pub offline: bool,
}

#[derive(Clone, Debug, PartialEq)]
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
    pub cost: Cost,
}

/// One hypothetical served tier, preserving every recorded effective tier.
#[derive(Clone, Debug, PartialEq)]
pub struct TierScenario {
    pub tier: &'static str,
    pub cost: Cost,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExposureSummary {
    pub calibration: Vec<Calibration>,
    pub assumptions: Vec<String>,
}

/// One host's analysis, which the text report presents.
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
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
    /// What each subagent type or agent role cost and ran on, as evidence only.
    pub subagents: Vec<SubagentCost>,
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

#[cfg(test)]
mod usd_tests {
    use super::Usd;

    fn most_negative() -> Usd {
        Usd::from_f64(-(2f64.powi(62))) - Usd::from_f64(2f64.powi(62))
    }

    #[test]
    #[should_panic(expected = "dollar total overflow")]
    fn magnitude_of_the_most_negative_total_panics() {
        most_negative().abs();
    }

    #[test]
    #[should_panic(expected = "dollar total overflow")]
    fn negating_the_most_negative_total_panics() {
        let _ = -most_negative();
    }
}
