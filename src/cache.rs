//! Explicit cache-state replay for Claude TTL counterfactuals.

use std::collections::HashMap;

use indexmap::{Equivalent, IndexMap};

use crate::model::{
    Agent, BillingPart, CachePredecessor, CacheReplayEvidence, EventKind, Harness, KnownState,
    Millis, PartKind, PrefixEvidence, ReplayOutcome, Request, StructuralEvent, Ttl, Usage,
};

const MINUTE: Millis = 60_000;

/// Documented Claude lifetimes shared by every cache counterfactual.
pub fn ttl_milliseconds(ttl: Ttl) -> Millis {
    match ttl {
        Ttl::FiveMinutes => 5 * MINUTE,
        Ttl::OneHour => 60 * MINUTE,
    }
}

/// Default billed-write retention used for conditional new Codex prefixes.
pub const CODEX_CACHE_MILLISECONDS: Millis = 30 * MINUTE;

/// Model, provider, speed, kind and agent; None marks an unrecorded dimension.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Cohort {
    model: Option<String>,
    provider: Option<String>,
    speed: Option<String>,
    kind: Option<PartKind>,
    agent: Option<Agent>,
}

/// A borrowed cohort for lookups; its derived hash matches `Cohort`'s field for field.
#[derive(Clone, Copy, Debug, Hash)]
struct CohortRef<'a> {
    model: Option<&'a str>,
    provider: Option<&'a str>,
    speed: Option<&'a str>,
    kind: Option<PartKind>,
    agent: Option<Agent>,
}

impl Equivalent<Cohort> for CohortRef<'_> {
    fn equivalent(&self, key: &Cohort) -> bool {
        self.model == key.model.as_deref()
            && self.provider == key.provider.as_deref()
            && self.speed == key.speed.as_deref()
            && self.kind == key.kind
            && self.agent == key.agent
    }
}

impl CohortRef<'_> {
    fn complete(&self) -> bool {
        self.model.is_some()
            && self.provider.is_some()
            && self.speed.is_some()
            && self.kind.is_some()
            && self.agent.is_some()
    }

    fn compatible(&self, other: &Cohort) -> bool {
        fn agree<T: PartialEq>(a: Option<T>, b: Option<T>) -> bool {
            a.is_none() || b.is_none() || a == b
        }
        agree(self.model, other.model.as_deref())
            && agree(self.provider, other.provider.as_deref())
            && agree(self.speed, other.speed.as_deref())
            && agree(self.kind, other.kind)
            && agree(self.agent, other.agent)
    }

    fn owned(self) -> Cohort {
        Cohort {
            model: self.model.map(str::to_string),
            provider: self.provider.map(str::to_string),
            speed: self.speed.map(str::to_string),
            kind: self.kind,
            agent: self.agent,
        }
    }
}

/// One possible cache prefix; known and estimated states always have a prefix.
#[derive(Clone, Debug)]
struct State {
    kind: KnownState,
    prefix: Option<i64>,
    origin: CachePredecessor,
    order: i64,
}

/// The smallest and largest prefix in a span, as indices into `History::states`.
#[derive(Clone, Copy, Debug)]
struct Range {
    min: usize,
    max: usize,
}

fn low(state: &State) -> f64 {
    state.prefix.map_or(f64::NEG_INFINITY, |p| p as f64)
}

fn high(state: &State) -> f64 {
    state.prefix.map_or(f64::INFINITY, |p| p as f64)
}

fn merge(states: &[State], a: Option<Range>, b: Option<Range>) -> Option<Range> {
    let (Some(a), Some(b)) = (a, b) else {
        return a.or(b);
    };
    Some(Range {
        min: if low(&states[a.min]) <= low(&states[b.min]) {
            a.min
        } else {
            b.min
        },
        max: if high(&states[a.max]) >= high(&states[b.max]) {
            a.max
        } else {
            b.max
        },
    })
}

/// The states since one cohort's latest anchor.
///
/// Prefix conversion is monotone, so range extrema suffice to prove invariance. The growing
/// segment tree keeps missing-cohort histories from requiring quadratic scans.
struct History {
    cohort: Cohort,
    states: Vec<State>,
    /// Whether `states[0]` is a known or estimated anchor.
    anchored: bool,
    capacity: usize,
    tree: Vec<Option<Range>>,
}

