//! Keepalive check: price pinging idle sessions just under the cache lifetime, for each cap
//! on how long to keep pinging, against leaving them idle and idle compaction.

use std::collections::HashMap;

use std::borrow::Cow;

use indexmap::IndexMap;

use crate::cache::{
    CODEX_CACHE_MILLISECONDS, CacheBoundaries, CacheChain, Touch, observed_ttl, ttl_milliseconds,
};
use crate::compaction::{
    CompactionSamples, WindowEvents, WindowOptions, WindowSetting, compaction_samples,
    replay_windows,
};
use crate::idle::{Scopes, recommended_windows};
use crate::model::{
    Action, Agent, BillingPart, Bucket, CheckBase, CompactionCharge, CompactionCheck, Coverage,
    EventKind, Evidence, Harness, IdleAttribution, IdleCheck, KeepaliveCheck, KeepaliveScenario,
    Millis, PartKind, PerBucket, Profile, Request, Scan, ScenarioWindow, StructuralEvent, Ttl,
    TtlCheck, Usage, UsageEstimate,
};
use crate::prices::{
    Catalog, PRICE_SOURCES, TIER_SENSITIVITY, price_part, tier_sensitive, write_billing,
};

const MINUTE: Millis = 60_000;
const DAY: Millis = 24 * 60 * MINUTE;
/// Recurring Claude Code scheduled tasks expire seven days after creation, which bounds a cap.
const LONGEST_CAP: Millis = 7 * DAY;
const SOURCES: [&str; 3] = [
    "https://code.claude.com/docs/en/scheduled-tasks (2026-11-08)",
    "https://code.claude.com/docs/en/prompt-caching (2026-11-07)",
    "https://developers.openai.com/api/docs/guides/prompt-caching (2026-11-08)",
];

/// Inputs to the keepalive checks, which run after the TTL and idle decisions.
pub struct KeepaliveOptions<'a> {
    pub scan: &'a Scan,
    pub catalog: &'a Catalog,
    pub ttls: &'a [&'a TtlCheck],
    pub windows: &'a [&'a CompactionCheck],
    pub summaries: &'a [CompactionCharge],
    pub estimates: &'a IndexMap<String, UsageEstimate>,
    pub idle: &'a [&'a IdleCheck],
    pub since: Option<Millis>,
    pub now: Millis,
}

/// A duration as the report prints it, such as "50m" or "2h 30m".
pub fn duration(ms: Millis) -> String {
    let minutes = ms / MINUTE;
    let seconds = (ms % MINUTE) / 1000;
    let (days, hours, minutes) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    let parts: Vec<String> = [(days, "d"), (hours, "h"), (minutes, "m"), (seconds, "s")]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, unit)| format!("{n}{unit}"))
        .collect();
    if parts.is_empty() {
        "0m".into()
    } else {
        parts.join(" ")
    }
}

/// The policy a scenario names.
pub fn policy(interval: Millis, cap: Millis) -> String {
    format!("every {} for up to {}", duration(interval), duration(cap))
}

fn message(r: &Request) -> impl DoubleEndedIterator<Item = &BillingPart> {
    r.parts.iter().filter(|p| p.kind == PartKind::Message)
}

fn median(mut values: Vec<i64>) -> Option<i64> {
    values.sort_unstable();
    values.get(values.len() / 2).copied()
}

/// How a harness bills one request's cache, so pings and misses price like recorded requests.
#[derive(Clone, Copy)]
enum Cache {
    Claude(Ttl),
    /// A documented 30m minimum lifetime, refreshed by reads, with billed writes.
    Codex,
    CodexLong,
}

impl Cache {
    /// Preserve a request's measured write lifetime instead of substituting ping pricing.
    fn request(self, p: &BillingPart) -> Option<Self> {
        match self {
            Cache::Claude(_) => Some(self),
            Cache::Codex | Cache::CodexLong => {
                let (short, long) = (p.usage.write30m?, p.usage.write12h?);
                if short + long != p.usage.write? || (short > 0 && long > 0) {
                    return None;
                }
                Some(if long > 0 {
                    Cache::CodexLong
                } else {
                    Cache::Codex
                })
            }
        }
    }

    fn usage(self, read: i64, write: i64, ordinary: i64, output: i64) -> Usage {
        let (write5m, write1h, write30m, write12h) = match self {
            Cache::Claude(Ttl::FiveMinutes) => (Some(write), Some(0), None, None),
            Cache::Claude(Ttl::OneHour) => (Some(0), Some(write), None, None),
            Cache::Codex => (None, None, Some(write), Some(0)),
            Cache::CodexLong => (None, None, Some(0), Some(write)),
        };
        Usage {
            input: Some(read + write + ordinary),
            output: Some(output),
            read: Some(read),
            write: Some(write),
            write5m,
            write1h,
            write30m,
            write12h,
            reasoning: None,
        }
    }
}

struct Lifetime {
    ms: Millis,
    cache: Cache,
    reason: String,
}

/// The cache lifetime pings must beat: the recommended main TTL, else the current or observed one.
fn claude_lifetime(
    profile: &Profile,
    ttls: &[&TtlCheck],
    requests: &[&Request],
) -> Option<Lifetime> {
    let check = ttls
        .iter()
        .find(|c| c.base.profile == profile.id && c.bucket == Bucket::Main);
    let (ttl, reason) = if let Some(t) = check
        .filter(|c| c.base.action == Action::Change)
        .and_then(|c| c.base.recommended)
    {
        (t, "recommended main TTL")
    } else if let Some(t) = profile.ttl.main.value {
        (t, "current main TTL")
    } else if let Some(t) = check.and_then(|c| c.base.recommended) {
        (t, "main TTL the replay favors")
    } else {
        let t = requests
            .iter()
            .rev()
            .flat_map(|r| message(r).rev())
            .find_map(observed_ttl)?;
        (t, "main TTL of the latest recorded cache write")
    };
    Some(Lifetime {
        ms: ttl_milliseconds(ttl),
        cache: Cache::Claude(ttl),
        reason: format!("{reason}, {}", ttl.as_str()),
    })
}

/// Keepalive checks for every profile with main-conversation requests.
pub fn keepalive_checks(options: &KeepaliveOptions) -> Vec<KeepaliveCheck> {
    options
        .scan
        .profiles
        .iter()
        .filter_map(|p| check(options, p))
        .collect()
}

/// One session's complete replay at a cap. Unknown geometry excludes the session from
/// every cap, so alternatives and sensitivities always compare the same recorded work.
#[derive(Default)]
struct Run {
    conditional: bool,
    recorded: f64,
    pings: Vec<f64>,
    finals: Vec<f64>,
    reads: HashMap<String, i64>,
}

struct Session<'a> {
    requests: Vec<&'a Request>,
    events: Vec<&'a StructuralEvent>,
    windows: Vec<ScenarioWindow>,
    ttls: PerBucket<Option<Ttl>>,
}

struct Replayer<'a> {
    options: &'a KeepaliveOptions<'a>,
    samples: &'a CompactionSamples<'a>,
    events: &'a WindowEvents<'a>,
    lifetime: &'a Lifetime,
    interval: Millis,
    new: i64,
    output: i64,
    since: Millis,
}

