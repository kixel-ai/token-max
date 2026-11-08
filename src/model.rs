//! Shared data types. Serialized names match JSON report schema version 1.

use indexmap::IndexMap;
use serde::Serialize;

/// Milliseconds since the Unix epoch.
pub type Millis = i64;

/// A shared immutable string, for identifiers repeated across many records.
pub type Text = std::sync::Arc<str>;

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
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
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
    pub model: Option<String>,
    pub usage: Usage,
    pub tier: Option<String>,
    pub speed: Option<String>,
    pub geo: Option<String>,
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
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Attempt {
    pub max_output_tokens: Option<i64>,
}

/// Requested controls never establish the billed response tier.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RequestedSpeed {
    pub tier: Option<String>,
    pub speed: Option<String>,
}

/// One deduplicated model request, with no message content.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub evidence: RequestEvidence,
    pub id: String,
    pub harness: Harness,
    pub session: String,
    pub agent: Agent,
    pub time: Millis,
    pub end_time: Millis,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub api: Api,
    pub source: Option<String>,
    pub trigger: Option<String>,
    pub reference: Option<String>,
    pub turn: Option<String>,
    pub version: Option<String>,
    pub client: Option<String>,
    pub cwd: Option<String>,
    pub parts: Vec<BillingPart>,
    pub server_tools: Option<IndexMap<String, i64>>,
    pub profiles: Vec<String>,
    pub locations: Vec<Location>,
    pub accounting: Accounting,
    pub attempt: Option<Attempt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_speed: Option<RequestedSpeed>,
    pub issues: Vec<String>,
}

/// A safe location under one conversation alias.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Origin {
    pub session: String,
    pub location: Location,
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
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Pricing {
    pub model: Option<String>,
    pub provider: Option<String>,
    pub tier: Option<String>,
    pub speed: Option<String>,
    pub geo: Option<String>,
}

/// Structural context and timing evidence, never a billed boundary request.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuralEvent {
    pub id: String,
    /// All observed conversation aliases and safe locations for a copied event.
    pub origins: Vec<Origin>,
    pub harness: Harness,
    pub session: String,
    pub time: Millis,
    pub kind: EventKind,
    pub profiles: Vec<String>,
    pub location: Location,
    pub request: Option<String>,
    pub model: Option<String>,
    pub version: Option<String>,
    pub client: Option<String>,
    pub cwd: Option<String>,
    pub trigger: Option<String>,
    pub precomputed: Option<bool>,
    pub pre_tokens: Option<i64>,
    pub post_tokens: Option<i64>,
    pub duration_ms: Option<i64>,
    pub tool: Option<String>,
    pub tool_id: Option<String>,
    pub settings: IndexMap<String, Scalar>,
    pub idle_attribution: IdleAttribution,
    /// Preceding metadata from this file, retained even before the selected window.
    pub pricing: Option<Pricing>,
}

/// A current window control, without inventing a tuned model default.
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
    pub compaction: CompactionControls,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle: Option<IdleControl>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<SpeedControl>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed_directories: Option<IndexMap<String, SpeedControl>>,
    /// Current project controls observed at recorded working directories.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_directories: Option<IndexMap<String, IdleControl>>,
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
    pub event: String,
    pub request: Option<String>,
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
    pub request: String,
    pub evidence: EstimateEvidence,
    pub basis: Vec<String>,
    pub part: Option<BillingPart>,
    pub max_output_tokens: Option<i64>,
    pub reason: String,
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
    pub request: String,
    pub part: usize,
    pub time: Millis,
    pub state: KnownState,
    pub prefix_evidence: PrefixEvidence,
    pub cohort_known: bool,
    pub reason: String,
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
    pub request: String,
    pub part: usize,
    pub ttl: Ttl,
    pub state: KnownState,
    pub predecessors: Vec<CachePredecessor>,
    pub outcome: ReplayOutcome,
    pub reason: String,
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
    pub event: String,
    pub request: String,
}