impl History {
    fn new(cohort: Cohort) -> Self {
        Self {
            cohort,
            states: Vec::new(),
            anchored: false,
            capacity: 1,
            tree: vec![None, None],
        }
    }

    fn anchor(&self) -> Option<&State> {
        self.anchored.then(|| &self.states[0])
    }

    fn append(&mut self, state: State) {
        if state.kind != KnownState::Unknown {
            self.states.clear();
            self.capacity = 1;
            self.tree = vec![None, None];
            self.anchored = true;
        }
        self.states.push(state);
        let len = self.states.len();
        if len > self.capacity {
            self.capacity *= 2;
            self.tree = vec![None; self.capacity * 2];
            for i in 0..len {
                self.tree[self.capacity + i] = Some(Range { min: i, max: i });
            }
            for i in (1..self.capacity).rev() {
                self.tree[i] = merge(&self.states, self.tree[i * 2], self.tree[i * 2 + 1]);
            }
        } else {
            let mut index = self.capacity + len - 1;
            self.tree[index] = Some(Range {
                min: len - 1,
                max: len - 1,
            });
            loop {
                index /= 2;
                if index == 0 {
                    break;
                }
                self.tree[index] =
                    merge(&self.states, self.tree[index * 2], self.tree[index * 2 + 1]);
            }
        }
    }

    /// The first index whose key exceeds `value`, by the same bisection as the reference
    /// implementation so out-of-order histories pick the same witnesses.
    fn after(&self, value: i64, key: impl Fn(&State) -> i64) -> usize {
        let (mut low, mut high) = (0, self.states.len());
        while low < high {
            let middle = (low + high) / 2;
            if key(&self.states[middle]) <= value {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low
    }

    /// Witnesses after `order`: the latest expired and recent states and the prefix extrema
    /// between the two lifetimes.
    fn candidates(&self, order: i64, time: Millis) -> Vec<&State> {
        let start = self.after(order, |s| s.order);
        let expired =
            start.max(self.after(time - ttl_milliseconds(Ttl::OneHour), |s| s.origin.time));
        let recent =
            start.max(self.after(time - ttl_milliseconds(Ttl::FiveMinutes), |s| s.origin.time));
        let mut result = Vec::new();
        if expired > start {
            result.push(&self.states[expired - 1]);
        }
        if recent < self.states.len() {
            result.push(self.states.last().expect("nonempty"));
        }
        let mut left = expired + self.capacity;
        let mut right = recent + self.capacity;
        let mut range = None;
        while left < right {
            if left % 2 == 1 {
                range = merge(&self.states, range, self.tree[left]);
                left += 1;
            }
            if right % 2 == 1 {
                right -= 1;
                range = merge(&self.states, range, self.tree[right]);
            }
            left /= 2;
            right /= 2;
        }
        if let Some(range) = range {
            result.push(&self.states[range.min]);
            result.push(&self.states[range.max]);
        }
        result
    }
}

/// Recognize a recorded, unambiguous write lifetime without inventing a missing split.
pub fn observed_ttl(p: &BillingPart) -> Option<Ttl> {
    if p.usage.write5m.unwrap_or(0) > 0 && p.usage.write1h == Some(0) {
        return Some(Ttl::FiveMinutes);
    }
    if p.usage.write1h.unwrap_or(0) > 0 && p.usage.write5m == Some(0) {
        return Some(Ttl::OneHour);
    }
    None
}

/// The last possible cache touch before a request.
#[derive(Clone, Copy)]
struct Previous {
    time: Millis,
    prefix: i64,
}

/// Rewrite one part's cache usage under `ttl` after `previous`.
fn apply(
    usage: Usage,
    ttl: Ttl,
    previous: Option<Previous>,
    time: Millis,
    cold: bool,
    observed: Option<Ttl>,
) -> Usage {
    let mut u = usage;
    let (Some(mut read), Some(mut write)) = (u.read, u.write) else {
        return u;
    };
    let five = ttl_milliseconds(Ttl::FiveMinutes);
    let hour = ttl_milliseconds(Ttl::OneHour);
    if let Some(previous) = previous {
        let gap = time - previous.time;
        if gap >= five && gap < hour {
            if ttl == Ttl::FiveMinutes && observed != Some(Ttl::FiveMinutes) {
                write += read;
                read = 0;
            }
            if ttl == Ttl::OneHour && observed != Some(Ttl::OneHour) {
                let reusable = write.min(0.max(previous.prefix - read));
                write -= reusable;
                read += reusable;
            }
            if ttl == Ttl::OneHour && cold {
                write += read;
                read = 0;
            }
        }
    }
    u.read = Some(read);
    u.write = Some(write);
    u.write5m = Some(if ttl == Ttl::FiveMinutes { write } else { 0 });
    u.write1h = Some(if ttl == Ttl::OneHour { write } else { 0 });
    u
}

/// One part's cache touch to replay.
#[derive(Clone, Copy, Debug)]
pub struct Touch<'a> {
    pub request: &'a Request,
    pub p: &'a BillingPart,
    /// The part's index in its request.
    pub part: usize,
    /// Whether the part's geometry is estimated rather than recorded.
    pub estimated: bool,
    pub ttl: Ttl,
    /// Whether a surviving 1h prefix is assumed cold anyway.
    pub cold: bool,
    /// Whether the part is in the selected window; unselected touches only inform later state.
    pub selected: bool,
}

/// A selected part's replayed geometry and the evidence behind it.
#[derive(Clone, Debug, PartialEq)]
pub struct Replayed {
    pub p: BillingPart,
    /// Whether the possible predecessors imply different cache reads/writes.
    pub unknown: bool,
    pub evidence: Option<CacheReplayEvidence>,
}

struct Session {
    histories: IndexMap<Cohort, History>,
    /// The next unpassed boundary in each layer.
    cursors: Vec<usize>,
}

/// Claude compaction boundary times in each origin session, sorted; build once per event set
/// and share across every replay of it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CacheBoundaries {
    times: HashMap<String, Vec<Millis>>,
}