impl Replayer<'_> {
    /// Add ping turns to the recorded geometry before running the shared window replay.
    /// Its modeled summaries then remove that context from subsequent pings and requests.
    fn expand(&self, session: &Session, cap: u64) -> Option<(Vec<Request>, HashMap<String, bool>)> {
        enum Tick<'a> {
            Request(&'a Request),
            Boundary(&'a StructuralEvent),
            Ping(&'a Request, u64, bool),
        }
        let main: Vec<_> = session
            .requests
            .iter()
            .copied()
            .filter(|r| message(r).next().is_some())
            .collect();
        let mut ticks = vec![];
        let idle_summaries: Vec<_> = session
            .events
            .iter()
            .filter(|e| e.idle_attribution == IdleAttribution::Notice && e.time >= self.since)
            .filter_map(|e| {
                self.options
                    .summaries
                    .iter()
                    .find(|s| s.event == e.id)
                    .and_then(|s| s.request.as_deref())
                    .or(e.request.as_deref())
            })
            .collect();
        for &r in &session.requests {
            let order = if message(r).next().is_some() { 2 } else { 0 };
            ticks.push((r.time, order, Tick::Request(r)));
        }
        ticks.extend(
            session
                .events
                .iter()
                .map(|&e| (e.time, 1, Tick::Boundary(e))),
        );
        for (i, &r) in main
            .iter()
            .enumerate()
            .filter(|(_, r)| r.time >= self.since)
        {
            let next = main.get(i + 1).copied();
            for n in 1..=cap {
                let time = r.end_time + n as Millis * self.interval;
                if next.is_some_and(|b| time >= b.time) {
                    break;
                }
                ticks.push((time, 3, Tick::Ping(r, n, next.is_none())));
            }
        }
        ticks.sort_by_key(|(time, order, _)| (*time, *order));
        let mut requests = vec![];
        let mut ping_ids = HashMap::new();
        let mut offset = 0;
        let mut context = None;
        for (time, _, tick) in ticks {
            match tick {
                Tick::Boundary(e) => {
                    if e.idle_attribution == IdleAttribution::Notice && e.time >= self.since {
                        // Both pinging and leaving the client idle disable idle compaction.
                        // Undo the recorded replacement before the shared window simulation.
                        let pre = e.pre_tokens?;
                        let next = main.iter().find(|r| r.time >= e.time);
                        if let Some(next) = next {
                            let p = message(next).next()?;
                            let full = p.usage.read? + p.usage.write?;
                            offset += pre - full;
                            context = Some((full, 0));
                        } else {
                            context = Some((pre, 0));
                        }
                    } else {
                        offset = 0;
                        context = e.post_tokens.map(|n| (n, 0));
                    }
                }
                Tick::Ping(r, n, final_period) => {
                    let part = message(r).next_back()?;
                    let (input, reply) = context?;
                    let prefix = input + reply + offset;
                    let id = format!("keepalive:{}:{n}", r.id);
                    requests.push(Request {
                        id: id.clone(),
                        time,
                        end_time: time,
                        parts: vec![BillingPart {
                            usage: self.lifetime.cache.usage(prefix, self.new, 0, self.output),
                            ..part.clone()
                        }],
                        ..r.clone()
                    });
                    ping_ids.insert(id, final_period);
                    offset += self.new + self.output;
                }
                Tick::Request(r) => {
                    let mut adjusted = self
                        .options
                        .estimates
                        .get(&r.id)
                        .and_then(|e| e.part.as_ref())
                        .map_or_else(
                            || r.clone(),
                            |p| Request {
                                parts: vec![p.clone()],
                                ..r.clone()
                            },
                        );
                    if idle_summaries.contains(&r.id.as_str()) {
                        adjusted.parts.retain(|p| p.kind != PartKind::Compaction);
                    }
                    if adjusted.parts.is_empty() {
                        continue;
                    }
                    for p in &mut adjusted.parts {
                        // Advisor calls have their own context; retaining them does not carry
                        // the main conversation's inserted ping turns into their billing.
                        if p.kind == PartKind::Advisor {
                            continue;
                        }
                        let input = p.usage.input;
                        if p.kind == PartKind::Message
                            && offset != 0
                            && matches!((context, input), (Some((before, _)), Some(now)) if now < before)
                        {
                            return None;
                        }
                        let cache = self.lifetime.cache.request(p);
                        if offset != 0 {
                            if !matches!(p.kind, PartKind::Message | PartKind::Compaction) {
                                return None;
                            }
                            p.usage.input = Some(input? + offset);
                            let ordinary_summary = p.kind == PartKind::Compaction
                                && p.usage.read == Some(0)
                                && p.usage.write == Some(0);
                            if !ordinary_summary {
                                let observed = observed_ttl(p);
                                p.usage.write = Some(p.usage.write? + offset);
                                match self.lifetime.cache {
                                    Cache::Codex | Cache::CodexLong => match cache? {
                                        Cache::CodexLong => {
                                            p.usage.write12h = Some(p.usage.write12h? + offset)
                                        }
                                        _ => p.usage.write30m = Some(p.usage.write30m? + offset),
                                    },
                                    Cache::Claude(_) => {
                                        let ttl = if p.kind == PartKind::Compaction {
                                            session.ttls.subagent.or(observed)?
                                        } else {
                                            session.ttls.main?
                                        };
                                        let write = p.usage.write?;
                                        p.usage.write5m =
                                            Some(if ttl == Ttl::FiveMinutes { write } else { 0 });
                                        p.usage.write1h =
                                            Some(if ttl == Ttl::OneHour { write } else { 0 });
                                    }
                                }
                            }
                        }
                        if p.kind == PartKind::Message {
                            context = input.zip(p.usage.output);
                        }
                        if p.kind == PartKind::Compaction {
                            offset = 0;
                            context = None;
                        }
                    }
                    requests.push(adjusted);
                }
            }
        }
        Some((requests, ping_ids))
    }

    fn run(&self, session: &Session, cap: u64) -> Option<Run> {
        let (requests, ping_ids) = self.expand(session, cap)?;
        let active_events: Vec<_> = session
            .events
            .iter()
            .copied()
            .filter(|e| e.idle_attribution != IdleAttribution::Notice || e.time < self.since)
            .collect();
        let settings: Vec<_> = session
            .windows
            .iter()
            .map(|w| WindowSetting {
                value: w.value,
                models: &w.models,
            })
            .collect();
        let simulation = replay_windows(
            &requests,
            &settings,
            &WindowOptions {
                events: self.events,
                estimates: &IndexMap::new(),
                catalog: self.options.catalog,
                samples: self.samples,
                since: self.since,
                ttls: session.ttls.clone(),
            },
        );
        if !simulation.errors.is_empty() {
            return None;
        }
        let mut events: Vec<_> = active_events
            .iter()
            .copied()
            .chain(&simulation.events)
            .collect();
        events.sort_by_key(|e| e.time);
        let mut boundaries = events.into_iter().peekable();
        // Cache entries survive a temporary switch back to another pricing cohort.
        let mut cache: HashMap<_, [Option<(Millis, i64)>; 2]> = HashMap::new();
        let mut run = Run::default();
        let mut positions = HashMap::new();
        let original: HashMap<_, _> = session
            .requests
            .iter()
            .filter(|r| r.evidence == crate::model::RequestEvidence::Measured)
            .map(|r| (r.id.as_str(), *r))
            .collect();
        // Only the shared TTL replay decides whether recorded Claude reads survive
        // the chosen policy. The ping simulation separately establishes added warmth.
        let mut observed_reads = HashMap::new();
        if let Cache::Claude(ttl) = self.lifetime.cache {
            let boundaries = CacheBoundaries::new(session.events.iter().copied());
            let mut chain = CacheChain::new([&boundaries]);
            for &r in &session.requests {
                let estimate = self
                    .options
                    .estimates
                    .get(&r.id)
                    .and_then(|e| e.part.as_ref());
                let parts = estimate.map_or(r.parts.as_slice(), std::slice::from_ref);
                let mut message = 0;
                for (part, p) in parts.iter().enumerate() {
                    let replayed = chain.replay(&Touch {
                        request: r,
                        p,
                        part,
                        estimated: estimate.is_some()
                            || r.evidence != crate::model::RequestEvidence::Measured,
                        ttl,
                        cold: false,
                        selected: p.kind == PartKind::Message,
                    });
                    if p.kind == PartKind::Message {
                        observed_reads.insert(
                            (r.id.as_str(), message),
                            replayed.filter(|r| !r.unknown).and_then(|r| r.p.usage.read),
                        );
                        message += 1;
                    }
                }
            }
        }
        for r in simulation.requests.iter().map(AsRef::as_ref) {
            if boundaries.peek().is_some_and(|e| e.time <= r.time) {
                cache.clear();
                while boundaries.peek().is_some_and(|e| e.time <= r.time) {
                    boundaries.next();
                }
            }
            let mut cost = 0.0;
            let mut recorded = original
                .get(r.id.as_str())
                .into_iter()
                .flat_map(|r| message(r))
                .enumerate();
            for p in &r.parts {
                let mut billed = Cow::Borrowed(p);
                if p.kind == PartKind::Message {
                    if r.harness == Harness::Codex
                        && write_billing(p.model.as_deref()?) != Some(true)
                    {
                        return None;
                    }
                    let u = p.usage;
                    let (input, read, write, output) = (u.input?, u.read?, u.write?, u.output?);
                    let ordinary = input - read - write;
                    if ordinary < 0 || read < 0 || write < 0 {
                        return None;
                    }
                    let cacheable = read + write;
                    let cohort = (
                        r.provider.as_deref(),
                        p.model.as_deref(),
                        p.speed.as_deref(),
                        p.geo.as_deref(),
                    );
                    let billing = self.lifetime.cache.request(p)?;
                    let long = matches!(billing, Cache::CodexLong);
                    let entries = cache.entry(cohort).or_default();
                    let modeled = entries
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| !long || *i == 1)
                        .filter_map(|(_, entry)| {
                            entry
                                .filter(|(until, _)| r.time < *until)
                                .map(|(_, prefix)| prefix)
                        })
                        .max()
                        .map(|prefix| cacheable.min(prefix))
                        .unwrap_or(0);
                    let observed = recorded.next().filter(|(_, source)| *source == p);
                    // Measured reads can be externally warmed even after the local entry
                    // expires. They prove warmth only for the unchanged recorded context.
                    let measured = match observed {
                        Some((part, source)) => match billing {
                            Cache::Claude(_) => observed_reads
                                .get(&(r.id.as_str(), part))
                                .copied()
                                .flatten()?,
                            _ => source.usage.read?,
                        },
                        None => 0,
                    };
                    let reads = modeled.max(measured);
                    billed = Cow::Owned(BillingPart {
                        usage: billing.usage(reads, cacheable - reads, ordinary, output),
                        ..p.clone()
                    });
                    run.reads.insert(r.id.clone(), reads);
                    entries[usize::from(long)] = Some((
                        r.end_time
                            + if long {
                                12 * 60 * MINUTE
                            } else {
                                self.lifetime.ms
                            },
                        cacheable,
                    ));
                } else if p.kind == PartKind::Compaction {
                    cache.clear();
                    if let Cache::Claude(_) = self.lifetime.cache
                        && p.usage.write != Some(0)
                    {
                        let write = p.usage.write?;
                        let ttl = session.ttls.subagent.or_else(|| observed_ttl(p))?;
                        billed = Cow::Owned(BillingPart {
                            usage: Usage {
                                write5m: Some(if ttl == Ttl::FiveMinutes { write } else { 0 }),
                                write1h: Some(if ttl == Ttl::OneHour { write } else { 0 }),
                                ..p.usage
                            },
                            ..p.clone()
                        });
                    }
                }
                if r.time >= self.since {
                    let priced = price_part(self.options.catalog, r, &billed);
                    run.conditional |= tier_sensitive(&priced);
                    cost += priced.total_usd?;
                }
            }
            if r.time < self.since {
                continue;
            }
            match ping_ids.get(&r.id) {
                Some(true) => {
                    positions.insert(r.id.as_str(), (true, run.finals.len()));
                    run.finals.push(cost);
                }
                Some(false) => {
                    positions.insert(r.id.as_str(), (false, run.pings.len()));
                    run.pings.push(cost);
                }
                None => run.recorded += cost,
            }
        }
        for added in simulation.added {
            run.conditional |= tier_sensitive(&added.summary_cost);
            let cost = added.summary_cost.total_usd?;
            match positions.get(added.request.as_str()) {
                Some(&(true, i)) => run.finals[i] += cost,
                Some(&(false, i)) => run.pings[i] += cost,
                None => run.recorded += cost,
            }
        }
        Some(run)
    }
}

