//! Compaction-window checks: calibrate summaries from linked billed usage, simulate earlier
//! compactions under candidate windows, and compare them after the recommended TTLs.

use crate::collections::{HashMap, HashSet, IndexMap, IndexSet};
use std::collections::BTreeSet;
use std::iter;

use crate::cache::{CODEX_CACHE_MILLISECONDS, CacheBoundaries, ttl_milliseconds};
use crate::controls::{CLAUDE_WINDOW_MAXIMUM, CLAUDE_WINDOW_MINIMUM, window_control};
use crate::discovery::locale_compare;
use crate::model::{
    Action, AddedCompaction, Agent, BillingPart, Bucket, CacheReplayEvidence, CheckBase,
    CompactionBasis, CompactionCharge, CompactionCheck, Cost, Coverage, EventKind, Evidence,
    Harness, Id, Millis, Origin, PartKind, PerBucket, Request, RequestEvidence, Scalar, Scan,
    StructuralEvent, Text, Ttl, TtlCheck, Usage, UsageEstimate, WindowBasisKind, WindowControl,
    WindowScenario, WindowSensitivity,
};
use crate::prices::{
    Catalog, PRICE_SOURCES, Sum, TIER_SENSITIVITY, add_costs, paired_savings, price_components,
    price_part, server_cost, tier_sensitive, unknown_cost, write_billing, zero_cost,
};
use crate::scenarios::{ReplayOptions, ReplayTtls, ReplayedPart, replay_parts};

const SOURCES: [&str; 7] = [
    "https://code.claude.com/docs/en/model-config#context-window-and-auto-compaction (2026-11-07)",
    "https://code.claude.com/docs/en/model-config#default-auto-compact-thresholds (2026-11-11)",
    "https://platform.claude.com/docs/en/build-with-claude/context-windows (2026-11-11)",
    "https://platform.claude.com/docs/en/build-with-claude/compaction (2026-11-11)",
    "https://code.claude.com/docs/en/settings-reference#modelsettings (2026-11-07)",
    "https://developers.openai.com/codex/config-reference (2026-11-07)",
    "https://developers.openai.com/api/docs/guides/compaction (2026-11-07)",
];

/// A request as recorded, or a boxed copy with modeled usage.
///
/// Simulations hold one per request of a profile's whole history and change few, so a
/// recorded request costs a reference rather than a request's size.
#[derive(Clone, Debug, PartialEq)]
pub enum Modeled<'r> {
    Recorded(&'r Request),
    Changed(Box<Request>),
}

impl Modeled<'_> {
    /// The request, copied first if it is still the recorded one.
    pub fn to_mut(&mut self) -> &mut Request {
        if let Modeled::Recorded(r) = self {
            *self = Modeled::Changed(Box::new((*r).clone()));
        }
        match self {
            Modeled::Changed(r) => r,
            Modeled::Recorded(_) => unreachable!("copied above"),
        }
    }
}

impl std::ops::Deref for Modeled<'_> {
    type Target = Request;
    fn deref(&self) -> &Request {
        match self {
            Modeled::Recorded(r) => r,
            Modeled::Changed(r) => r,
        }
    }
}

impl AsRef<Request> for Modeled<'_> {
    fn as_ref(&self) -> &Request {
        self
    }
}

/// Linked summary output and the full first post-compaction cache prefix.
#[derive(Clone, Debug, PartialEq)]
pub struct CompactionSample<'a> {
    /// The observed compaction boundary.
    pub event: &'a Id,
    /// The first request resumed after it, and its only message part.
    pub request: &'a Request,
    pub p: &'a BillingPart,
    /// Billed summary output tokens.
    pub output: i64,
    /// The resumed request's full cache prefix.
    pub prefix: i64,
    /// The linked request and its only compaction part.
    pub summary_request: &'a Request,
    pub summary: &'a BillingPart,
}

/// The conversation cohort a sample can calibrate; every field must match exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct SampleCohort<'a> {
    model: &'a str,
    provider: &'a str,
    agent: Agent,
    tier: Option<&'a str>,
    speed: Option<&'a str>,
    geo: Option<&'a str>,
}

impl<'a> SampleCohort<'a> {
    /// None when the part cannot match any sample: an unrecorded model, provider or agent.
    fn of(r: &'a Request, p: &'a BillingPart) -> Option<Self> {
        if r.agent == Agent::Unknown {
            return None;
        }
        Some(Self {
            model: p.model.as_deref()?,
            provider: r.provider.as_deref()?,
            agent: r.agent,
            tier: p.tier.as_deref(),
            speed: p.speed.as_deref(),
            geo: p.geo.as_deref(),
        })
    }
}

/// The samples of one conversation cohort and the geometry every added pass derives from them,
/// computed once rather than per affected request.
#[derive(Clone, Debug)]
struct CohortSamples {
    /// Indexes into the samples, in sample order; never empty.
    indexes: Vec<usize>,
    /// Whether every sample agrees on the summary model, provider, tier, speed and geography.
    uniform_summary: bool,
    median_output: i64,
    median_prefix: i64,
    max_output: i64,
    max_prefix: i64,
    basis: Vec<CompactionBasis>,
}

/// Calibration samples in order, indexed by conversation cohort.
#[derive(Clone, Debug, Default)]
pub struct CompactionSamples<'a> {
    pub samples: Vec<CompactionSample<'a>>,
    /// Each cohort's position in `cohorts`.
    index: HashMap<SampleCohort<'a>, usize>,
    cohorts: Vec<CohortSamples>,
}

impl<'a> CompactionSamples<'a> {
    /// Index samples by cohort, keeping their order within each cohort.
    pub fn new(samples: Vec<CompactionSample<'a>>) -> Self {
        let mut index: HashMap<SampleCohort<'a>, usize> = HashMap::default();
        let mut groups: Vec<Vec<usize>> = vec![];
        for (i, s) in samples.iter().enumerate() {
            if let Some(cohort) = SampleCohort::of(s.request, s.p) {
                let position = *index.entry(cohort).or_insert_with(|| {
                    groups.push(vec![]);
                    groups.len() - 1
                });
                groups[position].push(i);
            }
        }
        let cohorts = groups
            .into_iter()
            .map(|indexes| {
                let matches: Vec<&CompactionSample> =
                    indexes.iter().map(|&i| &samples[i]).collect();
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
                CohortSamples {
                    uniform_summary: summaries.len() == 1,
                    median_output: median(matches.iter().map(|s| s.output).collect()),
                    median_prefix: median(matches.iter().map(|s| s.prefix).collect()),
                    max_output: matches.iter().fold(0, |n, s| n.max(s.output)),
                    max_prefix: matches.iter().fold(0, |n, s| n.max(s.prefix)),
                    basis: matches
                        .iter()
                        .map(|s| CompactionBasis {
                            event: s.event.clone(),
                            request: s.request.id.clone(),
                        })
                        .collect(),
                    indexes,
                }
            })
            .collect();
        Self {
            samples,
            index,
            cohorts,
        }
    }

    fn cohort(&self, r: &Request, p: &BillingPart) -> Option<&CohortSamples> {
        // The lookup key borrows the request, so copy the position out before borrowing a cohort.
        let index = *self.index.get(&SampleCohort::of(r, p)?)?;
        Some(&self.cohorts[index])
    }
}

/// Compaction boundary times of every harness in each origin session, sorted.
#[derive(Clone, Debug, Default)]
struct SessionBoundaries {
    times: HashMap<String, Vec<Millis>>,
}

impl SessionBoundaries {
    fn new<'e>(events: impl IntoIterator<Item = &'e StructuralEvent>) -> Self {
        let mut times: HashMap<String, Vec<Millis>> = HashMap::default();
        for event in events {
            if event.kind != EventKind::Compaction {
                continue;
            }
            for origin in &event.origins {
                times
                    .entry(origin.session.to_string())
                    .or_default()
                    .push(event.time);
            }
        }
        for list in times.values_mut() {
            list.sort_unstable();
        }
        Self { times }
    }

    /// The earliest boundary in `session` strictly after `time`.
    fn first_after(&self, session: &str, time: Millis) -> Option<Millis> {
        let list = self.times.get(session)?;
        list.get(list.partition_point(|&t| t <= time)).copied()
    }

    /// Whether a boundary in `session` falls in `(after, until]`.
    fn crossed(&self, session: &str, after: Millis, until: Millis) -> bool {
        self.first_after(session, after).is_some_and(|t| t <= until)
    }
}

/// One profile's structural events, indexed once for every window simulation and replay.
pub struct WindowEvents<'a> {
    /// The first event with each identity.
    by_id: HashMap<&'a Id, &'a StructuralEvent>,
    boundaries: SessionBoundaries,
    /// Claude cache-reset boundaries for `replay_parts` over these events.
    pub cache: CacheBoundaries,
}

impl<'a> WindowEvents<'a> {
    /// Index these events by identity and by compaction boundary.
    pub fn new(events: impl IntoIterator<Item = &'a StructuralEvent>) -> Self {
        let events: Vec<&'a StructuralEvent> = events.into_iter().collect();
        let mut by_id = HashMap::default();
        for &event in &events {
            by_id.entry(&event.id).or_insert(event);
        }
        Self {
            by_id,
            boundaries: SessionBoundaries::new(events.iter().copied()),
            cache: CacheBoundaries::new(events.iter().copied()),
        }
    }

    /// Keep calibration identities while removing idle boundaries from an idle-off policy.
    pub(crate) fn suppress_idle(&mut self, since: Millis) {
        let events: Vec<_> = self
            .by_id
            .values()
            .copied()
            .filter(|e| {
                e.idle_attribution != crate::model::IdleAttribution::Notice || e.time < since
            })
            .collect();
        self.boundaries = SessionBoundaries::new(events.iter().copied());
        self.cache = CacheBoundaries::new(events);
    }
}

fn is_message(p: &BillingPart) -> bool {
    p.kind == PartKind::Message
}

fn is_conversation(p: &BillingPart) -> bool {
    matches!(p.kind, PartKind::Message | PartKind::Unknown)
}

fn median(mut values: Vec<i64>) -> i64 {
    values.sort_unstable();
    let mid = values.len() / 2;
    if values.len() % 2 == 1 {
        values[mid]
    } else {
        ((values[mid - 1] + values[mid]) as f64 / 2.0).ceil() as i64
    }
}

/// The cache prefix a request carried: all Codex input, or Claude reads plus writes.
fn prefix(harness: Harness, u: &Usage) -> Option<i64> {
    match harness {
        Harness::Codex => u.input,
        Harness::Claude => match (u.read, u.write) {
            (Some(read), Some(write)) => Some(read + write),
            _ => None,
        },
    }
}

/// The first request in `list` (indexes into `requests`, in array order) inside `(after, before)`.
fn first_between(
    requests: &[&Request],
    list: &[usize],
    sorted: bool,
    after: Millis,
    before: Option<Millis>,
) -> Option<usize> {
    let inside = |i: usize| {
        let t = requests[i].time;
        t > after && before.is_none_or(|b| t < b)
    };
    if sorted {
        // Later entries are no earlier, so only the first one after `after` can qualify.
        let start = list.partition_point(|&i| requests[i].time <= after);
        list.get(start).copied().filter(|&i| inside(i))
    } else {
        list.iter().copied().find(|&i| inside(i))
    }
}

/// Calibrate summaries from billed output and full resumed requests, never postTokens alone.
pub fn compaction_samples<'a>(
    requests: &[&'a Request],
    events: &[&'a StructuralEvent],
    summaries: &[CompactionCharge],
) -> CompactionSamples<'a> {
    let mut result = Vec::new();
    // Later duplicates win, like a JavaScript Map built from the array.
    let by_id: HashMap<&str, &'a Request> = requests.iter().map(|r| (&*r.id, *r)).collect();
    let mut charges: HashMap<&Id, &CompactionCharge> = HashMap::default();
    for charge in summaries {
        charges.entry(&charge.event).or_insert(charge);
    }
    let boundaries = SessionBoundaries::new(events.iter().copied());
    // Requests with a message part, per session in array order, and whether each list is
    // also in time order so the first match can be found by bisection.
    let mut sessions: HashMap<&str, (Vec<usize>, bool)> = HashMap::default();
    for (index, r) in requests.iter().enumerate() {
        if !r.parts.iter().any(is_message) {
            continue;
        }
        let (list, sorted) = sessions.entry(&*r.session).or_insert((vec![], true));
        if let Some(&last) = list.last() {
            *sorted &= requests[last].time <= r.time;
        }
        list.push(index);
    }
    for &event in events.iter().filter(|e| e.kind == EventKind::Compaction) {
        let aliases: IndexSet<&str> = event.origins.iter().map(|o| &*o.session).collect();
        let next_boundary = aliases
            .iter()
            .filter_map(|alias| boundaries.first_after(alias, event.time))
            .min();
        let Some(next) = aliases
            .iter()
            .filter_map(|alias| {
                let (list, sorted) = sessions.get(alias)?;
                first_between(requests, list, *sorted, event.time, next_boundary)
            })
            .min()
            .map(|i| requests[i])
        else {
            continue;
        };
        let mut messages = next.parts.iter().filter(|p| is_message(p));
        let (Some(p), None) = (messages.next(), messages.next()) else {
            continue;
        };
        // An event without its own model or pricing metadata matches no part, even a model-less one.
        let model = match (&event.model, &event.pricing) {
            (Some(model), _) => Some(Some(&**model)),
            (None, Some(pricing)) => Some(pricing.model.as_deref()),
            (None, None) => None,
        };
        if model != Some(p.model.as_deref()) {
            continue;
        }
        let Some(size) = prefix(next.harness, &p.usage) else {
            continue;
        };
        if size <= 0 || next.evidence != RequestEvidence::Measured {
            continue;
        }
        let charge = charges.get(&event.id);
        let key = charge
            .and_then(|c| c.request.as_deref())
            .or(event.request.as_deref())
            .unwrap_or("");
        let linked = by_id.get(key).copied();
        let mut parts = linked
            .into_iter()
            .flat_map(|r| r.parts.iter())
            .filter(|p| p.kind == PartKind::Compaction);
        let summary = match (parts.next(), parts.next()) {
            (Some(only), None) => Some(only),
            _ => None,
        };
        // The resumed context can contain retained items and instructions. It is not
        // evidence of billed summary output; unmatched proxies stay in the baseline.
        let (Some(linked), Some(summary)) = (linked, summary) else {
            continue;
        };
        let Some(output) = summary.usage.output.filter(|&o| o > 0) else {
            continue;
        };
        result.push(CompactionSample {
            event: &event.id,
            request: next,
            p,
            output,
            prefix: size,
            summary_request: linked,
            summary,
        });
    }
    CompactionSamples::new(result)
}

/// A summary pass reading `input` tokens at ordinary rates, optionally as a cold cache write.
fn summary_part(sample: &CompactionSample, input: i64, output: i64, cold: bool) -> BillingPart {
    let mut u = Usage {
        input: Some(input),
        output: Some(output),
        reasoning: None,
        read: Some(0),
        write: Some(0),
        write5m: Some(0),
        write1h: Some(0),
        write30m: Some(0),
        write12h: Some(0),
    };
    if cold {
        u.write = Some(input);
        if sample.summary_request.harness == Harness::Claude {
            u.write1h = Some(input);
        } else if write_billing(sample.summary.model.as_deref().unwrap_or("")) == Some(true) {
            u.write12h = Some(input);
        } else {
            u.write = Some(0);
        }
    }
    BillingPart {
        kind: PartKind::Compaction,
        usage: u,
        ..sample.summary.clone()
    }
}

fn output_unit(catalog: &Catalog, item: &ReplayedPart) -> Option<f64> {
    let p = BillingPart {
        usage: Usage {
            output: Some(1),
            ..item.p.usage
        },
        ..item.p.clone()
    };
    price_components(catalog, item.request, &p).output.total_usd
}

/// Requests after modeled compactions, with what the simulation could not establish.
#[derive(Clone, Debug, Default)]
pub struct Simulation<'r> {
    /// Recorded requests are unchanged; changed ones carry applied estimates or new geometry.
    pub requests: Vec<Modeled<'r>>,
    /// Whether any window ran. When false, `requests` are the inputs and usage estimates are
    /// not applied yet, so replays still need them.
    pub applied: bool,
    pub added: Vec<AddedCompaction>,
    pub errors: Vec<&'static str>,
    /// Sessions whose replay is unknown from this time on; other sessions stay priced.
    pub uncertain: IndexMap<Text, Millis>,
    pub affected: IndexSet<Text>,
    /// Modeled compaction boundaries, to replay after the given events.
    pub events: Vec<StructuralEvent>,
    pub estimated_parts: HashSet<(Text, usize)>,
}

impl Simulation<'_> {
    fn fail(&mut self, session: &Text, time: Millis, reason: &'static str) {
        self.errors.push(reason);
        if !self.uncertain.contains_key(session) {
            self.uncertain.insert(session.clone(), time);
        }
    }
}

/// Inputs shared by every window simulation of one profile.
#[derive(Clone)]
pub struct WindowOptions<'a> {
    pub events: &'a WindowEvents<'a>,
    pub estimates: &'a IndexMap<Text, UsageEstimate>,
    pub catalog: &'a Catalog,
    pub samples: &'a CompactionSamples<'a>,
    /// Requests at or after this time are selected; earlier ones only inform state.
    pub since: Millis,
    pub ttls: PerBucket<Option<Ttl>>,
}

/// One candidate window applied to the models it owns.
#[derive(Clone, Copy, Debug)]
pub struct WindowSetting<'m> {
    pub value: Option<i64>,
    pub models: &'m [Option<Text>],
}

#[derive(Clone, Copy)]
struct Pass<'m> {
    window: i64,
    models: &'m [Option<Text>],
    larger: bool,
    cold: bool,
}

/// Provider, model, agent and speed of the context a modeled removal belongs to.
#[derive(Clone, Debug, PartialEq)]
struct ContextCohort {
    provider: Option<Text>,
    model: Option<Text>,
    agent: Agent,
    speed: Option<Text>,
}