impl CacheBoundaries {
    /// Index every Claude compaction boundary under each of its origin sessions.
    pub fn new<'a>(events: impl IntoIterator<Item = &'a StructuralEvent>) -> Self {
        let mut times: HashMap<String, Vec<Millis>> = HashMap::new();
        for event in events {
            if event.harness != Harness::Claude || event.kind != EventKind::Compaction {
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

    fn get(&self, session: &str) -> &[Millis] {
        self.times.get(session).map_or(&[][..], Vec::as_slice)
    }
}

/// Replay explicit cache states, preserving unknown histories across cohorts and windows.
pub struct CacheChain<'b> {
    sessions: HashMap<String, Session>,
    /// Boundary layers. Histories reset when a boundary in any layer passes, so a prebuilt
    /// index plus a few extra events replays exactly like one index of their concatenation.
    boundaries: Vec<&'b CacheBoundaries>,
    order: i64,
}

impl<'b> CacheChain<'b> {
    /// A chain whose histories reset at every boundary in any of `boundaries`.
    pub fn new(boundaries: impl IntoIterator<Item = &'b CacheBoundaries>) -> Self {
        Self {
            sessions: HashMap::new(),
            boundaries: boundaries.into_iter().collect(),
            order: 0,
        }
    }

    /// Record one touch; selected touches also return their replayed geometry.
    pub fn replay(&mut self, touch: &Touch) -> Option<Replayed> {
        let r = touch.request;
        let p = touch.p;
        let ttl = touch.ttl;
        if !self.sessions.contains_key(&r.session) {
            let session = Session {
                histories: IndexMap::new(),
                cursors: vec![0; self.boundaries.len()],
            };
            self.sessions.insert(r.session.clone(), session);
        }
        let session = self.sessions.get_mut(&r.session).expect("inserted");
        for (layer, cursor) in self.boundaries.iter().zip(&mut session.cursors) {
            let times = layer.get(&r.session);
            while *cursor < times.len() && times[*cursor] <= r.time {
                session.histories.clear();
                *cursor += 1;
            }
        }
        let histories = &mut session.histories;
        let cohort = CohortRef {
            model: p.model.as_deref(),
            provider: r.provider.as_deref(),
            speed: p.speed.as_deref(),
            kind: (p.kind != PartKind::Unknown).then_some(p.kind),
            agent: (r.agent != Agent::Unknown).then_some(r.agent),
        };
        let complete = cohort.complete();
        let mut result = None;
        if touch.selected {
            let anchor = if complete {
                histories.get(&cohort).and_then(History::anchor)
            } else {
                None
            };
            let mut candidates: IndexMap<i64, &State> = IndexMap::new();
            if let Some(anchor) = anchor {
                candidates.insert(anchor.order, anchor);
            }
            for history in histories.values() {
                if !cohort.compatible(&history.cohort) {
                    continue;
                }
                // A fully specified cohort supersedes its older alternatives. An omitted
                // dimension cannot supersede another cohort's evidence.
                let floor = anchor.or(history.anchor());
                if anchor.is_none()
                    && let Some(own) = history.anchor()
                {
                    candidates.insert(own.order, own);
                }
                for state in history.candidates(floor.map_or(-1, |s| s.order), r.time) {
                    candidates.insert(state.order, state);
                }
            }
            let states: Vec<&State> = candidates.into_values().collect();
            // A copied bracket split cannot prove an estimated attempt was externally warmed.
            let observed = if touch.estimated {
                None
            } else {
                observed_ttl(p)
            };
            let replay = |previous| apply(p.usage, ttl, previous, r.time, touch.cold, observed);
            let mut versions = Vec::new();
            if anchor.is_none() {
                versions.push(replay(None));
            }
            for state in &states {
                let time = state.origin.time;
                match state.prefix {
                    Some(prefix) => versions.push(replay(Some(Previous { time, prefix }))),
                    None => {
                        let whole = p.usage.read.unwrap_or(0) + p.usage.write.unwrap_or(0);
                        for prefix in [0, whole] {
                            versions.push(replay(Some(Previous { time, prefix })));
                        }
                    }
                }
            }
            let first = versions[0];
            let missing = p.usage.read.is_none() || p.usage.write.is_none();
            let unknown = missing
                || versions
                    .iter()
                    .any(|v| v.read != first.read || v.write != first.write);
            let state = match (anchor, states.as_slice()) {
                (Some(_), [only]) if only.kind != KnownState::Unknown => only.kind,
                _ => KnownState::Unknown,
            };
            let reason = if missing {
                "request cache usage is not recorded"
            } else if unknown {
                "cache predecessor evidence cannot establish TTL-dependent reads/writes"
            } else {
                match state {
                    KnownState::Unknown => {
                        "cache geometry is invariant across the possible recorded predecessor histories"
                    }
                    KnownState::Estimated => {
                        "cache geometry uses an estimated predecessor prefix under the fixed-behavior replay"
                    }
                    KnownState::Known => {
                        "cache geometry uses the recorded predecessor prefix under the fixed-behavior replay"
                    }
                }
            };
            let evidence = if state == KnownState::Known && !unknown {
                None
            } else if !states.is_empty() || unknown {
                Some(CacheReplayEvidence {
                    request: r.id.clone(),
                    part: touch.part,
                    ttl,
                    state,
                    predecessors: states.iter().map(|s| s.origin.clone()).collect(),
                    outcome: if unknown {
                        ReplayOutcome::Unknown
                    } else if state == KnownState::Estimated {
                        ReplayOutcome::Estimated
                    } else {
                        ReplayOutcome::Invariant
                    },
                    reason: reason.to_string(),
                })
            } else {
                None
            };
            result = Some(Replayed {
                p: BillingPart {
                    usage: if unknown { replay(None) } else { first },
                    ..p.clone()
                },
                unknown,
                evidence,
            });
        }
        let prefix = match (p.usage.read, p.usage.write) {
            (Some(read), Some(write)) => Some(read + write),
            _ => None,
        };
        // A recorded zero-cache request cannot renew or erase an older cached prefix.
        if prefix != Some(0) {
            let kind = match prefix {
                Some(_) if complete && touch.estimated => KnownState::Estimated,
                Some(_) if complete => KnownState::Known,
                _ => KnownState::Unknown,
            };
            let prefix_evidence = match prefix {
                None => PrefixEvidence::Unknown,
                Some(_) if touch.estimated => PrefixEvidence::Estimated,
                Some(_) => PrefixEvidence::Measured,
            };
            let mut reason = match prefix_evidence {
                PrefixEvidence::Unknown => "request cache usage/execution is not established",
                PrefixEvidence::Estimated => {
                    "cache prefix estimated from measured request brackets"
                }
                PrefixEvidence::Measured => "recorded cache prefix",
            }
            .to_string();
            if !complete {
                reason.push_str("; cache cohort has unrecorded model/provider/speed/kind/agent");
            }
            let state = State {
                kind,
                prefix,
                origin: CachePredecessor {
                    request: r.id.clone(),
                    part: touch.part,
                    time: r.time,
                    state: kind,
                    prefix_evidence,
                    cohort_known: complete,
                    reason,
                },
                order: self.order,
            };
            self.order += 1;
            match histories.get_mut(&cohort) {
                Some(history) => history.append(state),
                None => {
                    let mut history = History::new(cohort.owned());
                    history.append(state);
                    histories.insert(cohort.owned(), history);
                }
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{IdleAttribution, Location, Origin};
    use crate::test_support::{ClaudeUsage, claude_usage, make_request};
    use std::collections::HashSet;

    fn feed(
        chain: &mut CacheChain,
        r: &Request,
        ttl: Ttl,
        selected: bool,
        cold: bool,
    ) -> Option<Replayed> {
        chain.replay(&Touch {
            request: r,
            p: &r.parts[0],
            part: 0,
            estimated: false,
            ttl,
            cold,
            selected,
        })
    }

    fn at(id: &str, time: Millis, p: BillingPart) -> Request {
        Request {
            id: id.into(),
            time,
            end_time: time,
            parts: vec![p],
            ..make_request()
        }
    }

    #[test]
    fn compressed_ambiguous_histories_agree_with_every_possible_last_touch_at_lifetime_boundaries()
    {
        // The oracle enumerates possible last touches, including the original anchor.
        // Dense histories exercise range-tree growth, expiration and prefix extrema.
        let anchor = Request {
            id: "anchor".into(),
            time: 12 * 60000,
            ..make_request()
        };
        let attempts: Vec<Request> = (0..150)
            .map(|index| {
                at(
                    &format!("possible-{index}"),
                    index * 30000,
                    BillingPart {
                        speed: None,
                        usage: claude_usage(ClaudeUsage {
                            write: 1 + ((index * 239) % 1600),
                            ..ClaudeUsage::default()
                        }),
                        ..anchor.parts[0].clone()
                    },
                )
            })
            .collect();
        for ttl in Ttl::ALL {
            for observed in [Some(Ttl::FiveMinutes), Some(Ttl::OneHour), None] {
                for cold in [false, true] {
                    let mut usage = claude_usage(ClaudeUsage {
                        read: 300,
                        write: 700,
                        ttl: observed.unwrap_or(Ttl::FiveMinutes),
                        ..ClaudeUsage::default()
                    });
                    if observed.is_none() {
                        usage.write5m = Some(350);
                        usage.write1h = Some(350);
                    }
                    let last = at(
                        "final",
                        75 * 60000,
                        BillingPart {
                            usage,
                            ..anchor.parts[0].clone()
                        },
                    );
                    let possible = std::iter::once(&anchor)
                        .chain(attempts.iter().filter(|r| r.time >= anchor.time));
                    let geometries: HashSet<(Option<i64>, Option<i64>)> = possible
                        .map(|touch| {
                            let mut chain = CacheChain::new([]);
                            feed(&mut chain, &anchor, ttl, false, false);
                            if touch.id != anchor.id {
                                let mut standard = touch.clone();
                                standard.parts[0].speed = Some("standard".into());
                                feed(&mut chain, &standard, ttl, false, false);
                            }
                            let result = feed(&mut chain, &last, ttl, true, cold).unwrap();
                            assert!(!result.unknown);
                            (result.p.usage.read, result.p.usage.write)
                        })
                        .collect();
                    let mut chain = CacheChain::new([]);
                    let mut all: Vec<&Request> = attempts.iter().chain([&anchor]).collect();
                    all.sort_by_key(|r| r.time);
                    for request in all {
                        feed(&mut chain, request, ttl, false, false);
                    }
                    let result = feed(&mut chain, &last, ttl, true, cold).unwrap();
                    assert_eq!(
                        result.unknown,
                        geometries.len() > 1,
                        "{ttl:?}/{observed:?}/{cold}"
                    );
                    if !result.unknown {
                        assert!(geometries.contains(&(result.p.usage.read, result.p.usage.write)));
                    }
                    assert!(result.evidence.unwrap().predecessors.len() < attempts.len());
                }
            }
        }
    }

    #[test]
    fn zero_cache_requests_do_not_renew_an_older_prefix_or_clear_unknown_state() {
        let first = Request {
            id: "first".into(),
            ..make_request()
        };
        let with_usage = |id: &str, time: Millis, usage: Usage| Request {
            id: id.into(),
            time,
            parts: vec![BillingPart {
                usage,
                ..first.parts[0].clone()
            }],
            ..make_request()
        };
        let empty = with_usage(
            "empty",
            17 * 60000,
            claude_usage(ClaudeUsage {
                write: 0,
                ..ClaudeUsage::default()
            }),
        );
        let last = with_usage(
            "final",
            20 * 60000,
            claude_usage(ClaudeUsage {
                read: 1000,
                write: 0,
                ttl: Ttl::OneHour,
                ..ClaudeUsage::default()
            }),
        );
        let mut known = CacheChain::new([]);
        for r in [&first, &empty] {
            feed(&mut known, r, Ttl::FiveMinutes, false, false);
        }
        let result = feed(&mut known, &last, Ttl::FiveMinutes, true, false).unwrap();
        assert_eq!(result.p.usage.read, Some(0));
        let mut unknown = CacheChain::new([]);
        let missing = with_usage(
            "missing",
            16 * 60000,
            Usage {
                read: None,
                write: None,
                ..first.parts[0].usage
            },
        );
        for r in [&first, &missing, &empty] {
            feed(&mut unknown, r, Ttl::FiveMinutes, false, false);
        }
        let result = feed(&mut unknown, &last, Ttl::FiveMinutes, true, false).unwrap();
        assert!(result.unknown);
        assert!(
            result
                .evidence
                .unwrap()
                .predecessors
                .iter()
                .any(|p| p.request == missing.id)
        );
    }

    #[test]
    fn copied_compaction_boundaries_reset_unknown_cache_evidence_in_every_origin_session() {
        let first = make_request();
        let missing = Request {
            id: "missing".into(),
            time: 17 * 60000,
            parts: vec![BillingPart {
                speed: None,
                usage: Usage {
                    read: None,
                    write: None,
                    ..first.parts[0].usage
                },
                ..first.parts[0].clone()
            }],
            ..make_request()
        };
        let last = Request {
            id: "final".into(),
            time: 20 * 60000,
            parts: vec![BillingPart {
                usage: claude_usage(ClaudeUsage {
                    read: 1000,
                    write: 200,
                    ttl: Ttl::OneHour,
                    ..ClaudeUsage::default()
                }),
                ..first.parts[0].clone()
            }],
            ..make_request()
        };
        let location = Location {
            file: "synthetic".into(),
            line: 1,
        };
        let boundary = StructuralEvent {
            id: "copied-boundary".into(),
            origins: ["original", "copy"]
                .into_iter()
                .map(|session| Origin {
                    session: session.into(),
                    location: location.clone(),
                })
                .collect(),
            harness: Harness::Claude,
            session: "original".into(),
            time: 18 * 60000,
            kind: EventKind::Compaction,
            profiles: vec!["profile".into()],
            location,
            request: None,
            model: None,
            version: Some("2.1.314".into()),
            client: Some("sdk-ts".into()),
            cwd: None,
            trigger: None,
            precomputed: None,
            pre_tokens: None,
            post_tokens: None,
            duration_ms: None,
            tool: None,
            tool_id: None,
            settings: IndexMap::new(),
            idle_attribution: IdleAttribution::Unknown,
            shell: None,
            t3: None,
            pricing: None,
        };
        for session in ["original", "copy"] {
            let boundaries = CacheBoundaries::new([&boundary]);
            let mut chain = CacheChain::new([&boundaries]);
            for r in [&first, &missing] {
                let r = Request {
                    session: session.into(),
                    ..r.clone()
                };
                feed(&mut chain, &r, Ttl::FiveMinutes, false, false);
            }
            let r = Request {
                session: session.into(),
                ..last.clone()
            };
            let result = feed(&mut chain, &r, Ttl::FiveMinutes, true, false).unwrap();
            assert!(!result.unknown);
            assert_eq!(result.p.usage.read, Some(1000));
            assert_eq!(result.evidence, None);
        }
    }
}