/// New prompt input from consecutive un-compacted turns, excluding the preceding reply.
fn ping_input(requests: &[&Request], scan: &Scan) -> Option<i64> {
    let mut boundaries: HashMap<&str, Vec<Millis>> = HashMap::new();
    for e in scan
        .cache_context
        .boundaries
        .iter()
        .chain(&scan.events)
        .filter(|e| e.kind == EventKind::Compaction)
    {
        for session in
            std::iter::once(e.session.as_ref()).chain(e.origins.iter().map(|o| o.session.as_ref()))
        {
            boundaries.entry(session).or_default().push(e.time);
        }
    }
    for times in boundaries.values_mut() {
        times.sort_unstable();
        times.dedup();
    }
    let mut previous: HashMap<&str, &Request> = HashMap::new();
    let mut growth = vec![];
    for &r in requests {
        let before = previous.insert(&r.session, r);
        let Some(a) = before else {
            continue;
        };
        let mut parts = message(a);
        let (Some(ap), None) = (parts.next(), parts.next()) else {
            continue;
        };
        let mut parts = message(r);
        let (Some(p), None) = (parts.next(), parts.next()) else {
            continue;
        };
        if a.provider != r.provider
            || ap.model != p.model
            || ap.speed != p.speed
            || ap.geo != p.geo
            || a.parts
                .iter()
                .chain(&r.parts)
                .any(|p| p.kind == PartKind::Compaction)
            || boundaries.get(r.session.as_str()).is_some_and(|times| {
                times
                    .get(times.partition_point(|&time| time <= a.time))
                    .is_some_and(|&time| time <= r.time)
            })
        {
            continue;
        }
        if let (Some(before), Some(after), Some(reply)) =
            (ap.usage.input, p.usage.input, ap.usage.output)
            && let Some(new) = after.checked_sub(before).and_then(|n| n.checked_sub(reply))
            && new >= 0
        {
            growth.push(new);
        }
    }
    median(growth)
}

