//! Idle compaction checks: find idle request gaps, replay a summary at the estimated
//! near-expiry trigger with idle compaction on and off, and compare the aggregate cost.

use std::borrow::Cow;
use std::cell::{OnceCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use indexmap::map::Entry;
use indexmap::{IndexMap, IndexSet};
use serde_json::{Map, Number, Value};

use crate::cache::{CacheBoundaries, ttl_milliseconds};
use crate::compaction::{
    CompactionSample, CompactionSamples, WindowEvents, WindowOptions, WindowSetting,
    compaction_samples, replay_windows,
};
use crate::controls::{Env, SettingsScope, idle_controls, window_control};
use crate::discovery::locale_compare;
use crate::model::{
    Action, ActivationCohort, Agent, BillingPart, Bucket, CheckBase, CompactionCharge,
    CompactionCheck, ContextDelta, Cost, EventKind, Evidence, Harness, IdleAttribution, IdleCheck,
    IdleControl, IdlePeriod, IdleScenario, IdleSensitivity, Millis, OnOff, PartKind, PerBucket,
    PeriodCalibration, PeriodOutcome, PeriodStatus, Profile, Request, ReturnTiming, Scalar, Scan,
    ScenarioWindow, StructuralEvent, Ttl, TtlCheck, Usage, UsageEstimate, WindowControl,
};
use crate::prices::{
    Catalog, PRICE_SOURCES, add_costs, price_components, price_part, server_cost, unknown_cost,
    zero_cost,
};
use crate::scenarios::{ReplayOptions, ReplayTtls, replay_parts};

/// The one-hour cache lifetime.
const HOUR: Millis = 60 * 60_000;
/// The inspected native 2.1.316 implementation arms at 90% of the lifetime.
/// This is a timing estimate, not a documented timer setting or version guarantee.
const EARLY: Millis = HOUR / 10 * 9;
const SOURCES: [&str; 2] = [
    "https://code.claude.com/docs/en/settings-reference#idlecompaction (2026-11-07)",
    "https://code.claude.com/docs/en/prompt-caching#compacting-the-conversation (2026-11-07)",
];
const POLICY_CHANGE: &str = "effective project TTL/window policy changes within a conversation; its counterfactual is unresolved";

/// Inputs to the idle checks, which run after the TTL and window decisions.
pub struct IdleOptions<'a> {
    pub scan: &'a Scan,
    pub catalog: &'a Catalog,
    pub summaries: &'a [CompactionCharge],
    pub estimates: &'a IndexMap<String, UsageEstimate>,
    pub ttls: &'a [&'a TtlCheck],
    pub windows: &'a [&'a CompactionCheck],
    pub since: Option<Millis>,
    pub now: Millis,
}

/// The main and sub-agent TTLs a scenario replays.
type Ttls = PerBucket<Option<Ttl>>;

/// A request's idle-control scope: an index into the profile's project directories, or None
/// for the profile-wide control.
type Scope = Option<usize>;

/// Whether `version` is exactly `major.minor.patch` digits and at least v2.1.`patch`.
fn supported(version: Option<&str>, patch: i64) -> bool {
    let Some(version) = version else {
        return false;
    };
    let mut parts = version.split('.');
    let (Some(major), Some(minor), Some(micro), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let number = |text: &str| {
        (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
            .then(|| text.parse::<f64>().unwrap_or(f64::INFINITY))
    };
    let (Some(major), Some(minor), Some(micro)) = (number(major), number(minor), number(micro))
    else {
        return false;
    };
    major > 2.0 || (major == 2.0 && (minor > 1.0 || (minor == 1.0 && micro >= patch as f64)))
}

/// The middle value, rounding an even split up.
fn median(mut values: Vec<i64>) -> i64 {
    values.sort_unstable();
    let mid = values.len() / 2;
    if values.len() % 2 == 1 {
        values[mid]
    } else {
        ((values[mid - 1] + values[mid]) as f64 / 2.0).ceil() as i64
    }
}

/// A cost times `fraction`; a zero fraction is a known zero, dropping unknowns.
fn scale(cost: &Cost, fraction: f64) -> Cost {
    if fraction == 0.0 {
        return zero_cost();
    }
    Cost {
        known_usd: cost.known_usd * fraction,
        total_usd: cost.total_usd.map(|t| t * fraction),
        ..cost.clone()
    }
}

/// `after` minus `before`, keeping both sides' unknowns.
fn difference(after: &Cost, before: &Cost) -> Cost {
    if after == before {
        return zero_cost();
    }
    let assumptions: IndexSet<&String> = after
        .assumptions
        .iter()
        .chain(&before.assumptions)
        .collect();
    Cost {
        known_usd: after.known_usd - before.known_usd,
        total_usd: match (after.total_usd, before.total_usd) {
            (Some(a), Some(b)) => Some(a - b),
            _ => None,
        },
        unknown: after
            .unknown
            .iter()
            .chain(&before.unknown)
            .cloned()
            .collect(),
        assumptions: assumptions.into_iter().cloned().collect(),
    }
}

/// The full cache prefix: reads plus writes.
fn prefix(p: &BillingPart) -> Option<i64> {
    Some(p.usage.read? + p.usage.write?)
}

fn is_message(p: &BillingPart) -> bool {
    p.kind == PartKind::Message
}

fn is_conversation(p: &BillingPart) -> bool {
    matches!(p.kind, PartKind::Message | PartKind::Unknown)
}

/// JavaScript's `Math.sign`.
fn sign(value: f64) -> f64 {
    if value > 0.0 {
        1.0
    } else if value < 0.0 {
        -1.0
    } else {
        value
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Client {
    Supported,
    Unsupported,
    Unknown,
}

/// Whether a request ran in a client the confirmed interactive idle policy covers.
fn client(r: &Request) -> Client {
    let label = r.client.as_deref();
    if r.agent == Agent::Subagent
        || r.source.as_deref().is_some_and(|s| s.starts_with("agent:"))
        || label.is_some_and(|c| c.starts_with("sdk-"))
    {
        return Client::Unsupported;
    }
    // A CLI label also occurs on headless runs; the REPL source establishes interactivity.
    if label == Some("cli") && r.source.as_deref() == Some("repl_main_thread") {
        return Client::Supported;
    }
    if matches!(label, Some("claude-vscode" | "ssh-remote")) {
        return Client::Supported;
    }
    if label.is_some_and(|c| c.starts_with("remote"))
        || matches!(label, Some("mcp" | "claude-code-github-action"))
    {
        return Client::Unsupported;
    }
    // Desktop entrypoints alone do not distinguish local/SSH Code from hosted execution.
    Client::Unknown
}

/// Idle controls resolved from the profile's own settings, for profiles discovery did not resolve.
fn settings_control(profile: &Profile) -> IdleControl {
    let config: Map<String, Value> = profile
        .settings
        .iter()
        .map(|(key, value)| {
            let value = match value {
                Scalar::Bool(b) => Value::Bool(*b),
                Scalar::Number(n) => Number::from_f64(*n).map_or(Value::Null, Value::Number),
                Scalar::Text(t) => Value::String(t.clone()),
            };
            (key.clone(), value)
        })
        .collect();
    let scopes = [SettingsScope {
        config: &config,
        file: &profile.settings_file,
    }];
    idle_controls(&scopes, &Env::new()).unwrap_or_else(|error| panic!("{error}"))
}

/// One profile's idle controls by working directory, with window controls memoized per scope
/// and model because every request of every replay asks for them.
struct Scopes<'p> {
    profile: &'p Profile,
    fallback: OnceCell<IdleControl>,
    /// The profile without its per-directory controls, for scoped window overrides.
    bare: OnceCell<Profile>,
    windows: RefCell<HashMap<Scope, HashMap<String, Rc<WindowControl>>>>,
}

impl<'p> Scopes<'p> {
    fn new(profile: &'p Profile) -> Self {
        Self {
            profile,
            fallback: OnceCell::new(),
            bare: OnceCell::new(),
            windows: RefCell::new(HashMap::new()),
        }
    }

    fn scope(&self, r: &Request) -> Scope {
        let cwd = r.cwd.as_deref().filter(|c| !c.is_empty())?;
        self.profile.idle_directories.as_ref()?.get_index_of(cwd)
    }

    /// The project control at the recorded directory, else the profile's, else its settings'.
    fn control(&self, scope: Scope) -> &IdleControl {
        if let Some((_, c)) =
            scope.and_then(|i| self.profile.idle_directories.as_ref()?.get_index(i))
        {
            return c;
        }
        self.profile
            .idle
            .as_ref()
            .unwrap_or_else(|| self.fallback.get_or_init(|| settings_control(self.profile)))
    }

    /// The scenario TTLs where the profile owns them; a project-owned TTL keeps its value.
    fn ttls(&self, scope: Scope, ttls: &Ttls) -> Ttls {
        let profile = &self.profile.ttl;
        let current = self.control(scope).ttl.as_ref().unwrap_or(profile);
        PerBucket {
            main: if current.main.owner == profile.main.owner {
                ttls.main
            } else {
                current.main.value
            },
            subagent: if current.subagent.owner == profile.subagent.owner {
                ttls.subagent
            } else {
                current.subagent.value
            },
        }
    }

    /// The window control for `model` under the scope's compaction controls.
    fn window(&self, scope: Scope, model: Option<&str>) -> Rc<WindowControl> {
        // window_control treats a missing model as the empty string.
        let key = model.unwrap_or("");
        if let Some(w) = self.windows.borrow().get(&scope).and_then(|m| m.get(key)) {
            return w.clone();
        }
        let w = Rc::new(match &self.control(scope).compaction {
            Some(compaction) => {
                let bare = self.bare.get_or_init(|| Profile {
                    idle_directories: None,
                    speed_directories: None,
                    ..self.profile.clone()
                });
                let scoped = Profile {
                    compaction: compaction.clone(),
                    ..bare.clone()
                };
                window_control(&scoped, model)
            }
            None => window_control(self.profile, model),
        });
        self.windows
            .borrow_mut()
            .entry(scope)
            .or_default()
            .insert(key.to_string(), w.clone());
        w
    }
}

/// A selected replayed part, kept for the shared baseline cost.
struct Selected {
    request: String,
    compaction: bool,
    cost: Cost,
}

/// Requests after one TTL/window configuration, with their replayed parts.
struct Replay<'a> {
    errors: Vec<String>,
    /// Summary charges of added window compactions.
    added: Vec<Cost>,
    /// Modeled window compaction boundaries.
    modeled: Vec<StructuralEvent>,
    selected: Vec<Selected>,
    /// Every request in time order, with replayed parts.
    requests: Vec<Cow<'a, Request>>,
}

/// The cohort a calibration sample can stand in for; every field must match exactly.
type SampleKey<'a> = (
    &'a str,
    &'a str,
    &'a str,
    &'a str,
    Agent,
    Option<&'a str>,
    Option<&'a str>,
    Option<&'a str>,
);

/// Provider, version, client, model, agent, tier, speed and geography; None when the provider,
/// version, client or model is unrecorded, which matches no sample.
fn sample_key<'a>(r: &'a Request, p: &'a BillingPart) -> Option<SampleKey<'a>> {
    Some((
        r.provider.as_deref()?,
        r.version.as_deref()?,
        r.client.as_deref()?,
        p.model.as_deref()?,
        r.agent,
        p.tier.as_deref(),
        p.speed.as_deref(),
        p.geo.as_deref(),
    ))
}

/// Summary geometry from one cohort's samples.
struct Geometry {
    output: i64,
    prefix: i64,
    read: i64,
    overhead: i64,
}

/// One cohort's matching samples, with median and maximum geometry computed once.
struct Calibration<'a> {
    samples: Vec<&'a CompactionSample<'a>>,
    /// Whether every sample shares one summary pricing cohort.
    uniform: bool,
    median: Geometry,
    larger: Geometry,
}

/// One replay's requests and events, indexed by session and identity once for every owner
/// and run.
struct ReplayIndex<'r> {
    /// Non-sub-agent conversation requests per session, in first-seen order.
    conversations: IndexMap<&'r str, Vec<&'r Request>>,
    /// Events under each of their origin sessions, in time order.
    events: HashMap<&'r str, Vec<&'r StructuralEvent>>,
    requests: HashMap<&'r str, Vec<&'r Request>>,
    boundaries: HashMap<&'r str, Vec<&'r StructuralEvent>>,
    /// The last request with each identity.
    by_id: HashMap<&'r str, &'r Request>,
    /// Identities with an event at or after the window start.
    selected_events: HashSet<&'r str>,
    calibration: HashMap<SampleKey<'r>, Calibration<'r>>,
}