/// One extra modeled compaction and the observations supporting its geometry.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AddedCompaction {
    pub request: String,
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
    pub model: Option<String>,
    pub models: Vec<Option<String>>,
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
    pub events: Vec<String>,
    pub summary_tokens: f64,
    pub prefix_tokens: f64,
}

/// One request gap or final observation period, without an assumed session end.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdlePeriod {
    pub request: String,
    pub resume: Option<String>,
    pub session: String,
    pub context_tokens: Option<i64>,
    pub gap_ms: i64,
    pub status: PeriodStatus,
    pub reason: String,
    pub confirmed: bool,
    pub return_timing: ReturnTiming,
    pub outcome: PeriodOutcome,
    pub event: Option<String>,
    pub version: Option<String>,
    pub client: Option<String>,
    pub flag_state: Option<String>,
    pub activation: Option<f64>,
    pub activation_basis: String,
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
    pub models: Vec<Option<String>>,
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
    pub version: String,
    pub client: String,
    pub flag_state: String,
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
    #[serde(rename = "fast-mode")]
    Speed(SpeedCheck),
}

impl CheckResult {
    /// The recommendation shared by every check.
    pub fn action(&self) -> Action {
        match self {
            CheckResult::Ttl(c) => c.base.action,
            CheckResult::Compaction(c) => c.base.action,
            CheckResult::Idle(c) => c.base.action,
            CheckResult::Speed(c) => c.base.action,
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
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Size {
    pub characters: Option<i64>,
    pub recorded_tokens: Option<i64>,
    pub fingerprint: String,
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
    pub id: String,
    pub key: String,
    /// Readable name: a tool, skill, server or file path, never content.
    pub name: String,
    pub category: Category,
    pub owner: Owner,
    pub group: Option<String>,
    pub provenance: String,
    pub locations: Vec<Location>,
    pub first_seen: Millis,
    /// Always "unresolved".
    pub current: &'static str,
    pub scope: Scope,
    pub members: Vec<String>,
    pub targets: Vec<String>,
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
    pub harness: Harness,
    pub client: Option<String>,
    pub version: Option<String>,
    pub time: Millis,
    pub profiles: Vec<String>,
    pub locations: Vec<Location>,
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
    pub base: Option<String>,
    pub keep: Option<i64>,
    pub tail: Ids,
    pub items: Option<Digests>,
}

/// A shared list of identifiers; equal lists are usually one allocation.
pub type Ids = std::sync::Arc<[Text]>;

/// A request's message digests, sharing the prefix it keeps from its base request.
///
/// Each request usually keeps every digest of the one before it, so flat lists
/// would grow quadratically with conversation length.
#[derive(Clone)]
pub struct Digests(std::sync::Arc<DigestNode>);

struct DigestNode {
    /// The base request, whose first `keep` digests lead this list.
    base: Option<Digests>,
    keep: usize,
    tail: Ids,
    len: usize,
}

impl Digests {
    /// The first `keep` digests of `base` (which must hold that many), then `tail`.
    pub fn new(base: Option<(&Digests, usize)>, tail: Ids) -> Self {
        let (mut base, keep) = match base {
            Some((b, keep)) if keep > 0 => (Some(b), keep),
            _ => (None, 0),
        };
        assert!(
            base.is_none_or(|b| keep <= b.len()),
            "digest prefix too long"
        );
        // Skip ancestors whose own tail this list does not keep.
        while let Some(b) = base.filter(|b| keep <= b.0.keep) {
            base = b.0.base.as_ref();
        }
        Digests(std::sync::Arc::new(DigestNode {
            base: base.cloned(),
            keep,
            len: keep + tail.len(),
            tail,
        }))
    }
    pub fn len(&self) -> usize {
        self.0.len
    }
    pub fn is_empty(&self) -> bool {
        self.0.len == 0
    }
    /// The digests in order.
    pub fn iter(&self) -> impl Iterator<Item = &Text> {
        self.iter_from(0)
    }
    /// The digests from index `start` on, without walking the shared prefix before it.
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
    /// The list whose first digests lead this one, and how many it keeps.
    pub fn base(&self) -> Option<(&Digests, usize)> {
        self.0.base.as_ref().map(|b| (b, self.0.keep))
    }
    /// An identity for this shared list, stable while it is alive.
    pub fn address(&self) -> usize {
        std::sync::Arc::as_ptr(&self.0) as usize
    }
    fn last(&self) -> Option<&Text> {
        let mut node = &*self.0;
        let index = node.len.checked_sub(1)?;
        loop {
            if index >= node.keep {
                return node.tail.get(index - node.keep);
            }
            node = &node.base.as_ref()?.0;
        }
    }
}

impl Drop for DigestNode {
    // Release long chains iteratively instead of recursing once per request.
    fn drop(&mut self) {
        let mut next = self.base.take();
        while let Some(Digests(node)) = next {
            next = match std::sync::Arc::try_unwrap(node) {
                Ok(mut node) => node.base.take(),
                Err(_) => None,
            };
        }
    }
}

impl PartialEq for Digests {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
            || (self.len() == other.len() && self.iter().eq(other.iter()))
    }
}

impl Eq for Digests {}

impl std::hash::Hash for Digests {
    // Consistent with element-wise equality without walking the whole list.
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.len().hash(state);
        self.last().hash(state);
    }
}

impl std::fmt::Debug for Digests {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl Serialize for Digests {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.iter())
    }
}