fn check(options: &KeepaliveOptions, profile: &Profile) -> Option<KeepaliveCheck> {
    let mut requests: Vec<_> = options
        .scan
        .cache_context
        .requests
        .iter()
        .chain(&options.scan.requests)
        .filter(|r| {
            r.profiles.contains(&profile.id)
                && r.harness == profile.harness
                && ((match r.harness {
                    Harness::Claude => r.agent == Agent::Main,
                    Harness::Codex => r.agent != Agent::Subagent,
                }) || r.parts.iter().any(|p| p.kind == PartKind::Compaction))
        })
        .collect();
    requests.sort_by_key(|r| r.time);
    let main: Vec<_> = requests
        .iter()
        .copied()
        .filter(|r| message(r).next().is_some() && options.since.is_none_or(|s| r.time >= s))
        .collect();
    let first = main.first()?.time;
    let since = options.since.unwrap_or(first);
    let lifetime = match profile.harness {
        Harness::Claude => claude_lifetime(profile, options.ttls, &main),
        Harness::Codex => Some(Lifetime {
            ms: CODEX_CACHE_MILLISECONDS,
            cache: Cache::Codex,
            reason: "documented 30m minimum cache lifetime, refreshed by reads".into(),
        }),
    };
    let interval = lifetime.as_ref().map(|l| l.ms / 6 * 5);
    let new_tokens = ping_input(&requests, options.scan);
    let output_tokens = median(
        main.iter()
            .filter_map(|r| message(r).next_back()?.usage.output)
            .collect(),
    );
    let scopes = Scopes::new(profile);
    let windows = recommended_windows(&scopes, &main, options.windows);
    let mut ttls = PerBucket {
        main: profile.ttl.main.value,
        subagent: profile.ttl.subagent.value,
    };
    for c in options
        .ttls
        .iter()
        .filter(|c| c.base.profile == profile.id && c.base.action == Action::Change)
    {
        *ttls.get_mut(c.bucket) = c.base.recommended;
    }
    if let Some(Lifetime {
        cache: Cache::Claude(ttl),
        ..
    }) = &lifetime
    {
        ttls.main = Some(*ttl);
    }
    let all_events: Vec<_> = options
        .scan
        .cache_context
        .boundaries
        .iter()
        .chain(&options.scan.events)
        .filter(|e| e.kind == EventKind::Compaction && e.profiles.contains(&profile.id))
        .collect();
    let samples = compaction_samples(&requests, &all_events, options.summaries);
    let mut window_events = WindowEvents::new(all_events.iter().copied());
    window_events.suppress_idle(since);
    let mut groups: IndexMap<&str, Vec<&Request>> = IndexMap::new();
    for &r in &requests {
        groups.entry(&r.session).or_default().push(r);
    }
    let mut events: HashMap<&str, Vec<&StructuralEvent>> = HashMap::new();
    for e in options
        .scan
        .cache_context
        .boundaries
        .iter()
        .chain(&options.scan.events)
        .filter(|e| e.kind == EventKind::Compaction && e.profiles.contains(&profile.id))
    {
        for session in
            std::iter::once(e.session.as_ref()).chain(e.origins.iter().map(|o| o.session.as_ref()))
        {
            let list = events.entry(session).or_default();
            if !list.iter().any(|old| old.id == e.id) {
                list.push(e);
            }
        }
    }
    for list in events.values_mut() {
        list.sort_by_key(|e| e.time);
    }
    let mut caps = vec![1];
    let mut returns = 0;
    let mut finals = 0;
    for list in groups.values() {
        let selected: Vec<_> = list
            .iter()
            .copied()
            .filter(|r| r.time >= since && message(r).next().is_some())
            .collect();
        if selected.is_empty() {
            continue;
        }
        finals += 1;
        for pair in selected.windows(2) {
            let gap = pair[1].time - pair[0].end_time;
            if let (Some(l), Some(i)) = (&lifetime, interval) {
                if gap > i {
                    returns += 1;
                }
                if gap >= l.ms {
                    let n = ((gap - l.ms) / i + 1) as u64;
                    if n as Millis * i <= LONGEST_CAP {
                        caps.push(n);
                    }
                }
            }
        }
    }
    caps.sort_unstable();
    caps.dedup();
    let mut scenarios: Vec<_> = caps
        .iter()
        .map(|&cap| KeepaliveScenario {
            cap_ms: cap as Millis * interval.unwrap_or(0),
            pings: 0,
            final_pings: 0,
            rescued: 0,
            ping_cost_usd: 0.0,
            avoided_usd: 0.0,
            savings_usd: 0.0,
            closed_savings_usd: 0.0,
        })
        .collect();
    let mut unpriced = 0;
    let mut priced = 0;
    let mut conditional = false;
    let mut final_costs: Vec<Vec<f64>> = vec![vec![]; caps.len()];
    let mut uncertainty = vec![
        "current ping state is unknown: scheduled-task metadata does not establish a task's purpose; recorded turns remain activity, with hypothetical pings added on top".into(),
        "transcripts do not record when clients close or sleep; returning sessions stay awake and final periods ping for the full cap, including beyond the observation window".into(),
    ];
    if let (Some(l), Some(interval), Some(new), Some(output)) =
        (&lifetime, interval, new_tokens, output_tokens)
    {
        let replayer = Replayer {
            options,
            samples: &samples,
            events: &window_events,
            lifetime: l,
            interval,
            new,
            output,
            since,
        };
        for (session, list) in groups {
            if !list
                .iter()
                .any(|r| r.time >= since && message(r).next().is_some())
            {
                continue;
            }
            let main_list: Vec<_> = list
                .iter()
                .copied()
                .filter(|r| message(r).next().is_some())
                .collect();
            let periods = 1 + main_list
                .windows(2)
                .filter(|pair| pair[0].time >= since && pair[1].time - pair[0].end_time > interval)
                .count() as u64;
            let scope = scopes.scope(main_list[0]);
            let policy = scopes.ttls(scope, &ttls);
            let owned: Vec<_> = windows
                .iter()
                .filter(|w| {
                    w.models
                        .iter()
                        .any(|m| scopes.window(scope, m.as_deref()).owner == w.owner)
                })
                .cloned()
                .collect();
            let changes = matches!(l.cache, Cache::Claude(ttl) if policy.main != Some(ttl))
                || main_list.iter().any(|r| {
                    let s = scopes.scope(r);
                    scopes.ttls(s, &ttls) != policy
                        || owned.iter().any(|w| {
                            w.models
                                .iter()
                                .any(|m| scopes.window(s, m.as_deref()).owner != w.owner)
                        })
                });
            let prepared = Session {
                requests: list,
                events: events.remove(session).unwrap_or_default(),
                windows: owned,
                ttls: policy,
            };
            let runs = (!changes)
                .then(|| {
                    let baseline = replayer.run(&prepared, 0)?;
                    let candidates: Vec<_> = caps
                        .iter()
                        .map(|&cap| replayer.run(&prepared, cap))
                        .collect::<Option<_>>()?;
                    Some((baseline, candidates))
                })
                .flatten();
            let Some((baseline, runs)) = runs else {
                unpriced += periods;
                continue;
            };
            priced += periods;
            conditional |= baseline.conditional || runs.iter().any(|run| run.conditional);
            for (i, run) in runs.into_iter().enumerate() {
                let s = &mut scenarios[i];
                s.pings += run.pings.len() as u64;
                s.final_pings += run.finals.len() as u64;
                s.rescued += run
                    .reads
                    .iter()
                    .filter(|(id, read)| {
                        baseline
                            .reads
                            .get(*id)
                            .is_some_and(|b| *b == 0 && **read > 0)
                    })
                    .count() as u64;
                s.avoided_usd += baseline.recorded - run.recorded;
                s.ping_cost_usd += run.pings.iter().chain(&run.finals).sum::<f64>();
                s.closed_savings_usd +=
                    baseline.recorded - run.recorded - run.pings.iter().sum::<f64>();
                let size = run.finals.len().max(final_costs[i].len());
                final_costs[i].resize(size, 0.0);
                for (n, cost) in run.finals.into_iter().enumerate() {
                    final_costs[i][n] += cost;
                }
            }
        }
    } else {
        unpriced = returns + finals;
    }
    for s in &mut scenarios {
        s.savings_usd = s.avoided_usd - s.ping_cost_usd;
    }
    if priced == 0 {
        scenarios.clear();
    }
    let idle_checks: Vec<_> = options
        .idle
        .iter()
        .filter(|c| c.base.profile == profile.id)
        .collect();
    // A partial keepalive replay cannot be compared to an all-session idle aggregate.
    let idle_savings = if profile.harness == Harness::Claude {
        idle_checks.iter().try_fold(0.0_f64, |sum, c| {
            let scenario = &c.after_recommendations;
            if !scenario.applicable {
                return Some(sum);
            }
            Some(sum + (-scenario.delta_usd?).max(0.0))
        })
    } else {
        None
    };
    let alternative = idle_savings.unwrap_or(0.0);
    let best = scenarios
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.savings_usd.total_cmp(&b.savings_usd));
    let comparable = !conditional
        && unpriced == 0
        && (profile.harness == Harness::Codex || idle_savings.is_some());
    if conditional {
        uncertainty.push(TIER_SENSITIVITY.into());
    }
    if profile.harness == Harness::Claude && idle_savings.is_none() {
        uncertainty.push("an applicable idle-compaction alternative is unpriced, so the aggregate winner is unresolved".into());
    }
    let margin = best
        .filter(|_| comparable)
        .map(|(_, s)| s.savings_usd - alternative);
    let savings = margin.filter(|_| unpriced == 0).map(|s| s.max(0.0));
    let recommended = margin.filter(|_| unpriced == 0).map(|m| {
        if m > 0.0 {
            policy(interval.unwrap(), best.unwrap().1.cap_ms)
        } else {
            "off".into()
        }
    });
    let break_even = scenarios.iter().enumerate()
        .filter(|(_, s)| comparable && margin.is_none_or(|m| m <= 0.0) && s.closed_savings_usd > alternative && s.savings_usd <= alternative)
        .max_by(|(_, a), (_, b)| a.closed_savings_usd.total_cmp(&b.closed_savings_usd))
        .and_then(|(i, s)| {
        let spare = s.closed_savings_usd - alternative;
        if spare <= 0.0 || s.savings_usd > alternative {return None;}
        let mut total = 0.0;
        let count = final_costs[i].iter().take_while(|&&cost| {total += cost; total <= spare}).count();
        Some(format!("pinging {} wins only if sessions that never came back close within {} of their last request",
            policy(interval.unwrap(), s.cap_ms), duration((count + 1) as Millis * interval.unwrap())))
    });
    if unpriced > 0 {
        uncertainty.push(format!("{unpriced} periods have unresolved pricing or context geometry; priced-session savings cannot establish the aggregate winner"));
    }
    let elapsed = options.now - since;
    Some(KeepaliveCheck {
        base: CheckBase {
            profile: profile.id.clone(), history_profiles: main.iter().flat_map(|r| r.profiles.clone()).collect::<indexmap::IndexSet<_>>().into_iter().collect(),
            evidence: if priced > 0 {Evidence::Estimated} else {Evidence::Unknown},
            action: Action::Advisory, current: None, recommended,
            owner: match profile.harness {
                Harness::Claude => "no setting: a /loop task pings on a fixed schedule with no idle cap, and stops only when cancelled or after seven days",
                Harness::Codex => "no setting: Codex has no way to schedule pings",
            }.into(),
            since: Some(since), until: Some(options.now),
            window_basis: if options.since.is_some() {"explicit --since window"} else {"available transcript window"}.into(),
            baseline: if alternative > 0.0 {"idle compaction as recommended, with no pings"} else {"sessions left idle, with no pings"}.into(),
            requests: main.len() as u64, coverage: Some(Coverage {priced, total: priced + unpriced}),
            savings_usd: savings, monthly_savings_usd: savings.filter(|_| elapsed > 0).map(|s| s * 30.0 * DAY as f64 / elapsed as f64), break_even,
            assumptions: vec![
                lifetime.as_ref().map_or_else(|| "cache lifetime unknown".into(), |l| format!("cache lifetime: {}", l.reason)),
                "each session is replayed sequentially at each cap and with no pings, carrying cache warmth and ping context; all policies use the same recommended compaction windows and their modeled summary costs".into(),
                "unchanged recorded context retains measured cache reads, including external warmth; rewritten context uses the modeled session-local cache without assuming unrelated activity warms the changed prefix".into(),
                "Codex requests retain a measured uniform 12h write lifetime and inherited long coverage; mixed write lifetimes have unresolved prefix geometry".into(),
                "each ping uses the preceding request's pricing metadata and median new input and output; every later request uses its own provider, model, tier, speed, geography and context tier".into(),
                "compactions replace prior ping context; pings after a boundary use its recorded post-context and survive into the return; missing geometry leaves the session unpriced".into(),
                "pinging competes with idle compaction rather than adding its savings; compaction after the cap is not combined with pings".into(),
                "caps include one ping and the counts needed to rescue recorded returns, up to the seven-day scheduled-task lifetime".into(),
            ], uncertainty, sources: SOURCES.into_iter().map(String::from).chain(PRICE_SOURCES.into_iter().map(String::from)).collect(),
        }, bucket: "main", lifetime_ms: lifetime.as_ref().map(|l| l.ms), interval_ms: interval,
        ping_input_tokens: new_tokens, ping_output_tokens: output_tokens,
        returns, finals, unpriced, idle_compaction_savings_usd: idle_savings, margin_usd: margin, scenarios,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CacheContext, ExposureScan, ScanCoverage};
    use crate::test_support::{ClaudeUsage, claude_usage, make_profile, make_request, prices};

    const HOUR: Millis = 60 * MINUTE;

    fn scan(profile: Profile, requests: Vec<Request>) -> Scan {
        let cache_context = requests
            .iter()
            .find(|r| message(r).next().is_some())
            .map(|source| {
                let mut before = source.clone();
                before.id = "ping-size-before".into();
                before.session = "ping-size".into();
                before.time = -2 * MINUTE;
                before.end_time = before.time;
                let cache = match source.harness {
                    Harness::Claude => Cache::Claude(Ttl::OneHour),
                    Harness::Codex => Cache::Codex,
                };
                before.parts = vec![BillingPart {
                    usage: cache.usage(19_000, 1000, 0, 500),
                    ..source.parts[0].clone()
                }];
                let mut after = before.clone();
                after.id = "ping-size-after".into();
                after.time = -MINUTE;
                after.end_time = after.time;
                after.parts[0].usage = cache.usage(20_000, 1500, 0, 500);
                CacheContext {
                    requests: vec![before, after],
                    ..CacheContext::default()
                }
            })
            .unwrap_or_default();
        Scan {
            snapshot_started_at: String::new(),
            snapshot_finished_at: String::new(),
            since: Some(0),
            roots: vec![],
            profiles: vec![profile],
            snapshots: vec![],
            requests,
            events: vec![],
            exposure: ExposureScan::default(),
            cache_context,
            coverage: ScanCoverage::default(),
        }
    }

    fn one_hour() -> Profile {
        let mut profile = make_profile("profile", "root");
        profile.ttl.main.value = Some(Ttl::OneHour);
        profile
    }

    /// A main request with a large cold prefix; replay may warm it with an earlier turn.
    fn turn(session: &str, time: Millis) -> Request {
        let mut r = make_request();
        r.id = format!("{session}:{time}");
        r.session = session.into();
        r.time = time;
        r.end_time = time;
        r.parts[0].usage = claude_usage(ClaudeUsage {
            read: 0,
            write: 101_000,
            output: 500,
            ttl: Ttl::OneHour,
            input: 0,
        });
        r
    }

    fn warm_turn(session: &str, time: Millis) -> Request {
        let mut r = turn(session, time);
        r.parts[0].usage = Cache::Claude(Ttl::OneHour).usage(100_000, 1000, 0, 500);
        r
    }

    fn run(scan: &Scan, now: Millis) -> KeepaliveCheck {
        let mut checks = keepalive_checks(&KeepaliveOptions {
            scan,
            catalog: &prices(),
            ttls: &[],
            idle: &[],
            windows: &[],
            summaries: &[],
            estimates: &IndexMap::new(),
            since: scan.since,
            now,
        });
        assert_eq!(checks.len(), 1);
        checks.remove(0)
    }

    #[test]
    fn a_rescued_return_beats_leaving_the_session_idle() {
        let scan = scan(one_hour(), vec![turn("a", 0), turn("a", 2 * HOUR)]);
        let check = run(&scan, 3 * HOUR);
        assert_eq!(check.interval_ms, Some(50 * MINUTE));
        assert_eq!((check.returns, check.finals, check.unpriced), (1, 1, 0));
        // Two pings at 50m and 1h40m keep the cache warm until 2h40m.
        assert_eq!(check.scenarios.len(), 2);
        let s = &check.scenarios[1];
        assert_eq!(
            (s.cap_ms, s.pings, s.final_pings, s.rescued),
            (100 * MINUTE, 2, 2, 1)
        );
        assert!(s.savings_usd > 0.0 && s.closed_savings_usd > s.savings_usd);
        assert_eq!(check.base.action, Action::Advisory);
        assert_eq!(
            check.base.recommended.as_deref(),
            Some("every 50m for up to 1h 40m")
        );
        assert_eq!(check.base.savings_usd, Some(s.savings_usd));
    }

    #[test]
    fn sessions_that_never_return_make_pings_lose() {
        let mut requests = vec![turn("a", 0), turn("a", 2 * HOUR)];
        requests.extend((0..20).map(|i| turn(&format!("gone{i}"), i * MINUTE)));
        let check = run(&scan(one_hour(), requests), 3 * HOUR);
        assert_eq!(check.finals, 21);
        assert!(check.margin_usd.unwrap() < 0.0);
        assert_eq!(check.base.action, Action::Advisory);
        assert_eq!(check.base.current, None);
        assert_eq!(check.base.recommended.as_deref(), Some("off"));
        assert!(check.base.break_even.unwrap().contains("close within"));
    }

    #[test]
    fn gaps_inside_the_lifetime_have_nothing_to_rescue() {
        let requests = vec![turn("a", 0), turn("a", 55 * MINUTE), turn("a", 70 * MINUTE)];
        let check = run(&scan(one_hour(), requests), 2 * HOUR);
        // The 55m gap still pays one unneeded ping; no cap rescues anything.
        assert_eq!((check.returns, check.finals), (1, 1));
        assert_eq!(check.scenarios.len(), 1);
        assert!(check.margin_usd.unwrap() < 0.0);
        assert_eq!(check.scenarios[0].rescued, 0);
        assert_eq!(check.base.action, Action::Advisory);
        assert_eq!(check.base.current, None);
    }

    #[test]
    fn an_unknown_lifetime_is_not_priced() {
        let mut profile = one_hour();
        profile.ttl.main.value = None;
        let mut request = turn("a", 0);
        request.parts[0].usage.write1h = None;
        let check = run(&scan(profile, vec![request]), HOUR);
        assert_eq!(check.lifetime_ms, None);
        assert_eq!(check.base.action, Action::Advisory);
        assert_eq!(check.base.recommended, None);
    }

    #[test]
    fn codex_tiers_price_only_documented_lifetimes() {
        let mut profile = make_profile("profile", "root");
        profile.harness = Harness::Codex;
        let codex = |model: &str, session: &str, time: Millis| {
            let mut r = turn(session, time);
            r.harness = Harness::Codex;
            r.api = crate::model::Api::Responses;
            r.provider = Some("openai".into());
            r.model = Some(model.into());
            let part = &mut r.parts[0];
            part.model = Some(model.into());
            part.speed = None;
            part.geo = None;
            part.usage.write5m = None;
            part.usage.write1h = None;
            part.usage.write30m = part.usage.write;
            part.usage.write12h = Some(0);
            r
        };
        let requests = vec![
            codex("gpt-6.1-sol", "a", 0),
            codex("gpt-6.1-sol", "a", HOUR),
            codex("gpt-5.4", "b", 0),
        ];
        let mut conditional = requests[..2].to_vec();
        conditional[0].parts[0].tier = None;
        conditional[1].parts[0].tier = Some("priority".into());
        let conditional = run(&scan(profile.clone(), conditional), 2 * HOUR);
        assert_eq!(conditional.unpriced, 0);
        assert!(!conditional.scenarios.is_empty());
        assert_eq!(conditional.base.recommended, None);
        assert_eq!(conditional.base.savings_usd, None);
        assert_eq!(conditional.margin_usd, None);
        assert_eq!(conditional.base.break_even, None);
        assert!(
            conditional
                .base
                .uncertainty
                .iter()
                .any(|s| s == crate::prices::TIER_SENSITIVITY)
        );
        let check = run(&scan(profile, requests), 2 * HOUR);
        assert_eq!(check.lifetime_ms, Some(30 * MINUTE));
        assert_eq!(check.interval_ms, Some(25 * MINUTE));
        assert_eq!((check.returns, check.finals, check.unpriced), (1, 2, 1));
        assert_eq!(check.idle_compaction_savings_usd, None);
        // Pings at 25m and 50m keep the cache warm past the 1h return.
        assert_eq!(check.scenarios[1].cap_ms, 50 * MINUTE);

        // A measured 12h write remains warm for a two-hour return. Pings use 30m,
        // preserve that long coverage, and cannot claim a rewrite rescue.
        let long = |time| {
            let mut r = codex("gpt-6.1-sol", "long", time);
            r.parts[0].usage.write30m = Some(0);
            r.parts[0].usage.write12h = r.parts[0].usage.write;
            r
        };
        let mut profile = make_profile("profile", "root");
        profile.harness = Harness::Codex;
        let long = run(&scan(profile, vec![long(0), long(2 * HOUR)]), 3 * HOUR);
        assert_eq!(long.unpriced, 0);
        assert!(
            long.scenarios
                .iter()
                .all(|s| s.rescued == 0 && s.avoided_usd < 0.0)
        );
    }

    /// The default report's keepalive line.
    fn line(scan: Scan, now: Millis) -> String {
        let report = crate::replay::analyze(crate::replay::AnalyzeOptions {
            scan,
            catalog: &prices(),
            since: None,
            now,
            max_cost_per_use_usd: 1.0,
        })
        .unwrap();
        let text = crate::report::format_report(
            &report,
            crate::report::ReportOptions {
                width: None,
                color: false,
                unicode: false,
                details: false,
                host: None,
            },
        );
        text.lines()
            .find(|l| l.trim_start().starts_with("Keepalive pings"))
            .unwrap()
            .trim()
            .to_string()
    }

    #[test]
    fn the_report_prices_each_verdict_in_one_line() {
        // A return after 30m makes the TTL check keep 1h, which the pings then follow.
        let session = || {
            vec![
                warm_turn("a", 0),
                warm_turn("a", 30 * MINUTE),
                turn("a", 150 * MINUTE),
            ]
        };
        let win = line(scan(one_hour(), session()), 30 * DAY);
        assert!(
            win.starts_with("Keepalive pings: unknown. Pinging idle sessions every 50m for up to 1h 40m would save $"),
            "{win}"
        );
        assert!(win.ends_with("but a /loop task pings on a fixed schedule with no idle cap, and stops only when cancelled or after seven days."), "{win}");
        let mut requests = session();
        requests.extend((0..20).map(|i| warm_turn(&format!("gone{i}"), i * MINUTE)));
        let lose = line(scan(one_hour(), requests), 30 * DAY);
        assert!(
            lose.starts_with(
                "Keepalive pings: unknown. Pinging idle sessions every 50m would cost $"
            ),
            "{lose}"
        );
        assert!(lose.ends_with("more than leaving them idle."), "{lose}");
        // With 5m recommended, pings must come every 4m 10s.
        let short = line(
            scan(one_hour(), vec![turn("a", 0), turn("a", 2 * HOUR)]),
            30 * DAY,
        );
        assert!(short.contains("every 4m 10s"), "{short}");
    }

    #[test]
    fn no_ping_rescues_a_return_after_a_model_switch_or_compaction() {
        let mut switched = turn("a", 2 * HOUR);
        switched.parts[0].model = Some("claude-haiku-5-5".into());
        let check = run(&scan(one_hour(), vec![turn("a", 0), switched]), 3 * HOUR);
        assert_eq!((check.returns, check.unpriced), (1, 0));
        assert!(check.scenarios.iter().all(|s| s.rescued == 0));

        let mut compacted = scan(one_hour(), vec![turn("b", 0), turn("b", 2 * HOUR)]);
        compacted.events.push(crate::model::StructuralEvent {
            id: "boundary".into(),
            // Recorded under another alias of the same copied history.
            origins: vec![crate::model::Origin {
                session: "b".into(),
                location: crate::model::Location {
                    file: "synthetic".into(),
                    line: 1,
                },
            }],
            harness: Harness::Claude,
            session: "copy".into(),
            time: HOUR,
            kind: EventKind::Compaction,
            profiles: vec!["profile".into()],
            location: crate::model::Location {
                file: "synthetic".into(),
                line: 1,
            },
            request: None,
            model: None,
            version: None,
            client: None,
            cwd: None,
            trigger: None,
            precomputed: None,
            pre_tokens: None,
            post_tokens: None,
            duration_ms: None,
            tool: None,
            tool_id: None,
            settings: IndexMap::new(),
            idle_attribution: crate::model::IdleAttribution::Unknown,
            shell: None,
            pricing: None,
        });
        assert!(run(&compacted, 3 * HOUR).scenarios.is_empty());
    }

    #[test]
    fn ping_turns_cost_every_later_read_and_unrescued_returns() {
        let rescued = |extra: Vec<Request>| {
            let mut requests = vec![turn("a", 0), turn("a", 2 * HOUR)];
            requests.extend(extra);
            run(&scan(one_hour(), requests), 3 * HOUR).scenarios[1].avoided_usd
        };
        let alone = rescued(vec![]);
        // Two later requests read the two pings' turns again.
        let later = rescued(vec![
            turn("a", 2 * HOUR + MINUTE),
            turn("a", 2 * HOUR + 2 * MINUTE),
        ]);
        // A 55m gap sends one ping but was warm anyway, so it only reads that ping's turn.
        let warm = rescued(vec![turn("b", 0), turn("b", 55 * MINUTE)]);
        assert!(later < alone && warm < alone, "{alone} {later} {warm}");
    }

    #[test]
    fn an_unpriced_return_past_the_cache_keeps_the_verdict_open() {
        let mut unpriced = turn("a", 2 * HOUR);
        unpriced.parts[0].usage.input = None;
        let check = run(&scan(one_hour(), vec![turn("a", 0), unpriced]), 3 * HOUR);
        // Its own final period cannot be priced either.
        assert_eq!(check.unpriced, 2);
        assert!(check.scenarios.is_empty());
        assert_eq!(check.base.action, Action::Advisory);
        assert_eq!(check.base.recommended, None);
    }

    fn boundary(session: &str, time: Millis, post: Option<i64>) -> StructuralEvent {
        StructuralEvent {
            id: format!("boundary:{session}:{time}"),
            session: "other-alias".into(),
            origins: vec![crate::model::Origin {
                session: session.into(),
                location: crate::model::Location {
                    file: "synthetic".into(),
                    line: 1,
                },
            }],
            harness: Harness::Claude,
            time,
            kind: EventKind::Compaction,
            profiles: vec!["profile".into()],
            location: crate::model::Location {
                file: "synthetic".into(),
                line: 1,
            },
            request: None,
            model: None,
            version: None,
            client: None,
            cwd: None,
            trigger: None,
            precomputed: None,
            pre_tokens: None,
            post_tokens: post,
            duration_ms: None,
            tool: None,
            tool_id: None,
            settings: IndexMap::new(),
            idle_attribution: IdleAttribution::Unknown,
            shell: None,
            pricing: None,
        }
    }

    /// Run a complete session with an explicit window and cap, using public fixture prices.
    fn replay(requests: Vec<Request>, events: Vec<StructuralEvent>, cap: u64) -> (Run, Run) {
        let profile = one_hour();
        let scan = scan(profile.clone(), requests);
        let catalog = prices();
        let estimates = IndexMap::new();
        let options = KeepaliveOptions {
            scan: &scan,
            catalog: &catalog,
            ttls: &[],
            windows: &[],
            summaries: &[],
            estimates: &estimates,
            idle: &[],
            since: None,
            now: 30 * DAY,
        };
        let requests: Vec<_> = scan.requests.iter().collect();
        let events: Vec<_> = events.iter().collect();
        let samples = compaction_samples(&requests, &events, &[]);
        let mut window_events = WindowEvents::new(events.iter().copied());
        window_events.suppress_idle(0);
        let lifetime = Lifetime {
            ms: HOUR,
            cache: Cache::Claude(Ttl::OneHour),
            reason: "test".into(),
        };
        let replayer = Replayer {
            options: &options,
            samples: &samples,
            events: &window_events,
            lifetime: &lifetime,
            interval: 50 * MINUTE,
            new: 1000,
            output: 500,
            since: 0,
        };
        let session = Session {
            requests,
            events,
            windows: vec![],
            ttls: PerBucket {
                main: Some(Ttl::OneHour),
                subagent: Some(Ttl::FiveMinutes),
            },
        };
        (
            replayer.run(&session, 0).unwrap(),
            replayer.run(&session, cap).unwrap(),
        )
    }

    fn near(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < 1e-10, "{actual} != {expected}");
    }

    #[test]
    fn observed_cache_hits_do_not_become_hypothetical_ping_rescues() {
        let requests = vec![turn("a", 0), warm_turn("a", 2 * HOUR)];
        let (baseline, _) = replay(requests.clone(), vec![], 2);
        assert_eq!(baseline.reads["a:7200000"], 100_000);
        let expected: f64 = requests
            .iter()
            .flat_map(|r| {
                r.parts
                    .iter()
                    .map(|p| price_part(&prices(), r, p).total_usd.unwrap())
            })
            .sum();
        near(baseline.recorded, expected);
        let check = run(&scan(one_hour(), requests), 3 * HOUR);
        assert_eq!(check.unpriced, 0);
        assert!(check.scenarios.iter().all(|s| s.rescued == 0));
    }

    #[test]
    fn all_read_returns_do_not_inherit_long_warmth_under_a_short_policy() {
        let mut profile = one_hour();
        profile.ttl.main.value = Some(Ttl::FiveMinutes);
        let mut returned = warm_turn("a", 30 * MINUTE);
        returned.parts[0].usage = Cache::Claude(Ttl::OneHour).usage(101_000, 0, 0, 500);
        let check = run(&scan(profile, vec![turn("a", 0), returned]), HOUR);
        assert_eq!(check.lifetime_ms, Some(5 * MINUTE));
        assert_eq!(check.unpriced, 0);
        assert!(check.scenarios.iter().any(|s| s.rescued == 1));
    }

    #[test]
    fn removing_an_idle_summary_keeps_its_message_and_advisor_work() {
        let mut mixed = turn("a", 55 * MINUTE);
        let mut summary = mixed.parts[0].clone();
        summary.kind = PartKind::Compaction;
        let mut advisor = mixed.parts[0].clone();
        advisor.kind = PartKind::Advisor;
        mixed.parts.extend([summary, advisor.clone()]);
        let mut idle = boundary("a", HOUR, Some(20_000));
        idle.idle_attribution = IdleAttribution::Notice;
        idle.pre_tokens = Some(101_000);
        idle.request = Some(mixed.id.clone());
        let mut returned = turn("a", 2 * HOUR);
        returned.parts[0].usage = Cache::Claude(Ttl::OneHour).usage(0, 21_000, 0, 500);
        let (base, pinged) = replay(
            vec![turn("a", 0), mixed.clone(), returned.clone()],
            vec![idle.clone()],
            2,
        );
        assert!(base.reads.contains_key(&mixed.id));
        assert!(pinged.reads.contains_key(&mixed.id));
        mixed.parts.pop();
        let advisor_cost = price_part(&prices(), &mixed, &advisor).total_usd.unwrap();
        let (without_base, without_pinged) =
            replay(vec![turn("a", 0), mixed, returned], vec![idle], 2);
        near(base.recorded - without_base.recorded, advisor_cost);
        near(pinged.recorded - without_pinged.recorded, advisor_cost);
    }

    #[test]
    fn later_periods_read_the_accumulated_ping_context() {
        let (_, run) = replay(
            vec![turn("a", 0), turn("a", 2 * HOUR), turn("a", 4 * HOUR)],
            vec![],
            2,
        );
        assert_eq!(run.pings.len(), 4);
        // The second gap's two pings read the preceding gap's 2,000 added tokens.
        near(run.pings[2] - run.pings[0], 3000.0 * 0.2 / 1e6);
        near(run.pings[3] - run.pings[1], 3000.0 * 0.2 / 1e6);
        assert_eq!(run.reads["a:14400000"], 107_000);
    }

    #[test]
    fn each_later_request_uses_its_actual_model_speed_geography_and_tier() {
        for (model, speed) in [
            ("claude-haiku-5-5", "standard"),
            ("claude-opus-5-5", "fast"),
        ] {
            let mut later = turn("a", 2 * HOUR + MINUTE);
            later.parts[0].model = Some(model.into());
            later.parts[0].speed = Some(speed.into());
            later.parts[0].geo = Some("us".into());
            let original = later.clone();
            let (base, pinged) = replay(vec![turn("a", 0), turn("a", 2 * HOUR), later], vec![], 2);
            let (short_base, short_pinged) =
                replay(vec![turn("a", 0), turn("a", 2 * HOUR)], vec![], 2);
            let mut expected = original.parts[0].clone();
            expected.usage = Cache::Claude(Ttl::OneHour).usage(0, 104_000, 0, 500);
            let mut before = original.parts[0].clone();
            before.usage = Cache::Claude(Ttl::OneHour).usage(0, 101_000, 0, 500);
            let cost = |p: &BillingPart| price_part(&prices(), &original, p).total_usd.unwrap();
            near(
                (pinged.recorded - base.recorded) - (short_pinged.recorded - short_base.recorded),
                cost(&expected) - cost(&before),
            );
            assert_eq!(pinged.reads[&original.id], 0);
        }
    }

    #[test]
    fn only_pre_boundary_pings_are_replaced_and_post_boundary_pings_warm_the_return() {
        let mut returned = turn("a", 2 * HOUR);
        returned.parts[0].usage = claude_usage(ClaudeUsage {
            read: 20_000,
            ..ClaudeUsage::default()
        });
        let (baseline, pinged) = replay(
            vec![turn("a", 0), returned],
            vec![boundary("a", HOUR, Some(20_000))],
            2,
        );
        // The 50m ping is replaced at 1h. The 1h40m ping writes the new 20k prefix,
        // and its 1k turn remains present at 2h and in the final period.
        assert_eq!(baseline.reads["a:7200000"], 20_000);
        assert_eq!(pinged.reads["a:7200000"], 21_000);
        let mut r = turn("a", 100 * MINUTE);
        r.parts[0].usage = Cache::Claude(Ttl::OneHour).usage(0, 21_000, 0, 500);
        near(
            pinged.pings[1],
            price_part(&prices(), &r, &r.parts[0]).total_usd.unwrap(),
        );
        let mut compacted = scan(one_hour(), vec![turn("a", 0), turn("a", 2 * HOUR)]);
        compacted.events = vec![boundary("a", HOUR, None)];
        assert_eq!(run(&compacted, 3 * HOUR).unpriced, 2);
    }

    #[test]
    fn added_context_reprices_the_whole_later_request_at_its_context_tier() {
        let mut a = turn("a", 0);
        a.parts[0].model = Some("claude-haiku-5-5".into());
        a.parts[0].usage = claude_usage(ClaudeUsage {
            read: 98_000,
            write: 1000,
            output: 500,
            ttl: Ttl::OneHour,
            input: 0,
        });
        let mut b = a.clone();
        b.id = "return".into();
        b.time = 2 * HOUR;
        b.end_time = b.time;
        b.parts[0].usage = Cache::Claude(Ttl::OneHour).usage(0, 99_000, 0, 500);
        let (base, pinged) = replay(vec![a, b.clone()], vec![], 2);
        let mut expected = b.parts[0].clone();
        expected.usage = Cache::Claude(Ttl::OneHour).usage(102_000, 0, 0, 500);
        let mut before = b.parts[0].clone();
        before.usage = Cache::Claude(Ttl::OneHour).usage(0, 99_000, 0, 500);
        near(
            pinged.recorded - base.recorded,
            price_part(&prices(), &b, &expected).total_usd.unwrap()
                - price_part(&prices(), &b, &before).total_usd.unwrap(),
        );
    }

    #[test]
    fn recorded_idle_compaction_is_removed_from_both_no_ping_and_ping_paths() {
        let mut returned = turn("a", 2 * HOUR);
        returned.parts[0].usage = claude_usage(ClaudeUsage {
            read: 20_000,
            write: 1000,
            ttl: Ttl::OneHour,
            output: 500,
            input: 0,
        });
        let mut idle = boundary("a", HOUR, Some(20_000));
        idle.idle_attribution = IdleAttribution::Notice;
        idle.pre_tokens = Some(101_000);
        let (base, pinged) = replay(vec![turn("a", 0), returned], vec![idle], 2);
        assert_eq!(base.reads["a:7200000"], 0);
        assert_eq!(pinged.reads["a:7200000"], 103_500);
        assert!(base.recorded > pinged.recorded);
    }

    #[test]
    fn ping_and_idle_alternatives_apply_the_same_recommended_window_and_samples() {
        let mut profile = one_hour();
        profile.compaction =
            crate::controls::compaction_controls(&crate::controls::CompactionSources {
                harness: Harness::Claude,
                user: &serde_json::from_value(serde_json::json!({"autoCompactWindow": 400_000}))
                    .unwrap(),
                managed: &serde_json::Map::new(),
                env: &crate::controls::Env::new(),
                file: "settings.json",
                managed_file: "managed.json",
                scopes: None,
            })
            .unwrap();
        let mut summary = turn("sample", MINUTE);
        summary.id = "summary".into();
        summary.agent = Agent::Subagent;
        summary.parts[0].kind = PartKind::Compaction;
        summary.parts[0].usage = claude_usage(ClaudeUsage {
            read: 300_000,
            write: 0,
            output: 2000,
            ttl: Ttl::FiveMinutes,
            input: 0,
        });
        let mut sample = turn("sample", 3 * MINUTE);
        sample.parts[0].usage = claude_usage(ClaudeUsage {
            read: 0,
            write: 20_000,
            output: 500,
            ttl: Ttl::OneHour,
            input: 0,
        });
        let mut event = boundary("sample", 2 * MINUTE, Some(2000));
        event.model = Some("claude-opus-5-5".into());
        event.request = Some("summary".into());
        event.pre_tokens = Some(300_000);
        let target = |time: Millis, prefix: i64| {
            let mut r = turn("target", time);
            r.parts[0].usage = claude_usage(ClaudeUsage {
                read: if time == 220 * MINUTE {
                    0
                } else {
                    prefix - 1000
                },
                write: if time == 220 * MINUTE { prefix } else { 1000 },
                output: 500,
                ttl: Ttl::OneHour,
                input: 0,
            });
            r
        };
        let mut scan = scan(
            profile,
            vec![
                summary,
                sample,
                target(100 * MINUTE, 150_000),
                target(220 * MINUTE, 151_000),
                target(221 * MINUTE, 152_000),
            ],
        );
        scan.events = vec![event];
        let catalog = prices();
        let summaries = crate::replay::compaction_charges(&mut scan, &catalog);
        let estimates = IndexMap::new();
        let since = Some(100 * MINUTE);
        let windows = crate::compaction::compaction_checks(&crate::compaction::CompactionOptions {
            scan: &scan,
            catalog: &catalog,
            summaries: &summaries,
            estimates: &estimates,
            ttls: &[],
            since,
            now: 30 * DAY,
        });
        assert_eq!(windows[0].base.recommended, Some(100_000));
        assert_eq!(windows[0].base.action, Action::Change);
        let refs: Vec<_> = windows.iter().collect();
        let idle = crate::idle::idle_checks(&crate::idle::IdleOptions {
            scan: &scan,
            catalog: &catalog,
            summaries: &summaries,
            estimates: &estimates,
            ttls: &[],
            windows: &refs,
            since,
            now: 30 * DAY,
        });
        let idle_refs: Vec<_> = idle.iter().collect();
        assert!(idle.iter().all(|c| {
            c.after_recommendations
                .windows
                .iter()
                .all(|w| w.value == Some(100_000))
        }));
        let options = KeepaliveOptions {
            scan: &scan,
            catalog: &catalog,
            ttls: &[],
            windows: &refs,
            summaries: &summaries,
            estimates: &estimates,
            idle: &idle_refs,
            since,
            now: 30 * DAY,
        };
        let smaller = keepalive_checks(&options).remove(0);
        assert_eq!(smaller.unpriced, 0);
        let original = keepalive_checks(&KeepaliveOptions {
            windows: &[],
            ..options
        })
        .remove(0);
        assert_eq!(original.unpriced, 0);
        assert!(smaller.scenarios[1].avoided_usd < original.scenarios[1].avoided_usd);
    }

    #[test]
    fn a_temporary_model_switch_preserves_the_earlier_cohorts_warm_cache() {
        let mut other = turn("a", 110 * MINUTE);
        other.parts[0].model = Some("claude-haiku-5-5".into());
        let (base, pinged) = replay(
            vec![turn("a", 0), other, turn("a", 159 * MINUTE)],
            vec![],
            2,
        );
        assert_eq!(base.reads["a:9540000"], 0);
        assert_eq!(pinged.reads["a:9540000"], 104_000);
    }

    #[test]
    fn cold_cache_rewrites_are_not_new_ping_input() {
        let requests = (0..4)
            .map(|i| {
                let mut r = turn("a", i * 2 * HOUR);
                r.parts[0].usage = claude_usage(ClaudeUsage {
                    read: 0,
                    write: 100_000 + i * 1500,
                    output: 500,
                    ttl: Ttl::OneHour,
                    input: 0,
                });
                r
            })
            .collect();
        let mut scan = scan(one_hour(), requests);
        scan.cache_context.requests.clear();
        assert_eq!(run(&scan, 8 * HOUR).ping_input_tokens, Some(1000));
        scan.requests.truncate(1);
        let check = run(&scan, 8 * HOUR);
        assert_eq!(check.ping_input_tokens, None);
        assert_eq!(check.base.recommended, None);
    }

    #[test]
    fn an_unpriced_applicable_idle_policy_remains_an_unknown_alternative() {
        let mut scan = scan(one_hour(), vec![turn("a", 0), turn("a", 2 * HOUR)]);
        for r in &mut scan.requests {
            r.client = Some("cli".into());
            r.source = Some("repl_main_thread".into());
            r.parts[0].usage.read = Some(300_000);
            r.parts[0].usage.input = Some(301_000);
            r.parts[0].usage.write = Some(1000);
            r.parts[0].usage.write1h = Some(1000);
        }
        let catalog = prices();
        let estimates = IndexMap::new();
        let idle = crate::idle::idle_checks(&crate::idle::IdleOptions {
            scan: &scan,
            catalog: &catalog,
            summaries: &[],
            estimates: &estimates,
            ttls: &[],
            windows: &[],
            since: Some(0),
            now: 3 * HOUR,
        });
        assert!(idle[0].after_recommendations.applicable);
        assert_eq!(idle[0].after_recommendations.delta_usd, None);
        let refs: Vec<_> = idle.iter().collect();
        let check = keepalive_checks(&KeepaliveOptions {
            scan: &scan,
            catalog: &catalog,
            ttls: &[],
            windows: &[],
            summaries: &[],
            estimates: &estimates,
            idle: &refs,
            since: Some(0),
            now: 3 * HOUR,
        })
        .remove(0);
        assert_eq!(check.unpriced, 0);
        assert!(!check.scenarios.is_empty());
        assert_eq!(check.idle_compaction_savings_usd, None);
        assert_eq!(check.margin_usd, None);
        assert_eq!(check.base.recommended, None);
        assert!(
            check
                .base
                .uncertainty
                .iter()
                .any(|s| s.contains("idle-compaction alternative is unpriced"))
        );
    }

    #[test]
    fn carried_context_uses_the_recorded_compactions_ordinary_input_rate() {
        let first = turn("a", 0);
        let mut summary = turn("a", 80 * MINUTE);
        summary.agent = Agent::Subagent;
        summary.parts[0].kind = PartKind::Compaction;
        summary.parts[0].usage = claude_usage(ClaudeUsage {
            read: 0,
            write: 0,
            input: 101_500,
            output: 500,
            ttl: Ttl::FiveMinutes,
        });
        let mut returned = turn("a", 2 * HOUR);
        returned.parts[0].usage = claude_usage(ClaudeUsage {
            read: 20_000,
            write: 1000,
            input: 0,
            output: 500,
            ttl: Ttl::OneHour,
        });
        let (_, pinged) = replay(
            vec![first.clone(), summary.clone(), returned.clone()],
            vec![boundary("a", 81 * MINUTE, Some(20_000))],
            2,
        );
        summary.parts[0].usage.input = Some(103_000);
        returned.parts[0].usage = Cache::Claude(Ttl::OneHour).usage(21_000, 1500, 0, 500);
        let expected: f64 = [&first, &summary, &returned]
            .into_iter()
            .map(|r| price_part(&prices(), r, &r.parts[0]).total_usd.unwrap())
            .sum();
        near(pinged.recorded, expected);
    }

    #[test]
    fn durations_read_plainly() {
        assert_eq!(duration(50 * MINUTE), "50m");
        assert_eq!(duration(4 * MINUTE + 10_000), "4m 10s");
        assert_eq!(duration(2 * DAY + 3 * HOUR), "2d 3h");
    }
}