impl ContextCohort {
    fn of(r: &Request, p: &BillingPart) -> Self {
        Self {
            provider: r.provider.clone(),
            model: p.model.clone(),
            agent: r.agent,
            speed: p.speed.clone(),
        }
    }

    fn matches(&self, r: &Request, p: &BillingPart) -> bool {
        self.provider == r.provider
            && self.model == p.model
            && self.agent == r.agent
            && self.speed == p.speed
    }
}

struct WindowState {
    removed: i64,
    input: Option<i64>,
    cohort: ContextCohort,
    time: Millis,
    prefix: Option<i64>,
}

fn simulate<'r>(
    requests: Vec<Modeled<'r>>,
    extra: &[StructuralEvent],
    estimates: Option<&IndexMap<Text, UsageEstimate>>,
    pass: Pass,
    options: &WindowOptions,
) -> Simulation<'r> {
    let Pass {
        window,
        models,
        larger,
        cold,
    } = pass;
    let catalog = options.catalog;
    let mut result = Simulation {
        requests: Vec::with_capacity(requests.len()),
        applied: true,
        ..Simulation::default()
    };
    let extra = SessionBoundaries::new(extra);
    let mut states: HashMap<Text, WindowState> = HashMap::default();
    for source in requests {
        let billing = estimates
            .and_then(|e| e.get(&source.id))
            .and_then(|e| e.part.as_ref());
        let mut req = match billing {
            Some(billing) => {
                let mut clone = source;
                let changed = clone.to_mut();
                changed.evidence = RequestEvidence::Estimated;
                changed.parts = vec![billing.clone()];
                clone
            }
            None => source,
        };
        // `state` stands for the JavaScript object in the session map: one taken from the map
        // is written back even when a part bails out, while a fresh one replaces the map entry
        // only once a part completes.
        let (key, previous) = match states.remove_entry(&req.session) {
            Some((key, state)) => (key, Some(state)),
            None => (req.session.clone(), None),
        };
        let crossed = previous.as_ref().is_some_and(|s| {
            options
                .events
                .boundaries
                .crossed(&req.session, s.time, req.time)
                || extra.crossed(&req.session, s.time, req.time)
        });
        let (mut state, kept) = if crossed {
            (None, previous)
        } else {
            (previous, None)
        };
        let mut aliased = state.is_some();
        let messages = req.parts.iter().filter(|p| is_conversation(p)).count();
        for index in 0..req.parts.len() {
            let original = &req.parts[index];
            if !is_conversation(original) {
                continue;
            }
            let kind = original.kind;
            let eligible = original.model.is_none() || models.contains(&original.model);
            let bills_writes = write_billing(original.model.as_deref().unwrap_or("")) == Some(true);
            let u = original.usage;
            let st = state.get_or_insert_with(|| WindowState {
                removed: 0,
                input: None,
                cohort: ContextCohort::of(&req, &req.parts[index]),
                time: req.time,
                prefix: None,
            });
            let selected = req.time >= options.since;
            let old_prefix = prefix(req.harness, &u);
            // Unknown usage could cross any window; it is not an unaffected zero-cost request.
            if selected
                && eligible
                && u.input.is_none_or(|input| {
                    (old_prefix.is_none() || kind == PartKind::Unknown)
                        && (st.removed > 0 || input > window)
                })
            {
                result.fail(
                    &req.session,
                    req.time,
                    "missing input/cache geometry or iteration semantics can affect window eligibility",
                );
                st.input = None;
                continue;
            }
            let same_cohort = st.cohort.matches(&req, &req.parts[index]);
            if st.removed > 0
                && (messages != 1
                    || !same_cohort
                    || u.input.is_none()
                    || matches!((st.input, u.input), (Some(before), Some(now)) if now < before))
            {
                result.fail(
                    &req.session,
                    req.time,
                    "compacted context crosses multipart, model/agent/provider/speed changes or an unexplained input decrease",
                );
                st.removed = 0;
            }
            if st.removed > 0 && old_prefix.is_some_and(|old| old < st.removed) {
                result.fail(
                    &req.session,
                    req.time,
                    "recorded prefix cannot contain the modeled removed context",
                );
                st.removed = 0;
            }
            let mut added: Option<usize> = None;
            if selected
                && eligible
                && let Some(input) = u.input
                && input - st.removed > window
            {
                let matches = options.samples.cohort(&req, &req.parts[index]);
                match (old_prefix, matches) {
                    (Some(old_prefix), Some(matches)) if messages == 1 => {
                        let template = &options.samples.samples[matches.indexes[0]];
                        // Mixed summary models cannot establish which model an added pass would use.
                        if !matches.uniform_summary {
                            result.fail(
                                &req.session,
                                req.time,
                                "compatible samples disagree about summary model/provider/tier/speed/geography",
                            );
                        }
                        let (output, resumed) = if larger {
                            (matches.max_output, matches.max_prefix)
                        } else {
                            (matches.median_output, matches.median_prefix)
                        };
                        let ordinary = input - old_prefix;
                        if resumed + ordinary > window || resumed >= old_prefix - st.removed {
                            result.fail(
                                &req.session,
                                req.time,
                                "observed resumed-prefix proxy does not shrink this context below the candidate window",
                            );
                        } else {
                            let summary_input = input - st.removed;
                            result.added.push(AddedCompaction {
                                request: req.id.clone(),
                                input_tokens: summary_input,
                                summary_tokens: output,
                                prefix_tokens: resumed,
                                basis: matches.basis.clone(),
                                summary_cost: price_part(
                                    catalog,
                                    template.summary_request,
                                    &summary_part(template, summary_input, output, cold),
                                ),
                                prefix_cost: zero_cost(),
                            });
                            added = Some(result.added.len() - 1);
                            let event = options
                                .events
                                .by_id
                                .get(template.event)
                                .expect("calibration samples come from the simulated events");
                            result.events.push(StructuralEvent {
                                id: format!("modeled:{}:{index}", req.id).into(),
                                time: req.time,
                                session: req.session.clone(),
                                origins: req
                                    .locations
                                    .iter()
                                    .map(|location| Origin {
                                        session: req.session.clone(),
                                        location: location.clone(),
                                    })
                                    .collect(),
                                request: None,
                                ..(*event).clone()
                            });
                            st.removed = old_prefix - resumed;
                        }
                    }
                    _ => result.fail(
                        &req.session,
                        req.time,
                        "no compatible measured compaction summary and resumed-prefix sample, or ambiguous multipart request",
                    ),
                }
            }
            if selected
                && st.removed > 0
                && let (Some(input), Some(old_prefix)) = (u.input, old_prefix)
            {
                let mut next = u;
                let remaining = old_prefix - st.removed;
                next.input = Some(input - st.removed);
                let ttl = if req.agent == Agent::Subagent {
                    options.ttls.subagent
                } else {
                    options.ttls.main
                };
                let billed_writes = req.harness == Harness::Claude || bills_writes;
                let lifetime = match req.harness {
                    Harness::Claude => ttl.map(ttl_milliseconds),
                    Harness::Codex => billed_writes.then_some(CODEX_CACHE_MILLISECONDS),
                };
                // A newly generated prefix cannot inherit the old transcript's external warming.
                let warm = added.is_none()
                    && lifetime.is_some_and(|lifetime| req.time - st.time < lifetime)
                    && st.prefix.is_some();
                if req.harness == Harness::Codex
                    && !billed_writes
                    && added.is_none()
                    && next.read.unwrap_or(0) > 0
                {
                    result.fail(
                        &req.session,
                        req.time,
                        "counterfactual prefix cache retention is not established for this Codex model",
                    );
                }
                let read = match st.prefix {
                    Some(prefix) if warm => prefix.min(remaining),
                    _ => 0,
                };
                next.read = Some(read);
                if req.harness == Harness::Claude {
                    let write = remaining - read;
                    next.write = Some(write);
                    next.write5m = Some(if ttl == Some(Ttl::FiveMinutes) {
                        write
                    } else {
                        0
                    });
                    next.write1h = Some(if ttl == Some(Ttl::OneHour) { write } else { 0 });
                } else if billed_writes {
                    if u.write.is_none() {
                        result.fail(
                            &req.session,
                            req.time,
                            "Codex cache-write geometry is missing for an affected request",
                        );
                    }
                    let write = remaining - read;
                    next.write = Some(write);
                    next.write30m = Some(write);
                    next.write12h = Some(0);
                }
                st.prefix = Some(remaining);
                result.estimated_parts.insert((req.id.clone(), index));
                result.affected.insert(req.id.clone());
                req.to_mut().parts[index].usage = next;
                if let Some(added) = added {
                    let just_prefix = BillingPart {
                        usage: Usage {
                            output: Some(0),
                            read: Some(0),
                            write: Some(remaining),
                            write30m: Some(remaining),
                            write12h: Some(0),
                            ..next
                        },
                        ..req.parts[index].clone()
                    };
                    // Attribute only the new prefix charge; it already lives in this request's total.
                    let cost = if billed_writes {
                        price_components(catalog, &req, &just_prefix).cache
                    } else {
                        let ordinary = BillingPart {
                            usage: Usage {
                                input: Some(remaining),
                                write: Some(0),
                                ..just_prefix.usage
                            },
                            ..just_prefix
                        };
                        price_part(catalog, &req, &ordinary)
                    };
                    result.added[added].prefix_cost = cost;
                }
            }
            st.input = u.input;
            if !same_cohort {
                st.cohort = ContextCohort::of(&req, &req.parts[index]);
            }
            st.time = req.time;
            aliased = true;
        }
        match (state, kept) {
            (Some(state), _) if aliased => {
                states.insert(key, state);
            }
            (_, Some(kept)) => {
                states.insert(key, kept);
            }
            _ => {}
        }
        result.requests.push(req);
    }
    result
}

/// Apply selected window controls using the same simulation as the window check.
pub fn replay_windows<'r>(
    requests: impl IntoIterator<Item = &'r Request>,
    windows: &[WindowSetting],
    options: &WindowOptions,
) -> Simulation<'r> {
    let mut result = Simulation {
        requests: requests.into_iter().map(Modeled::Recorded).collect(),
        ..Simulation::default()
    };
    for window in windows {
        let Some(value) = window.value else {
            continue;
        };
        let next = simulate(
            std::mem::take(&mut result.requests),
            &result.events,
            (!result.applied).then_some(options.estimates),
            Pass {
                window: value,
                models: window.models,
                larger: false,
                cold: false,
            },
            options,
        );
        result.requests = next.requests;
        result.applied = true;
        result.events.extend(next.events);
        result.added.extend(next.added);
        result.errors.extend(next.errors);
        for (session, time) in next.uncertain {
            let entry = result.uncertain.entry(session).or_insert(time);
            *entry = time.min(*entry);
        }
        result.affected.extend(next.affected);
        result.estimated_parts.extend(next.estimated_parts);
    }
    result
}

/// Inputs to the compaction-window checks.
#[derive(Clone, Copy)]
pub struct CompactionOptions<'a> {
    pub scan: &'a Scan,
    pub catalog: &'a Catalog,
    pub summaries: &'a [CompactionCharge],
    pub estimates: &'a IndexMap<Text, UsageEstimate>,
    /// TTL checks whose recommendations form the baseline.
    pub ttls: &'a [&'a TtlCheck],
    pub since: Option<Millis>,
    pub now: Millis,
}

/// Selected-part replays of one profile after its TTL recommendations.
struct Replayer<'a> {
    catalog: &'a Catalog,
    ttls: ReplayTtls,
    since: Millis,
    cache: &'a CacheBoundaries,
}

impl Replayer<'_> {
    fn selected<'r>(
        &self,
        requests: impl IntoIterator<Item = &'r Request>,
        estimates: &IndexMap<Text, UsageEstimate>,
        extra: Option<&CacheBoundaries>,
        estimated_parts: Option<&HashSet<(Text, usize)>>,
    ) -> Vec<ReplayedPart<'r>> {
        let options = ReplayOptions {
            estimates,
            catalog: self.catalog,
            ttls: self.ttls,
            since: self.since,
            cold_prefixes: false,
            estimated_parts,
            attempt_scenario: Default::default(),
        };
        replay_parts(requests, iter::once(self.cache).chain(extra), &options)
            .filter(|i| i.selected)
            .collect()
    }

    /// Replay a simulated window, handing each selected part to `visit` in order, and total
    /// it with the fixed charges and added summaries, returning its errors and total.
    fn replay<'s>(
        &self,
        simulation: &'s Simulation<'s>,
        control: &WindowControl,
        fixed: &Cost,
        mut visit: impl FnMut(&ReplayedPart<'s>),
    ) -> (Vec<Cost>, Cost) {
        let extra = CacheBoundaries::new(&simulation.events);
        let no_estimates = IndexMap::default();
        let options = ReplayOptions {
            estimates: &no_estimates,
            catalog: self.catalog,
            ttls: self.ttls,
            since: self.since,
            cold_prefixes: false,
            estimated_parts: Some(&simulation.estimated_parts),
            attempt_scenario: Default::default(),
        };
        let mut sum = Sum::new();
        sum.add(fixed);
        let parts = replay_parts(
            simulation.requests.iter().map(AsRef::as_ref),
            [self.cache, &extra],
            &options,
        );
        for item in parts.filter(|i| i.selected) {
            sum.add(&item.components.total());
            visit(&item);
        }
        let mut reasons: IndexSet<&str> = simulation.errors.iter().copied().collect();
        if let Some(reason) = &control.comparison_unknown {
            reasons.insert(reason);
        }
        let errors = if reasons.is_empty() {
            vec![]
        } else {
            let reasons: Vec<&str> = reasons.into_iter().collect();
            vec![unknown_cost(reasons.join("; "))]
        };
        for a in &simulation.added {
            sum.add(&a.summary_cost);
        }
        for e in &errors {
            sum.add(e);
        }
        (errors, sum.cost())
    }

    /// Replay a simulated window and keep every selected part, to compare windows against.
    fn point<'s>(
        &self,
        simulation: &'s Simulation<'s>,
        control: &WindowControl,
        fixed: &Cost,
    ) -> Point<'s> {
        let mut after = vec![];
        let (errors, cost) = self.replay(simulation, control, fixed, |i| after.push(i.clone()));
        Point {
            simulation,
            after,
            errors,
            cost,
        }
    }
}

/// One simulated window and its replay.
struct Point<'s> {
    simulation: &'s Simulation<'s>,
    after: Vec<ReplayedPart<'s>>,
    errors: Vec<Cost>,
    cost: Cost,
}

/// A candidate's comparison against the current window.
struct Run {
    cost: Cost,
    baseline_comparison_cost: Cost,
    comparison_cost: Cost,
    savings_usd: Option<f64>,
    coverage: Coverage,
    added: Vec<AddedCompaction>,
    affected: usize,
    cache_evidence: Vec<CacheReplayEvidence>,
}