/// A request-local view of historical state, including unresolved visibility.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Exposure {
    pub request: String,
    /// Identical views are shared, since consecutive requests usually carry the same context.
    #[serde(flatten)]
    pub view: std::sync::Arc<ExposureView>,
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
    pub wire_model: Option<String>,
    pub reasons: Vec<String>,
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
    pub client: Option<String>,
    pub version: Option<String>,
    pub profiles: Vec<String>,
    pub component: Option<String>,
    pub since: Millis,
    pub until: Millis,
    pub exposure: Visible,
    pub direct_calls: Observed,
    pub nested_calls: Visible,
    pub typed_skills: Observed,
    /// Always false.
    pub automatic_unused: bool,
    pub reasons: Vec<String>,
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
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CalibrationSample {
    pub request: String,
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
    Skill,
    Instructions,
    System,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Off,
    Keep,
}

/// What one piece of always-sent context cost over the window, and how to turn it off.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextCost {
    pub profiles: Vec<String>,
    pub kind: ContextKind,
    pub name: String,
    /// The settings change that removes it, or null when no setting does.
    pub control: Option<Control>,
    /// Selected requests that carried it.
    pub requests: u64,
    /// How many of those requests could be priced.
    pub priced: u64,
    /// Tokens it took in the most recent request that carried it.
    pub tokens: Option<f64>,
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
    /// "off" when it cost more per use than the limit; null when it cannot be switched off or was never priced.
    pub verdict: Option<Verdict>,
}

// ---------------------------------------------------------------------------
// Scan and report.

/// Pre-window Claude metadata supplies cache evidence, never selected billing or coverage.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct CacheContext {
    pub requests: Vec<Request>,
    pub boundaries: Vec<StructuralEvent>,
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
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PriceSource {
    pub source: String,
    pub fetched_at: Option<String>,
    pub offline: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct HistoricalCost {
    pub bucket: String,
    pub profiles: Vec<String>,
    pub requests: u64,
    /// Always "estimated".
    pub evidence: &'static str,
    pub cost: Cost,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ExposureSummary {
    pub calibration: Vec<Calibration>,
    pub assumptions: Vec<String>,
}

/// Stable versioned machine output, independent of terminal presentation.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    /// Always 1.
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