impl<'r> ReplayIndex<'r> {
    fn new(
        requests: &'r [Cow<'r, Request>],
        events: &[&'r StructuralEvent],
        samples: &'r CompactionSamples<'r>,
        since: Millis,
    ) -> Self {
        let mut index = Self {
            conversations: IndexMap::new(),
            events: HashMap::new(),
            requests: HashMap::new(),
            boundaries: HashMap::new(),
            by_id: HashMap::new(),
            selected_events: HashSet::new(),
            calibration: HashMap::new(),
        };
        for r in requests.iter().map(AsRef::as_ref) {
            if r.agent != Agent::Subagent && r.parts.iter().any(is_conversation) {
                index
                    .conversations
                    .entry(r.session.as_str())
                    .or_default()
                    .push(r);
            }
            index
                .requests
                .entry(r.session.as_str())
                .or_default()
                .push(r);
            index.by_id.insert(r.id.as_str(), r);
        }
        // The last event with each identity, like a JavaScript Map built from the array.
        let mut by_id: HashMap<&str, &StructuralEvent> = HashMap::new();
        for &e in events {
            by_id.insert(e.id.as_str(), e);
            if e.time >= since {
                index.selected_events.insert(e.id.as_str());
            }
            for (i, origin) in e.origins.iter().enumerate() {
                let session = origin.session.as_str();
                if e.origins[..i].iter().any(|o| o.session == session) {
                    continue;
                }
                index.events.entry(session).or_default().push(e);
                if e.kind == EventKind::Compaction {
                    index.boundaries.entry(session).or_default().push(e);
                }
            }
        }
        let mut groups: HashMap<SampleKey, Vec<&CompactionSample>> = HashMap::new();
        for s in &samples.samples {
            if let Some(key) = sample_key(s.request, s.p) {
                groups.entry(key).or_default().push(s);
            }
        }
        for (key, matches) in groups {
            let summaries: HashSet<_> = matches
                .iter()
                .map(|s| {
                    (
                        s.summary_request.provider.as_deref(),
                        s.summary.model.as_deref(),
                        s.summary.tier.as_deref(),
                        s.summary.speed.as_deref(),
                        s.summary.geo.as_deref(),
                    )
                })
                .collect();
            let column = |f: &dyn Fn(&CompactionSample) -> i64| -> Vec<i64> {
                matches.iter().map(|s| f(s)).collect()
            };
            let outputs = column(&|s| s.output);
            let prefixes = column(&|s| s.prefix);
            let reads = column(&|s| s.p.usage.read.unwrap_or(0));
            let overheads = column(&|s| {
                let input = s.summary.usage.input;
                let before = by_id
                    .get(s.event)
                    .and_then(|e| e.pre_tokens)
                    .or(input)
                    .unwrap_or(0);
                (input.unwrap_or(0) - before).max(0)
            });
            let max = |values: &[i64]| values.iter().copied().max().expect("samples");
            let larger = Geometry {
                output: max(&outputs),
                prefix: max(&prefixes),
                read: max(&reads),
                overhead: max(&overheads),
            };
            let median = Geometry {
                output: median(outputs),
                prefix: median(prefixes),
                read: median(reads),
                overhead: median(overheads),
            };
            index.calibration.insert(
                key,
                Calibration {
                    samples: matches,
                    uniform: summaries.len() == 1,
                    median,
                    larger,
                },
            );
        }
        index
    }

    fn session<'s, T>(map: &'s HashMap<&str, Vec<T>>, session: &str) -> &'s [T] {
        map.get(session).map_or(&[], Vec::as_slice)
    }
}

/// An idle period with the context its replay needs.
struct Prepared<'r> {
    previous: &'r Request,
    next: Option<&'r Request>,
    boundary: Option<&'r StructuralEvent>,
    known_on: bool,
    window: Option<i64>,
    threshold: Option<i64>,
    ttls: Ttls,
}

/// How a run activates unobserved summaries and sizes them.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Point,
    Closed,
    Open,
    Larger,
    Late,
    Fraction(f64),
}

/// Context shared by every carried state, apart from its probability weight.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Path<'r> {
    offset: i64,
    prefix: Option<i64>,
    /// None before the first request, which no lifetime reaches.
    time: Option<Millis>,
    lifetime: Millis,
    retained: i64,
    /// The period whose summary changed this path, as a canonical period index.
    active: Option<usize>,
    input: Option<i64>,
    cohort: Option<ContextCohort<'r>>,
}

/// Provider, model, agent, speed and geography of the context a path carries.
type ContextCohort<'r> = (
    Option<&'r str>,
    Option<&'r str>,
    Agent,
    Option<&'r str>,
    Option<&'r str>,
);

#[derive(Clone, Copy, Debug)]
struct State<'r> {
    weight: f64,
    path: Path<'r>,
}

/// Whether a prefix written at `time` with `lifetime` is still warm at `now`.
fn warm(now: Millis, time: Option<Millis>, lifetime: Millis) -> bool {
    time.is_some_and(|t| now - t < lifetime)
}

/// Combine states with the same path, keeping first-seen order and dropping zero weights.
fn merge(states: Vec<State>) -> Vec<State> {
    let mut grouped: IndexMap<Path, State> = IndexMap::with_capacity(states.len());
    for state in states {
        if state.weight == 0.0 {
            continue;
        }
        match grouped.entry(state.path) {
            Entry::Occupied(mut prior) => prior.get_mut().weight += state.weight,
            Entry::Vacant(slot) => {
                slot.insert(state);
            }
        }
    }
    grouped.into_values().collect()
}

/// A part with the carried context offset, or None when its geometry cannot be established.
fn adjusted<'p>(
    p: &'p BillingPart,
    state: &State,
    r: &Request,
    ttl: Option<Ttl>,
) -> Option<Cow<'p, BillingPart>> {
    let s = &state.path;
    if s.offset == 0 {
        return Some(Cow::Borrowed(p));
    }
    let size = prefix(p)?;
    let input = p.usage.input?;
    let ttl = ttl?;
    if size + s.offset < 0 || input + s.offset < 0 {
        return None;
    }
    let remaining = size + s.offset;
    let warm = match s.prefix {
        Some(prefix) if warm(r.time, s.time, s.lifetime) => remaining.min(prefix),
        _ => 0,
    };
    // Actual resumed reads can establish external warming of the unchanged prefix.
    let read = warm.max(p.usage.read.unwrap_or(0).min(s.retained).min(remaining));
    let write = remaining - read;
    Some(Cow::Owned(BillingPart {
        usage: Usage {
            input: Some(input + s.offset),
            read: Some(read),
            write: Some(write),
            write5m: Some(if ttl == Ttl::FiveMinutes { write } else { 0 }),
            write1h: Some(if ttl == Ttl::OneHour { write } else { 0 }),
            ..p.usage
        },
        ..p.clone()
    }))
}

/// The price change from `before` to `after`; output is repriced only when its rate changes.
fn changed_cost(
    catalog: &Catalog,
    r: &Request,
    before: &BillingPart,
    after: Option<&BillingPart>,
) -> Cost {
    let Some(after) = after else {
        return unknown_cost("counterfactual retained/replacement prefix cannot be established");
    };
    if std::ptr::eq(before, after) || before == after {
        return zero_cost();
    }
    let a = price_components(catalog, r, after);
    let b = price_components(catalog, r, before);
    let output_unit = |p: &BillingPart| {
        let unit = BillingPart {
            usage: Usage {
                output: Some(1),
                ..p.usage
            },
            ..p.clone()
        };
        price_components(catalog, r, &unit).output
    };
    let output = if difference(&output_unit(after), &output_unit(before)).total_usd == Some(0.0) {
        zero_cost()
    } else {
        difference(&a.output, &b.output)
    };
    add_costs([
        &difference(&a.cache, &b.cache),
        &difference(&a.ordinary, &b.ordinary),
        &output,
    ])
}

/// A running `add_costs`, so a run's charges need not be kept.
#[derive(Clone, Debug)]
struct Sum {
    known: f64,
    total: Option<f64>,
    unknown: IndexSet<String>,
    assumptions: IndexSet<String>,
}

impl Sum {
    fn new() -> Self {
        Self {
            known: 0.0,
            total: Some(0.0),
            unknown: IndexSet::new(),
            assumptions: IndexSet::new(),
        }
    }

    fn add(&mut self, cost: &Cost) {
        self.add_scaled(cost, 1.0);
    }

    /// Add `scale(cost, weight)` without building it.
    fn add_scaled(&mut self, cost: &Cost, weight: f64) {
        if weight == 0.0 {
            self.known += 0.0;
            self.total = self.total.map(|t| t + 0.0);
            return;
        }
        self.known += if weight == 1.0 {
            cost.known_usd
        } else {
            cost.known_usd * weight
        };
        self.total = match (self.total, cost.total_usd) {
            (Some(sum), Some(value)) => {
                Some(sum + if weight == 1.0 { value } else { value * weight })
            }
            _ => None,
        };
        for reason in &cost.unknown {
            if !self.unknown.contains(reason.as_str()) {
                self.unknown.insert(reason.clone());
            }
        }
        for assumption in &cost.assumptions {
            if !self.assumptions.contains(assumption.as_str()) {
                self.assumptions.insert(assumption.clone());
            }
        }
    }

    fn cost(self) -> Cost {
        Cost {
            known_usd: self.known,
            total_usd: self.total,
            unknown: self.unknown.into_iter().collect(),
            assumptions: self.assumptions.into_iter().collect(),
        }
    }
}

/// A run's charges in total and per period.
struct Ledger {
    total: Sum,
    by_period: Vec<Option<Sum>>,
}

impl Ledger {
    fn charge(&mut self, period: Option<usize>, cost: &Cost, weight: f64) {
        self.total.add_scaled(cost, weight);
        if let Some(period) = period {
            self.by_period[period]
                .get_or_insert_with(Sum::new)
                .add_scaled(cost, weight);
        }
    }
}

/// One run's changed charges relative to the shared baseline.
struct Run {
    adjustment: Cost,
    /// Charges per canonical period index.
    by_period: Vec<Option<Cost>>,
    resumed: f64,
}

impl Run {
    fn period(&self, canonical: usize) -> Cow<'_, Cost> {
        self.by_period[canonical]
            .as_ref()
            .map_or_else(|| Cow::Owned(zero_cost()), Cow::Borrowed)
    }
}

/// One event in a session's replay, in time order.
enum Tick<'r> {
    Request {
        r: &'r Request,
        ttls: Ttls,
    },
    Boundary {
        e: &'r StructuralEvent,
        idle: Option<usize>,
        ttls: Ttls,
        summary: Option<&'r CompactionCharge>,
        linked: Option<&'r Request>,
    },
    Idle(usize),
}

/// A total replay order for a tick at `position` in the list of requests, then boundaries,
/// then idle triggers: time, then boundaries first, then list order.
///
/// The TypeScript comparator calls any tied boundary smaller, even against another boundary,
/// which its engine's stable merge sort resolves by reversing tied boundaries.
fn tick_key(time: Millis, boundary: bool, position: usize) -> (Millis, u8, i64) {
    let position = position as i64;
    if boundary {
        (time, 0, -position)
    } else {
        (time, 1, position)
    }
}

/// One session's ticks and periods.
struct Session<'r> {
    ticks: Vec<(Millis, Tick<'r>)>,
    periods: Vec<usize>,
}

/// Charges shared by every profile.
struct Summaries<'a> {
    /// The last charge for each event, like a JavaScript Map built from the array.
    by_event: HashMap<&'a str, &'a CompactionCharge>,
    requests: HashSet<&'a str>,
}

/// One owner's periods in one replay, with ticks sorted once for every run.
struct Runner<'r, 'x> {
    catalog: &'x Catalog,
    since: Millis,
    summaries: &'x Summaries<'r>,
    index: &'x ReplayIndex<'r>,
    prepared: &'x [Prepared<'r>],
    /// The first period with each request identity, which owns its charges.
    canonical: Vec<usize>,
    sessions: Vec<Session<'r>>,
}