/// Parse one version component as JavaScript `Number` would; None for NaN.
pub(crate) fn js_number(text: &str) -> Option<f64> {
    // JavaScript trims Unicode spaces and the byte-order mark, but not NEL.
    let text = text.trim_matches(|c: char| (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}');
    if text.is_empty() {
        return Some(0.0);
    }
    let radix = |digits: &str, radix: u32| {
        (!digits.is_empty() && digits.chars().all(|c| c.is_digit(radix))).then(|| {
            digits.chars().fold(0.0, |n, c| {
                n * radix as f64 + c.to_digit(radix).unwrap_or(0) as f64
            })
        })
    };
    let lower = text.to_ascii_lowercase();
    if let Some(digits) = lower.strip_prefix("0x") {
        return radix(digits, 16);
    }
    if let Some(digits) = lower.strip_prefix("0o") {
        return radix(digits, 8);
    }
    if let Some(digits) = lower.strip_prefix("0b") {
        return radix(digits, 2);
    }
    match text.trim_start_matches(['+', '-']) {
        "Infinity" if text.len() - "Infinity".len() <= 1 => Some(if text.starts_with('-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        }),
        _ if text
            .bytes()
            .all(|b| b.is_ascii_digit() || b"+-.eE".contains(&b)) =>
        {
            text.parse().ok()
        }
        _ => None,
    }
}

/// Whether a Claude client version is at least v2.1.`patch`, as safe non-negative integers.
fn version_supported(version: Option<&str>, patch: i64) -> bool {
    let Some(version) = version else {
        return false;
    };
    let parts: Vec<Option<f64>> = version.split('.').map(js_number).collect();
    let [Some(major), Some(minor), Some(micro)] = parts[..] else {
        return false;
    };
    let safe = |v: f64| v.fract() == 0.0 && v.abs() <= 9_007_199_254_740_991.0 && v >= 0.0;
    if !(safe(major) && safe(minor) && safe(micro)) {
        return false;
    }
    major > 2.0 || (major == 2.0 && (minor > 1.0 || (minor == 1.0 && micro >= patch as f64)))
}

/// Compare compaction windows after the recommended TTLs, without summing independent savings.
pub fn compaction_checks(options: &CompactionOptions) -> Vec<CompactionCheck> {
    let CompactionOptions {
        scan, catalog, now, ..
    } = *options;
    let selected_ids: HashSet<&str> = scan.requests.iter().map(|r| &*r.id).collect();
    let mut checks = Vec::new();
    for profile in &scan.profiles {
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
        let events: Vec<&StructuralEvent> = scan
            .cache_context
            .boundaries
            .iter()
            .chain(&scan.events)
            .filter(|e| in_profile(&e.profiles))
            .collect();
        let selected: Vec<&Request> = requests
            .iter()
            .copied()
            .filter(|r| selected_ids.contains(&*r.id))
            .collect();
        let Some(first_selected) = selected.iter().map(|r| r.time).min() else {
            continue;
        };
        let ttl_results: Vec<&TtlCheck> = options
            .ttls
            .iter()
            .copied()
            .filter(|t| t.base.profile == profile.id)
            .collect();
        let recommended_ttl = |bucket: Bucket| {
            ttl_results
                .iter()
                .find(|t| t.bucket == bucket)
                .and_then(|t| t.base.recommended)
                .or(profile.ttl.get(bucket).value)
        };
        let ttls = PerBucket {
            main: recommended_ttl(Bucket::Main),
            subagent: recommended_ttl(Bucket::Subagent),
        };
        let first_boundary = scan
            .events
            .iter()
            .filter(|e| in_profile(&e.profiles) && e.kind == EventKind::Compaction)
            .map(|e| e.time)
            .min();
        let since = options.since.unwrap_or_else(|| {
            ttl_results
                .iter()
                .filter(|t| t.window_basis_kind == WindowBasisKind::TtlTransition)
                .filter_map(|t| t.base.since)
                .fold(
                    first_boundary.map_or(first_selected, |b| first_selected.min(b)),
                    Millis::max,
                )
        });
        let index = WindowEvents::new(events.iter().copied());
        let replayer = Replayer {
            catalog,
            ttls: ReplayTtls {
                main: ttls.main,
                subagent: ttls.subagent,
                unknown: None,
            },
            since,
            cache: &index.cache,
        };
        let baseline = replayer.selected(requests.iter().copied(), options.estimates, None, None);
        let baseline_totals: Vec<Cost> = baseline.iter().map(|i| i.components.total()).collect();
        let summary_ids: HashSet<&Id> = events
            .iter()
            .filter(|e| e.time >= since)
            .map(|e| &e.id)
            .collect();
        let unmatched = add_costs(
            options
                .summaries
                .iter()
                .filter(|s| summary_ids.contains(&s.event) && s.request.is_none())
                .map(|s| &s.cost),
        );
        let servers: Vec<Cost> = selected
            .iter()
            .filter(|r| r.time >= since)
            .map(|r| server_cost(r))
            .collect();
        let fixed = add_costs(iter::once(&unmatched).chain(&servers));
        let recorded_cost = add_costs(iter::once(&fixed).chain(&baseline_totals));
        let observed_summary_cost = add_costs(
            iter::once(&unmatched).chain(
                baseline
                    .iter()
                    .zip(&baseline_totals)
                    .filter(|(i, _)| i.p.kind == PartKind::Compaction)
                    .map(|(_, total)| total),
            ),
        );
        let calibration = compaction_samples(&requests, &events, options.summaries);
        let observed_models: IndexSet<Option<Text>> = baseline
            .iter()
            .filter(|i| is_conversation(&i.p))
            .map(|i| i.p.model.clone())
            .collect();
        // One effective key has one aggregate winner, including global/environment controls.
        let mut groups: IndexMap<String, (WindowControl, Vec<Option<Text>>)> = IndexMap::default();
        for model in observed_models {
            let control = window_control(
                &profile.compaction,
                match profile.harness {
                    Harness::Codex => None,
                    Harness::Claude => model.as_deref(),
                },
            );
            groups
                .entry(control.owner.clone())
                .or_insert_with(|| (control, vec![]))
                .1
                .push(model);
        }
        let window_options = WindowOptions {
            events: &index,
            estimates: options.estimates,
            catalog,
            samples: &calibration,
            since,
            ttls: ttls.clone(),
        };
        for (control, models) in groups.values() {
            let model = match models.as_slice() {
                [only] => only.clone(),
                _ => None,
            };
            let relevant: Vec<&ReplayedPart> = baseline
                .iter()
                .filter(|i| is_conversation(&i.p))
                .filter(|i| i.p.model.is_none() || models.contains(&i.p.model))
                .collect();
            let Some(recent) = relevant.last().map(|i| i.request) else {
                continue;
            };
            let claude = profile.harness == Harness::Claude;
            let minimum = if claude { CLAUDE_WINDOW_MINIMUM } else { 1 };
            let ceiling = control.value.unwrap_or_else(|| {
                relevant
                    .iter()
                    .fold(0, |n, i| n.max(i.p.usage.input.unwrap_or(0)))
            });
            let mut candidates: BTreeSet<i64> = BTreeSet::new();
            if claude {
                candidates.insert(minimum);
            }
            // Economic discontinuities and measured replacement geometry form a small,
            // evidence-derived family instead of one replay per distinct request size.
            for i in &relevant {
                if let (Some(provider), Some(model)) = (&i.request.provider, &i.p.model) {
                    // Catalog sizes are whole token counts; a fraction would compare like its floor.
                    candidates.extend(catalog.tier_sizes(provider, model).map(|s| s as i64));
                }
            }
            let mut geometries: IndexMap<_, Vec<i64>> = IndexMap::default();
            for sample in calibration
                .samples
                .iter()
                .filter(|s| models.contains(&s.p.model))
            {
                let key = (
                    sample.request.provider.as_deref(),
                    sample.p.model.as_deref(),
                    sample.request.agent,
                    sample.p.tier.as_deref(),
                    sample.p.speed.as_deref(),
                    sample.p.geo.as_deref(),
                );
                geometries.entry(key).or_default().push(sample.prefix);
            }
            let ordinary = relevant.iter().fold(0, |n, i| {
                match (i.p.usage.input, prefix(i.request.harness, &i.p.usage)) {
                    (Some(input), Some(prefix)) => n.max(input - prefix),
                    _ => n.max(0),
                }
            });
            for sizes in geometries.into_values() {
                candidates.insert(median(sizes) + ordinary);
            }
            if let Some(value) = control.value {
                candidates.insert(value);
            }
            let windows: Vec<i64> = candidates
                .into_iter()
                .filter(|&n| {
                    n >= minimum && n <= ceiling && (!claude || n <= CLAUDE_WINDOW_MAXIMUM)
                })
                .collect();
            let raw = |window: i64, larger: bool, cold: bool| {
                simulate(
                    requests.iter().map(|r| Modeled::Recorded(r)).collect(),
                    &[],
                    Some(options.estimates),
                    Pass {
                        window,
                        models,
                        larger,
                        cold,
                    },
                    &window_options,
                )
            };
            let skipped = skipped_lines(scan, &profile.id);
            // Compare each candidate with the current window under the same summary scenario
            // (point, larger, cold), streaming the candidate's replay against the current one.
            let run = |window: i64, scenario: usize, before: Option<&Point>| -> Run {
                let detail = scenario == 0;
                let (larger, cold) = (scenario == 1, scenario == 2);
                let owned_simulation;
                let simulation = match before {
                    Some(before) if Some(window) == control.value => before.simulation,
                    _ => {
                        owned_simulation = raw(window, larger, cold);
                        &owned_simulation
                    }
                };
                let previous = before.map_or(&baseline, |b| &b.after);
                // One pair per request that gains or loses a summary, so the unpriced
                // bound compares against single differences rather than their total.
                let mut summaries: IndexMap<&str, (Vec<&Cost>, Vec<&Cost>)> = IndexMap::default();
                for a in before.into_iter().flat_map(|b| &b.simulation.added) {
                    summaries
                        .entry(&*a.request)
                        .or_default()
                        .0
                        .push(&a.summary_cost);
                }
                for a in &simulation.added {
                    summaries
                        .entry(&*a.request)
                        .or_default()
                        .1
                        .push(&a.summary_cost);
                }
                let mut pairs: Vec<(Cost, Cost)> = summaries
                    .into_values()
                    .map(|(b, a)| (add_costs(b), add_costs(a)))
                    .collect();
                let uncertain = |r: &Request| {
                    before
                        .map(|b| &b.simulation.uncertain)
                        .into_iter()
                        .chain([&simulation.uncertain])
                        .any(|sessions| sessions.get(&r.session).is_some_and(|&t| t <= r.time))
                };
                let mut cache_evidence = vec![];
                let mut index = 0;
                let mut compare = |item: &ReplayedPart| {
                    let original = &previous[index];
                    index += 1;
                    if detail && let Some(evidence) = &item.evidence {
                        cache_evidence.push((**evidence).clone());
                    }
                    if uncertain(item.request) {
                        let reason = "session replay is unknown after a request it cannot model";
                        pairs.push((unknown_cost(reason), unknown_cost(reason)));
                        return;
                    }
                    if original.p == item.p {
                        return;
                    }
                    let mut baseline_parts =
                        vec![&original.components.cache, &original.components.ordinary];
                    let mut alternative_parts =
                        vec![&item.components.cache, &item.components.ordinary];
                    let old_output = output_unit(catalog, original);
                    let new_output = output_unit(catalog, item);
                    if old_output.is_none() || new_output.is_none() || old_output != new_output {
                        baseline_parts.push(&original.components.output);
                        alternative_parts.push(&item.components.output);
                    }
                    pairs.push((add_costs(baseline_parts), add_costs(alternative_parts)));
                };
                let owned_errors;
                let (errors, cost) = match before {
                    Some(before) if Some(window) == control.value => {
                        before.after.iter().for_each(&mut compare);
                        (&before.errors, before.cost.clone())
                    }
                    _ => {
                        let (errors, cost) = replayer.replay(simulation, control, &fixed, compare);
                        owned_errors = errors;
                        (&owned_errors, cost)
                    }
                };
                let before_errors = before.map_or(&[][..], |b| b.errors.as_slice());
                let baseline_comparison_cost =
                    add_costs(pairs.iter().map(|p| &p.0).chain(before_errors));
                let comparison_cost = add_costs(pairs.iter().map(|p| &p.1).chain(errors));
                // Per-session failures are unpriced pairs; an unknown current control still blocks a price.
                let paired = paired_savings(pairs.iter().map(|(b, a)| (b, a)), skipped);
                let savings_usd = if control.comparison_unknown.is_some() || !paired.decided {
                    None
                } else {
                    Some(paired.usd)
                };
                Run {
                    cost,
                    baseline_comparison_cost,
                    comparison_cost,
                    savings_usd,
                    coverage: Coverage {
                        priced: paired.priced,
                        total: paired.total,
                    },
                    // Sensitivity runs report only their savings, so skip copying their detail.
                    added: if detail {
                        simulation.added.clone()
                    } else {
                        vec![]
                    },
                    affected: simulation.affected.len(),
                    cache_evidence,
                }
            };
            // One current replay at a time: each scenario's is kept only while every window
            // is compared against it.
            let mut baseline_cost = recorded_cost.clone();
            let mut runs: [Vec<Run>; 3] = Default::default();
            for (scenario, runs) in runs.iter_mut().enumerate() {
                let current = control
                    .value
                    .map(|value| raw(value, scenario == 1, scenario == 2));
                let before = current.as_ref().map(|s| replayer.point(s, control, &fixed));
                if scenario == 0
                    && let Some(before) = &before
                {
                    baseline_cost = before.cost.clone();
                }
                for &window in &windows {
                    runs.push(run(window, scenario, before.as_ref()));
                }
            }
            let [points, larger, cold] = runs;
            let scenarios: Vec<WindowScenario> = windows
                .iter()
                .zip(points.into_iter().zip(larger.into_iter().zip(cold)))
                .map(|(&window, (point, (larger, cold)))| WindowScenario {
                    window,
                    cost: point.cost,
                    baseline_comparison_cost: point.baseline_comparison_cost,
                    comparison_cost: point.comparison_cost,
                    savings_usd: point.savings_usd,
                    coverage: point.coverage,
                    added: point.added,
                    affected_requests: point.affected as u64,
                    cache_evidence: point.cache_evidence,
                    sensitivity: WindowSensitivity {
                        larger_summary_savings_usd: larger.savings_usd,
                        cold_summary_savings_usd: cold.savings_usd,
                        reread_break_even_usd: point
                            .savings_usd
                            .map(|s| if s > 0.0 { s } else { 0.0 }),
                    },
                })
                .collect();
            let priced: Vec<&WindowScenario> = scenarios
                .iter()
                .filter(|s| s.savings_usd.is_some())
                .collect();
            let mut best: Option<&WindowScenario> = None;
            for &s in &priced {
                let better = match best {
                    None => true,
                    Some(best) => {
                        s.savings_usd > best.savings_usd
                            || (s.savings_usd == best.savings_usd
                                && Some(s.window) == control.value)
                    }
                };
                if better {
                    best = Some(s);
                }
            }
            let winning = best.filter(|b| b.savings_usd.is_some_and(|s| s > 0.0));
            let conditional = scenarios.iter().any(|s| {
                tier_sensitive(&s.baseline_comparison_cost) || tier_sensitive(&s.comparison_cost)
            });
            let recommended = winning
                .map(|w| w.window)
                .or(if priced.len() == scenarios.len() {
                    control.value
                } else {
                    None
                })
                .filter(|_| !conditional);
            let patch = control.minimum_claude_patch.unwrap_or(288);
            let per_model_supported =
                !claude || version_supported(recent.version.as_deref(), patch);
            let window_key = if claude {
                "autoCompactWindow"
            } else {
                "model_auto_compact_token_limit"
            };
            let historical_conflict = events.iter().any(|e| {
                e.kind == EventKind::Settings
                    && e.time >= since
                    && matches!(e.settings.get(window_key), Some(Scalar::Number(n))
                        if control.setting.or(control.value).is_none_or(|v| *n != v as f64))
                    && (e.model.is_none()
                        || models.iter().any(|m| m.as_deref() == e.model.as_deref()))
            });
            let applicable =
                control.applicable && per_model_supported && (!claude || !models.contains(&None));
            let profiles: IndexSet<&String> =
                selected.iter().flat_map(|r| r.profiles.iter()).collect();
            let savings = winning
                .and_then(|w| w.savings_usd)
                .or(recommended.map(|_| 0.0))
                .filter(|_| !conditional);
            let predecessors: HashSet<&str> = baseline
                .iter()
                .filter_map(|i| i.evidence.as_ref())
                .flat_map(|e| &e.predecessors)
                .map(|p| &*p.request)
                .collect();
            let mut uncertainty: IndexSet<String> = [
                "compaction loses detail; re-reading files, repeated tool calls or changed reasoning may erase savings; that work is unmeasured, not priced at zero",
                "observed boundary timestamps, precomputed flags and durations do not prove summary generation time or idle attribution; unlogged/discarded summaries remain unknown",
                "compaction trigger timing and replacement-prefix identity are estimated from token counts, not observed counterfactual behavior; no samples means affected candidates remain unpriced",
            ]
            .map(String::from)
            .into_iter()
            .collect();
            if conditional {
                uncertainty.insert(TIER_SENSITIVITY.into());
            }
            if !control.applicable {
                uncertainty.insert(control.reason.clone());
            }
            if !per_model_supported {
                uncertainty.insert(format!(
                    "current client support for this window control is not established (requires Claude v2.1.{patch})"
                ));
            }
            if historical_conflict {
                uncertainty.insert(
                    "historical recorded limits differ from current files; this replay applies current controls rather than inferring historical ownership".into(),
                );
            }
            if profiles.len() > 1 {
                uncertainty.insert("shared history has ambiguous profile attribution".into());
            }
            for s in &scenarios {
                uncertainty.extend(s.comparison_cost.unknown.iter().cloned());
                uncertainty.extend(s.cost.unknown.iter().cloned());
            }
            for issue in selected.iter().flat_map(|r| &r.issues) {
                if !uncertainty.contains(&**issue) {
                    uncertainty.insert(issue.to_string());
                }
            }
            let coverage = winning
                .or_else(|| scenarios.iter().find(|s| Some(s.window) == control.value))
                .map(|s| s.coverage);
            checks.push(CompactionCheck {
                base: CheckBase {
                    profile: profile.id.clone(),
                    history_profiles: profiles.into_iter().cloned().collect(),
                    evidence: if recommended.is_none() {
                        Evidence::Unknown
                    } else {
                        Evidence::Estimated
                    },
                    action: match recommended {
                        Some(r) if applicable && Some(r) == control.value => Action::Keep,
                        Some(_) if applicable => Action::Change,
                        _ => Action::Advisory,
                    },
                    current: control.value,
                    recommended,
                    owner: control.owner.clone(),
                    since: Some(since),
                    until: Some(now),
                    window_basis: if options.since.is_some() {
                        "explicit --since; current controls applied to history"
                    } else {
                        "available selected history after the TTL comparison windows; current controls applied to history"
                    }
                    .into(),
                    baseline: if control.value.is_none() {
                        "recorded requests after available Claude TTL recommendations (otherwise current controls); current window unknown, advisory comparison only"
                    } else {
                        "current window applied to recorded requests after available Claude TTL recommendations (otherwise current controls); observed summaries retained once; savings are incremental and checks must not be summed"
                    }
                    .into(),
                    requests: selected.iter().filter(|r| r.time >= since).count() as u64,
                    coverage,
                    savings_usd: savings,
                    monthly_savings_usd: savings
                        .filter(|_| now > since)
                        .map(|s| (s * 30.0 * 86400000.0) / (now - since) as f64),
                    break_even: savings.filter(|&s| s > 0.0).map(|s| {
                        format!(
                            "${s:.4} of additional re-read or changed-work cost erases the modeled saving over this window"
                        )
                    }),
                    assumptions: vec![
                        control.reason.clone(),
                        "candidate windows are the current explicit limit, provider minimum, catalog tier boundaries and median observed resumed-prefix geometry plus ordinary input; one aggregate winner is chosen for each owning setting; this is not an exhaustive optimizer over integer thresholds".into(),
                        "only earlier compactions are modeled; every observed compaction is retained once even if an earlier pass might replace it; higher windows cannot reconstruct details already lost".into(),
                        "extra summaries read the whole modeled context at ordinary input rates; output and resumed prefix use cohort medians from linked billed summaries and first subsequent requests; first resumed prefix is cold and charged in that request, never added twice".into(),
                        "first post-boundary prefix includes fixed instructions and retained/new context, not just summary output; counts do not establish content identity; summaries may use the recorded summary model rather than the conversation model".into(),
                        "ordinary requests, outputs, tools and incremental growth are held fixed until each observed boundary; proportional quality or behavioral improvements are not assumed".into(),
                        "available TTL recommendations form the baseline; where no priced TTL recommendation exists, the current explicit control is used and unknown affected TTL charges remain unpriced".into(),
                        "unknown unchanged costs cancel only in the comparison; full totals remain unknown; affected context tiers reprice the entire request including output".into(),
                        "changed prefixes are fully reused only within the chosen Claude lifetime or the documented default 30m billed Codex lifetime, with stable prefix identity assumed; expiry makes the new prefix cold and cannot inherit external warming of the old transcript".into(),
                        "cold-summary sensitivity charges 1h Claude writes or 12h billed Codex writes; larger-summary sensitivity uses observed cohort maxima; neither sensitivity vetoes the aggregate estimate".into(),
                        "shared history is conditional for this current profile; model/check scenarios overlap and their savings must not be added".into(),
                    ],
                    uncertainty: uncertainty.into_iter().collect(),
                    sources: SOURCES.iter().chain(&PRICE_SOURCES).map(|s| s.to_string()).collect(),
                },
                bucket: "all",
                model,
                models: models.clone(),
                unset: control.unset && control.value.is_some(),
                setting: control.setting,
                baseline_cost: baseline_cost.clone(),
                observed_summary_cost: observed_summary_cost.clone(),
                ttls: ttls.clone(),
                scenarios,
                usage_estimates: requests
                    .iter()
                    .filter(|r| r.time >= since || predecessors.contains(&*r.id))
                    .filter_map(|r| options.estimates.get(&r.id).cloned())
                    .collect(),
                cache_evidence: baseline
                    .iter()
                    .filter_map(|i| i.evidence.as_deref().cloned())
                    .collect(),
            });
        }
    }
    checks
}

/// Unreadable lines in a profile's transcripts; each could have been a request.
pub fn skipped_lines(scan: &Scan, profile: &str) -> u64 {
    scan.snapshots
        .iter()
        .filter(|s| s.profiles.iter().any(|p| p == profile))
        .map(|s| s.skipped_lines)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controls::{CompactionSources, Env, compaction_controls};
    use crate::estimates::estimate_usage;
    use crate::model::{
        Accounting, Api, Attempt, CacheContext, CompactionControls, ExposureScan, IdleAttribution,
        Location, PerTtl, Pricing, Profile, ScanCoverage, Snapshot, TtlControl,
    };
    use crate::prices::price_request;
    use crate::safe::identity;
    use crate::test_support::{ClaudeUsage, claude_usage, make_request, prices};
    use serde_json::{Map, Value, json};

    const NOW: Millis = 18_000_000;
    const HOUR: Millis = 3_600_000;

    fn controls(harness: Harness, user: Value, env: &Env, file: &str) -> CompactionControls {
        let user = user.as_object().cloned().unwrap_or_default();
        compaction_controls(&CompactionSources {
            harness,
            user: &user,
            managed: &Map::new(),
            env,
            file,
            managed_file: "managed.json",
            scopes: None,
        })
        .unwrap()
    }

    fn ttl(value: Ttl, owner: &str) -> TtlControl {
        TtlControl {
            value: Some(value),
            owner: owner.into(),
            reason: "current explicit setting".into(),
        }
    }

    fn usage(write: i64, read: i64, output: i64, ttl: Ttl, input: i64) -> Usage {
        claude_usage(ClaudeUsage {
            write,
            read,
            output,
            ttl,
            input,
        })
    }

    fn writes(write: i64) -> Usage {
        usage(write, 0, 10, Ttl::FiveMinutes, 0)
    }

    fn at(id: &str, session: &str, time: Millis) -> Request {
        Request {
            id: id.into(),
            session: session.into(),
            time,
            end_time: time,
            ..make_request()
        }
    }

    fn boundary() -> StructuralEvent {
        let location = Location {
            file: "synthetic".into(),
            line: 1,
        };
        StructuralEvent {
            id: "sample-boundary".into(),
            origins: vec![Origin {
                session: "sample".into(),
                location: location.clone(),
            }],
            harness: Harness::Claude,
            session: "sample".into(),
            time: 0,
            kind: EventKind::Compaction,
            profiles: vec!["profile".into()].into(),
            location,
            request: Some("sample-resume".into()),
            model: Some("claude-opus-5-5".into()),
            version: Some("2.1.314".into()),
            client: Some("sdk-ts".into()),
            cwd: None,
            trigger: Some("auto".into()),
            precomputed: Some(false),
            pre_tokens: Some(300000),
            post_tokens: Some(2000),
            duration_ms: Some(1000),
            tool: None,
            tool_id: None,
            settings: Default::default(),
            idle_attribution: IdleAttribution::Unknown,
            shell: None,
            t3: None,
            pricing: Some(std::sync::Arc::new(Pricing {
                provider: Some("anthropic".into()),
                model: Some("claude-opus-5-5".into()),
                tier: Some("standard".into()),
                speed: Some("standard".into()),
                geo: Some("global".into()),
            })),
        }
    }

    /// One measured compaction calibrates `count` hourly 150k-token requests under a 400k window.
    fn data(count: i64) -> Scan {
        let profile = Profile {
            agents: Default::default(),
            id: "profile".into(),
            harness: Harness::Claude,
            root: "synthetic".into(),
            aliases: vec!["synthetic".into()],
            histories: vec![],
            settings_file: "synthetic/settings.json".into(),
            ttl: PerBucket {
                main: ttl(Ttl::FiveMinutes, "settings.json:promptCacheTtl"),
                subagent: ttl(Ttl::FiveMinutes, "settings.json:subagentPromptCacheTtl"),
            },
            provider: Some("anthropic".into()),
            settings: IndexMap::default(),
            mcp_servers: vec![],
            compaction: controls(
                Harness::Claude,
                json!({"modelSettings": {"claude-opus-5-5": {"autoCompactWindow": 400000}}}),
                &Env::default(),
                "settings.json",
            ),
            idle: None,
            speed: None,
            bash: None,
            speed_directories: None,
            idle_directories: None,
            background: vec![],
            background_directories: IndexMap::default(),
            background_requests: IndexMap::default(),
            t3: None,
        };
        let mut first = at("sample-resume", "sample", 1000);
        first.parts[0].usage = writes(20000);
        first.parts.push(BillingPart {
            kind: PartKind::Compaction,
            usage: usage(0, 0, 2000, Ttl::FiveMinutes, 300000),
            ..first.parts[0].clone()
        });
        let mut requests = vec![first];
        for i in 0..count {
            let mut r = at(&format!("large-{i}"), "session", HOUR * (i + 1));
            r.parts[0].usage = writes(150000);
            requests.push(r);
        }
        // Coverage and snapshot times match the TypeScript fixture, which the text report prints.
        let n = requests.len() as u64;
        Scan {
            subagents: vec![],
            t3: vec![],
            sessions: Default::default(),
            snapshot_started_at: crate::safe::iso(0),
            snapshot_finished_at: crate::safe::iso(0),
            since: None,
            roots: vec![],
            profiles: vec![profile],
            snapshots: vec![],
            requests,
            events: vec![boundary()],
            exposure: ExposureScan::default(),
            cache_context: CacheContext::default(),
            coverage: ScanCoverage {
                requests: n,
                source_known: n,
                tier_known: n,
                ..ScanCoverage::default()
            },
        }
    }

    /// The replay's summary charges for explicit links and Claude proxies, which these scans use.
    fn charges(scan: &Scan, catalog: &Catalog) -> Vec<CompactionCharge> {
        let mut result = vec![];
        for e in scan
            .events
            .iter()
            .filter(|e| e.kind == EventKind::Compaction)
        {
            let linked = e
                .request
                .as_ref()
                .and_then(|id| scan.requests.iter().find(|r| &r.id == id));
            if let Some(linked) = linked {
                let costs: Vec<Cost> = linked
                    .parts
                    .iter()
                    .filter(|p| p.kind == PartKind::Compaction)
                    .map(|p| price_part(catalog, linked, p))
                    .collect();
                result.push(CompactionCharge {
                    event: e.id.clone(),
                    request: Some(linked.id.clone()),
                    evidence: Evidence::Measured,
                    cost: add_costs(&costs),
                    ttl_cost: PerTtl {
                        five_minutes: zero_cost(),
                        one_hour: zero_cost(),
                    },
                    assumptions: vec![],
                });
                continue;
            }
            let pricing = Pricing::clone(e.pricing.as_ref().unwrap());
            let proxy = Request {
                evidence: RequestEvidence::Estimated,
                id: identity(&e.id.to_string()).into(),
                harness: e.harness,
                session: e.session.clone(),
                time: e.time,
                end_time: e.time,
                agent: Agent::Subagent,
                model: pricing.model.clone(),
                provider: pricing.provider.clone(),
                api: Api::Messages,
                source: Some("compaction".into()),
                trigger: e.trigger.as_deref().map(Text::from),
                reference: None,
                turn: None,
                version: e.version.clone(),
                client: e.client.clone(),
                cwd: e.cwd.clone(),
                effort: None,
                profiles: e.profiles.clone(),
                locations: vec![e.location.clone()],
                accounting: Accounting::Individual,
                attempt: None,
                server_tools: Some(crate::model::ServerTools::default()),
                requested_speed: None,
                t3: None,
                parts: vec![BillingPart {
                    kind: PartKind::Compaction,
                    model: pricing.model,
                    tier: pricing.tier,
                    speed: pricing.speed,
                    geo: pricing.geo,
                    usage: Usage {
                        input: e.pre_tokens,
                        output: e.post_tokens,
                        read: Some(0),
                        write: Some(0),
                        write5m: Some(0),
                        write1h: Some(0),
                        write30m: None,
                        write12h: None,
                        reasoning: None,
                    },
                }],
                issues: vec!["summary model/tier/provider assumed from preceding metadata in its own file; public API equivalent if endpoint is unrecorded".into()],
            };
            let cost = price_request(catalog, &proxy);
            result.push(CompactionCharge {
                event: e.id.clone(),
                request: None,
                evidence: Evidence::Estimated,
                cost: cost.clone(),
                ttl_cost: PerTtl {
                    five_minutes: cost.clone(),
                    one_hour: cost,
                },
                assumptions: vec![],
            });
        }
        result
    }

    fn checks_since(scan: &Scan, since: Option<Millis>) -> Vec<CompactionCheck> {
        let catalog = prices();
        let summaries = charges(scan, &catalog);
        let estimates = estimate_usage(scan);
        compaction_checks(&CompactionOptions {
            scan,
            catalog: &catalog,
            summaries: &summaries,
            estimates: &estimates,
            ttls: &[],
            since,
            now: NOW,
        })
    }

    fn checks(scan: &Scan) -> Vec<CompactionCheck> {
        checks_since(scan, None)
    }

    fn codex_usage(input: i64, write: i64, output: i64) -> Usage {
        Usage {
            input: Some(input),
            output: Some(output),
            read: Some(0),
            write: Some(write),
            ..Usage::default()
        }
    }

    /// The synthetic scans of the TypeScript compaction tests, called without TTL checks.
    fn scenario(name: &str) -> Scan {
        match name {
            "base" => data(4),
            "one" => data(1),
            "larger" => {
                let mut s = data(1);
                s.events[0].post_tokens = Some(3000);
                s.requests[0].parts[1].usage.output = Some(3000);
                s
            }
            "current" => {
                let mut s = data(4);
                s.profiles[0].compaction.models["claude-opus-5-5"].value = Some(100000);
                s
            }
            "codex" => {
                let mut s = data(4);
                let p = &mut s.profiles[0];
                p.harness = Harness::Codex;
                p.settings = [(
                    "model_auto_compact_token_limit".into(),
                    Scalar::Number(400000.0),
                )]
                .into_iter()
                .collect();
                p.compaction = controls(
                    Harness::Codex,
                    json!({"model_auto_compact_token_limit": 400000}),
                    &Env::default(),
                    "config.toml",
                );
                s.requests[0].parts.truncate(1);
                for r in &mut s.requests {
                    r.harness = Harness::Codex;
                    r.api = Api::Responses;
                    r.provider = Some("openai".into());
                    r.model = Some("gpt-6.1-sol".into());
                    r.parts[0].model = r.model.clone();
                    let size = if &*r.id == "sample-resume" {
                        20000
                    } else {
                        300000
                    };
                    r.parts[0].usage = codex_usage(size, size, 10);
                }
                let mut summary = Request {
                    id: "summary".into(),
                    time: -1000,
                    end_time: -1000,
                    ..s.requests[0].clone()
                };
                summary.parts = vec![BillingPart {
                    kind: PartKind::Compaction,
                    usage: codex_usage(300000, 0, 2000),
                    ..s.requests[0].parts[0].clone()
                }];
                s.requests.insert(0, summary);
                let e = &mut s.events[0];
                e.harness = Harness::Codex;
                e.request = Some("summary".into());
                e.post_tokens = None;
                e.model = Some("gpt-6.1-sol".into());
                s
            }
            "warm1h" | "multipart" => {
                let mut s = data(if name == "warm1h" { 3 } else { 2 });
                let ttls = &mut s.profiles[0].ttl;
                ttls.main.value = Some(Ttl::OneHour);
                ttls.subagent.value = Some(Ttl::OneHour);
                let time = s.requests[1].time + 600000;
                s.requests[2].time = time;
                s.requests[2].end_time = time;
                if name == "multipart" {
                    for r in &mut s.requests[1..3] {
                        r.parts.push(BillingPart {
                            kind: PartKind::Compaction,
                            usage: usage(1000, 150000, 100, Ttl::OneHour, 0),
                            ..r.parts[0].clone()
                        });
                    }
                }
                s
            }
            "attempt" => {
                let mut s = data(2);
                let before = s.requests[1].time;
                s.requests[2].time = before + 2000;
                s.requests[2].end_time = before + 2000;
                s.requests[2].parts[0].usage = usage(0, 150000, 10, Ttl::FiveMinutes, 0);
                let mut attempt = Request {
                    evidence: RequestEvidence::Unknown,
                    reference: Some("attempt-ref".into()),
                    attempt: Some(Attempt {
                        max_output_tokens: Some(100),
                    }),
                    ..at("attempt", "session", before + 1000)
                };
                attempt.parts[0].usage = Usage::default();
                s.requests.insert(2, attempt);
                s
            }
            "unknownSmall" | "unknownMissing" => {
                let mut s = data(4);
                let mut small = at("small-unknown", "separate-small", 1000);
                small.parts[0].kind = PartKind::Unknown;
                small.parts[0].usage = writes(5000);
                if name == "unknownMissing" {
                    small.parts[0].usage.input = None;
                }
                s.requests.push(small);
                s
            }
            "copies" => {
                let mut s = data(4);
                let large: Vec<Request> = s.requests[1..].to_vec();
                for k in 1..10 {
                    for r in &large {
                        s.requests.push(Request {
                            id: format!("{}-{k}", r.id).into(),
                            session: format!("copy-{k}").into(),
                            ..r.clone()
                        });
                    }
                }
                for k in 0..4 {
                    let mut missing = at(&format!("missing-{k}"), &format!("missing-{k}"), 1000);
                    missing.parts[0].kind = PartKind::Unknown;
                    missing.parts[0].usage = writes(5000);
                    missing.parts[0].usage.input = None;
                    s.requests.push(missing);
                }
                s.requests.sort_by_key(|r| r.time);
                s
            }
            "historical" => {
                let mut s = data(4);
                let mut e = s.events[0].clone();
                e.id = "settings".into();
                e.kind = EventKind::Settings;
                e.settings = [("autoCompactWindow".into(), Scalar::Number(1000000.0))]
                    .into_iter()
                    .collect();
                s.events.push(e);
                s
            }
            "unlinked" => {
                let mut s = data(4);
                s.requests[0].parts.truncate(1);
                s.events[0].request = None;
                s.events[0].post_tokens = Some(60000);
                s
            }
            "skipped" => {
                let mut s = data(4);
                s.snapshots = vec![Snapshot {
                    file: "synthetic".into(),
                    harness: Harness::Claude,
                    profiles: vec!["profile".into()],
                    bytes: 0,
                    device: 0,
                    inode: 0,
                    deferred_bytes: 0,
                    skipped_lines: 10000,
                    lines: 0,
                }];
                s
            }
            "shared" => {
                let mut s = data(4);
                let model = "claude-haiku-5-5";
                let mut other = s.requests[0].clone();
                other.id = "haiku-resume".into();
                other.session = "haiku-sample".into();
                other.model = Some(model.into());
                for p in &mut other.parts {
                    p.model = Some(model.into());
                }
                let mut event = s.events[0].clone();
                event.id = "haiku-boundary".into();
                event.session = other.session.clone();
                event.origins[0].session = other.session.clone();
                event.model = Some(model.into());
                event.request = Some(other.id.clone());
                std::sync::Arc::make_mut(event.pricing.as_mut().unwrap()).model =
                    Some(model.into());
                s.requests.push(other);
                s.events.push(event);
                for i in 0..3 {
                    let mut r = Request {
                        model: Some(model.into()),
                        ..at(&format!("haiku-{i}"), "haiku", (i + 1) * HOUR)
                    };
                    r.parts[0].model = Some(model.into());
                    r.parts[0].usage = writes(150000);
                    s.requests.push(r);
                }
                s.profiles[0].compaction = controls(
                    Harness::Claude,
                    json!({"autoCompactWindow": 400000}),
                    &Env::default(),
                    "settings.json",
                );
                s
            }
            // A Codex model without separate write billing cannot establish prefix retention.
            "codexOld" => {
                let mut s = scenario("codex");
                for r in &mut s.requests {
                    r.model = Some("gpt-5.4".into());
                    for p in &mut r.parts {
                        p.model = Some("gpt-5.4".into());
                    }
                    if r.id.starts_with("large-") {
                        r.parts[0].usage = Usage {
                            read: Some(100000),
                            ..codex_usage(300000, 0, 10)
                        };
                    }
                }
                s.events[0].model = Some("gpt-5.4".into());
                s
            }
            "since" => data(4),
            _ => unreachable!("unknown scenario {name}"),
        }
    }

    fn near(actual: Option<f64>, expected: f64) {
        let actual = actual.expect("priced");
        assert!((actual - expected).abs() < 1e-10, "{actual} != {expected}");
    }

    /// Recommendations and savings the TypeScript implementation reports for the same scans.
    #[test]
    fn unset_windows_take_the_latest_observed_trigger_else_the_documented_default() {
        let mut scan = data(4);
        let unset = |env: &Env| controls(Harness::Claude, json!({}), env, "settings.json");
        scan.profiles[0].compaction = unset(&Env::default());
        for (model, write) in [
            ("claude-fable-5-2", 1000),
            ("claude-opus-4-6", 150000),
            ("claude-sonnet-4-6", 250000),
            ("gateway-opus", 1000),
        ] {
            let mut r = at(model, model, HOUR);
            r.model = Some(model.into());
            r.parts[0].model = Some(model.into());
            r.parts[0].usage = writes(write);
            scan.requests.push(r);
        }
        let mut later = boundary();
        later.time = 10;
        later.pre_tokens = Some(310000);
        // Neither an idle compaction nor a manual one shows where the window triggers.
        let mut idle = boundary();
        idle.time = 20;
        idle.pre_tokens = Some(150000);
        idle.idle_attribution = IdleAttribution::Notice;
        let mut manual = boundary();
        manual.time = 30;
        manual.pre_tokens = Some(150000);
        manual.trigger = Some("manual".into());
        scan.events.extend([later, idle, manual]);
        crate::controls::tune_windows(&mut scan);
        let window = |scan: &Scan, model| window_control(&scan.profiles[0].compaction, Some(model));
        let observed = window(&scan, "claude-opus-5-5-20261101");
        assert_eq!(observed.value, Some(310000));
        assert!(observed.applicable);
        assert_eq!(
            observed.owner,
            "settings.json:modelSettings.claude-opus-5-5.autoCompactWindow"
        );
        assert_eq!(window(&scan, "claude-fable-5-2").value, Some(967_000));
        assert_eq!(window(&scan, "claude-opus-4-6").value, Some(200_000));
        // A [1m] variant may have run the request above 200k, and the gateway ID is undocumented.
        for model in ["claude-sonnet-4-6", "gateway-opus"] {
            let control = window(&scan, model);
            assert_eq!(control.value, None);
            assert!(!control.applicable);
            assert!(control.reason.contains("default is not established"));
        }

        let mut capped = scan.clone();
        let mut env = Env::default();
        env.insert("CLAUDE_CODE_DISABLE_1M_CONTEXT".into(), "1".into());
        capped.profiles[0].compaction = unset(&env);
        crate::controls::tune_windows(&mut capped);
        assert_eq!(window(&capped, "claude-fable-5-2").value, Some(200_000));
        assert_eq!(window(&capped, "claude-sonnet-4-6").value, Some(200_000));
        assert_eq!(window(&capped, "claude-opus-5-5").value, Some(200_000));
        assert_eq!(window(&capped, "gateway-opus").value, Some(200_000));

        // A declared window may size an unrecognized ID, but not a documented model.
        env.insert("CLAUDE_CODE_MAX_CONTEXT_TOKENS".into(), "0".into());
        capped.profiles[0].compaction = unset(&env);
        crate::controls::tune_windows(&mut capped);
        assert_eq!(window(&capped, "gateway-opus").value, Some(200_000));
        env.insert("CLAUDE_CODE_MAX_CONTEXT_TOKENS".into(), "400000".into());
        capped.profiles[0].compaction = unset(&env);
        crate::controls::tune_windows(&mut capped);
        assert_eq!(window(&capped, "gateway-opus").value, Some(400_000));
        assert_eq!(window(&capped, "claude-fable-5-2").value, Some(200_000));
        // Without the cap, a declared window sizes a gateway ID whose requests fit it.
        let mut declared = scan.clone();
        let mut env = Env::default();
        env.insert("CLAUDE_CODE_MAX_CONTEXT_TOKENS".into(), "400000".into());
        declared.profiles[0].compaction = unset(&env);
        crate::controls::tune_windows(&mut declared);
        let control = window(&declared, "gateway-opus");
        assert_eq!(control.value, Some(400_000));
        assert!(control.applicable);
        assert!(control.reason.contains("assumed"));
        assert!(
            control
                .owner
                .ends_with("(under CLAUDE_CODE_MAX_CONTEXT_TOKENS=400000)")
        );
        // A request above the declared window shows a [1m] spelling ran, which ignores it.
        env.insert("CLAUDE_CODE_MAX_CONTEXT_TOKENS".into(), "500".into());
        declared.profiles[0].compaction = unset(&env);
        crate::controls::tune_windows(&mut declared);
        assert_eq!(window(&declared, "gateway-opus").value, None);

        // Each recorded directory's controls, which idle and keepalive read, tune under what
        // sizes their context, and directories sized apart never share an owner.
        let directory = |compaction| crate::model::IdleControl {
            value: None,
            owner: String::new(),
            threshold: None,
            threshold_owner: String::new(),
            auto_compact: None,
            reason: String::new(),
            resolved: true,
            ttl: None,
            compaction: Some(compaction),
        };
        let sized = |key: &str, value: &str| {
            let mut env = Env::default();
            env.insert(key.into(), value.into());
            directory(unset(&env))
        };
        let mut directories = scan.clone();
        let explicit = controls(
            Harness::Claude,
            json!({"autoCompactWindow": 250000}),
            &Env::default(),
            "project.json",
        );
        let aliased = controls(
            Harness::Claude,
            json!({"modelSettings": {"fable": {"autoCompactWindow": 500000}}}),
            &Env::default(),
            "project.json",
        );
        let auto = |env: &Env| {
            controls(
                Harness::Claude,
                json!({"modelSettings": {"claude-fable-5-2": {"autoCompactWindow": "auto"}}}),
                env,
                "project.json",
            )
        };
        let mut cap = Env::default();
        cap.insert("CLAUDE_CODE_DISABLE_1M_CONTEXT".into(), "1".into());
        directories.profiles[0].idle_directories = Some(IndexMap::from_iter([
            ("/open".to_string(), directory(unset(&Env::default()))),
            (
                "/capped".to_string(),
                sized("CLAUDE_CODE_DISABLE_1M_CONTEXT", "1"),
            ),
            (
                "/declared".to_string(),
                sized("CLAUDE_CODE_MAX_CONTEXT_TOKENS", "300000"),
            ),
            (
                "/redeclared".to_string(),
                sized("CLAUDE_CODE_MAX_CONTEXT_TOKENS", "400000"),
            ),
            ("/explicit".to_string(), directory(explicit)),
            ("/aliased".to_string(), directory(aliased)),
            ("/auto".to_string(), directory(auto(&Env::default()))),
            ("/auto-capped".to_string(), directory(auto(&cap))),
        ]));
        // A trigger counts only under the sizing it ran with and without an explicit window.
        for (time, cwd, model, tokens) in [
            (35, "/open", "claude-fable-5-2", 400000),
            (40, "/capped", "claude-fable-5-2", 300000),
            (45, "/declared", "gateway-opus", 280000),
            (50, "/explicit", "claude-fable-5-2", 250000),
            (55, "/aliased", "claude-fable-5-2", 500000),
        ] {
            let mut e = boundary();
            e.time = time;
            e.cwd = Some(cwd.into());
            e.model = Some(model.into());
            e.pre_tokens = Some(tokens);
            directories.events.push(e);
        }
        crate::controls::tune_windows(&mut directories);
        let scoped = |cwd: &str, model| {
            let idle = &directories.profiles[0].idle_directories.as_ref().unwrap()[cwd];
            window_control(idle.compaction.as_ref().unwrap(), Some(model))
        };
        let row = "settings.json:modelSettings.claude-fable-5-2.autoCompactWindow";
        let profile = window(&directories, "claude-fable-5-2");
        assert_eq!((profile.value, profile.owner.as_str()), (Some(400000), row));
        assert_eq!(scoped("/open", "claude-fable-5-2"), profile);
        // A declared window never sizes a documented model.
        assert_eq!(scoped("/declared", "claude-fable-5-2"), profile);
        let held = scoped("/capped", "claude-fable-5-2");
        assert_eq!(held.value, Some(200_000));
        assert_eq!(
            held.owner,
            format!("{row} (under CLAUDE_CODE_DISABLE_1M_CONTEXT=1)")
        );
        let explicit = scoped("/explicit", "claude-fable-5-2");
        assert_eq!(explicit.value, Some(250000));
        assert_eq!(explicit.owner, "project.json:autoCompactWindow");
        let declared = scoped("/declared", "gateway-opus");
        assert_eq!(declared.value, Some(280000));
        assert_eq!(
            declared.owner,
            "settings.json:modelSettings.gateway-opus.autoCompactWindow (under CLAUDE_CODE_MAX_CONTEXT_TOKENS=300000)"
        );
        // A row that already names the model takes the sizing too.
        let auto_row = "project.json:modelSettings.claude-fable-5-2.autoCompactWindow";
        let auto = scoped("/auto", "claude-fable-5-2");
        assert_eq!((auto.value, auto.owner.as_str()), (Some(400000), auto_row));
        let auto_capped = scoped("/auto-capped", "claude-fable-5-2");
        assert_eq!(auto_capped.value, Some(200_000));
        assert_eq!(
            auto_capped.owner,
            format!("{auto_row} (under CLAUDE_CODE_DISABLE_1M_CONTEXT=1)")
        );
        // Another declared value, or none, sizes the ID differently, so the trigger is not theirs:
        // a directory declaring another value assumes its own declared window instead.
        let redeclared = scoped("/redeclared", "gateway-opus");
        assert_eq!(redeclared.value, Some(400000));
        assert!(
            redeclared
                .owner
                .ends_with("(under CLAUDE_CODE_MAX_CONTEXT_TOKENS=400000)")
        );
        assert!(redeclared.reason.contains("assumed"));
        assert_eq!(window(&directories, "gateway-opus").value, None);

        env.insert("DISABLE_AUTO_COMPACT".into(), "1".into());
        capped.profiles[0].compaction = unset(&env);
        crate::controls::tune_windows(&mut capped);
        let disabled = window(&capped, "claude-fable-5-2");
        assert_eq!(disabled.value, None);
        assert_eq!(disabled.owner, "settings.json:autoCompactWindow");
    }

    #[test]
    fn explicit_windows_are_capped_at_the_model_context_window() {
        let mut scan = data(4);
        for (model, write) in [
            ("claude-opus-4-6", 150000),
            ("claude-sonnet-4-6", 250000),
            ("gateway-opus", 1000),
        ] {
            let mut r = at(model, model, HOUR);
            r.model = Some(model.into());
            r.parts[0].model = Some(model.into());
            r.parts[0].usage = writes(write);
            scan.requests.push(r);
        }
        let tuned = |user: Value, env: &Env| {
            let mut scan = scan.clone();
            scan.profiles[0].compaction = controls(Harness::Claude, user, env, "settings.json");
            crate::controls::tune_windows(&mut scan);
            scan
        };
        let window = |scan: &Scan, model| window_control(&scan.profiles[0].compaction, Some(model));

        // Opus 4.6 runs at 200k without its [1m] variant; a request above 200k shows Sonnet 4.6's ran.
        let top = tuned(json!({"autoCompactWindow": 400000}), &Env::default());
        let opus = window(&top, "claude-opus-4-6");
        assert_eq!((opus.value, opus.setting), (Some(200_000), Some(400_000)));
        assert!(opus.applicable);
        let opus_row = "settings.json:modelSettings.claude-opus-4-6.autoCompactWindow";
        assert_eq!(opus.owner, format!("{opus_row} (capped at 200000)"));
        // Without a declared window, an ID outside the table may be one Claude Code recognizes.
        for model in ["claude-sonnet-4-6", "claude-opus-5-5", "gateway-opus"] {
            let control = window(&top, model);
            assert_eq!((control.value, control.setting), (Some(400_000), None));
            assert_eq!(control.owner, "settings.json:autoCompactWindow");
        }
        let below = tuned(json!({"autoCompactWindow": 150000}), &Env::default());
        assert_eq!(window(&below, "claude-opus-4-6").value, Some(150_000));

        // A per-model row already changes its model alone.
        let row = tuned(
            json!({"modelSettings": {"claude-opus-4-6": {"autoCompactWindow": 400000}}}),
            &Env::default(),
        );
        let opus = window(&row, "claude-opus-4-6");
        assert_eq!((opus.value, opus.setting), (Some(200_000), Some(400_000)));
        assert_eq!(opus.owner, format!("{opus_row} (capped at 200000)"));

        let held = " (under CLAUDE_CODE_DISABLE_1M_CONTEXT=1)";
        let mut env = Env::default();
        env.insert("CLAUDE_CODE_DISABLE_1M_CONTEXT".into(), "1".into());
        let capped = tuned(json!({"autoCompactWindow": 400000}), &env);
        for model in ["claude-opus-5-5", "claude-sonnet-4-6", "gateway-opus"] {
            let control = window(&capped, model);
            assert_eq!(control.value, Some(200_000));
            assert_eq!(
                control.owner,
                format!("settings.json:modelSettings.{model}.autoCompactWindow{held}")
            );
        }
        // A declared window sizes an unrecognized ID instead of the cap, and caps it in turn.
        let gateway_row = "settings.json:modelSettings.gateway-opus.autoCompactWindow";
        env.insert("CLAUDE_CODE_MAX_CONTEXT_TOKENS".into(), "400000".into());
        let declared = tuned(json!({"autoCompactWindow": 400000}), &env);
        assert_eq!(window(&declared, "gateway-opus").value, Some(400_000));
        assert_eq!(window(&declared, "claude-opus-5-5").value, Some(200_000));
        env.insert("CLAUDE_CODE_MAX_CONTEXT_TOKENS".into(), "300000".into());
        let declared = tuned(json!({"autoCompactWindow": 400000}), &env);
        let gateway = window(&declared, "gateway-opus");
        assert_eq!(
            (gateway.value, gateway.setting),
            (Some(300_000), Some(400_000))
        );
        assert_eq!(
            gateway.owner,
            format!("{gateway_row} (under CLAUDE_CODE_MAX_CONTEXT_TOKENS=300000)")
        );
        // Without the opt-out, a request above the declared window shows a [1m] spelling ran.
        let mut small = Env::default();
        small.insert("CLAUDE_CODE_MAX_CONTEXT_TOKENS".into(), "500".into());
        let escaped = tuned(json!({"autoCompactWindow": 400000}), &small);
        assert_eq!(window(&escaped, "gateway-opus").value, Some(400_000));
        small.insert("CLAUDE_CODE_DISABLE_1M_CONTEXT".into(), "1".into());
        let held_small = tuned(json!({"autoCompactWindow": 400000}), &small);
        assert_eq!(window(&held_small, "gateway-opus").value, Some(500));

        // The variable outranks every per-model row, so no setting changes one model alone.
        env.insert("CLAUDE_CODE_MAX_CONTEXT_TOKENS".into(), "400000".into());
        env.insert("CLAUDE_CODE_AUTO_COMPACT_WINDOW".into(), "400k".into());
        let shared = tuned(json!({}), &env);
        let opus = window(&shared, "claude-opus-5-5");
        assert_eq!((opus.value, opus.setting), (Some(200_000), Some(400_000)));
        assert!(!opus.applicable);
        assert_eq!(opus.owner, format!("CLAUDE_CODE_AUTO_COMPACT_WINDOW{held}"));
        assert!(opus.reason.contains(crate::controls::CAPPED_SHARED));
        let gateway = window(&shared, "gateway-opus");
        assert_eq!(gateway.owner, "CLAUDE_CODE_AUTO_COMPACT_WINDOW");
        let shared = tuned(json!({}), &{
            let mut env = Env::default();
            env.insert("CLAUDE_CODE_AUTO_COMPACT_WINDOW".into(), "400k".into());
            env
        });
        assert_eq!(
            window(&shared, "claude-opus-4-6").owner,
            "CLAUDE_CODE_AUTO_COMPACT_WINDOW (capped at 200000)"
        );
        assert_eq!(
            window(&shared, "claude-opus-5-5").owner,
            "CLAUDE_CODE_AUTO_COMPACT_WINDOW"
        );

        // Directories sized apart cap one row differently, so they never share its owner.
        let mut directories = scan.clone();
        let row = json!({"modelSettings": {"claude-opus-5-5": {"autoCompactWindow": 400000}}});
        let directory = |env: &Env| {
            let mut idle = crate::controls::idle_controls(&[], &Env::default()).unwrap();
            idle.compaction = Some(controls(Harness::Claude, row.clone(), env, "project.json"));
            idle
        };
        let mut opt_out = Env::default();
        opt_out.insert("CLAUDE_CODE_DISABLE_1M_CONTEXT".into(), "1".into());
        directories.profiles[0].idle_directories = Some(IndexMap::from_iter([
            ("/open".into(), directory(&Env::default())),
            ("/capped".into(), directory(&opt_out)),
        ]));
        crate::controls::tune_windows(&mut directories);
        let scoped = |cwd: &str| {
            let idle = &directories.profiles[0].idle_directories.as_ref().unwrap()[cwd];
            window_control(idle.compaction.as_ref().unwrap(), Some("claude-opus-5-5"))
        };
        let project_row = "project.json:modelSettings.claude-opus-5-5.autoCompactWindow";
        let open = scoped("/open");
        assert_eq!(
            (open.value, open.owner.as_str()),
            (Some(400_000), project_row)
        );
        let held_row = scoped("/capped");
        assert_eq!(
            (held_row.value, held_row.setting),
            (Some(200_000), Some(400_000))
        );
        assert_eq!(held_row.owner, format!("{project_row}{held}"));

        // The check replays the capped window and names the setting's own value.
        let mut scan = data(4);
        env.remove("CLAUDE_CODE_AUTO_COMPACT_WINDOW");
        scan.profiles[0].compaction = controls(
            Harness::Claude,
            json!({"autoCompactWindow": 400000}),
            &env,
            "settings.json",
        );
        crate::controls::tune_windows(&mut scan);
        let check = &checks(&scan)[0];
        assert_eq!(
            (check.base.current, check.setting),
            (Some(200_000), Some(400_000))
        );
        assert_eq!(
            check.base.owner,
            format!("settings.json:modelSettings.claude-opus-5-5.autoCompactWindow{held}")
        );
        assert!(check.base.assumptions[0].contains("context window"));
    }

    #[test]
    fn an_unset_window_is_priced_against_its_tuned_default() {
        let mut scan = data(4);
        scan.profiles[0].compaction =
            controls(Harness::Claude, json!({}), &Env::default(), "settings.json");
        assert_eq!(checks(&scan)[0].base.action, Action::Advisory);
        crate::controls::tune_windows(&mut scan);
        let observed = &checks(&scan)[0];
        assert_eq!(observed.base.current, Some(300000));
        assert_eq!(
            observed.base.owner,
            "settings.json:modelSettings.claude-opus-5-5.autoCompactWindow"
        );
        assert_ne!(observed.base.action, Action::Advisory);
        assert!(observed.base.savings_usd.is_some());

        scan.events[0].trigger = Some("manual".into());
        scan.profiles[0].compaction.tuned.clear();
        crate::controls::tune_windows(&mut scan);
        let documented = &checks(&scan)[0];
        assert_eq!(documented.base.current, Some(967_000));
        assert!(documented.unset);
        assert!(documented.base.assumptions[0].contains("documented default"));
        assert_ne!(documented.base.action, Action::Advisory);
    }

    #[test]
    fn checks_match_the_typescript_recommendations() {
        type Expected = (
            Option<i64>,
            Action,
            Option<f64>,
            &'static [(i64, Option<f64>)],
        );
        let expected: [(&str, Expected); 17] = [
            (
                "codexOld",
                (
                    None,
                    Action::Advisory,
                    None,
                    &[(20000, None), (400000, Some(0.0))],
                ),
            ),
            (
                "since",
                (
                    Some(100000),
                    Action::Change,
                    Some(1.31),
                    &[(100000, Some(1.31)), (400000, Some(0.0))],
                ),
            ),
            (
                "base",
                (
                    Some(100000),
                    Action::Change,
                    Some(1.96),
                    &[(100000, Some(1.96)), (400000, Some(0.0))],
                ),
            ),
            (
                "one",
                (
                    Some(100000),
                    Action::Change,
                    Some(0.01),
                    &[(100000, Some(0.01)), (400000, Some(0.0))],
                ),
            ),
            (
                "larger",
                (
                    Some(400000),
                    Action::Keep,
                    Some(0.0),
                    &[(100000, Some(-0.01)), (400000, Some(0.0))],
                ),
            ),
            (
                "current",
                (
                    Some(100000),
                    Action::Keep,
                    Some(0.0),
                    &[(100000, Some(0.0))],
                ),
            ),
            (
                "codex",
                (
                    Some(20000),
                    Action::Change,
                    Some(4.5702),
                    &[
                        (20000, Some(4.5702)),
                        (272000, Some(4.5702)),
                        (400000, Some(0.0)),
                    ],
                ),
            ),
            (
                "warm1h",
                (
                    Some(100000),
                    Action::Change,
                    Some(1.466),
                    &[(100000, Some(1.466)), (400000, Some(0.0))],
                ),
            ),
            (
                "multipart",
                (
                    Some(100000),
                    Action::Change,
                    Some(0.426),
                    &[(100000, Some(0.426)), (400000, Some(0.0))],
                ),
            ),
            (
                "attempt",
                (
                    Some(100000),
                    Action::Change,
                    Some(0.062),
                    &[(100000, Some(0.062)), (400000, Some(0.0))],
                ),
            ),
            (
                "unknownSmall",
                (
                    Some(100000),
                    Action::Change,
                    Some(1.96),
                    &[(100000, Some(1.96)), (400000, Some(0.0))],
                ),
            ),
            (
                "unknownMissing",
                (
                    Some(100000),
                    Action::Change,
                    Some(1.96),
                    &[(100000, Some(1.96)), (400000, None)],
                ),
            ),
            (
                "copies",
                (
                    Some(100000),
                    Action::Change,
                    Some(19.6),
                    &[(100000, Some(19.6)), (400000, None)],
                ),
            ),
            (
                "historical",
                (
                    Some(100000),
                    Action::Change,
                    Some(1.96),
                    &[(100000, Some(1.96)), (400000, Some(0.0))],
                ),
            ),
            (
                "unlinked",
                (
                    None,
                    Action::Advisory,
                    None,
                    &[(100000, None), (400000, Some(0.0))],
                ),
            ),
            (
                "skipped",
                (
                    None,
                    Action::Advisory,
                    None,
                    &[(100000, None), (400000, None)],
                ),
            ),
            (
                "shared",
                (
                    Some(100000),
                    Action::Change,
                    Some(2.15381),
                    &[(100000, Some(2.15381)), (400000, Some(0.0))],
                ),
            ),
        ];
        for (name, (recommended, action, savings, windows)) in expected {
            // An explicit --since bounds the window instead of the first selected request.
            let since = (name == "since").then_some(2 * HOUR);
            let result = checks_since(&scenario(name), since);
            assert_eq!(result.len(), 1, "{name}");
            let check = &result[0];
            assert_eq!(check.base.recommended, recommended, "{name}");
            assert_eq!(check.base.action, action, "{name}");
            match savings {
                Some(s) => near(check.base.savings_usd, s),
                None => assert_eq!(check.base.savings_usd, None, "{name}"),
            }
            assert_eq!(check.scenarios.len(), windows.len(), "{name}");
            for (scenario, &(window, savings)) in check.scenarios.iter().zip(windows) {
                assert_eq!(scenario.window, window, "{name}");
                match savings {
                    Some(s) => near(scenario.savings_usd, s),
                    None => assert_eq!(scenario.savings_usd, None, "{name} {window}"),
                }
            }
        }
    }

    #[test]
    fn missing_codex_tiers_price_scenarios_without_selecting_a_winner() {
        let mut scan = scenario("codex");
        for r in &mut scan.requests {
            for p in &mut r.parts {
                p.tier = None;
                p.speed = None;
            }
        }
        let result = &checks(&scan)[0];
        assert_eq!(result.base.action, Action::Advisory);
        assert_eq!(result.base.recommended, None);
        assert_eq!(result.base.savings_usd, None);
        assert_eq!(result.base.monthly_savings_usd, None);
        assert_eq!(result.base.break_even, None);
        assert!(
            result
                .base
                .uncertainty
                .iter()
                .any(|s| s == TIER_SENSITIVITY)
        );
        assert!(
            result
                .scenarios
                .iter()
                .any(|s| s.savings_usd.is_some_and(|v| v > 0.0))
        );
        assert!(result.baseline_cost.total_usd.is_some());
    }

    #[test]
    fn unused_pre_window_tiers_do_not_block_a_recorded_window() {
        let mut scan = scenario("codex");
        let expected = checks_since(&scan, Some(0))[0].base.recommended;
        assert!(expected.is_some());
        let mut unrelated = scan.requests.last().unwrap().clone();
        unrelated.id = "old-unrelated".into();
        unrelated.session = "old-unrelated".into();
        unrelated.time = -2000;
        unrelated.end_time = -2000;
        unrelated.parts[0].tier = None;
        scan.requests.insert(0, unrelated);
        let result = &checks_since(&scan, Some(0))[0];
        assert_eq!(result.base.recommended, expected);
        assert!(
            !result
                .base
                .uncertainty
                .iter()
                .any(|s| s == TIER_SENSITIVITY)
        );
    }

    #[test]
    fn net_winner_prices_summary_and_new_prefix_once() {
        let check = &checks(&data(4))[0];
        assert_eq!(check.base.current, Some(400000));
        assert_eq!(
            check.base.owner,
            "settings.json:modelSettings.claude-opus-5-5.autoCompactWindow"
        );
        let scenario = &check.scenarios[0];
        near(
            Some(check.baseline_cost.total_usd.unwrap() - scenario.cost.total_usd.unwrap()),
            1.96,
        );
        assert_eq!(scenario.added.len(), 1);
        let added = &scenario.added[0];
        near(added.summary_cost.total_usd, 0.64);
        near(added.prefix_cost.total_usd, 0.1);
        assert_eq!(
            (
                added.input_tokens,
                added.summary_tokens,
                added.prefix_tokens
            ),
            (150000, 2000, 20000)
        );
        assert_eq!(
            added.basis,
            [CompactionBasis {
                event: "sample-boundary".into(),
                request: "sample-resume".into(),
            }]
        );
        near(check.observed_summary_cost.total_usd, 1.24);
        assert!(
            check
                .base
                .break_even
                .as_deref()
                .unwrap()
                .starts_with("$1.9600 of")
        );
        near(scenario.sensitivity.reread_break_even_usd, 1.96);
        assert_eq!(check.ttls.main, Some(Ttl::FiveMinutes));
        assert_eq!(check.base.since, Some(0));
        assert_eq!(check.base.requests, 5);
    }

    #[test]
    fn unpriced_sessions_and_unknown_iterations_stay_separate_pairs() {
        let missing = &checks(&scenario("unknownMissing"))[0].scenarios[0];
        assert_eq!(missing.coverage.total - missing.coverage.priced, 1);
        let copies = &checks(&scenario("copies"))[0].scenarios[0];
        assert_eq!(copies.added.len(), 10);
        assert_eq!(copies.coverage.total - copies.coverage.priced, 4);
        let historical = &checks(&scenario("historical"))[0];
        assert!(
            historical
                .base
                .uncertainty
                .iter()
                .any(|u| u.starts_with("historical recorded limits differ"))
        );
        let shared = &checks(&scenario("shared"))[0];
        assert_eq!(shared.base.owner, "settings.json:autoCompactWindow");
        // Equal times order by id, so the haiku resume replays first.
        assert_eq!(
            shared.models,
            [
                Some("claude-haiku-5-5".into()),
                Some("claude-opus-5-5".into())
            ]
        );
    }

    fn sample_scan() -> (Vec<Request>, Vec<StructuralEvent>) {
        let s = data(2);
        (s.requests, s.events)
    }

    fn samples_of<'a>(
        requests: &'a [Request],
        events: &'a [StructuralEvent],
        summaries: &[CompactionCharge],
    ) -> CompactionSamples<'a> {
        let requests: Vec<&Request> = requests.iter().collect();
        let events: Vec<&StructuralEvent> = events.iter().collect();
        compaction_samples(&requests, &events, summaries)
    }

    #[test]
    fn samples_link_billed_summary_and_first_resumed_request() {
        let (requests, events) = sample_scan();
        let samples = samples_of(&requests, &events, &[]);
        assert_eq!(samples.samples.len(), 1);
        let s = &samples.samples[0];
        assert_eq!(
            (s.event.to_string(), &*s.request.id, s.output, s.prefix),
            ("sample-boundary".to_string(), "sample-resume", 2000, 20000)
        );
        assert_eq!(s.summary.kind, PartKind::Compaction);
        // A charge without a request falls back to the event's own link.
        let unlinked = CompactionCharge {
            event: "sample-boundary".into(),
            request: None,
            evidence: Evidence::Estimated,
            cost: zero_cost(),
            ttl_cost: PerTtl {
                five_minutes: zero_cost(),
                one_hour: zero_cost(),
            },
            assumptions: vec![],
        };
        assert_eq!(
            samples_of(&requests, &events, std::slice::from_ref(&unlinked))
                .samples
                .len(),
            1
        );
        // A charge naming another request wins over the event's link.
        let other = CompactionCharge {
            request: Some("large-0".into()),
            ..unlinked
        };
        assert!(samples_of(&requests, &events, &[other]).samples.is_empty());
    }

    #[test]
    fn samples_take_only_the_first_resumed_request_before_the_next_boundary() {
        let (mut requests, mut events) = sample_scan();
        // The first message request after the boundary is multipart: no later request substitutes.
        let mut multipart = at("multipart", "sample", 500);
        multipart.parts.push(multipart.parts[0].clone());
        requests.insert(0, multipart);
        assert!(samples_of(&requests, &events, &[]).samples.is_empty());
        requests.remove(0);
        // A later boundary in the same session ends the search before the resumed request.
        let mut next = boundary();
        next.id = "next".into();
        next.time = 900;
        next.request = None;
        events.push(next);
        let samples = samples_of(&requests, &events, &[]);
        assert!(samples.samples.is_empty());
        // Unless it shares no session alias with the first.
        events[1].origins[0].session = "elsewhere".into();
        assert_eq!(samples_of(&requests, &events, &[]).samples.len(), 1);
    }

    #[test]
    fn samples_need_event_model_metadata_and_measured_positive_prefixes() {
        let (requests, mut events) = sample_scan();
        events[0].model = None;
        assert_eq!(samples_of(&requests, &events, &[]).samples.len(), 1);
        events[0].pricing = None;
        assert!(samples_of(&requests, &events, &[]).samples.is_empty());
        let (mut requests, events) = sample_scan();
        requests[0].evidence = RequestEvidence::Estimated;
        assert!(samples_of(&requests, &events, &[]).samples.is_empty());
        let (mut requests, events) = sample_scan();
        requests[0].parts[1].usage.output = Some(0);
        assert!(samples_of(&requests, &events, &[]).samples.is_empty());
    }

    /// A direct transliteration of the TypeScript scan, quadratic but obviously first-match.
    fn naive_samples(
        requests: &[&Request],
        events: &[&StructuralEvent],
        summaries: &[CompactionCharge],
    ) -> Vec<(Id, Text)> {
        let mut result = vec![];
        let boundaries: Vec<&&StructuralEvent> = events
            .iter()
            .filter(|e| e.kind == EventKind::Compaction)
            .collect();
        for event in &boundaries {
            let aliases: HashSet<&str> = event.origins.iter().map(|o| &*o.session).collect();
            let next_boundary = boundaries
                .iter()
                .filter(|e| {
                    e.time > event.time && e.origins.iter().any(|o| aliases.contains(&*o.session))
                })
                .map(|e| e.time)
                .min();
            let next = requests.iter().find(|r| {
                aliases.contains(&*r.session)
                    && r.time > event.time
                    && next_boundary.is_none_or(|b| r.time < b)
                    && r.parts.iter().any(is_message)
            });
            let Some(next) = next else { continue };
            if next.parts.iter().filter(|p| is_message(p)).count() != 1 {
                continue;
            }
            let p = next.parts.iter().find(|p| is_message(p)).unwrap();
            if event
                .model
                .as_deref()
                .or(event.pricing.as_ref().and_then(|x| x.model.as_deref()))
                != p.model.as_deref()
            {
                continue;
            }
            if prefix(next.harness, &p.usage).is_none_or(|s| s <= 0) {
                continue;
            }
            let charge = summaries.iter().find(|c| c.event == event.id);
            let key = charge
                .and_then(|c| c.request.clone())
                .or(event.request.clone());
            let linked = requests.iter().rev().find(|r| Some(&r.id) == key.as_ref());
            let Some(linked) = linked else { continue };
            let parts: Vec<_> = linked
                .parts
                .iter()
                .filter(|p| p.kind == PartKind::Compaction)
                .collect();
            if parts.len() != 1 || parts[0].usage.output.is_none_or(|o| o <= 0) {
                continue;
            }
            result.push((event.id.clone(), next.id.clone()));
        }
        result
    }

    #[test]
    fn indexed_samples_match_the_first_match_scan() {
        let mut seed: u64 = 7;
        let mut next = move |n: u64| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) % n
        };
        for _ in 0..200 {
            let sessions = ["a", "b", "c"];
            let mut requests = vec![];
            for i in 0..12 {
                let mut r = at(
                    &format!("r{}", next(8)),
                    sessions[next(3) as usize],
                    next(10) as Millis * 100,
                );
                r.parts[0].usage = writes(next(3) as i64 * 1000);
                match next(5) {
                    0 => r.parts[0].kind = PartKind::Compaction,
                    1 => r.parts.push(BillingPart {
                        kind: PartKind::Compaction,
                        usage: usage(0, 0, next(2) as i64 * 100, Ttl::FiveMinutes, 1000),
                        ..r.parts[0].clone()
                    }),
                    2 => r.parts.push(r.parts[0].clone()),
                    _ => {}
                }
                r.id = format!("{}-{i}", r.id).into();
                requests.push(r);
            }
            let mut events = vec![];
            for i in 0..4 {
                let mut e = boundary();
                e.id = format!("e{i}").into();
                e.time = next(10) as Millis * 100 + 50;
                e.origins[0].session = sessions[next(3) as usize].into();
                if next(2) == 0 {
                    let mut extra = e.origins[0].clone();
                    extra.session = sessions[next(3) as usize].into();
                    e.origins.push(extra);
                }
                e.request = Some(requests[next(12) as usize].id.clone());
                events.push(e);
            }
            for sorted in [false, true] {
                if sorted {
                    requests.sort_by_key(|r| r.time);
                }
                let refs: Vec<&Request> = requests.iter().collect();
                let events: Vec<&StructuralEvent> = events.iter().collect();
                let indexed: Vec<(Id, Text)> = compaction_samples(&refs, &events, &[])
                    .samples
                    .iter()
                    .map(|s| (s.event.clone(), s.request.id.clone()))
                    .collect();
                assert_eq!(indexed, naive_samples(&refs, &events, &[]));
            }
        }
    }

    fn window_options<'a>(
        events: &'a WindowEvents<'a>,
        estimates: &'a IndexMap<Text, UsageEstimate>,
        catalog: &'a Catalog,
        samples: &'a CompactionSamples<'a>,
    ) -> WindowOptions<'a> {
        WindowOptions {
            events,
            estimates,
            catalog,
            samples,
            since: 0,
            ttls: PerBucket {
                main: Some(Ttl::FiveMinutes),
                subagent: Some(Ttl::FiveMinutes),
            },
        }
    }

    #[test]
    fn replay_windows_without_a_value_returns_inputs_unapplied() {
        let s = data(2);
        let catalog = prices();
        let estimates = IndexMap::default();
        let events = WindowEvents::new(&s.events);
        let samples = samples_of(&s.requests, &s.events, &[]);
        let options = window_options(&events, &estimates, &catalog, &samples);
        let models = [Some(Text::from("claude-opus-5-5"))];
        let result = replay_windows(
            &s.requests,
            &[WindowSetting {
                value: None,
                models: &models,
            }],
            &options,
        );
        assert!(!result.applied);
        assert!(
            result
                .requests
                .iter()
                .all(|r| matches!(r, Modeled::Recorded(_)))
        );
        let applied = replay_windows(
            &s.requests,
            &[WindowSetting {
                value: Some(100000),
                models: &models,
            }],
            &options,
        );
        assert!(applied.applied);
        // Only the two affected requests are copied; the rest stay borrowed.
        let owned: Vec<&str> = applied
            .requests
            .iter()
            .filter(|r| matches!(r, Modeled::Changed(_)))
            .map(|r| &*r.id)
            .collect();
        assert_eq!(owned, ["large-0", "large-1"]);
        assert_eq!(applied.added.len(), 1);
        assert_eq!(applied.events.len(), 1);
        assert_eq!(applied.events[0].id.to_string(), "modeled:large-0:0");
        assert_eq!(applied.requests[1].parts[0].usage.input, Some(20000));
        // The second request keeps the removed context; its expired prefix is rewritten cold.
        assert_eq!(applied.requests[2].parts[0].usage.write, Some(20000));
        assert!(applied.errors.is_empty());
    }

    #[test]
    fn a_bailed_out_part_still_updates_the_session_state() {
        let mut s = data(1);
        let mut unknown = at("unknown-prefix", "session", HOUR + 1000);
        unknown.parts[0].usage = writes(160000);
        unknown.parts[0].usage.read = None;
        let mut smaller = at("smaller", "session", HOUR + 2000);
        smaller.parts[0].usage = writes(140000);
        s.requests.extend([unknown, smaller]);
        let catalog = prices();
        let estimates = IndexMap::default();
        let events = WindowEvents::new(&s.events);
        let samples = samples_of(&s.requests, &s.events, &[]);
        let options = window_options(&events, &estimates, &catalog, &samples);
        let models = [Some(Text::from("claude-opus-5-5"))];
        let result = replay_windows(
            &s.requests,
            &[WindowSetting {
                value: Some(100000),
                models: &models,
            }],
            &options,
        );
        // The missing prefix fails, and its cleared input then hides the later decrease.
        assert_eq!(
            result.errors,
            ["missing input/cache geometry or iteration semantics can affect window eligibility"]
        );
        assert_eq!(result.uncertain.get("session"), Some(&(HOUR + 1000)));
    }

    #[test]
    fn version_support_reads_components_like_javascript_numbers() {
        assert_eq!(js_number(""), Some(0.0));
        assert_eq!(js_number(" 2 "), Some(2.0));
        assert_eq!(js_number("0x1F"), Some(31.0));
        assert_eq!(js_number("1e3"), Some(1000.0));
        assert_eq!(js_number("-Infinity"), Some(f64::NEG_INFINITY));
        assert_eq!(js_number("inf"), None);
        assert_eq!(js_number("1a"), None);
        assert!(version_supported(Some("2.1.288"), 288));
        assert!(version_supported(Some("3.0.0"), 288));
        assert!(version_supported(Some("2.01.0x200"), 288));
        assert!(!version_supported(Some("2.1.287"), 288));
        assert!(!version_supported(Some("2.1"), 288));
        assert!(!version_supported(Some("2.1.x"), 288));
        assert!(version_supported(Some("2.1.-0"), 0));
        assert!(!version_supported(None, 0));
    }

    #[test]
    fn median_rounds_even_counts_up() {
        assert_eq!(median(vec![3, 1, 2]), 2);
        assert_eq!(median(vec![1, 4]), 3);
        assert_eq!(median(vec![1, 2]), 2);
    }

    #[test]
    fn skipped_lines_sum_the_profile_snapshots() {
        let s = scenario("skipped");
        assert_eq!(skipped_lines(&s, "profile"), 10000);
        assert_eq!(skipped_lines(&s, "other"), 0);
    }

    /// Matches the TypeScript replayWindows on the same scan, including an inert first window.
    #[test]
    fn replay_windows_applies_estimates_in_the_first_pass_and_chains_later_ones() {
        let mut scan = scenario("attempt");
        scan.requests[3].time = scan.requests[2].time + 60000;
        let catalog = prices();
        let estimates = estimate_usage(&scan);
        let mut requests: Vec<&Request> = scan.requests.iter().collect();
        requests.sort_by_key(|r| r.time);
        let events: Vec<&StructuralEvent> = scan.events.iter().collect();
        let samples = compaction_samples(&requests, &events, &[]);
        let index = WindowEvents::new(events.iter().copied());
        let options = window_options(&index, &estimates, &catalog, &samples);
        let models = [Some(Text::from("claude-opus-5-5"))];
        let windows = [Some(200000), None, Some(100000)].map(|value| WindowSetting {
            value,
            models: &models,
        });
        let result = replay_windows(requests.iter().copied(), &windows, &options);
        let geometry: Vec<_> = result
            .requests
            .iter()
            .map(|r| {
                let u = r.parts[0].usage;
                (&*r.id, r.evidence, u.input, u.read, u.write)
            })
            .collect();
        assert_eq!(
            geometry,
            [
                (
                    "sample-resume",
                    RequestEvidence::Measured,
                    Some(20000),
                    Some(0),
                    Some(20000)
                ),
                (
                    "large-0",
                    RequestEvidence::Measured,
                    Some(20000),
                    Some(0),
                    Some(20000)
                ),
                (
                    "attempt",
                    RequestEvidence::Estimated,
                    Some(20000),
                    Some(20000),
                    Some(0)
                ),
                (
                    "large-1",
                    RequestEvidence::Measured,
                    Some(20000),
                    Some(20000),
                    Some(0)
                ),
            ]
        );
        assert_eq!(result.added.len(), 1);
        near(result.added[0].summary_cost.total_usd, 0.64);
        near(result.added[0].prefix_cost.total_usd, 0.1);
        assert!(result.errors.is_empty() && result.uncertain.is_empty());
        let affected: Vec<&str> = result.affected.iter().map(|a| &**a).collect();
        assert_eq!(affected, ["large-0", "attempt", "large-1"]);
        let modeled: Vec<_> = result
            .events
            .iter()
            .map(|e| (e.id.to_string(), e.time))
            .collect();
        assert_eq!(modeled, [("modeled:large-0:0".to_string(), HOUR)]);
        assert_eq!(result.estimated_parts.len(), 3);
    }

    /// The TypeScript compaction tests, each through `analyze` (with the TTL checks it runs) and the text report.
    mod end_to_end {
        use super::{at, controls, data, near, usage, writes};
        use crate::controls::{CompactionSources, Env, compaction_controls, window_control};
        use crate::discovery::DiscoveryOptions;
        use crate::model::*;
        use crate::reader::{ScanOptions, scan};
        use crate::replay::{AnalyzeOptions, analyze};
        use crate::report::{ReportOptions, format_report};
        use crate::test_support::{Workspace, make_request, prices};
        use serde_json::{Map, Value, json};

        const NOW: Millis = 18_000_000;
        const HOUR: Millis = 3_600_000;

        fn report(scan: &Scan) -> Report {
            analyze(AnalyzeOptions {
                scan: scan.clone(),
                catalog: &prices(),
                since: None,
                now: NOW,
                max_cost_per_use_usd: 1.0,
            })
            .unwrap()
        }

        fn windows(report: &Report) -> Vec<&CompactionCheck> {
            report
                .checks
                .iter()
                .filter_map(|c| match c {
                    CheckResult::Compaction(c) => Some(c),
                    _ => None,
                })
                .collect()
        }

        fn compaction(report: &Report) -> &CompactionCheck {
            windows(report)[0]
        }

        fn check(scan: &Scan) -> CompactionCheck {
            compaction(&report(scan)).clone()
        }

        fn text(report: &Report, width: Option<usize>, details: bool) -> String {
            format_report(
                report,
                ReportOptions {
                    width,
                    details,
                    ..ReportOptions::default()
                },
            )
        }

        fn window(check: &CompactionCheck, window: i64) -> &WindowScenario {
            check.scenarios.iter().find(|c| c.window == window).unwrap()
        }

        fn without_whitespace(text: &str) -> String {
            text.chars().filter(|c| !c.is_whitespace()).collect()
        }

        fn codex_usage(value: Value) -> Usage {
            crate::reader::usage(&value, Harness::Codex).unwrap()
        }

        fn object(value: Value) -> Map<String, Value> {
            value.as_object().cloned().unwrap()
        }

        fn scan_options(w: &Workspace) -> ScanOptions {
            let text = |p: &std::path::Path| p.to_string_lossy().into_owned();
            ScanOptions {
                discovery: DiscoveryOptions {
                    home: text(&w.home),
                    env: Env::default(),
                    claude_dirs: vec![],
                    codex_dirs: vec![],
                    t3_dirs: vec![],
                    managed_settings_file: text(&w.managed_settings_file()),
                    codex_system_config_file: text(&w.home.join("system.toml")),
                    codex_requirements_file: text(&w.home.join("requirements.toml")),
                },
                since: None,
            }
        }

        // Ported from compaction.test.ts: "net window winner prices summary and new prefix once, retaining observed summaries".
        #[test]
        fn net_window_winner_prices_summary_and_new_prefix_once_retaining_observed_summaries() {
            let report = report(&data(4));
            let check = compaction(&report);
            assert_eq!(check.base.action, Action::Change);
            assert_eq!(check.base.current, Some(400000));
            assert_eq!(check.base.recommended, Some(100000));
            assert_eq!(
                check.base.owner,
                "settings.json:modelSettings.claude-opus-5-5.autoCompactWindow"
            );
            let scenario = window(check, 100000);
            near(scenario.savings_usd, 1.96);
            near(
                Some(check.baseline_cost.total_usd.unwrap() - scenario.cost.total_usd.unwrap()),
                1.96,
            );
            assert_eq!(scenario.added.len(), 1);
            near(scenario.added[0].summary_cost.total_usd, 0.64);
            near(scenario.added[0].prefix_cost.total_usd, 0.1);
            near(check.observed_summary_cost.total_usd, 1.24);
            assert_eq!(
                scenario.added[0].basis,
                [CompactionBasis {
                    event: "sample-boundary".into(),
                    request: "sample-resume".into(),
                }]
            );
            assert!(text(&report, None, true).contains("compaction loses detail"));
            assert!(check.base.break_even.as_deref().unwrap().contains("1.9600"));
            near(scenario.sensitivity.reread_break_even_usd, 1.96);
            assert_eq!(check.ttls.main, Some(Ttl::FiveMinutes));
        }

        // Ported from compaction.test.ts: "window presentation keeps conditional baselines, every scenario and geometry evidence reachable".
        #[test]
        fn window_presentation_keeps_conditional_baselines_every_scenario_and_geometry_evidence() {
            let report = report(&data(4));
            let check = compaction(&report);
            let compact = without_whitespace(&text(&report, Some(40), true));
            let values = [&*check.base.owner, &*check.base.baseline]
                .into_iter()
                .chain(check.models.iter().flatten().map(|m| &**m))
                .chain(check.base.assumptions.iter().map(String::as_str))
                .chain(check.base.uncertainty.iter().map(String::as_str))
                .chain(check.base.sources.iter().map(String::as_str));
            for value in values {
                assert!(compact.contains(&without_whitespace(value)), "{value}");
            }
            assert!(compact.contains("Current:400000tokens"));
            assert!(compact.contains("Suggested:100000tokens"));
            assert!(text(&report, None, false).contains("degrade as context grows (context rot)"));
            assert!(compact.contains("ConditionalTTLbaseline:main5m;subagent5m"));
            for scenario in &check.scenarios {
                assert!(compact.contains(&format!("Candidatewindow:{}tokens", scenario.window)));
                if !scenario.added.is_empty() {
                    assert!(compact.contains(&format!(
                        "Addedcompactions:{}summaries;input",
                        scenario.added.len()
                    )));
                    assert!(compact.contains("Addedsummaries:"));
                    assert!(compact.contains("Resumedprefixes:"));
                }
                for added in &scenario.added {
                    for basis in &added.basis {
                        assert!(compact.contains(&format!(
                            "boundary{};resumedrequest{}",
                            basis.event, basis.request
                        )));
                    }
                }
            }
            let mut unknown = report.clone();
            let unresolved = unknown
                .checks
                .iter_mut()
                .find_map(|c| match c {
                    CheckResult::Compaction(c) => Some(c),
                    _ => None,
                })
                .unwrap();
            unresolved.base.current = None;
            unresolved.base.recommended = None;
            unresolved.base.action = Action::Advisory;
            unresolved.base.evidence = Evidence::Unknown;
            unresolved.base.savings_usd = None;
            unresolved.base.monthly_savings_usd = None;
            unresolved.baseline_cost = Cost {
                known_usd: 1.23,
                total_usd: None,
                unknown: vec!["synthetic unknown charge".into()],
                assumptions: vec![],
            };
            let plain = text(&unknown, None, true);
            assert!(plain.contains("ADVISORY  compaction-window"), "{plain}");
            assert!(plain.contains("Current-window baseline cost: unknown ($1.23 known subtotal)"));
            assert!(plain.contains(
                "Estimated net saving vs baseline: unknown over window; unknown / 30 days"
            ));
        }

        // Ported from compaction.test.ts: "summary overhead can lose, sensitivity cannot veto a positive aggregate estimate".
        #[test]
        fn summary_overhead_can_lose_and_sensitivity_cannot_veto_a_positive_aggregate() {
            let one = check(&data(1));
            assert_eq!(one.base.recommended, Some(100000));
            assert_eq!(one.base.action, Action::Change);
            let scenario = &one.scenarios[0];
            near(scenario.savings_usd, 0.01);
            assert!(scenario.sensitivity.cold_summary_savings_usd.unwrap() < 0.0);
            let mut larger = data(1);
            larger.events[0].post_tokens = Some(3000);
            larger.requests[0].parts[1].usage.output = Some(3000);
            let loss = check(&larger);
            assert_eq!(loss.base.recommended, Some(400000));
            assert_eq!(loss.base.action, Action::Keep);
            assert!(loss.scenarios[0].savings_usd.unwrap() < 0.0);
        }

        // Ported from compaction.test.ts: "current smaller window is the comparison baseline, rather than old larger history".
        #[test]
        fn current_smaller_window_is_the_comparison_baseline() {
            let mut s = data(4);
            s.profiles[0].compaction.models["claude-opus-5-5"].value = Some(100000);
            let check = check(&s);
            assert_eq!(check.base.recommended, Some(100000));
            assert_eq!(check.base.action, Action::Keep);
            near(check.base.savings_usd, 0.0);
            near(check.scenarios[0].savings_usd, 0.0);
            near(
                check.baseline_cost.total_usd,
                check.scenarios[0].cost.total_usd.unwrap(),
            );
        }

        // Ported from compaction.test.ts: "unsupported affected usage is unpriced; unknown unchanged output cancels with provenance".
        #[test]
        fn unsupported_affected_usage_is_unpriced_and_unknown_unchanged_output_cancels() {
            let mut s = data(4);
            s.requests[2].parts[0].usage.output = None;
            let output = check(&s);
            assert_eq!(output.baseline_cost.total_usd, None);
            assert_eq!(output.base.action, Action::Change);
            near(output.base.savings_usd, 1.96);
            let mut missing = data(4);
            missing.requests[2].parts[0].usage.input = None;
            missing.requests[2].evidence = RequestEvidence::Unknown;
            let unknown = check(&missing);
            assert_eq!(unknown.base.action, Action::Advisory);
            assert_eq!(unknown.scenarios[0].savings_usd, None);
            assert!(unknown.base.uncertainty.join(" ").contains("missing input"));
            let mut no_sample = data(4);
            no_sample.events[0].post_tokens = None;
            no_sample.requests[0].parts[1].usage.output = None;
            assert_eq!(check(&no_sample).scenarios[0].savings_usd, None);
        }

        // Ported from compaction.test.ts: "historical-only or unsupported client controls remain advisory, including unknown candidates".
        #[test]
        fn historical_only_or_unsupported_client_controls_remain_advisory() {
            let mut s = data(4);
            let control = &mut s.profiles[0].compaction.models["claude-opus-5-5"];
            control.value = None;
            control.applicable = false;
            let unknown_current = check(&s);
            assert_eq!(unknown_current.base.recommended, Some(100000));
            assert_eq!(unknown_current.base.action, Action::Advisory);
            let mut old = data(4);
            for r in &mut old.requests {
                r.version = Some("2.1.287".into());
            }
            assert_eq!(check(&old).base.action, Action::Advisory);
            for r in &mut old.requests {
                r.version = Some("3.unverified".into());
            }
            assert_eq!(check(&old).base.action, Action::Advisory);
            let mut unsupported = data(4);
            unsupported.requests[2].parts[0].model = Some("unknown-model".into());
            assert_eq!(check(&unsupported).scenarios[0].savings_usd, None);
        }

        // Ported from compaction.test.ts: "Codex standalone compaction uses linked billed summary and cold prefix at the full context tier".
        #[test]
        fn codex_standalone_compaction_uses_linked_billed_summary_and_cold_prefix() {
            let mut s = data(4);
            let p = &mut s.profiles[0];
            p.harness = Harness::Codex;
            p.settings = [(
                "model_auto_compact_token_limit".into(),
                Scalar::Number(400000.0),
            )]
            .into_iter()
            .collect();
            let mut user = json!({ "model_auto_compact_token_limit": 400000 });
            p.compaction = controls(Harness::Codex, user.clone(), &Env::default(), "config.toml");
            s.requests[0].parts.truncate(1);
            for r in &mut s.requests {
                r.harness = Harness::Codex;
                r.api = Api::Responses;
                r.provider = Some("openai".into());
                r.model = Some("gpt-6.1-sol".into());
                r.parts[0].model = r.model.clone();
                let size = if &*r.id == "sample-resume" {
                    20000
                } else {
                    300000
                };
                r.parts[0].usage = codex_usage(json!({
                    "input_tokens": size,
                    "cached_input_tokens": 0,
                    "cache_write_input_tokens": size,
                    "output_tokens": 10,
                }));
            }
            let mut summary = Request {
                id: "summary".into(),
                time: -1000,
                end_time: -1000,
                ..s.requests[0].clone()
            };
            summary.parts = vec![BillingPart {
                kind: PartKind::Compaction,
                usage: codex_usage(json!({
                    "input_tokens": 300000,
                    "cached_input_tokens": 0,
                    "cache_write_input_tokens": 0,
                    "output_tokens": 2000,
                })),
                ..s.requests[0].parts[0].clone()
            }];
            s.requests.insert(0, summary);
            let e = &mut s.events[0];
            e.harness = Harness::Codex;
            e.request = Some("summary".into());
            e.post_tokens = None;
            e.model = Some("gpt-6.1-sol".into());
            let first_check = check(&s);
            assert_eq!(first_check.base.action, Action::Change);
            assert_eq!(first_check.base.recommended, Some(20000));
            near(first_check.observed_summary_cost.total_usd, 1.23);
            let first = &window(&first_check, 20000).added[0];
            near(first.summary_cost.total_usd, 1.23);
            near(first.prefix_cost.total_usd, 0.05);
            near(window(&first_check, 20000).savings_usd, 4.5702);
            let mut unpriced = s.clone();
            let unsupported = &mut unpriced.profiles[0];
            unsupported.settings.insert(
                "model_auto_compact_token_limit_scope".into(),
                Scalar::Text("body_after_prefix".into()),
            );
            user["model_auto_compact_token_limit_scope"] = json!("body_after_prefix");
            unsupported.compaction = controls(Harness::Codex, user, &Env::default(), "config.toml");
            let u = check(&unpriced);
            assert_eq!(u.base.action, Action::Advisory);
            assert_eq!(u.scenarios[0].savings_usd, None);
        }

        // Ported from compaction.test.ts: "bracketed Claude model controls scan safely and unresolved aliases stay advisory".
        #[test]
        fn bracketed_claude_model_controls_scan_safely_and_unresolved_aliases_stay_advisory() {
            let w = Workspace::new();
            let user = json!({
                "autoCompactWindow": 600000,
                "modelSettings": {
                    "opus[1m]": { "autoCompactWindow": 400000 },
                    "claude-opus-4-6[1m]": { "autoCompactWindow": 300000 },
                },
            });
            std::fs::write(w.claude.join("settings.json"), user.to_string()).unwrap();
            let result = scan(&scan_options(&w)).unwrap();
            let mut p = result
                .profiles
                .iter()
                .find(|p| p.harness == Harness::Claude)
                .unwrap()
                .clone();
            assert_eq!(
                window_control(&p.compaction, Some("opus[1m]")).value,
                Some(400000)
            );
            let aliased = window_control(&p.compaction, Some("claude-opus-4-6"));
            assert_eq!(aliased.value, None);
            assert!(!aliased.applicable);
            assert!(aliased.reason.contains("alias"));
            let (user, managed) = (object(user), object(json!({ "autoCompactWindow": 500000 })));
            p.compaction = compaction_controls(&CompactionSources {
                harness: Harness::Claude,
                user: &user,
                managed: &managed,
                env: &Env::default(),
                file: "user.json",
                managed_file: "managed.json",
                scopes: None,
            })
            .unwrap();
            let current = window_control(&p.compaction, Some("claude-opus-4-6"));
            assert_eq!(current.value, Some(500000));
            assert_eq!(current.owner, "managed.json:autoCompactWindow");
            std::fs::write(
                w.claude.join("settings.json"),
                json!({ "modelSettings": { "opus[1m]": null } }).to_string(),
            )
            .unwrap();
            let error = scan(&scan_options(&w)).unwrap_err();
            assert!(
                error.message.contains("invalid per-model settings"),
                "{}",
                error.message
            );
        }

        // Ported from compaction.test.ts: "discovery retains only whitelisted compaction controls and malformed selected limits fail".
        #[test]
        fn discovery_retains_only_whitelisted_compaction_controls_and_malformed_limits_fail() {
            let w = Workspace::new();
            std::fs::write(
                w.claude.join("settings.json"),
                json!({
                    "modelSettings": {
                        "claude-opus-5-5": { "autoCompactWindow": 400000, "privateText": "PRIVATE" },
                    },
                })
                .to_string(),
            )
            .unwrap();
            std::fs::write(
                w.codex.join("config.toml"),
                "model_auto_compact_token_limit = 240000\nmodel_auto_compact_token_limit_scope = 'body_after_prefix'\n",
            )
            .unwrap();
            let result = scan(&scan_options(&w)).unwrap();
            let claude = result
                .profiles
                .iter()
                .find(|p| p.harness == Harness::Claude)
                .unwrap();
            assert_eq!(
                window_control(&claude.compaction, Some("claude-opus-5-5")).value,
                Some(400000)
            );
            let codex = result
                .profiles
                .iter()
                .find(|p| p.harness == Harness::Codex)
                .unwrap();
            assert_eq!(codex.compaction.default.value, Some(240000));
            assert!(!codex.compaction.default.applicable);
            assert!(!serde_json::to_string(&result).unwrap().contains("PRIVATE"));
            std::fs::write(
                w.codex.join("config.toml"),
                "model_auto_compact_token_limit = 'bad'\n",
            )
            .unwrap();
            let error = scan(&scan_options(&w)).unwrap_err();
            assert!(
                error.message.contains("invalid compaction token limit"),
                "{}",
                error.message
            );
        }

        // Ported from compaction.test.ts: "window replay uses TTL recommendations and never inherits old-prefix external warming".
        #[test]
        fn window_replay_uses_ttl_recommendations_and_never_inherits_external_warming() {
            let mut s = data(2);
            let time = s.requests[1].time + 600000;
            let next = &mut s.requests[2];
            next.time = time;
            next.end_time = time;
            next.parts[0].usage = writes(150000);
            let next = next.clone();
            let warm_check = check(&s);
            assert_eq!(warm_check.ttls.main, Some(Ttl::OneHour));
            near(warm_check.scenarios[0].added[0].prefix_cost.total_usd, 0.16);
            let warm = warm_check.scenarios[0].cost.total_usd.unwrap();
            let mut external = Request {
                id: "externally-warmed".into(),
                time: next.time + HOUR,
                end_time: next.time + HOUR,
                ..next.clone()
            };
            external.parts = vec![BillingPart {
                usage: usage(0, 150000, 10, Ttl::FiveMinutes, 0),
                ..next.parts[0].clone()
            }];
            s.requests.push(external);
            let cold = check(&s);
            assert_eq!(cold.ttls.main, Some(Ttl::OneHour));
            // The first new prefix expired: its 20k rewrite costs $0.16, not an inherited old read.
            near(
                Some(cold.scenarios[0].cost.total_usd.unwrap() - warm),
                0.1602,
            );
            assert_eq!(s.requests[2].parts[0].usage.read, Some(0));
        }

        // Ported from compaction.test.ts: "changing a message prefix preserves unchanged billed compaction iterations".
        #[test]
        fn changing_a_message_prefix_preserves_unchanged_billed_compaction_iterations() {
            let mut s = data(2);
            let time = s.requests[1].time + 600000;
            s.requests[2].time = time;
            s.requests[2].end_time = time;
            for r in &mut s.requests[1..3] {
                r.parts.push(BillingPart {
                    kind: PartKind::Compaction,
                    usage: usage(1000, 150000, 100, Ttl::OneHour, 0),
                    ..r.parts[0].clone()
                });
            }
            let before = s.requests.clone();
            let check = check(&s);
            assert_eq!(check.ttls.main, Some(Ttl::OneHour));
            assert_eq!(check.ttls.subagent, Some(Ttl::OneHour));
            near(check.base.savings_usd, 0.426);
            assert_eq!(s.requests, before);
        }

        // Ported from compaction.test.ts: "window estimates reuse bracketed attempt inputs and retain missing historical billing".
        #[test]
        fn window_estimates_reuse_bracketed_attempt_inputs_and_retain_missing_billing() {
            let mut s = data(2);
            let before = s.requests[1].clone();
            let after = &mut s.requests[2];
            after.time = before.time + 2000;
            after.end_time = after.time;
            after.parts[0].usage = usage(0, 150000, 10, Ttl::FiveMinutes, 0);
            let after = after.id.clone();
            let mut attempt = Request {
                id: "attempt".into(),
                time: before.time + 1000,
                end_time: before.time + 1000,
                evidence: RequestEvidence::Unknown,
                reference: Some("attempt-ref".into()),
                attempt: Some(Attempt {
                    max_output_tokens: Some(100),
                }),
                ..make_request()
            };
            attempt.parts[0].usage = crate::reader::usage(&json!({}), Harness::Claude).unwrap();
            s.requests.insert(2, attempt);
            let check = check(&s);
            assert_eq!(
                check.usage_estimates[0].evidence,
                EstimateEvidence::Estimated
            );
            assert_eq!(check.usage_estimates[0].basis, [before.id, after]);
            assert_eq!(check.baseline_cost.total_usd, None);
            assert_eq!(check.scenarios[0].cost.total_usd, None);
            near(check.base.savings_usd, 0.062);
            assert_eq!(s.requests[2].parts[0].usage.input, None);
        }

        // Ported from compaction.test.ts: "pre-window summary linkage calibrates selected work without charging old generation".
        #[test]
        fn pre_window_summary_linkage_calibrates_selected_work_without_charging_old_generation() {
            let mut s = data(4);
            let mut summary = at("old-summary", "sample", -1000);
            summary.parts[0].kind = PartKind::Compaction;
            summary.parts[0].usage = usage(0, 0, 2000, Ttl::FiveMinutes, 300000);
            s.requests[0].parts.truncate(1);
            let mut event = s.events.pop().unwrap();
            event.request = Some(summary.id.clone());
            event.pre_tokens = None;
            event.post_tokens = None;
            s.cache_context = CacheContext {
                requests: vec![summary],
                boundaries: vec![event],
                prompts: vec![],
                ..CacheContext::default()
            };
            s.since = Some(1000);
            let report = report(&s);
            let check = compaction(&report);
            assert_eq!(check.base.since, Some(1000));
            assert_eq!(check.base.requests, 5);
            near(check.observed_summary_cost.total_usd, 0.0);
            assert_eq!(report.compactions.len(), 0);
            near(check.scenarios[0].added[0].summary_cost.total_usd, 0.64);
            near(check.base.savings_usd, 1.96);
        }

        // Ported from compaction.test.ts: "an unsupported smaller window does not veto another candidate's positive aggregate".
        #[test]
        fn an_unsupported_smaller_window_does_not_veto_another_candidates_positive_aggregate() {
            let mut s = data(4);
            s.requests[0].parts[0].usage = writes(150000);
            for r in &mut s.requests[1..] {
                r.parts[0].usage = writes(300000);
            }
            let check = check(&s);
            assert_eq!(check.scenarios[0].savings_usd, None);
            assert_eq!(check.base.recommended, Some(150000));
            assert_eq!(check.base.action, Action::Change);
            assert!(check.base.savings_usd.unwrap() > 0.0);
        }

        // Ported from compaction.test.ts: "shared Claude controls produce one aggregate recommendation for all affected models".
        #[test]
        fn shared_claude_controls_produce_one_aggregate_recommendation_for_all_models() {
            for environment in [false, true] {
                let mut s = data(4);
                let model = "claude-haiku-5-5";
                let mut other = s.requests[0].clone();
                other.id = "haiku-resume".into();
                other.session = "haiku-sample".into();
                other.model = Some(model.into());
                for p in &mut other.parts {
                    p.model = Some(model.into());
                }
                let mut event = s.events[0].clone();
                event.id = "haiku-boundary".into();
                event.session = other.session.clone();
                event.origins[0].session = other.session.clone();
                event.model = Some(model.into());
                event.request = Some(other.id.clone());
                std::sync::Arc::make_mut(event.pricing.as_mut().unwrap()).model =
                    Some(model.into());
                s.requests.push(other);
                s.events.push(event);
                for i in 0..3 {
                    let mut request = Request {
                        model: Some(model.into()),
                        ..at(&format!("haiku-{i}"), "haiku", (i + 1) * HOUR)
                    };
                    request.parts[0].model = Some(model.into());
                    request.parts[0].usage = writes(150000);
                    s.requests.push(request);
                }
                let mut env = Env::default();
                if environment {
                    env.insert("CLAUDE_CODE_AUTO_COMPACT_WINDOW".into(), "400000".into());
                }
                s.profiles[0].compaction = controls(
                    Harness::Claude,
                    json!({ "autoCompactWindow": 400000 }),
                    &env,
                    "settings.json",
                );
                let report = report(&s);
                let checks = windows(&report);
                assert_eq!(checks.len(), 1);
                let models: crate::collections::HashSet<_> =
                    checks[0].models.iter().map(|m| m.as_deref()).collect();
                assert_eq!(
                    models,
                    [Some("claude-opus-5-5"), Some(model)].into_iter().collect()
                );
                assert_eq!(
                    checks[0].base.owner,
                    if environment {
                        "CLAUDE_CODE_AUTO_COMPACT_WINDOW"
                    } else {
                        "settings.json:autoCompactWindow"
                    }
                );
                assert_eq!(checks[0].base.recommended, Some(100000));
                assert_eq!(checks[0].base.action, Action::Change);
            }
        }

        // Ported from compaction.test.ts: "small unknown iterations cancel while truly missing input leaves only its own session unpriced".
        #[test]
        fn small_unknown_iterations_cancel_while_missing_input_leaves_only_its_session_unpriced() {
            let mut s = data(4);
            let mut small = at("small-unknown", "separate-small", 1000);
            small.parts[0].kind = PartKind::Unknown;
            small.parts[0].usage = writes(5000);
            s.requests.push(small);
            let first = check(&s);
            assert_eq!(first.baseline_cost.total_usd, None);
            assert_eq!(first.base.action, Action::Change);
            near(first.base.savings_usd, 1.96);
            s.requests.last_mut().unwrap().parts[0].usage.input = None;
            let scenario = check(&s).scenarios[0].clone();
            near(scenario.savings_usd, 1.96);
            assert_eq!(scenario.coverage.total - scenario.coverage.priced, 1);
        }

        // Ported from compaction.test.ts: "each added summary is its own pair, so a few unpriced sessions cannot outweigh many small differences".
        #[test]
        fn each_added_summary_is_its_own_pair_so_few_unpriced_sessions_cannot_outweigh_many() {
            let mut s = data(4);
            let large: Vec<Request> = s
                .requests
                .iter()
                .filter(|r| r.id.starts_with("large-"))
                .cloned()
                .collect();
            for k in 1..10 {
                for r in &large {
                    s.requests.push(Request {
                        id: format!("{}-{k}", r.id).into(),
                        session: format!("copy-{k}").into(),
                        ..r.clone()
                    });
                }
            }
            for k in 0..4 {
                let mut missing = at(&format!("missing-{k}"), &format!("missing-{k}"), 1000);
                missing.parts[0].kind = PartKind::Unknown;
                missing.parts[0].usage = writes(5000);
                missing.parts[0].usage.input = None;
                s.requests.push(missing);
            }
            // Stable, like Array.prototype.sort.
            s.requests.sort_by_key(|r| r.time);
            let check = check(&s);
            let scenario = window(&check, 100000);
            assert_eq!(scenario.added.len(), 10);
            assert_eq!(scenario.coverage.total - scenario.coverage.priced, 4);
            near(scenario.savings_usd, 19.6);
            assert_eq!(check.base.action, Action::Change);
        }

        // Ported from compaction.test.ts: "a window that changes no request is a keep, not missing evidence".
        #[test]
        fn a_window_that_changes_no_request_is_a_keep_not_missing_evidence() {
            let mut s = data(0);
            let mut small = at("small", "session", 1000);
            small.parts[0].usage = writes(20000);
            s.requests = vec![small];
            s.events = vec![];
            let report = report(&s);
            let check = compaction(&report);
            assert_eq!(check.base.action, Action::Keep);
            assert!(check.scenarios.iter().all(|c| c.coverage.total == 0));
            let text = text(&report, Some(200), false);
            let keep = text.find("Keep").expect("a Keep section");
            assert!(
                text[keep..].contains("Auto-compact window (claude-opus-5-5): 400,000 tokens"),
                "{text}"
            );
            assert!(!text.contains("could not confirm"), "{text}");
        }

        // Ported from compaction.test.ts: "unreadable lines count as unknown parts, so many of them block a recommendation".
        #[test]
        fn unreadable_lines_count_as_unknown_parts_so_many_block_a_recommendation() {
            let mut s = data(4);
            let snapshot = |skipped_lines| Snapshot {
                file: "synthetic".into(),
                harness: Harness::Claude,
                profiles: vec!["profile".into()],
                bytes: 0,
                device: 0,
                inode: 0,
                deferred_bytes: 0,
                skipped_lines,
                lines: 0,
            };
            s.snapshots = vec![snapshot(1)];
            assert_eq!(check(&s).base.action, Action::Change);
            s.snapshots = vec![snapshot(10000)];
            let blocked = check(&s);
            assert_eq!(blocked.base.recommended, None);
            assert_eq!(blocked.base.action, Action::Advisory);
        }

        // Ported from compaction.test.ts: "Claude historical window differences and structured TTL window alignment remain visible".
        #[test]
        fn claude_historical_window_differences_remain_visible() {
            let mut s = data(4);
            let mut e = s.events[0].clone();
            e.id = "settings".into();
            e.kind = EventKind::Settings;
            e.settings = [("autoCompactWindow".into(), Scalar::Number(1000000.0))]
                .into_iter()
                .collect();
            s.events.push(e);
            let check = check(&s);
            assert!(
                check
                    .base
                    .uncertainty
                    .join(" ")
                    .contains("historical recorded limits differ")
            );
            assert_eq!(check.base.action, Action::Change);
        }

        // Ported from compaction.test.ts: "added summaries require billed output and do not confuse postTokens with output".
        #[test]
        fn added_summaries_require_billed_output_and_do_not_confuse_post_tokens_with_output() {
            let mut s = data(4);
            s.requests[0].parts.truncate(1);
            s.events[0].request = None;
            s.events[0].post_tokens = Some(60000);
            let report = report(&s);
            assert_eq!(report.compactions[0].evidence, Evidence::Estimated);
            assert_eq!(compaction(&report).scenarios[0].savings_usd, None);
        }
    }
}