impl<'r, 'x> Runner<'r, 'x> {
    fn new(
        context: &'x Profiled<'r>,
        index: &'x ReplayIndex<'r>,
        prepared: &'x [Prepared<'r>],
        periods: &[IdlePeriod],
        ttls: &Ttls,
    ) -> Self {
        let mut first: HashMap<&str, usize> = HashMap::new();
        let canonical = periods
            .iter()
            .enumerate()
            .map(|(i, p)| *first.entry(p.request.as_str()).or_insert(i))
            .collect();
        let mut runner = Self {
            catalog: context.catalog,
            since: context.since,
            summaries: &context.summaries,
            index,
            prepared,
            canonical,
            sessions: vec![],
        };
        runner.sessions = runner.ticks(context, periods, ttls, false);
        runner
    }

    /// Each session's requests, boundaries and idle triggers in replay order.
    fn ticks(
        &self,
        context: &Profiled<'r>,
        periods: &[IdlePeriod],
        scenario: &Ttls,
        late: bool,
    ) -> Vec<Session<'r>> {
        let mut groups: IndexMap<&str, Vec<usize>> = IndexMap::new();
        for (i, p) in self.prepared.iter().enumerate() {
            groups
                .entry(p.previous.session.as_str())
                .or_default()
                .push(i);
        }
        groups
            .into_iter()
            .map(|(session, ps)| {
                let mut idle_by_event: HashMap<&str, usize> = HashMap::new();
                for &i in &ps {
                    if let Some(b) = self.prepared[i].boundary {
                        idle_by_event.insert(b.id.as_str(), i);
                    }
                }
                let requests = ReplayIndex::session(&self.index.requests, session);
                let boundaries = ReplayIndex::session(&self.index.boundaries, session);
                let mut keyed: Vec<((Millis, u8, i64), Tick)> = vec![];
                for &r in requests {
                    let key = tick_key(r.time, false, keyed.len());
                    let ttls = context.scopes.ttls(context.scopes.scope(r), scenario);
                    keyed.push((key, Tick::Request { r, ttls }));
                }
                for &e in boundaries {
                    let key = tick_key(e.time, true, keyed.len());
                    let idle = idle_by_event.get(e.id.as_str()).copied();
                    let source = idle
                        .map(|i| self.prepared[i].previous)
                        .or_else(|| {
                            let id = e.request.as_deref().unwrap_or("");
                            self.index.by_id.get(id).copied()
                        })
                        .unwrap_or(requests[0]);
                    let summary = self.summaries.by_event.get(e.id.as_str()).copied();
                    let id = summary.and_then(|s| s.request.as_deref()).unwrap_or("");
                    let tick = Tick::Boundary {
                        e,
                        idle,
                        ttls: context.scopes.ttls(context.scopes.scope(source), scenario),
                        summary,
                        linked: self.index.by_id.get(id).copied(),
                    };
                    keyed.push((key, tick));
                }
                for &i in &ps {
                    if self.prepared[i].boundary.is_none()
                        && periods[i].status == PeriodStatus::Eligible
                    {
                        let time = self.prepared[i].previous.time + if late { HOUR } else { EARLY };
                        keyed.push((tick_key(time, false, keyed.len()), Tick::Idle(i)));
                    }
                }
                keyed.sort_unstable_by_key(|(key, _)| *key);
                Session {
                    ticks: keyed.into_iter().map(|((time, ..), t)| (time, t)).collect(),
                    periods: ps,
                }
            })
            .collect()
    }

    /// Replay every session with idle compaction on or off; point runs also record each
    /// period's summary cost and calibration.
    fn run(
        &self,
        context: &Profiled<'r>,
        scenario: &Ttls,
        on: bool,
        mode: Mode,
        periods: &mut [IdlePeriod],
    ) -> Run {
        let late;
        let sessions = if mode == Mode::Late {
            late = self.ticks(context, periods, scenario, true);
            &late
        } else {
            &self.sessions
        };
        let point = mode == Mode::Point;
        let catalog = self.catalog;
        let since = self.since;
        let mut ledger = Ledger {
            total: Sum::new(),
            by_period: vec![None; self.prepared.len()],
        };
        let mut resumed = 0.0;
        let mut charged: HashSet<&str> = HashSet::new();
        for session in sessions {
            let mut states = vec![State {
                weight: 1.0,
                path: Path {
                    offset: 0,
                    prefix: None,
                    time: None,
                    lifetime: HOUR,
                    retained: 0,
                    active: None,
                    input: None,
                    cohort: None,
                },
            }];
            for (time, tick) in &session.ticks {
                match *tick {
                    Tick::Idle(i) => {
                        if !on {
                            continue;
                        }
                        let item = &self.prepared[i];
                        let time = *time;
                        if time > item.previous.time + periods[i].gap_ms || time < since {
                            continue;
                        }
                        let unobserved = periods[i].outcome == PeriodOutcome::Unobserved;
                        let activation = match mode {
                            Mode::Fraction(f) if unobserved => f,
                            Mode::Closed if unobserved => 0.0,
                            Mode::Open if unobserved => 1.0,
                            _ => periods[i]
                                .activation
                                .expect("eligible periods have an activation"),
                        };
                        if activation == 0.0 {
                            continue;
                        }
                        let part = item
                            .previous
                            .parts
                            .iter()
                            .find(|p| is_message(p))
                            .expect("eligible periods follow a message");
                        let calibration = sample_key(item.previous, part)
                            .and_then(|key| self.index.calibration.get(&key));
                        let context_tokens = periods[i].context_tokens;
                        let active = self.canonical[i];
                        let mut branched = Vec::with_capacity(states.len() * 2);
                        for state in states {
                            let weight = state.weight * activation;
                            let input = context_tokens.unwrap_or(0) + state.path.offset;
                            if item.threshold.is_some_and(|t| input < t) {
                                branched.push(state);
                                continue;
                            }
                            let usable = calibration.filter(|c| {
                                c.uniform
                                    && prefix(part).is_some()
                                    && item.window.is_some()
                                    && is_message(part)
                            });
                            let Some(c) = usable else {
                                if point {
                                    periods[i].summary_cost =
                                        unknown_cost("summary calibration is unresolved");
                                }
                                ledger.charge(
                                    Some(active),
                                    &unknown_cost(
                                        "no compatible linked summary and full post-compaction prefix calibration, or summary pricing cohort is ambiguous",
                                    ),
                                    weight,
                                );
                                branched.push(state);
                                continue;
                            };
                            let g = if mode == Mode::Larger {
                                &c.larger
                            } else {
                                &c.median
                            };
                            let sample = c.samples[0];
                            if point {
                                periods[i].calibration = Some(PeriodCalibration {
                                    events: c.samples.iter().map(|s| s.event.to_string()).collect(),
                                    summary_tokens: g.output,
                                    prefix_tokens: g.prefix,
                                });
                            }
                            let retained = (g.prefix - g.output).min(g.read);
                            let window = item.window.expect("usable calibration has a window");
                            if retained < 0 || g.prefix >= input || g.prefix >= window {
                                ledger.charge(
                                    Some(active),
                                    &unknown_cost(
                                        "calibrated full post-compaction prefix does not shrink this eligible context",
                                    ),
                                    weight,
                                );
                                branched.push(state);
                                continue;
                            }
                            let same_cache = sample.summary.model == part.model
                                && sample.summary_request.provider == item.previous.provider
                                && sample.summary.speed == part.speed
                                && sample.summary.geo == part.geo;
                            let read = if same_cache && warm(time, state.path.time, HOUR) {
                                let full = prefix(part).expect("usable calibration has a prefix");
                                input.min((full + state.path.offset).max(0))
                            } else {
                                0
                            };
                            let write = input - read;
                            let subagent = item.ttls.subagent;
                            let summary = BillingPart {
                                usage: Usage {
                                    input: Some(input + g.overhead),
                                    output: Some(g.output),
                                    read: Some(read),
                                    write: Some(write),
                                    write5m: Some(if subagent == Some(Ttl::FiveMinutes) {
                                        write
                                    } else {
                                        0
                                    }),
                                    write1h: Some(if subagent == Some(Ttl::OneHour) {
                                        write
                                    } else {
                                        0
                                    }),
                                    ..sample.summary.usage
                                },
                                ..sample.summary.clone()
                            };
                            let cost = match subagent {
                                None => unknown_cost("outside-main summary TTL is unresolved"),
                                Some(_) => price_part(catalog, sample.summary_request, &summary),
                            };
                            ledger.charge(Some(active), &cost, weight);
                            if point {
                                periods[i].summary_cost =
                                    add_costs([&periods[i].summary_cost, &scale(&cost, weight)]);
                            }
                            if item.next.is_some() {
                                resumed += weight;
                            }
                            branched.push(State {
                                weight: state.weight * (1.0 - activation),
                                ..state
                            });
                            branched.push(State {
                                weight,
                                path: Path {
                                    offset: g.prefix
                                        - context_tokens.expect("eligible periods have a size"),
                                    prefix: Some(retained),
                                    time: if same_cache {
                                        Some(time)
                                    } else {
                                        state.path.time
                                    },
                                    lifetime: if same_cache {
                                        subagent.map_or(0, ttl_milliseconds)
                                    } else {
                                        state.path.lifetime
                                    },
                                    retained,
                                    active: Some(active),
                                    input: state.path.input,
                                    cohort: state.path.cohort,
                                },
                            });
                        }
                        states = merge(branched);
                    }
                    Tick::Boundary {
                        e,
                        idle,
                        ref ttls,
                        summary,
                        linked,
                    } => {
                        let billed: Vec<&BillingPart> = linked
                            .map(|l| {
                                l.parts
                                    .iter()
                                    .filter(|p| p.kind == PartKind::Compaction)
                                    .collect()
                            })
                            .unwrap_or_default();
                        let summary_request = summary
                            .and_then(|s| s.request.as_deref())
                            .filter(|r| !r.is_empty());
                        let allocated = !charged.contains(e.id.as_str())
                            && (summary_request.is_none()
                                || summary.is_some_and(|s| s.cost.total_usd != Some(0.0)));
                        charged.insert(e.id.as_str());
                        // Changed input to the observed summary under each carried context.
                        let extra = |state: &State| -> Option<Cost> {
                            let linked = linked.filter(|_| !billed.is_empty())?;
                            let costs: Vec<Cost> = billed
                                .iter()
                                .map(|&p| {
                                    let after = adjusted(p, state, linked, ttls.subagent);
                                    changed_cost(catalog, linked, p, after.as_deref())
                                })
                                .collect();
                            Some(add_costs(&costs))
                        };
                        if let Some(i) = idle {
                            let item = &self.prepared[i];
                            let active = self.canonical[i];
                            let resumed_part = item
                                .next
                                .and_then(|n| n.parts.iter().find(|p| is_message(p)));
                            let full = resumed_part.and_then(prefix);
                            let base = on.then(|| match (summary_request, linked) {
                                (Some(_), Some(linked)) => {
                                    let costs: Vec<Cost> = billed
                                        .iter()
                                        .map(|p| price_part(catalog, linked, p))
                                        .collect();
                                    add_costs(&costs)
                                }
                                _ => summary.map(|s| s.cost.clone()).unwrap_or_else(|| {
                                    unknown_cost("applied idle summary charge is unavailable")
                                }),
                            });
                            for state in &mut states {
                                state.path.input = None;
                                state.path.cohort = None;
                                if let Some(base) = &base {
                                    let extra = extra(state).unwrap_or_else(|| {
                                        if state.path.offset == 0 {
                                            zero_cost()
                                        } else {
                                            unknown_cost(
                                                "changed input to an unmatched observed summary cannot be priced",
                                            )
                                        }
                                    });
                                    let cost = if allocated {
                                        add_costs([base, &extra])
                                    } else {
                                        zero_cost()
                                    };
                                    if linked.map_or(e.time, |l| l.time) >= since {
                                        ledger.charge(Some(active), &cost, state.weight);
                                    }
                                    if point {
                                        periods[i].summary_cost = add_costs([
                                            &periods[i].summary_cost,
                                            &scale(&cost, state.weight),
                                        ]);
                                    }
                                    if item.next.is_some() {
                                        resumed += state.weight;
                                    }
                                    state.path.offset = 0;
                                    state.path.active = None;
                                } else {
                                    if item.next.is_none() {
                                        state.path.active = None;
                                        continue;
                                    }
                                    match (e.pre_tokens, full) {
                                        (Some(pre), Some(full)) => state.path.offset += pre - full,
                                        _ => ledger.charge(
                                            Some(active),
                                            &unknown_cost(
                                                "applied idle has no pre-context and full resumed-prefix geometry for the off path",
                                            ),
                                            state.weight,
                                        ),
                                    }
                                    let summary_output = billed
                                        .first()
                                        .and_then(|p| p.usage.output)
                                        .or(e.post_tokens)
                                        .or(full)
                                        .unwrap_or(0);
                                    state.path.retained = resumed_part
                                        .and_then(|p| p.usage.read)
                                        .unwrap_or(0)
                                        .min((full.unwrap_or(0) - summary_output).max(0));
                                    state.path.active = Some(active);
                                }
                            }
                        } else {
                            // Later observed compactions converge the paths. Charge their changed
                            // summary input before dropping the carried context difference.
                            for state in &mut states {
                                if allocated && state.path.offset != 0 && e.time >= since {
                                    let extra = extra(state).unwrap_or_else(|| {
                                        unknown_cost(
                                            "counterfactual input to a later unmatched compaction is unknown",
                                        )
                                    });
                                    ledger.charge(state.path.active, &extra, state.weight);
                                }
                                let path = &mut state.path;
                                path.offset = 0;
                                path.prefix = None;
                                path.active = None;
                                path.retained = 0;
                                path.input = None;
                                path.cohort = None;
                            }
                        }
                        states = merge(states);
                    }
                    Tick::Request { r, ref ttls } => {
                        let messages = r.parts.iter().filter(|p| is_conversation(p)).count();
                        let summary = self.summaries.requests.contains(r.id.as_str());
                        for state in &mut states {
                            for p in &r.parts {
                                if p.kind == PartKind::Compaction && summary {
                                    continue;
                                }
                                if r.agent == Agent::Subagent || p.kind == PartKind::Advisor {
                                    continue;
                                }
                                let current = (
                                    r.provider.as_deref(),
                                    p.model.as_deref(),
                                    r.agent,
                                    p.speed.as_deref(),
                                    p.geo.as_deref(),
                                );
                                let conversation = is_conversation(p);
                                let s = &state.path;
                                let invalid = s.offset != 0
                                    && conversation
                                    && (messages != 1
                                        || s.cohort.is_some_and(|c| c != current)
                                        || s.input.is_some_and(|before| {
                                            p.usage.input.is_none_or(|input| input < before)
                                        }));
                                let next = if invalid {
                                    None
                                } else if messages == 1 {
                                    let ttl = if p.kind == PartKind::Compaction {
                                        ttls.subagent
                                    } else {
                                        ttls.main
                                    };
                                    adjusted(p, state, r, ttl)
                                } else if s.offset == 0 {
                                    Some(Cow::Borrowed(p))
                                } else {
                                    None
                                };
                                if r.time >= since {
                                    let cost = changed_cost(catalog, r, p, next.as_deref());
                                    ledger.charge(state.path.active, &cost, state.weight);
                                }
                                if conversation {
                                    let path = &mut state.path;
                                    path.prefix = next.as_deref().and_then(prefix);
                                    path.input = p.usage.input;
                                    path.cohort = Some(current);
                                    path.time = Some(r.time);
                                    path.lifetime = ttls.main.map_or(0, ttl_milliseconds);
                                }
                            }
                        }
                        states = merge(states);
                    }
                }
            }
            if on {
                for &i in &session.periods {
                    if periods[i].status == PeriodStatus::Unknown {
                        let cost = unknown_cost(periods[i].reason.clone());
                        if point {
                            periods[i].summary_cost = cost.clone();
                        }
                        ledger.charge(Some(self.canonical[i]), &cost, 1.0);
                    }
                }
            }
        }
        Run {
            adjustment: ledger.total.cost(),
            by_period: ledger
                .by_period
                .into_iter()
                .map(|s| s.map(Sum::cost))
                .collect(),
            resumed,
        }
    }
}

/// One Claude profile's requests, events and controls, shared by every replay and owner.
struct Profiled<'a> {
    catalog: &'a Catalog,
    estimates: &'a IndexMap<String, UsageEstimate>,
    all_summaries: &'a [CompactionCharge],
    summaries: Summaries<'a>,
    scopes: Scopes<'a>,
    since: Millis,
    now: Millis,
    /// Requests per session, in first-seen order of the time-sorted requests.
    groups: IndexMap<&'a str, Vec<&'a Request>>,
    /// Profile events indexed once for every window simulation and cache replay.
    window_events: WindowEvents<'a>,
    calibration: CompactionSamples<'a>,
}

impl<'a> Profiled<'a> {
    /// Apply window controls per conversation, then replay every part's cache under `ttls`.
    fn replay(&self, ttls: &Ttls, windows: &[ScenarioWindow]) -> Replay<'a> {
        let mut result = Replay {
            errors: vec![],
            added: vec![],
            modeled: vec![],
            selected: vec![],
            requests: vec![],
        };
        // Later duplicates win, like a JavaScript Map keyed by request and part.
        let mut replayed: HashMap<String, HashMap<usize, BillingPart>> = HashMap::new();
        let mut untouched: HashSet<&'a str> = HashSet::new();
        let no_estimates = IndexMap::new();
        for group in self.groups.values() {
            let source = group
                .iter()
                .copied()
                .find(|r| r.agent == Agent::Main && r.parts.iter().any(is_message))
                .unwrap_or(group[0]);
            let scope = self.scopes.scope(source);
            let policy = self.scopes.ttls(scope, ttls);
            let owned = |scope: Scope, model: &Option<String>| {
                self.scopes.window(scope, model.as_deref()).owner.clone()
            };
            let keys: Vec<&ScenarioWindow> = windows
                .iter()
                .filter(|w| w.models.iter().any(|m| owned(scope, m) == w.owner))
                .collect();
            // Requests in one scope share every control, so check each scope once.
            let scopes: IndexSet<Scope> = group.iter().map(|r| self.scopes.scope(r)).collect();
            if scopes.iter().any(|&s| {
                self.scopes.ttls(s, ttls) != policy
                    || keys
                        .iter()
                        .any(|w| w.models.iter().any(|m| owned(s, m) != w.owner))
            }) {
                result.errors.push(POLICY_CHANGE.to_string());
            }
            let settings: Vec<WindowSetting> = keys
                .iter()
                .map(|w| WindowSetting {
                    value: w.value,
                    models: &w.models,
                })
                .collect();
            let simulation = replay_windows(
                group.iter().copied(),
                &settings,
                &WindowOptions {
                    events: &self.window_events,
                    estimates: self.estimates,
                    catalog: self.catalog,
                    samples: &self.calibration,
                    since: self.since,
                    ttls: policy.clone(),
                },
            );
            let extra = CacheBoundaries::new(&simulation.events);
            let options = ReplayOptions {
                estimates: if simulation.applied {
                    &no_estimates
                } else {
                    self.estimates
                },
                catalog: self.catalog,
                ttls: ReplayTtls {
                    main: policy.main,
                    subagent: policy.subagent,
                    unknown: None,
                },
                since: self.since,
                cold_prefixes: false,
                estimated_parts: Some(&simulation.estimated_parts),
                attempt_scenario: Default::default(),
            };
            let items = replay_parts(
                simulation.requests.iter().map(AsRef::as_ref),
                [&self.window_events.cache, &extra],
                &options,
            );
            for item in items {
                if item.selected {
                    result.selected.push(Selected {
                        request: item.request.id.clone(),
                        compaction: item.p.kind == PartKind::Compaction,
                        cost: item.components.total(),
                    });
                }
                match replayed.get_mut(item.request.id.as_str()) {
                    Some(parts) => {
                        parts.insert(item.part, item.p);
                    }
                    None => {
                        let parts = HashMap::from([(item.part, item.p)]);
                        replayed.insert(item.request.id.clone(), parts);
                    }
                }
            }
            if !simulation.applied {
                untouched.extend(group.iter().map(|r| r.id.as_str()));
            }
            result.requests.extend(simulation.requests);
            result.modeled.extend(simulation.events);
            result
                .added
                .extend(simulation.added.into_iter().map(|a| a.summary_cost));
            result.errors.extend(simulation.errors);
        }
        let mut requests = std::mem::take(&mut result.requests);
        requests.sort_by(|a, b| {
            a.time
                .cmp(&b.time)
                .then_with(|| locale_compare(&a.id, &b.id))
        });
        result.requests = requests
            .into_iter()
            .map(|r| {
                let estimate = if untouched.contains(r.id.as_str()) {
                    self.estimates.get(&r.id).and_then(|e| e.part.as_ref())
                } else {
                    None
                };
                let base = estimate.map_or(r.parts.as_slice(), std::slice::from_ref);
                let found = replayed.get(r.id.as_str());
                let parts: Vec<BillingPart> = base
                    .iter()
                    .enumerate()
                    .map(|(i, p)| found.and_then(|f| f.get(&i)).unwrap_or(p).clone())
                    .collect();
                if parts == r.parts {
                    r
                } else {
                    let mut owned = r.into_owned();
                    owned.parts = parts;
                    Cow::Owned(owned)
                }
            })
            .collect();
        result
    }

    /// Classify each of `owner`'s request gaps and estimate unobserved activation by cohort.
    fn periods<'r>(
        &self,
        index: &ReplayIndex<'r>,
        ttls: &Ttls,
        windows: &[ScenarioWindow],
        owner: &str,
    ) -> (Vec<Prepared<'r>>, Vec<IdlePeriod>, Vec<ActivationCohort>) {
        let mut prepared = vec![];
        let mut periods = vec![];
        // The first window owning each (owner, model), as `find` would return.
        let mut lookup: HashMap<(&str, Option<&str>), Option<i64>> = HashMap::new();
        for w in windows {
            for model in &w.models {
                lookup
                    .entry((w.owner.as_str(), model.as_deref()))
                    .or_insert(w.value);
            }
        }
        for (&session, group) in &index.conversations {
            let events = ReplayIndex::session(&index.events, session);
            let boundaries: Vec<&StructuralEvent> = events
                .iter()
                .copied()
                .filter(|e| {
                    e.kind == EventKind::Compaction && e.idle_attribution == IdleAttribution::Notice
                })
                .collect();
            let mut boundary_index = 0;
            let mut settings_index = 0;
            let mut activity_index = 0;
            let mut turn_start = Millis::MIN;
            let mut turn_end = Millis::MIN;
            let mut pending: HashSet<&str> = HashSet::new();
            let mut old: HashMap<&str, &Scalar> = HashMap::new();
            for (position, &r) in group.iter().enumerate() {
                let scope = self.scopes.scope(r);
                let c = self.scopes.control(scope);
                if c.owner != owner {
                    continue;
                }
                let next = group.get(position + 1).copied();
                let end = next.map_or(self.now, |n| n.time);
                if end < self.since || r.time + EARLY < self.since || end <= r.end_time {
                    continue;
                }
                let gap_ms = end - r.time;
                let p = r
                    .parts
                    .iter()
                    .find(|p| is_conversation(p))
                    .expect("conversation requests have a conversation part");
                while boundary_index < boundaries.len() && boundaries[boundary_index].time <= r.time
                {
                    boundary_index += 1;
                }
                let boundary = boundaries
                    .get(boundary_index)
                    .copied()
                    .filter(|b| b.time <= end);
                while settings_index < events.len() && events[settings_index].time <= r.time {
                    let e = events[settings_index];
                    settings_index += 1;
                    if e.kind == EventKind::Settings {
                        old.extend(e.settings.iter().map(|(k, v)| (k.as_str(), v)));
                    }
                }
                let flag_state = if supported(r.version.as_deref(), 314) {
                    Some("not-required".to_string())
                } else {
                    match old.get("idleCompactionAvailable") {
                        Some(Scalar::Bool(b)) => Some(b.to_string()),
                        _ => None,
                    }
                };
                let context_tokens = match (p.usage.input, p.usage.output) {
                    (Some(input), Some(output)) => Some(input + output),
                    _ => None,
                };
                let native = self.scopes.window(scope, p.model.as_deref());
                let window = if supported(
                    r.version.as_deref(),
                    native.minimum_claude_patch.unwrap_or(288),
                ) {
                    lookup
                        .get(&(native.owner.as_str(), p.model.as_deref()))
                        .copied()
                        .flatten()
                } else {
                    None
                };
                let period_ttls = self.scopes.ttls(scope, ttls);
                while activity_index < events.len() && events[activity_index].time <= r.time + EARLY
                {
                    let e = events[activity_index];
                    activity_index += 1;
                    match e.kind {
                        EventKind::TurnStart => {
                            pending.clear();
                            turn_start = e.time;
                        }
                        EventKind::TurnEnd => {
                            pending.clear();
                            turn_end = e.time;
                        }
                        EventKind::ToolStart => {
                            pending.insert(e.tool_id.as_deref().unwrap_or(&e.id));
                        }
                        EventKind::ToolEnd => {
                            if let Some(id) = e.tool_id.as_deref().filter(|id| !id.is_empty()) {
                                pending.remove(id);
                            }
                        }
                        _ => {}
                    }
                }
                let interrupted =
                    turn_start > r.end_time && turn_start < end && turn_start < r.time + EARLY;
                let main_5m = period_ttls.main == Some(Ttl::FiveMinutes);
                let client = client(r);
                let busy = !pending.is_empty() || interrupted || r.end_time >= r.time + EARLY;
                let below =
                    matches!((context_tokens, c.threshold), (Some(t), Some(min)) if t < min);
                let narrow = matches!((window, c.threshold), (Some(w), Some(min)) if w <= min);
                let flag_off = flag_state.as_deref().is_none_or(|f| f == "false");
                let (status, reason) = if gap_ms < EARLY {
                    (
                        PeriodStatus::Censored,
                        if next.is_some() {
                            "return precedes the estimated near-expiry trigger"
                        } else {
                            "final observation has not reached the estimated idle trigger; no session end inferred"
                        },
                    )
                } else if main_5m
                    || c.auto_compact == Some(false)
                    || client == Client::Unsupported
                    || busy
                    || below
                    || narrow
                {
                    (
                        PeriodStatus::Ineligible,
                        if main_5m {
                            "main TTL is 5m"
                        } else if c.auto_compact == Some(false) {
                            "automatic compaction is disabled"
                        } else if client == Client::Unsupported {
                            "SDK/headless/remote/subagent client is outside the confirmed interactive policy"
                        } else if busy {
                            "tool wait or active turn interrupts the idle interval"
                        } else if below {
                            "context is below the effective idle threshold"
                        } else {
                            "auto-compact window removes the eligible size band"
                        },
                    )
                } else if !c.resolved
                    || period_ttls.main.is_none()
                    || context_tokens.is_none()
                    || p.kind == PartKind::Unknown
                    || c.threshold.is_none()
                    || window.is_none()
                    || (boundary.is_none()
                        && (client != Client::Supported
                            || !supported(r.version.as_deref(), 290)
                            || flag_off))
                {
                    (
                        PeriodStatus::Unknown,
                        "effective threshold/window/TTL, input size, interactive execution, version or pre-2.1.314 flag availability is unresolved",
                    )
                } else {
                    (
                        PeriodStatus::Eligible,
                        "interactive request gap under current native prerequisites; open-and-awake activation is estimated",
                    )
                };
                let known_on = old.get("idleCompaction") == Some(&&Scalar::Bool(true))
                    && matches!(old.get("promptCacheTtl"), Some(Scalar::Text(t)) if t == "1h")
                    && old.get("autoCompactEnabled") == Some(&&Scalar::Bool(true))
                    && !flag_off;
                let eligible = status == PeriodStatus::Eligible;
                periods.push(IdlePeriod {
                    request: r.id.clone(),
                    resume: next.map(|n| n.id.clone()),
                    session: r.session.clone(),
                    context_tokens,
                    gap_ms,
                    status,
                    reason: reason.to_string(),
                    confirmed: eligible && turn_end >= r.end_time && turn_end < r.time + EARLY,
                    return_timing: if next.is_none() {
                        ReturnTiming::Final
                    } else if (EARLY..=HOUR).contains(&gap_ms) {
                        ReturnTiming::NearExpiry
                    } else if gap_ms < HOUR {
                        ReturnTiming::BeforeExpiry
                    } else {
                        ReturnTiming::AfterExpiry
                    },
                    outcome: if boundary.is_some() {
                        PeriodOutcome::Applied
                    } else if known_on && next.is_some() {
                        PeriodOutcome::NotApplied
                    } else {
                        PeriodOutcome::Unobserved
                    },
                    event: boundary.map(|b| b.id.clone()),
                    version: r.version.clone(),
                    client: r.client.clone(),
                    flag_state,
                    activation: None,
                    activation_basis: "not applicable".into(),
                    summary_cost: zero_cost(),
                    calibration: None,
                });
                prepared.push(Prepared {
                    previous: r,
                    next,
                    boundary,
                    known_on,
                    window,
                    threshold: c.threshold,
                    ttls: period_ttls,
                });
            }
        }
        let truthy = |text: &Option<String>| text.clone().filter(|t| !t.is_empty());
        let mut cohorts: IndexMap<(String, String, String), ActivationCohort> = IndexMap::new();
        for (p, item) in periods.iter().zip(&prepared) {
            if p.status != PeriodStatus::Eligible || truthy(&p.resume).is_none() || !item.known_on {
                continue;
            }
            let (Some(version), Some(client), Some(flag_state)) =
                (truthy(&p.version), truthy(&p.client), p.flag_state.clone())
            else {
                continue;
            };
            let c = cohorts
                .entry((version.clone(), client.clone(), flag_state.clone()))
                .or_insert(ActivationCohort {
                    version,
                    client,
                    flag_state,
                    eligible: 0,
                    applied: 0,
                    fraction: 0.0,
                });
            c.eligible += 1;
            c.applied += u64::from(p.outcome == PeriodOutcome::Applied);
            c.fraction = c.applied as f64 / c.eligible as f64;
        }
        for p in &mut periods {
            if p.status != PeriodStatus::Eligible {
                p.activation = (p.status != PeriodStatus::Unknown).then_some(0.0);
                continue;
            }
            if p.outcome != PeriodOutcome::Unobserved {
                p.activation = Some(if p.outcome == PeriodOutcome::Applied {
                    1.0
                } else {
                    0.0
                });
                p.activation_basis = "recorded applied/not-applied returned outcome; missing discarded generation is separately unobserved".into();
                continue;
            }
            // Cohorts never hold a missing or empty version or client, so a missing value matches none.
            let c = match (&p.version, &p.client, &p.flag_state) {
                (Some(version), Some(client), Some(flag_state)) => {
                    cohorts.get(&(version.clone(), client.clone(), flag_state.clone()))
                }
                _ => None,
            };
            p.activation = Some(c.map_or(1.0, |c| c.fraction));
            p.activation_basis = match c {
                Some(c) => format!(
                    "same version/client/flag known-on returned applied fraction {}/{}; final periods use that same fraction with no second liveness factor",
                    c.applied, c.eligible
                ),
                None => "documented-trigger fallback: left open and awake, one summary; final periods have the same assumed activation and no return benefit".into(),
            };
        }
        (prepared, periods, cohorts.into_values().collect())
    }

    /// Compare idle on and off for `owner` in one replayed TTL/window configuration.
    fn scenario(
        &self,
        replay: &Replay,
        index: &ReplayIndex,
        owner: &str,
        ttls: &Ttls,
        windows: &[ScenarioWindow],
        baseline: &str,
    ) -> IdleScenario {
        let (prepared, mut periods, activation_cohorts) = self.periods(index, ttls, windows, owner);
        let idle_events: HashSet<&str> = prepared
            .iter()
            .filter_map(|p| p.boundary.map(|b| b.id.as_str()))
            .collect();
        let idle_requests: HashSet<&str> = self
            .all_summaries
            .iter()
            .filter(|s| idle_events.contains(s.event.as_str()) && s.cost.total_usd != Some(0.0))
            .filter_map(|s| s.request.as_deref().filter(|r| !r.is_empty()))
            .collect();
        let mut common = Sum::new();
        for s in &replay.selected {
            if !(s.compaction && idle_requests.contains(s.request.as_str())) {
                common.add(&s.cost);
            }
        }
        for s in self.all_summaries {
            if s.request.as_deref().is_none_or(str::is_empty)
                && !idle_events.contains(s.event.as_str())
                && index.selected_events.contains(s.event.as_str())
            {
                common.add(&s.cost);
            }
        }
        for r in &replay.requests {
            if r.time >= self.since {
                common.add(&server_cost(r));
            }
        }
        for cost in &replay.added {
            common.add(cost);
        }
        let common = common.cost();
        let runner = Runner::new(self, index, &prepared, &periods, ttls);
        let mut run = |on: bool, mode: Mode| runner.run(self, ttls, on, mode, &mut periods);
        let off = run(false, Mode::Point);
        let on = run(true, Mode::Point);
        let delta = |r: &Run| difference(&r.adjustment, &off.adjustment).total_usd;
        let closed = delta(&run(true, Mode::Closed));
        let open = delta(&run(true, Mode::Open));
        let mut lifetime_break_even_fraction = None;
        if let (Some(closed), Some(open)) = (closed, open) {
            if closed == 0.0 {
                lifetime_break_even_fraction = Some(0.0);
            } else if open == 0.0 {
                lifetime_break_even_fraction = Some(1.0);
            } else if sign(closed) != sign(open) {
                let mut low = 0.0;
                let mut high = 1.0;
                loop {
                    let middle = (low + high) / 2.0;
                    if high - low <= f64::EPSILON {
                        lifetime_break_even_fraction = Some(middle);
                        break;
                    }
                    let Some(value) = delta(&run(true, Mode::Fraction(middle))) else {
                        break;
                    };
                    if value == 0.0 {
                        lifetime_break_even_fraction = Some(middle);
                        break;
                    }
                    if sign(value) == sign(closed) {
                        low = middle;
                    } else {
                        high = middle;
                    }
                }
            }
        }
        let larger_summary_delta_usd = delta(&run(true, Mode::Larger));
        let late_trigger_delta_usd = delta(&run(true, Mode::Late));
        let unknown: IndexSet<&String> = replay
            .errors
            .iter()
            .chain(&on.adjustment.unknown)
            .chain(&off.adjustment.unknown)
            .collect();
        let unknown: Vec<String> = unknown.into_iter().cloned().collect();
        let applicable = prepared.iter().any(|p| {
            p.ttls.main != Some(Ttl::FiveMinutes)
                && self
                    .scopes
                    .control(self.scopes.scope(p.previous))
                    .auto_compact
                    != Some(false)
                && match (p.window, p.threshold) {
                    (Some(window), Some(threshold)) => window > threshold,
                    _ => true,
                }
        });
        let delta_usd = if unknown.is_empty() { delta(&on) } else { None };
        // Group once by size, keeping first-seen size order and period order within each.
        let mut sizes: IndexMap<Option<i64>, Vec<usize>> = IndexMap::new();
        for (i, p) in periods.iter().enumerate() {
            sizes.entry(p.context_tokens).or_default().push(i);
        }
        let by_context = sizes
            .into_iter()
            .map(|(tokens, selected)| {
                let total = |run: &Run| {
                    let costs: Vec<Cow<Cost>> = selected
                        .iter()
                        .map(|&i| run.period(runner.canonical[i]))
                        .collect();
                    add_costs(costs.iter().map(AsRef::as_ref))
                };
                ContextDelta {
                    tokens,
                    periods: selected.len() as u64,
                    delta_usd: difference(&total(&on), &total(&off)).total_usd,
                }
            })
            .collect();
        let elapsed = self.now - self.since;
        IdleScenario {
            baseline: baseline.to_string(),
            applicable,
            ttls: ttls.clone(),
            windows: windows.to_vec(),
            periods,
            cost: OnOff {
                off: add_costs([&common, &off.adjustment]),
                on: add_costs([&common, &on.adjustment]),
            },
            delta_usd,
            monthly_delta_usd: delta_usd
                .filter(|_| elapsed > 0)
                .map(|d| d * 30.0 * 86_400_000.0 / elapsed as f64),
            by_context,
            activation_cohorts,
            sensitivity: IdleSensitivity {
                closed_delta_usd: closed,
                open_delta_usd: open,
                larger_summary_delta_usd,
                late_trigger_delta_usd,
                lifetime_break_even_fraction,
                reread_break_even_usd_per_resumed_summary: delta_usd
                    .filter(|&d| d < 0.0 && on.resumed > 0.0)
                    .map(|d| -d / on.resumed),
            },
            adjustment: OnOff {
                on: on.adjustment,
                off: off.adjustment,
            },
            unknown,
        }
    }
}

/// Profile events followed by modeled window boundaries, in stable time order.
fn all_events<'e>(
    events: &[&'e StructuralEvent],
    modeled: &'e [StructuralEvent],
) -> Vec<&'e StructuralEvent> {
    let mut all: Vec<&StructuralEvent> = events.iter().copied().chain(modeled).collect();
    all.sort_by_key(|e| e.time);
    all
}

/// Whether the latest recorded settings for `r`'s session at its time made idle compaction available.
fn historically_available(events: &[&StructuralEvent], r: &Request) -> bool {
    let mut value = None;
    for e in events {
        if e.time > r.time {
            break;
        }
        if e.kind == EventKind::Settings
            && e.origins.iter().any(|o| o.session == r.session)
            && let Some(v) = e.settings.get("idleCompactionAvailable")
        {
            value = Some(v);
        }
    }
    value == Some(&Scalar::Bool(true))
}

/// Compare idle compaction on and off for each Claude profile.
pub fn idle_checks(options: &IdleOptions) -> Vec<IdleCheck> {
    let mut checks = Vec::new();
    for profile in &options.scan.profiles {
        if profile.harness == Harness::Claude {
            profile_checks(options, profile, &mut checks);
        }
    }
    checks
}

/// Idle checks for each owning idle control of one Claude profile.
fn profile_checks<'a>(
    options: &IdleOptions<'a>,
    profile: &'a Profile,
    checks: &mut Vec<IdleCheck>,
) {
    let scan = options.scan;
    let in_profile = |profiles: &[String]| profiles.contains(&profile.id);
    let mut requests: Vec<&Request> = scan
        .cache_context
        .requests
        .iter()
        .chain(&scan.requests)
        .filter(|r| in_profile(&r.profiles))
        .collect();
    requests.sort_by(|a, b| {
        a.time
            .cmp(&b.time)
            .then_with(|| locale_compare(&a.id, &b.id))
    });
    let main: Vec<&Request> = scan
        .requests
        .iter()
        .filter(|r| {
            in_profile(&r.profiles) && r.agent == Agent::Main && r.parts.iter().any(is_conversation)
        })
        .collect();
    let Some(first) = main.iter().map(|r| r.time).min() else {
        return;
    };
    let mut events: Vec<&StructuralEvent> = scan
        .cache_context
        .boundaries
        .iter()
        .chain(&scan.events)
        .filter(|e| in_profile(&e.profiles))
        .collect();
    events.sort_by_key(|e| e.time);
    let since = options.since.unwrap_or(first);
    let now = options.now;
    let mut groups: IndexMap<&str, Vec<&Request>> = IndexMap::new();
    for &r in &requests {
        groups.entry(r.session.as_str()).or_default().push(r);
    }
    let mut summaries = Summaries {
        by_event: HashMap::new(),
        requests: HashSet::new(),
    };
    for s in options.summaries {
        summaries.by_event.insert(s.event.as_str(), s);
        if let Some(r) = s.request.as_deref().filter(|r| !r.is_empty()) {
            summaries.requests.insert(r);
        }
    }
    let profiled = Profiled {
        catalog: options.catalog,
        estimates: options.estimates,
        all_summaries: options.summaries,
        summaries,
        scopes: Scopes::new(profile),
        since,
        now,
        groups,
        window_events: WindowEvents::new(events.iter().copied()),
        calibration: compaction_samples(&requests, &events, options.summaries),
    };
    let scopes = &profiled.scopes;
    let models: IndexSet<Option<&str>> = main
        .iter()
        .flat_map(|r| r.parts.iter().filter(|p| is_conversation(p)))
        .map(|p| p.model.as_deref())
        .collect();
    // A request whose scope was already seen adds no new (owner, model) pair.
    let main_scopes: IndexSet<Scope> = main.iter().map(|r| scopes.scope(r)).collect();
    let mut current_windows: Vec<ScenarioWindow> = vec![];
    let mut seen: HashSet<(String, Option<&str>)> = HashSet::new();
    for &scope in &main_scopes {
        for &model in &models {
            let c = scopes.window(scope, model);
            if seen.insert((c.owner.clone(), model)) {
                current_windows.push(ScenarioWindow {
                    owner: c.owner.clone(),
                    models: vec![model.map(str::to_string)],
                    value: c.value,
                });
            }
        }
    }
    let recommended_windows: Vec<ScenarioWindow> = current_windows
        .iter()
        .map(|w| {
            let check = options
                .windows
                .iter()
                .find(|c| c.base.profile == profile.id && c.base.owner == w.owner);
            ScenarioWindow {
                value: match check {
                    Some(c) if c.base.action == Action::Change => c.base.recommended,
                    _ => w.value,
                },
                ..w.clone()
            }
        })
        .collect();
    let current_ttls = PerBucket {
        main: profile.ttl.main.value,
        subagent: profile.ttl.subagent.value,
    };
    let mut after_ttls = current_ttls.clone();
    for bucket in Bucket::ALL {
        let check = options
            .ttls
            .iter()
            .find(|c| c.base.profile == profile.id && c.bucket == bucket);
        if let Some(c) = check.filter(|c| c.base.action == Action::Change) {
            *after_ttls.get_mut(bucket) = c.base.recommended;
        }
    }
    // Each owner keeps its first position and its last request's control.
    let mut controls: IndexMap<&str, &IdleControl> = IndexMap::new();
    for r in &main {
        let c = scopes.control(scopes.scope(r));
        controls.insert(c.owner.as_str(), c);
    }
    let current = profiled.replay(&current_ttls, &current_windows);
    let recommended = profiled.replay(&after_ttls, &recommended_windows);
    let current_events = all_events(&events, &current.modeled);
    let recommended_events = all_events(&events, &recommended.modeled);
    let current_index = ReplayIndex::new(
        &current.requests,
        &current_events,
        &profiled.calibration,
        since,
    );
    let recommended_index = ReplayIndex::new(
        &recommended.requests,
        &recommended_events,
        &profiled.calibration,
        since,
    );
    let history_profiles: IndexSet<&String> = main.iter().flat_map(|r| &r.profiles).collect();
    let request_ids: HashSet<&str> = requests.iter().map(|r| r.id.as_str()).collect();
    let usage_estimates: Vec<UsageEstimate> = options
        .estimates
        .values()
        .filter(|e| request_ids.contains(e.request.as_str()))
        .cloned()
        .collect();
    let elapsed = now - since;
    for (&owner, &c) in &controls {
        let current_settings = profiled.scenario(
            &current,
            &current_index,
            owner,
            &current_ttls,
            &current_windows,
            "idle on/off at current TTL and current window controls",
        );
        let after = profiled.scenario(
            &recommended,
            &recommended_index,
            owner,
            &after_ttls,
            &recommended_windows,
            "idle on/off after applicable TTL and compaction-window changes; independent savings are not additive",
        );
        let delta = after.delta_usd;
        // Equal costs are not evidence for either value, such as when no idle gap
        // was long enough to trigger a summary.
        let winner = delta.filter(|&d| d != 0.0).map(|d| d < 0.0);
        let recent = main
            .iter()
            .rev()
            .find(|r| scopes.control(scopes.scope(r)).owner == owner)
            .expect("every owner comes from a main request");
        let applicable = c.resolved
            && c.value.is_some()
            && client(recent) == Client::Supported
            && supported(recent.version.as_deref(), 290)
            && after.applicable
            && (supported(recent.version.as_deref(), 314)
                || historically_available(&events, recent));
        let savings = match (delta, c.value) {
            (Some(delta), Some(_)) => Some(if winner == c.value { 0.0 } else { delta.abs() }),
            _ => None,
        };
        let break_even = after.sensitivity.lifetime_break_even_fraction.map(|f| {
            let f = Number::from_f64(f).expect("a fraction is finite");
            format!(
                "uncertain-period open/awake activation fraction {} is a modeled break-even between the stated closure/open endpoints",
                crate::exposure::number(&f)
            )
        });
        let mut uncertainty: IndexSet<String> = [
            "missing applied boundaries do not prove that unlogged or discarded summaries never ran; their unobserved charges are not measured or included as known zero, and can change this conditional API-price estimate",
            "native transcripts do not establish historical idle settings, feature-flag fetch state or closed/asleep client lifetime; precomputed alone is not idle attribution; unknown eligibility or missing full-prefix calibration remains unpriced",
            "Desktop labels alone do not establish local execution; CLI labels require REPL provenance; T3 proactive policies are outside this native check",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        if !applicable {
            uncertainty.insert(if !after.applicable {
                "idle compaction is inapplicable after TTL/window recommendations or disabled automatic compaction".into()
            } else {
                "effective ownership or current native version/flag support is unresolved; this is advisory".into()
            });
        }
        uncertainty.extend(after.unknown.iter().cloned());
        let threshold = c
            .threshold
            .map_or_else(|| "unknown".to_string(), |t| t.to_string());
        checks.push(IdleCheck {
            base: CheckBase {
                profile: profile.id.clone(),
                history_profiles: history_profiles.iter().map(|p| p.to_string()).collect(),
                evidence: if delta.is_none() {
                    Evidence::Unknown
                } else {
                    Evidence::Estimated
                },
                action: match winner {
                    Some(winner) if applicable => {
                        if Some(winner) == c.value {
                            Action::Keep
                        } else {
                            Action::Change
                        }
                    }
                    _ => Action::Advisory,
                },
                current: c.value,
                recommended: if after.applicable { winner } else { None },
                owner: owner.to_string(),
                since: Some(since),
                until: Some(now),
                window_basis: if options.since.is_none() {
                    "available transcript window replayed under today's settings; historical on/off state is used only where recorded"
                } else {
                    "explicit --since window replayed under today's settings; historical on/off state is used only where recorded"
                }
                .into(),
                baseline: after.baseline.clone(),
                requests: main.iter().filter(|r| r.time >= since).count() as u64,
                coverage: None,
                savings_usd: savings,
                monthly_savings_usd: savings
                    .filter(|_| elapsed > 0)
                    .map(|s| s * 30.0 * 86_400_000.0 / elapsed as f64),
                break_even,
                assumptions: vec![
                    c.reason.clone(),
                    format!(
                        "effective idle threshold {threshold}; owner {}; threshold tuning is outside this check",
                        c.threshold_owner
                    ),
                    "delta is on cost minus off cost over the entire selected window, including final unresumed summaries; the aggregate point estimate decides, with a zero tie retaining the current value".into(),
                    "documented near-expiry trigger is modeled at 90% of one hour from inspected native v2.1.316; one-hour trigger sensitivity covers near-expiry returns; this timing is estimated for other versions and is not a documented timer control".into(),
                    "summary-to-resume delay uses that estimated trigger, never boundary time or durationMs; summary input reads the prior warm prefix and writes new context at the outside-main TTL".into(),
                    "hypothetical summaries use cohort median linked billed output and full first resumed cache prefix, including system/project/retained/new context, plus observed incremental growth; maxima supply summary-size sensitivity".into(),
                    "unchanged system/project prefix is estimated from post-compaction reads; actual resumed usage takes precedence for an applied idle; altered prefixes reuse only within their chosen lifetime and cannot inherit external warming beyond the unchanged prefix".into(),
                    "context changes carry forward through later requests and reprice context tiers and later summaries until an observed compaction converges the paths; observed summary usage and unmatched proxies are charged once".into(),
                    "activation transfers only within the same known version/client/flag cohort; without comparable recorded known-on outcomes the deciding fallback leaves the client open and awake; final periods incur one expected summary with no assumed return benefit".into(),
                    "closed/open sensitivity changes unobserved activation once, without a second lifetime probability; a bracketed break-even replays a common uncertain activation fraction, including sequential context changes; no bracket is not proof that no internal crossing exists".into(),
                    "fixed transcript behavior holds outputs, tools and incremental growth constant; lost detail can cause extra re-reads, whose per-resumed-summary break-even is a caveat, not an unbounded veto".into(),
                ],
                uncertainty: uncertainty.into_iter().collect(),
                sources: SOURCES
                    .iter()
                    .chain(PRICE_SOURCES.iter())
                    .map(|s| s.to_string())
                    .collect(),
            },
            bucket: "main",
            threshold: c.threshold,
            current_settings,
            after_recommendations: after,
            usage_estimates: usage_estimates.clone(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controls::{CompactionSources, compaction_controls};
    use crate::model::{
        CacheContext, CheckResult, CompactionCheck, EstimateEvidence, ExposureScan, Location,
        Origin, Pricing, Report, ScanCoverage, TtlSensitivity, WindowBasisKind,
    };
    use crate::reader::{ScanOptions, scan};
    use crate::replay::{AnalyzeOptions, analyze, compaction_charges};
    use crate::report::{ReportOptions, format_report};
    use crate::test_support::{ClaudeUsage, Workspace, claude_usage, make_request, prices};
    use serde_json::json;

    const MINUTE: Millis = 60_000;

    fn usage(write: i64, read: i64, output: i64, ttl: Ttl) -> Usage {
        claude_usage(ClaudeUsage {
            write,
            read,
            output,
            ttl,
            input: 0,
        })
    }

    /// An interactive CLI request in the target session with `tokens` of cold 1h prefix.
    fn request(id: &str, minute: f64, tokens: i64) -> Request {
        let time = (minute * MINUTE as f64) as Millis;
        let mut r = Request {
            id: id.into(),
            session: "target".into(),
            time,
            end_time: time,
            client: Some("cli".into()),
            source: Some("repl_main_thread".into()),
            ..make_request()
        };
        r.parts[0].usage = usage(tokens, 0, 0, Ttl::OneHour);
        r
    }

    fn in_session(mut r: Request, session: &str) -> Request {
        r.session = session.into();
        r
    }

    /// A billed summary request whose only part is a compaction.
    fn summary(id: &str, minute: f64, session: &str) -> Request {
        let mut r = in_session(request(id, minute, 0), session);
        r.parts[0].kind = PartKind::Compaction;
        r.parts[0].usage = usage(0, 300_000, 2000, Ttl::FiveMinutes);
        r
    }

    fn origin(session: &str, line: u64) -> Vec<Origin> {
        vec![Origin {
            session: session.into(),
            location: Location {
                file: "synthetic".into(),
                line,
            },
        }]
    }

    /// A linked compaction boundary in the sample session.
    fn event() -> StructuralEvent {
        StructuralEvent {
            id: "boundary".into(),
            origins: origin("sample", 1),
            harness: Harness::Claude,
            session: "sample".into(),
            time: 2 * MINUTE,
            kind: EventKind::Compaction,
            profiles: vec!["profile".into()],
            location: Location {
                file: "synthetic".into(),
                line: 1,
            },
            request: Some("summary".into()),
            model: Some("claude-opus-5-5".into()),
            version: Some("2.1.314".into()),
            client: Some("cli".into()),
            cwd: None,
            trigger: Some("manual".into()),
            precomputed: Some(false),
            pre_tokens: Some(300_000),
            post_tokens: Some(2000),
            duration_ms: Some(1000),
            tool: None,
            tool_id: None,
            settings: IndexMap::new(),
            idle_attribution: IdleAttribution::Unknown,
            pricing: Some(Pricing {
                model: Some("claude-opus-5-5".into()),
                provider: Some("anthropic".into()),
                tier: Some("standard".into()),
                speed: Some("standard".into()),
                geo: Some("global".into()),
            }),
        }
    }

    /// An event of `kind` in `session` at `minute`.
    fn session_event(id: &str, kind: EventKind, session: &str, minute: f64) -> StructuralEvent {
        StructuralEvent {
            id: id.into(),
            kind,
            session: session.into(),
            origins: origin(session, 2),
            time: (minute * MINUTE as f64) as Millis,
            ..event()
        }
    }

    /// An observed idle boundary in `session` linked to `request`.
    fn notice(id: &str, session: &str, minute: f64, request: Option<&str>) -> StructuralEvent {
        StructuralEvent {
            idle_attribution: IdleAttribution::Notice,
            trigger: Some("auto".into()),
            request: request.map(str::to_string),
            ..session_event(id, EventKind::Compaction, session, minute)
        }
    }

    fn profile() -> Profile {
        let mut user = Map::new();
        user.insert("idleCompaction".into(), Value::Bool(false));
        user.insert("autoCompactWindow".into(), Value::from(400_000));
        let managed = Map::new();
        let env = Env::new();
        let ttl = |value, owner: &str| crate::model::TtlControl {
            value: Some(value),
            owner: owner.into(),
            reason: "explicit".into(),
        };
        Profile {
            id: "profile".into(),
            harness: Harness::Claude,
            root: "synthetic".into(),
            aliases: vec![],
            histories: vec![],
            settings_file: "settings.json".into(),
            ttl: PerBucket {
                main: ttl(Ttl::OneHour, "settings.json:promptCacheTtl"),
                subagent: ttl(Ttl::FiveMinutes, "settings.json:subagentPromptCacheTtl"),
            },
            provider: Some("anthropic".into()),
            settings: IndexMap::from([("idleCompaction".to_string(), Scalar::Bool(false))]),
            mcp_servers: vec![],
            compaction: compaction_controls(&CompactionSources {
                harness: Harness::Claude,
                user: &user,
                managed: &managed,
                env: &env,
                file: "settings.json",
                managed_file: "managed.json",
                scopes: None,
            })
            .unwrap(),
            idle: Some(
                idle_controls(
                    &[SettingsScope {
                        config: &user,
                        file: "settings.json",
                    }],
                    &env,
                )
                .unwrap(),
            ),
            speed: None,
            speed_directories: None,
            idle_directories: None,
        }
    }

    /// The target requests beside one calibration sample: a linked summary and its resume.
    fn data(target: Vec<Request>, boundaries: Vec<StructuralEvent>) -> Scan {
        let mut sample = summary("summary", 1.0, "sample");
        sample.source = Some("compact".into());
        let mut resumed = in_session(request("sample-resume", 3.0, 15_000), "sample");
        resumed.parts[0].usage = usage(15_000, 5000, 0, Ttl::OneHour);
        let mut requests = vec![sample, resumed];
        requests.extend(target);
        requests.sort_by_key(|r| r.time);
        let mut events = vec![event()];
        events.extend(boundaries);
        events.sort_by_key(|e| e.time);
        Scan {
            snapshot_started_at: "1970-01-01T00:00:00.000Z".into(),
            snapshot_finished_at: "1970-01-01T00:00:00.000Z".into(),
            since: None,
            roots: vec![],
            profiles: vec![profile()],
            snapshots: vec![],
            requests,
            events,
            exposure: ExposureScan::default(),
            cache_context: CacheContext::default(),
            coverage: ScanCoverage::default(),
        }
    }

    #[derive(Default)]
    struct Policies {
        ttls: Vec<TtlCheck>,
        windows: Vec<CompactionCheck>,
        estimates: IndexMap<String, UsageEstimate>,
        /// Overrides the scan's window start.
        since: Option<Millis>,
    }

    fn check_with(scan: &Scan, now: f64, policies: &Policies) -> IdleCheck {
        let catalog = prices();
        // Charging converts linked summaries to compaction parts, which the idle replay reads.
        let mut scan = scan.clone();
        let summaries = compaction_charges(&mut scan, &catalog);
        let ttls: Vec<&TtlCheck> = policies.ttls.iter().collect();
        let windows: Vec<&CompactionCheck> = policies.windows.iter().collect();
        idle_checks(&IdleOptions {
            scan: &scan,
            catalog: &catalog,
            summaries: &summaries,
            estimates: &policies.estimates,
            ttls: &ttls,
            windows: &windows,
            since: policies.since.or(scan.since),
            now: (now * MINUTE as f64) as Millis,
        })
        .remove(0)
    }

    /// The full report for `scan`, as the CLI builds it.
    fn report(scan: Scan, now: f64) -> Report {
        analyze(AnalyzeOptions {
            scan,
            catalog: &prices(),
            since: None,
            now: (now * MINUTE as f64) as Millis,
            max_cost_per_use_usd: 1.0,
        })
        .unwrap()
    }

    fn check(scan: &Scan, now: f64) -> IdleCheck {
        check_with(scan, now, &Policies::default())
    }

    fn near(actual: Option<f64>, expected: f64) {
        let actual = actual.expect("a value");
        assert!((actual - expected).abs() < 1e-9, "{actual} != {expected}");
    }

    fn period<'c>(scenario: &'c IdleScenario, request: &str) -> &'c IdlePeriod {
        scenario
            .periods
            .iter()
            .find(|p| p.request == request)
            .expect("period")
    }

    fn base<V>(action: Action, owner: &str, recommended: Option<V>) -> CheckBase<V> {
        CheckBase {
            profile: "profile".into(),
            history_profiles: vec![],
            evidence: Evidence::Estimated,
            action,
            current: None,
            recommended,
            owner: owner.into(),
            since: None,
            until: None,
            window_basis: String::new(),
            baseline: String::new(),
            requests: 0,
            coverage: None,
            savings_usd: None,
            monthly_savings_usd: None,
            break_even: None,
            assumptions: vec![],
            uncertainty: vec![],
            sources: vec![],
        }
    }

    #[test]
    fn on_wins_with_full_prefix_warm_summary_and_expired_resume_until_convergence() {
        let mut follow = request("follow", 201.0, 1000);
        follow.parts[0].usage = usage(1000, 301_000, 0, Ttl::OneHour);
        let s = data(
            vec![
                request("before", 100.0, 300_000),
                request("return", 200.0, 301_000),
                follow,
            ],
            vec![],
        );
        let c = check(&s, 202.0);
        assert_eq!(c.base.action, Action::Change);
        assert_eq!(c.base.recommended, Some(true));
        let a = &c.after_recommendations;
        let p = period(a, "before");
        near(p.summary_cost.total_usd, 0.1);
        let calibration = p.calibration.as_ref().unwrap();
        assert_eq!(calibration.prefix_tokens, 20_000);
        assert_eq!(calibration.summary_tokens, 2000);
        // First replaced prefix: 21k cold writes; following 22k reads 21k and writes 1k.
        // Off: 301k cold writes; following 302k reads 301k and writes 1k.
        near(a.delta_usd, -2.196);
        near(
            Some(a.cost.on.total_usd.unwrap() - a.cost.off.total_usd.unwrap()),
            a.delta_usd.unwrap(),
        );
        assert!(
            a.sensitivity
                .reread_break_even_usd_per_resumed_summary
                .unwrap()
                > 0.0
        );
        assert_eq!(p.activation, Some(1.0));
        assert!(p.activation_basis.contains("fallback"));
        assert_eq!(p.return_timing, ReturnTiming::AfterExpiry);
    }

    #[test]
    fn unresumed_summaries_cost_money_and_recent_finals_are_censored() {
        let mut s = data(vec![request("final", 100.0, 300_000)], vec![]);
        let c = check(&s, 200.0);
        assert_eq!(c.base.recommended, Some(false));
        assert_eq!(c.base.action, Action::Keep);
        let a = &c.after_recommendations;
        near(a.delta_usd, 0.1);
        let last = period(a, "final");
        assert_eq!(last.resume, None);
        near(last.summary_cost.total_usd, 0.1);
        near(a.sensitivity.closed_delta_usd, 0.0);
        near(a.sensitivity.open_delta_usd, 0.1);
        let recent = check(&s, 150.0);
        let recent = &recent.after_recommendations;
        assert_eq!(period(recent, "final").status, PeriodStatus::Censored);
        near(recent.delta_usd, 0.0);
        s.profiles[0].idle.as_mut().unwrap().value = Some(true);
        let off = check(&s, 200.0);
        assert_eq!(off.base.action, Action::Change);
        assert_eq!(off.base.recommended, Some(false));
    }

    #[test]
    fn no_gap_long_enough_is_not_evidence() {
        let target = [0.0, 10.0, 20.0, 30.0]
            .iter()
            .map(|m| request(&format!("r{m}"), m + 100.0, 300_000))
            .collect();
        let mut s = data(target, vec![]);
        s.profiles[0].idle.as_mut().unwrap().value = Some(true);
        let c = check(&s, 140.0);
        near(c.after_recommendations.delta_usd, 0.0);
        assert!(
            c.after_recommendations
                .periods
                .iter()
                .all(|p| matches!(p.status, PeriodStatus::Censored | PeriodStatus::Ineligible))
        );
        assert_eq!(c.base.recommended, None);
        assert_eq!(c.base.action, Action::Advisory);
        let text = format_report(&report(s, 140.0), ReportOptions::default());
        assert!(
            !text.contains("Summarizing idle sessions cost less"),
            "{text}"
        );
        assert!(text.contains("never idle long enough"), "{text}");
    }

    #[test]
    fn external_cache_evidence_and_near_expiry_timing_can_make_on_lose() {
        for minute in [158.0, 200.0] {
            let mut back = request("return", minute, 1000);
            back.parts[0].usage = usage(1000, 300_000, 0, Ttl::OneHour);
            let c = check(
                &data(vec![request("before", 100.0, 300_000), back], vec![]),
                minute + 1.0,
            );
            assert_eq!(c.base.recommended, Some(false));
            let a = &c.after_recommendations;
            near(a.delta_usd, 0.161);
            let expected = if minute == 158.0 {
                ReturnTiming::NearExpiry
            } else {
                ReturnTiming::AfterExpiry
            };
            assert_eq!(period(a, "before").return_timing, expected);
            if minute == 158.0 {
                near(a.sensitivity.late_trigger_delta_usd, 0.0);
            }
        }
    }

    #[test]
    fn aggregate_winner_can_differ_from_most_periods_and_sensitivity_can_cross_zero() {
        let mut target = vec![
            request("before", 100.0, 300_000),
            request("return", 200.0, 301_000),
        ];
        for i in 0..3 {
            let id = format!("final-{i}");
            target.push(in_session(request(&id, 100.0, 300_000), &id));
        }
        let c = check(&data(target, vec![]), 201.0);
        assert_eq!(c.base.recommended, Some(true));
        near(c.after_recommendations.delta_usd, -1.84);
        let mut targets = vec![
            request("before", 100.0, 300_000),
            request("return", 200.0, 21_000),
        ];
        for i in 0..22 {
            let id = format!("uncertain-{i}");
            targets.push(in_session(request(&id, 100.0, 300_000), &id));
        }
        let observed = notice("observed-idle", "target", 199.0, None);
        let mixed = check(&data(targets, vec![observed]), 201.0);
        let sensitivity = &mixed.after_recommendations.sensitivity;
        assert!(sensitivity.closed_delta_usd.unwrap() < 0.0);
        assert!(sensitivity.open_delta_usd.unwrap() > 0.0);
        assert_eq!(mixed.base.recommended, Some(false));
        assert_eq!(mixed.base.action, Action::Keep);
        let fraction = sensitivity.lifetime_break_even_fraction.unwrap();
        assert!(fraction > 0.0 && fraction < 1.0, "{fraction}");
    }

    #[test]
    fn missing_full_prefix_calibration_and_unknown_eligibility_stay_unpriced() {
        for mode in [
            "sample",
            "version",
            "desktop",
            "flags",
            "threshold",
            "window",
            "input",
        ] {
            let mut s = data(vec![request("final", 100.0, 300_000)], vec![]);
            let last = s.requests.len() - 1;
            match mode {
                "sample" => s.requests.retain(|r| r.id != "summary"),
                "version" => s.requests[last].version = None,
                "desktop" => s.requests[last].client = Some("claude-desktop".into()),
                "flags" => s.requests[last].version = Some("2.1.300".into()),
                "threshold" => s.profiles[0].idle.as_mut().unwrap().threshold = None,
                "window" => s.profiles[0].compaction.default.value = None,
                _ => s.requests[last].parts[0].usage.input = None,
            }
            let c = check(&s, 200.0);
            assert_eq!(c.after_recommendations.delta_usd, None, "{mode}");
            assert_eq!(c.base.recommended, None, "{mode}");
            assert_eq!(c.base.action, Action::Advisory, "{mode}");
        }
    }

    #[test]
    fn idle_replay_preserves_comparison_estimates_without_an_explicit_window() {
        let mut s = data(vec![request("estimated", 100.0, 300_000)], vec![]);
        let last = s.requests.len() - 1;
        let part = s.requests[last].parts[0].clone();
        s.requests[last].parts[0].usage.input = None;
        s.profiles[0].compaction.default.value = None;
        let estimate = UsageEstimate {
            request: "estimated".into(),
            evidence: EstimateEvidence::Estimated,
            basis: vec!["synthetic recorded attempt input".into()],
            part: Some(part),
            max_output_tokens: None,
            reason: "comparison-only input estimate".into(),
            assumptions: vec![],
        };
        let policies = Policies {
            estimates: IndexMap::from([("estimated".to_string(), estimate)]),
            since: Some(100 * MINUTE),
            ..Policies::default()
        };
        let c = check_with(&s, 200.0, &policies);
        let current = &c.current_settings;
        assert_eq!(current.periods[0].context_tokens, Some(300_000));
        near(Some(current.cost.off.known_usd), 2.4);
        assert_eq!(current.delta_usd, None);
        assert_eq!(c.usage_estimates.len(), 1);
    }

    #[test]
    fn a_completed_or_replaced_turn_clears_abandoned_tool_waits() {
        for tool_id in [None, Some("abandoned")] {
            for kind in [EventKind::TurnStart, EventKind::TurnEnd] {
                let mut s = data(vec![request("final", 100.0, 300_000)], vec![]);
                s.events.push(StructuralEvent {
                    tool_id: tool_id.map(str::to_string),
                    ..session_event("abandoned", EventKind::ToolStart, "target", 50.0)
                });
                s.events
                    .push(session_event("completion", kind, "target", 90.0));
                s.events.sort_by_key(|e| e.time);
                let c = check(&s, 200.0);
                let a = &c.after_recommendations;
                assert_eq!(period(a, "final").status, PeriodStatus::Eligible);
                near(a.delta_usd, 0.1);
            }
        }
    }

    #[test]
    fn unsupported_clients_and_prerequisites_do_not_emit_an_idle_toggle() {
        for mode in [
            "sdk", "subagent", "tool", "size", "window", "ttl", "disabled", "old",
        ] {
            let mut s = data(vec![request("final", 100.0, 300_000)], vec![]);
            let last = s.requests.len() - 1;
            let r = &mut s.requests[last];
            match mode {
                "sdk" => r.client = Some("sdk-ts".into()),
                "subagent" => r.agent = Agent::Subagent,
                "tool" => {
                    let time = r.time;
                    s.events.push(StructuralEvent {
                        tool_id: Some("tool".into()),
                        time,
                        request: Some("final".into()),
                        ..session_event("tool", EventKind::ToolStart, "target", 0.0)
                    });
                }
                "size" => r.parts[0].usage = usage(100_000, 0, 0, Ttl::FiveMinutes),
                "window" => s.profiles[0].compaction.default.value = Some(200_000),
                "ttl" => s.profiles[0].ttl.main.value = Some(Ttl::FiveMinutes),
                "disabled" => s.profiles[0].idle.as_mut().unwrap().auto_compact = Some(false),
                _ => r.version = Some("2.1.289".into()),
            }
            let c = check(&s, 200.0);
            assert_ne!(c.base.action, Action::Change, "{mode}");
            if mode != "old" {
                near(c.after_recommendations.delta_usd, 0.0);
            }
        }
    }

    #[test]
    fn ttl_and_window_choices_decide_applicability_while_the_current_comparison_is_retained() {
        let s = data(vec![request("final", 100.0, 300_000)], vec![]);
        let ttl = TtlCheck {
            base: base(Action::Change, "", Some(Ttl::FiveMinutes)),
            window_basis_kind: WindowBasisKind::Available,
            bucket: Bucket::Main,
            one_hour_savings_usd: 0.0,
            cost: Default::default(),
            summary_cost: Default::default(),
            comparison_cost: Default::default(),
            fixed_cost: zero_cost(),
            usage_estimates: vec![],
            cache_evidence: vec![],
            sensitivity: TtlSensitivity {
                cold_prefix1h_usd: None,
                cold_prefix1h_comparison_usd: None,
                warm_reuse_break_even_fraction: None,
                unlinked: None,
            },
        };
        let window = CompactionCheck {
            base: base(
                Action::Change,
                "settings.json:autoCompactWindow",
                Some(100_000),
            ),
            bucket: "all",
            model: None,
            models: vec![],
            baseline_cost: zero_cost(),
            observed_summary_cost: zero_cost(),
            ttls: PerBucket::default(),
            scenarios: vec![],
            usage_estimates: vec![],
            cache_evidence: vec![],
        };
        for policies in [
            Policies {
                ttls: vec![ttl],
                ..Policies::default()
            },
            Policies {
                windows: vec![window],
                ..Policies::default()
            },
        ] {
            let c = check_with(&s, 200.0, &policies);
            near(c.current_settings.delta_usd, 0.1);
            assert!(!c.after_recommendations.applicable);
            assert_eq!(c.base.action, Action::Advisory);
            assert_eq!(c.base.recommended, None);
        }
    }

    #[test]
    fn known_applied_idle_uses_measured_summary_once_and_observed_resumed_geometry() {
        let applied = notice("applied", "target", 199.0, Some("idle-summary"));
        let s = data(
            vec![
                request("before", 100.0, 300_000),
                summary("idle-summary", 154.0, "target"),
                request("return", 200.0, 21_000),
            ],
            vec![applied],
        );
        let c = check(&s, 201.0);
        let a = &c.after_recommendations;
        let p = period(a, "before");
        assert_eq!(p.outcome, PeriodOutcome::Applied);
        near(p.summary_cost.total_usd, 0.1);
        near(a.delta_usd, 0.1 - ((300_000 - 21_000) * 8) as f64 / 1e6);
        near(
            Some(a.cost.on.total_usd.unwrap() - a.cost.off.total_usd.unwrap()),
            a.delta_usd.unwrap(),
        );
    }

    #[test]
    fn same_cohort_applied_fraction_charges_finals_once_and_never_transfers_versions() {
        let applied = notice("applied", "yes", 199.0, Some("idle-summary"));
        let mut different = in_session(request("different", 100.0, 300_000), "different");
        different.version = Some("2.1.315".into());
        let mut s = data(
            vec![
                in_session(request("yes", 100.0, 300_000), "yes"),
                summary("idle-summary", 154.0, "yes"),
                in_session(request("yes-return", 200.0, 21_000), "yes"),
                in_session(request("no", 100.0, 300_000), "no"),
                in_session(request("no-return", 200.0, 301_000), "no"),
                in_session(request("final", 100.0, 300_000), "final"),
                different,
            ],
            vec![applied],
        );
        for session in ["yes", "no"] {
            s.events.insert(
                0,
                StructuralEvent {
                    settings: IndexMap::from([
                        ("idleCompaction".to_string(), Scalar::Bool(true)),
                        ("promptCacheTtl".to_string(), Scalar::Text("1h".into())),
                        ("autoCompactEnabled".to_string(), Scalar::Bool(true)),
                    ]),
                    ..session_event(
                        &format!("settings-{session}"),
                        EventKind::Settings,
                        session,
                        99.0,
                    )
                },
            );
        }
        let copy = |id: &str, as_id: &str| Request {
            id: as_id.into(),
            session: "different-sample".into(),
            version: Some("2.1.315".into()),
            ..s.requests.iter().find(|r| r.id == id).unwrap().clone()
        };
        let copies = [
            copy("summary", "different-summary"),
            copy("sample-resume", "different-version-sample"),
        ];
        s.requests.extend(copies);
        s.requests.sort_by_key(|r| r.time);
        s.events.push(StructuralEvent {
            id: "different-boundary".into(),
            session: "different-sample".into(),
            origins: origin("different-sample", 7),
            request: Some("different-summary".into()),
            version: Some("2.1.315".into()),
            ..event()
        });
        s.events.sort_by_key(|e| e.time);
        let c = check(&s, 201.0);
        let a = &c.after_recommendations;
        assert_eq!(a.activation_cohorts[0].fraction, 0.5);
        let last = period(a, "final");
        near(last.activation, 0.5);
        near(last.summary_cost.total_usd, 0.05);
        let no = period(a, "no");
        assert_eq!(no.outcome, PeriodOutcome::NotApplied);
        near(no.summary_cost.total_usd, 0.0);
        let other = period(a, "different");
        near(other.activation, 1.0);
        near(other.summary_cost.total_usd, 0.1);
        assert!(
            c.base
                .uncertainty
                .join(" ")
                .contains("discarded summaries never ran")
        );
    }

    #[test]
    fn a_net_zero_tie_is_not_evidence_for_either_toggle() {
        for current in [false, true] {
            let mut s = data(
                vec![
                    request("before", 100.0, 312_500),
                    request("return", 200.0, 312_500),
                ],
                vec![],
            );
            s.requests[0].parts[0].usage = usage(0, 312_500, 3125, Ttl::FiveMinutes);
            s.requests[1].parts[0].usage = usage(296_875, 0, 0, Ttl::OneHour);
            s.events[0].pre_tokens = Some(312_500);
            s.since = Some(100 * MINUTE);
            s.profiles[0].idle.as_mut().unwrap().value = Some(current);
            let c = check(&s, 201.0);
            near(c.after_recommendations.delta_usd, 0.0);
            assert_eq!(c.base.recommended, None);
            assert_eq!(c.base.action, Action::Advisory);
            let mut report = report(s, 201.0);
            report.checks.retain(|r| matches!(r, CheckResult::Idle(_)));
            report.checks[0] = CheckResult::Idle(c);
            let text = format_report(&report, ReportOptions::default());
            assert!(text.contains("On and off cost the same"), "{text}");
        }
    }

    #[test]
    fn later_observed_compaction_reprices_carried_summary_input_once_and_converges() {
        let mut later = request("later-summary", 220.0, 0);
        later.parts[0].kind = PartKind::Compaction;
        later.parts[0].usage = usage(0, 302_000, 2000, Ttl::FiveMinutes);
        let boundary = StructuralEvent {
            request: Some("later-summary".into()),
            pre_tokens: Some(302_000),
            ..session_event("later", EventKind::Compaction, "target", 220.5)
        };
        let mut follow = request("follow", 210.0, 1000);
        follow.parts[0].usage = usage(1000, 301_000, 0, Ttl::OneHour);
        let s = data(
            vec![
                request("before", 100.0, 300_000),
                request("return", 200.0, 301_000),
                follow,
                later,
                request("converged", 221.0, 20_000),
            ],
            vec![boundary],
        );
        let c = check(&s, 222.0);
        let a = &c.after_recommendations;
        near(a.delta_usd, -2.252);
        near(
            Some(a.cost.on.total_usd.unwrap() - a.cost.off.total_usd.unwrap()),
            -2.252,
        );
    }

    #[test]
    fn recorded_final_idle_summaries_are_charged_without_a_resumed_prefix_estimate() {
        let applied = notice("applied-final", "target", 155.0, Some("idle-summary"));
        let s = data(
            vec![
                request("final", 100.0, 300_000),
                summary("idle-summary", 154.0, "target"),
            ],
            vec![applied],
        );
        let c = check(&s, 200.0);
        near(c.after_recommendations.delta_usd, 0.1);
        assert_eq!(c.base.recommended, Some(false));
        assert_eq!(
            period(&c.after_recommendations, "final").outcome,
            PeriodOutcome::Applied
        );
    }

    #[test]
    fn context_or_cohort_changes_after_a_summary_stay_unpriced_and_active_requests_are_ineligible()
    {
        for mode in ["decrease", "model", "active"] {
            let mut before = request("before", 100.0, 300_000);
            let tokens = if mode == "decrease" { 150_000 } else { 301_000 };
            let mut back = request("return", 200.0, tokens);
            if mode == "model" {
                back.parts[0].model = Some("claude-haiku-5-5".into());
            }
            if mode == "active" {
                before.end_time = 170 * MINUTE;
            }
            let c = check(&data(vec![before, back], vec![]), 201.0);
            let a = &c.after_recommendations;
            if mode == "active" {
                near(a.delta_usd, 0.0);
                assert_eq!(period(a, "before").status, PeriodStatus::Ineligible);
            } else {
                assert_eq!(a.delta_usd, None, "{mode}");
                assert_eq!(c.base.action, Action::Advisory, "{mode}");
            }
        }
    }

    #[test]
    fn selected_windows_do_not_charge_pre_window_triggers_or_generation() {
        let mut s = data(
            vec![
                request("before", 100.0, 300_000),
                request("return", 200.0, 301_000),
            ],
            vec![],
        );
        s.since = Some(160 * MINUTE);
        let c = check(&s, 201.0);
        near(c.after_recommendations.delta_usd, 0.0);
        assert!(
            !c.after_recommendations
                .periods
                .iter()
                .any(|p| p.request == "before")
        );
    }

    #[test]
    fn by_context_groups_sizes_in_first_seen_order_across_sessions() {
        let target = vec![
            in_session(request("a", 100.0, 300_000), "a"),
            in_session(request("b", 100.0, 250_000), "b"),
            in_session(request("c", 100.0, 300_000), "c"),
        ];
        let c = check(&data(target, vec![]), 200.0);
        let a = &c.after_recommendations;
        let order: Vec<&str> = a.periods.iter().map(|p| p.request.as_str()).collect();
        assert_eq!(order, ["sample-resume", "a", "b", "c"]);
        let groups: Vec<(Option<i64>, u64)> =
            a.by_context.iter().map(|g| (g.tokens, g.periods)).collect();
        assert_eq!(
            groups,
            [(Some(20_000), 1), (Some(300_000), 2), (Some(250_000), 1)]
        );
        // Each final period only adds its summary, so a size's delta sums its own periods'.
        let summary = |id| period(a, id).summary_cost.total_usd.unwrap();
        near(a.by_context[1].delta_usd, summary("a") + summary("c"));
        near(a.by_context[2].delta_usd, summary("b"));
        assert!(summary("b") < summary("a"));
    }

    #[test]
    fn tied_ticks_put_boundaries_first_in_reverse_list_order() {
        // Requests, then boundaries, then idle triggers, as the tick list is built.
        let mut ticks = [
            ("request", tick_key(5, false, 0)),
            ("first boundary", tick_key(5, true, 1)),
            ("second boundary", tick_key(5, true, 2)),
            ("earlier boundary", tick_key(4, true, 3)),
            ("idle", tick_key(5, false, 4)),
        ];
        ticks.sort_by_key(|(_, key)| *key);
        let order: Vec<&str> = ticks.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            order,
            [
                "earlier boundary",
                "second boundary",
                "first boundary",
                "request",
                "idle"
            ]
        );
    }

    #[test]
    fn merge_combines_equal_paths_in_first_seen_order_and_drops_zero_weights() {
        let path = |offset| Path {
            offset,
            prefix: None,
            time: None,
            lifetime: HOUR,
            retained: 0,
            active: None,
            input: None,
            cohort: None,
        };
        let state = |weight, offset| State {
            weight,
            path: path(offset),
        };
        let merged = merge(vec![
            state(0.25, 2),
            state(0.0, 3),
            state(0.5, 1),
            state(0.25, 2),
        ]);
        let result: Vec<(f64, i64)> = merged.iter().map(|s| (s.weight, s.path.offset)).collect();
        assert_eq!(result, [(0.5, 2), (0.5, 1)]);
    }

    #[test]
    fn versions_must_be_three_numeric_parts() {
        assert!(supported(Some("2.1.314"), 314));
        assert!(supported(Some("3.0.0"), 314));
        assert!(supported(Some("2.2.0"), 999));
        assert!(!supported(Some("2.1.313"), 314));
        assert!(!supported(Some("2.1.314-beta"), 314));
        assert!(!supported(Some("2.1"), 1));
        assert!(!supported(Some("2.1.3.4"), 1));
        assert!(!supported(Some(" 2.1.314"), 314));
        assert!(!supported(None, 0));
        assert!(supported(Some("02.01.0314"), 314));
    }

    #[test]
    fn native_notices_are_the_only_parsed_attribution_and_project_owners_and_threshold_floor_resolve()
     {
        let w = Workspace::new();
        let text = |path: &std::path::Path| path.to_string_lossy().into_owned();
        let project = w.home.join("project");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        std::fs::create_dir(project.join(".claude")).unwrap();
        let user =
            json!({"idleCompaction": true, "autoCompactWindow": 400000, "promptCacheTtl": "1h"});
        std::fs::write(w.claude.join("settings.json"), user.to_string()).unwrap();
        let local = json!({
            "idleCompaction": false,
            "env": {"CLAUDE_CODE_IDLE_COMPACT_MIN_TOKENS": "90000"},
            "promptCacheTtl": "5m",
        });
        let local_file = project.join(".claude").join("settings.local.json");
        std::fs::write(&local_file, local.to_string()).unwrap();
        let common = |mut row: Value| {
            row["sessionId"] = json!("native");
            row["version"] = json!("2.1.316");
            row["entrypoint"] = json!("cli");
            row["cwd"] = json!(text(&project));
            row
        };
        let request = common(json!({
            "type": "assistant",
            "querySource": "repl_main_thread",
            "timestamp": "1970-01-01T00:00:00.000Z",
            "message": {
                "id": "request",
                "model": "claude-opus-5-5",
                "usage": {
                    "input_tokens": 0,
                    "output_tokens": 0,
                    "cache_read_input_tokens": 0,
                    "cache_creation_input_tokens": 300000,
                    "cache_creation": {
                        "ephemeral_1h_input_tokens": 300000,
                        "ephemeral_5m_input_tokens": 0,
                    },
                    "service_tier": "standard",
                },
            },
        }));
        let boundary = common(json!({
            "type": "system",
            "subtype": "compact_boundary",
            "uuid": "boundary",
            "timestamp": "1970-01-01T00:00:01.000Z",
            "compactMetadata": {
                "trigger": "auto",
                "preTokens": 300000,
                "postTokens": 2000,
                "precomputed": true,
            },
        }));
        let notice = common(json!({
            "type": "system",
            "subtype": "informational",
            "level": "notice",
            "uuid": "notice",
            "parentUuid": "boundary",
            "timestamp": "1970-01-01T00:00:02.000Z",
            "content": "Compacted while idle, before the prompt cache expired · turn off with \"idleCompaction\": false in settings.json",
        }));
        let file = w.claude.join("projects/project/native.jsonl");
        let write = |rows: &[&Value]| {
            let lines: String = rows.iter().map(|r| format!("{r}\n")).collect();
            std::fs::write(&file, lines).unwrap();
        };
        let attribution = |scan: &Scan| {
            scan.events
                .iter()
                .find(|e| e.kind == EventKind::Compaction)
                .unwrap()
                .idle_attribution
        };

        write(&[&request, &boundary, &notice]);
        let parsed = scan(&w.scan_options()).unwrap();
        assert_eq!(attribution(&parsed), IdleAttribution::Notice);
        let profile = parsed
            .profiles
            .iter()
            .find(|p| p.harness == Harness::Claude)
            .unwrap();
        let owner = &profile.idle_directories.as_ref().unwrap()[text(&project).as_str()];
        assert_eq!(owner.value, Some(false));
        assert_eq!(owner.threshold, Some(100_000));
        assert_eq!(owner.owner, format!("{}:idleCompaction", text(&local_file)));
        assert_eq!(
            owner.ttl.as_ref().unwrap().main.value,
            Some(Ttl::FiveMinutes)
        );
        let full = report(parsed, 100.0);
        assert!(
            !serde_json::to_string(&full)
                .unwrap()
                .contains("Compacted while idle")
        );
        assert!(format_report(&full, ReportOptions::default()).contains("Idle compaction"));
        let details = ReportOptions {
            details: true,
            ..ReportOptions::default()
        };
        assert!(format_report(&full, details).contains("idle-compaction"));

        // The walk from a notice passes through the boundary's summary message.
        let summary = common(json!({
            "type": "user",
            "uuid": "compact-summary",
            "parentUuid": "boundary",
            "timestamp": "1970-01-01T00:00:01.500Z",
            "isCompactSummary": true,
            "message": {"role": "user", "content": "SYNTHETIC_PRIVATE_SUMMARY"},
        }));
        let mut linked = notice.clone();
        linked["parentUuid"] = json!("compact-summary");
        write(&[&request, &boundary, &summary, &linked]);
        let linked = scan(&w.scan_options()).unwrap();
        assert_eq!(attribution(&linked), IdleAttribution::Notice);

        write(&[&request, &boundary]);
        let absent = scan(&w.scan_options()).unwrap();
        assert_eq!(attribution(&absent), IdleAttribution::Unknown);

        // A notice on another branch must not relabel an old boundary, even in the same session.
        for trigger in ["auto", "manual"] {
            let mut old = boundary.clone();
            old["compactMetadata"]["trigger"] = json!(trigger);
            let mut orphan = notice.clone();
            orphan["parentUuid"] = json!("missing-boundary");
            orphan["timestamp"] = json!("1970-01-01T02:00:00.000Z");
            write(&[&request, &old, &orphan]);
            let orphan = scan(&w.scan_options()).unwrap();
            assert_eq!(attribution(&orphan), IdleAttribution::Unknown, "{trigger}");
        }

        let mut untimed = notice.clone();
        untimed.as_object_mut().unwrap().remove("timestamp");
        write(&[&untimed]);
        let error = scan(&w.scan_options()).unwrap_err();
        assert!(
            error.message.contains("missing idle notice timestamp"),
            "{}",
            error.message
        );

        // A nested directory inherits its project's owner, so it is not a policy change.
        let nested = project.join("nested");
        std::fs::create_dir(&nested).unwrap();
        let mut moved = request.clone();
        moved["cwd"] = json!(text(&nested));
        moved["timestamp"] = json!("1970-01-01T01:00:00.000Z");
        moved["message"]["id"] = json!("selected");
        write(&[&request, &moved]);
        let selected = scan(&ScanOptions {
            since: Some(MINUTE),
            ..w.scan_options()
        })
        .unwrap();
        let profile = selected
            .profiles
            .iter()
            .find(|p| p.harness == Harness::Claude)
            .unwrap();
        let ttl = &profile.idle_directories.as_ref().unwrap()[text(&project).as_str()].ttl;
        assert_eq!(ttl.as_ref().unwrap().main.value, Some(Ttl::FiveMinutes));
        let selected = report(selected, 200.0);
        let idle = selected
            .checks
            .iter()
            .find_map(|c| match c {
                CheckResult::Idle(c) => Some(c),
                _ => None,
            })
            .expect("an idle-compaction check");
        let unknown = idle.current_settings.unknown.join(" ");
        assert!(
            !unknown.contains("policy changes within a conversation"),
            "{unknown}"
        );
    }
}
