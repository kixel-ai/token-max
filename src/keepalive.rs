//! Keepalive check: price pinging idle sessions just under the cache lifetime, for each cap
//! on how long to keep pinging, against leaving them idle and idle compaction.

use std::collections::HashMap;

use indexmap::IndexMap;

use crate::cache::{CODEX_CACHE_MILLISECONDS, observed_ttl, ttl_milliseconds};
use crate::model::{
    Action, Agent, BillingPart, Bucket, CheckBase, Coverage, EventKind, Evidence, Harness,
    IdleCheck, KeepaliveCheck, KeepaliveScenario, Millis, PartKind, Profile, Request, Scan, Ttl,
    TtlCheck, Usage,
};
use crate::prices::{Catalog, PRICE_SOURCES, price_part, write_billing};

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
}

impl Cache {
    fn usage(self, read: i64, write: i64, ordinary: i64, output: i64) -> Usage {
        let (write5m, write1h, write30m, write12h) = match self {
            Cache::Claude(Ttl::FiveMinutes) => (Some(write), Some(0), None, None),
            Cache::Claude(Ttl::OneHour) => (Some(0), Some(write), None, None),
            Cache::Codex => (None, None, Some(write), Some(0)),
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

/// A price at each step of growing context: a line where the context tier holds, else every value.
enum Steps {
    Line { first: f64, step: f64 },
    Exact(Vec<f64>),
}

impl Steps {
    /// Prices `f(0)..f(n - 1)`. Rates only rise with the context tier, so a line through the
    /// first two steps meets the last one only when no tier boundary lies between them.
    fn new(n: u64, f: impl Fn(i64) -> Option<f64>) -> Option<Self> {
        let first = f(0)?;
        if n <= 1 {
            return Some(Steps::Line { first, step: 0.0 });
        }
        let step = f(1)? - first;
        let last = f(n as i64 - 1)?;
        if (first + step * (n - 1) as f64 - last).abs() <= 1e-9 * last.abs().max(1.0) {
            return Some(Steps::Line { first, step });
        }
        (0..n as i64)
            .map(f)
            .collect::<Option<Vec<f64>>>()
            .map(Steps::Exact)
    }
    fn at(&self, k: u64) -> f64 {
        match self {
            Steps::Line { first, step } => first + step * k as f64,
            Steps::Exact(values) => values[k as usize],
        }
    }
    /// The first `n` values added up.
    fn sum(&self, n: u64) -> f64 {
        match self {
            Steps::Line { first, step } => {
                let n = n as f64;
                n * first + step * n * (n - 1.0) / 2.0
            }
            Steps::Exact(values) => values[..n as usize].iter().sum(),
        }
    }
}

/// How a return resumes after an idle gap.
struct Return {
    /// Pings that fit before it.
    room: u64,
    /// Pings that would keep its cache warm, or None when its cache was already warm or no
    /// ping can help, such as after a compaction or a model switch.
    needed: Option<u64>,
    /// Whether it read a warm cache with no pings.
    warm: bool,
    /// Its price after `k` pings' turns, reading a warm or rewriting a cold prefix.
    warm_price: Steps,
    cold_price: Steps,
    /// Later requests before a compaction that found the cache warm and cold, which also
    /// read or write the pings' turns.
    later: (u64, u64),
}

/// One idle gap or final period.
struct Period {
    /// The cost of each successive ping.
    pings: Steps,
    /// None for a final period, which pings for the whole cap.
    resume: Option<Return>,
}

impl Period {
    /// Pings sent under a cap of `cap` pings.
    fn sent(&self, cap: u64) -> u64 {
        self.resume.as_ref().map_or(cap, |r| r.room.min(cap))
    }
    fn rescued(&self, cap: u64) -> bool {
        self.resume
            .as_ref()
            .is_some_and(|r| r.needed.is_some_and(|n| cap >= n))
    }
    /// The return's cost with no pings minus with them, including later reads of ping turns.
    fn avoided(&self, cap: u64) -> f64 {
        let Some(r) = &self.resume else {
            return 0.0;
        };
        let sent = self.sent(cap);
        let price = |warm: bool, k: u64| {
            if warm {
                r.warm_price.at(k)
            } else {
                r.cold_price.at(k)
            }
        };
        let (warm, cold) = r.later;
        let carried = warm as f64 * (r.warm_price.at(sent) - r.warm_price.at(0))
            + cold as f64 * (r.cold_price.at(sent) - r.cold_price.at(0));
        price(r.warm, 0) - price(r.warm || self.rescued(cap), sent) - carried
    }
    /// Net saving under a cap of `cap` pings; finals count only when `open`.
    fn net(&self, cap: u64, open: bool) -> f64 {
        if self.resume.is_none() && !open {
            return 0.0;
        }
        self.avoided(cap) - self.pings.sum(self.sent(cap))
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

fn check(options: &KeepaliveOptions, profile: &Profile) -> Option<KeepaliveCheck> {
    let catalog = options.catalog;
    let mut requests: Vec<&Request> = options
        .scan
        .requests
        .iter()
        .filter(|r| {
            r.profiles.contains(&profile.id)
                && r.harness == profile.harness
                && options.since.is_none_or(|s| r.time >= s)
                && match r.harness {
                    Harness::Claude => r.agent == Agent::Main,
                    Harness::Codex => r.agent != Agent::Subagent,
                }
                && r.parts
                    .iter()
                    .any(|p| matches!(p.kind, PartKind::Message | PartKind::Compaction))
        })
        .collect();
    let first = requests.iter().map(|r| r.time).min()?;
    requests.sort_by_key(|r| r.time);
    let since = options.since.unwrap_or(first);
    let now = options.now;
    let lifetime = match profile.harness {
        Harness::Claude => claude_lifetime(profile, options.ttls, &requests),
        Harness::Codex => Some(Lifetime {
            ms: CODEX_CACHE_MILLISECONDS,
            cache: Cache::Codex,
            reason: "documented 30m minimum cache lifetime, refreshed by reads".into(),
        }),
    };
    // Below the lifetime, and below Claude's idle-compaction trigger at 90% of it,
    // so pinging keeps idle compaction from firing while it runs.
    let interval = lifetime.as_ref().map(|l| l.ms / 6 * 5);
    let new_tokens = median(
        requests
            .iter()
            .filter_map(|r| {
                let p = message(r).next_back()?;
                Some(p.usage.input? - p.usage.read?)
            })
            .collect(),
    );
    let output_tokens = median(
        requests
            .iter()
            .filter_map(|r| message(r).next_back()?.usage.output)
            .collect(),
    );

    let mut compactions: HashMap<&str, Vec<Millis>> = HashMap::new();
    for e in &options.scan.events {
        if e.kind == EventKind::Compaction && e.profiles.contains(&profile.id) {
            // A copied history keeps the boundary under every conversation alias.
            let aliases = e.origins.iter().map(|o| o.session.as_str());
            for session in std::iter::once(e.session.as_str()).chain(aliases) {
                compactions.entry(session).or_default().push(e.time);
            }
        }
    }
    let mut sessions: IndexMap<&str, Vec<&Request>> = IndexMap::new();
    for &r in &requests {
        sessions.entry(r.session.as_str()).or_default().push(r);
    }
    let compacts = |r: &Request| r.parts.iter().any(|p| p.kind == PartKind::Compaction);

    /// An idle period before its pings are priced.
    struct Gap<'r> {
        a: &'r Request,
        /// The return, its gap, whether a compaction came between, and how many later
        /// requests before the next compaction found the cache warm and cold.
        b: Option<(&'r Request, Millis, bool, (u64, u64))>,
    }
    let mut gaps: Vec<Gap> = vec![];
    if let (Some(lifetime), Some(interval)) = (&lifetime, interval) {
        for (session, list) in &sessions {
            let times = compactions.get(session);
            // A compaction between two requests replaces the cached prefix.
            let boundary: Vec<bool> = (0..list.len())
                .map(|k| {
                    k > 0
                        && (compacts(list[k - 1])
                            || times.is_some_and(|t| {
                                t.iter().any(|&t| t > list[k - 1].time && t <= list[k].time)
                            }))
                })
                .collect();
            let mut later = vec![(0u64, 0u64); list.len()];
            for k in (0..list.len().saturating_sub(1)).rev() {
                if !boundary[k + 1] {
                    let (warm, cold) = later[k + 1];
                    later[k] = if list[k + 1].time - list[k].end_time <= lifetime.ms {
                        (warm + 1, cold)
                    } else {
                        (warm, cold + 1)
                    };
                }
            }
            for (i, &a) in list.iter().enumerate() {
                let b = list.get(i + 1).map(|&b| {
                    (
                        b,
                        (b.time - a.end_time).max(0),
                        boundary[i + 1],
                        later[i + 1],
                    )
                });
                if b.is_none_or(|(_, gap, _, _)| gap > interval) {
                    gaps.push(Gap { a, b });
                }
            }
        }
    }
    let (returns, finals) = gaps.iter().fold((0, 0), |(r, f), g| {
        if g.b.is_some() {
            (r + 1, f)
        } else {
            (r, f + 1)
        }
    });
    let longest = interval.map_or(0, |i| (LONGEST_CAP / i) as u64);
    let needed = |gap: Millis| match (&lifetime, interval) {
        (Some(l), Some(i)) if gap > l.ms => Some(((gap - l.ms + i - 1) / i) as u64),
        _ => None,
    };
    // Pricing every ping up to the longest useful cap covers every candidate.
    let most = gaps
        .iter()
        .filter_map(|g| needed(g.b?.1))
        .filter(|&n| n <= longest)
        .max()
        .unwrap_or(0);
    let mut periods: Vec<Period> = vec![];
    let (mut unpriced, mut unpriced_returns) = (0u64, 0u64);
    if let (Some(lifetime), Some(interval)) = (&lifetime, interval) {
        let cache = lifetime.cache;
        let price = |r: &Request, p: &BillingPart, usage: Usage| {
            price_part(catalog, r, &BillingPart { usage, ..p.clone() }).total_usd
        };
        let documented = |p: &BillingPart| match (cache, &p.model) {
            // Older Codex models' retention is not a documented lifetime.
            (Cache::Codex, Some(model)) => write_billing(model) == Some(true),
            (Cache::Codex, None) => false,
            (Cache::Claude(_), _) => true,
        };
        for g in &gaps {
            let priced = (|| {
                let part = message(g.a).next_back().filter(|p| documented(p))?;
                // The first ping reads everything the last request sent; its own reply is part
                // of the median new input, as it is for recorded requests.
                let context = part.usage.input?;
                let (new, output) = (new_tokens?, output_tokens?);
                let room = g.b.map(|(_, gap, _, _)| ((gap - 1) / interval) as u64);
                let sent = room.map_or(most, |room| room.min(most));
                let pings = Steps::new(sent.max(1), |k| {
                    price(g.a, part, cache.usage(context + k * new, new, 0, output))
                })?;
                let resume = match g.b {
                    None => None,
                    Some((b, gap, compacted, later)) => {
                        let resumed = b
                            .parts
                            .iter()
                            .find(|p| matches!(p.kind, PartKind::Message | PartKind::Compaction))
                            .filter(|p| documented(p))?;
                        let u = &resumed.usage;
                        let ordinary = u.input? - u.read? - u.write?;
                        let cacheable = u.input? - ordinary;
                        let reusable = !compacted
                            && resumed.model == part.model
                            && resumed.speed == part.speed
                            && b.provider == g.a.provider;
                        let prefix = if reusable { context.min(cacheable) } else { 0 };
                        let output = u.output?;
                        // A compaction between them replaced the pings' turns too.
                        let added = |k: i64| if compacted { 0 } else { k * new };
                        Some(Return {
                            room: room?,
                            needed: needed(gap).filter(|_| reusable),
                            warm: reusable && gap <= lifetime.ms,
                            warm_price: Steps::new(sent + 1, |k| {
                                let read = prefix + added(k);
                                price(
                                    b,
                                    resumed,
                                    cache.usage(
                                        read,
                                        cacheable + added(k) - read,
                                        ordinary,
                                        output,
                                    ),
                                )
                            })?,
                            cold_price: Steps::new(sent + 1, |k| {
                                price(
                                    b,
                                    resumed,
                                    cache.usage(0, cacheable + added(k), ordinary, output),
                                )
                            })?,
                            later,
                        })
                    }
                };
                Some(Period { pings, resume })
            })();
            match priced {
                Some(p) => periods.push(p),
                None => {
                    unpriced += 1;
                    if g.b.is_some_and(|(_, gap, _, _)| gap > lifetime.ms) {
                        unpriced_returns += 1;
                    }
                }
            }
        }
    }

    let mut caps: Vec<u64> = periods
        .iter()
        .filter_map(|p| p.resume.as_ref()?.needed)
        .filter(|&n| n <= most)
        .collect();
    caps.sort_unstable();
    caps.dedup();
    let interval_ms = interval.unwrap_or(0);
    let scenarios: Vec<KeepaliveScenario> = caps
        .iter()
        .map(|&cap| {
            let (mut pings, mut final_pings, mut rescued) = (0, 0, 0);
            let (mut ping_cost, mut avoided, mut savings, mut closed) = (0.0, 0.0, 0.0, 0.0);
            for p in &periods {
                let sent = p.sent(cap);
                if p.resume.is_some() {
                    pings += sent;
                } else {
                    final_pings += sent;
                }
                ping_cost += p.pings.sum(sent);
                rescued += u64::from(p.rescued(cap));
                avoided += p.avoided(cap);
                savings += p.net(cap, true);
                closed += p.net(cap, false);
            }
            KeepaliveScenario {
                cap_ms: cap as Millis * interval_ms,
                pings,
                final_pings,
                rescued,
                ping_cost_usd: ping_cost,
                avoided_usd: avoided,
                savings_usd: savings,
                closed_savings_usd: closed,
            }
        })
        .collect();

    // Idle compaction competes only where its own check recommends it; the two never
    // run in the same idle period, so their savings are never added.
    let idle: Vec<&&IdleCheck> = options
        .idle
        .iter()
        .filter(|c| c.base.profile == profile.id)
        .collect();
    let idle_savings = (profile.harness == Harness::Claude).then(|| {
        idle.iter()
            .filter(|c| c.base.recommended == Some(true))
            .filter_map(|c| c.after_recommendations.delta_usd)
            .map(|d| (-d).max(0.0))
            .sum::<f64>()
    });
    let alternative = idle_savings.unwrap_or(0.0).max(0.0);
    let best = scenarios
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.savings_usd.total_cmp(&b.savings_usd))
        .map(|(i, s)| (caps[i], s));
    let margin = best.map(|(_, s)| s.savings_usd - alternative);
    // Unpriced periods cannot flip the result when each is bounded by the largest priced one.
    let largest = periods
        .iter()
        .map(|p| match best {
            Some((cap, _)) => p.net(cap, true).abs(),
            None => p.pings.sum(p.sent(1)),
        })
        .fold(0.0, f64::max);
    let decided = |margin: f64| {
        unpriced == 0 || (!periods.is_empty() && margin.abs() > unpriced as f64 * largest)
    };
    let mechanism = match profile.harness {
        Harness::Claude => {
            "no setting: a /loop task pings on a fixed schedule with no idle cap, and stops only when cancelled or after seven days"
        }
        Harness::Codex => "no setting: Codex has no way to schedule pings",
    };
    let (action, recommended, savings) = if lifetime.is_none() || periods.is_empty() {
        (Action::Advisory, None, None)
    } else {
        match margin {
            Some(m) if m > 0.0 && decided(m) => {
                let (cap, _) = best.expect("a margin comes from a scenario");
                (
                    Action::Advisory,
                    Some(policy(interval_ms, cap as Millis * interval_ms)),
                    Some(m),
                )
            }
            Some(m) if !decided(m) => (Action::Advisory, None, None),
            // No priced return outlasted the cache, so pings could only add cost, unless an
            // unpriced one did.
            None if unpriced_returns > 0 => (Action::Advisory, None, None),
            _ => (Action::Keep, Some("off".to_string()), Some(0.0)),
        }
    };
    let elapsed = now - since;
    let break_even = best.and_then(|(cap, s)| {
        // How long final periods could stay open on average before pinging stops paying.
        let spare = s.closed_savings_usd - alternative;
        if spare <= 0.0 || s.savings_usd > alternative {
            return None;
        }
        let finals: Vec<&Period> = periods.iter().filter(|p| p.resume.is_none()).collect();
        // The most pings each such session can send before pinging stops paying.
        let open = (0..cap)
            .take_while(|&n| finals.iter().map(|p| p.pings.sum(n)).sum::<f64>() <= spare)
            .last()?;
        // Closing before ping `open + 1` keeps it paying.
        Some(format!(
            "pinging {} wins only if sessions that never came back close within {} of their last request",
            policy(interval_ms, cap as Millis * interval_ms),
            duration((open + 1) as Millis * interval_ms)
        ))
    });
    let mut history_profiles: Vec<String> = vec![];
    for r in &requests {
        for p in &r.profiles {
            if !history_profiles.contains(p) {
                history_profiles.push(p.clone());
            }
        }
    }
    let mut uncertainty = vec![
        "native transcripts do not record when a client closed or slept; returns within the cap assume it stayed open and awake, and final periods ping for the full cap".to_string(),
        "recorded turns, including any scheduled-task turns, are replayed as activity; pings are added on top, so pings already running are not detected".to_string(),
    ];
    if lifetime.is_none() {
        uncertainty.push("the main cache TTL is unknown, so no ping interval can be priced".into());
    }
    if new_tokens.is_none() || output_tokens.is_none() {
        uncertainty.push("no recorded request gives a ping size".into());
    }
    if unpriced > 0 {
        uncertainty.push(format!(
            "{unpriced} periods could not be priced; each is bounded by the largest priced period"
        ));
    }
    Some(KeepaliveCheck {
        base: CheckBase {
            profile: profile.id.clone(),
            history_profiles,
            evidence: if savings.is_some() {
                Evidence::Estimated
            } else {
                Evidence::Unknown
            },
            action,
            current: Some("off".into()),
            recommended,
            owner: mechanism.into(),
            since: Some(since),
            until: Some(now),
            window_basis: if options.since.is_none() {
                "available transcript window"
            } else {
                "explicit --since window"
            }
            .into(),
            baseline: if alternative > 0.0 {
                "idle compaction as recommended, with no pings"
            } else {
                "sessions left idle, with no pings"
            }
            .into(),
            requests: requests.len() as u64,
            coverage: Some(Coverage {
                priced: periods.len() as u64,
                total: periods.len() as u64 + unpriced,
            }),
            savings_usd: savings,
            monthly_savings_usd: savings
                .filter(|_| elapsed > 0)
                .map(|s| s * 30.0 * DAY as f64 / elapsed as f64),
            break_even,
            assumptions: vec![
                lifetime.as_ref().map_or_else(
                    || "cache lifetime unknown".into(),
                    |l| format!("cache lifetime: {}", l.reason),
                ),
                "a ping fires each interval after the last request, five sixths of the lifetime, which is also below Claude's idle-compaction trigger at 90% of one hour".into(),
                "each ping reads the last request's whole input and writes the median recorded request's new input, which includes the previous reply, plus its output; later pings, the return and later requests before a compaction read those turns".into(),
                "a return within the last ping plus the lifetime reads its resumed prefix instead of rewriting it at the lifetime's write rate, unless a compaction or a model, speed or provider change came between; the whole return is repriced, including its context tier".into(),
                "pinging prevents idle compaction while it runs, so caps are priced with idle compaction off and compete with it rather than adding to it; idle compaction's saving comes from its own check, after any window recommendation, while pings replay recorded contexts".into(),
                "caps are the ping counts that rescue each recorded return, up to the seven-day expiry of recurring Claude Code tasks".into(),
                "every ping and return is priced at its own context tier; ping turns do not grow later periods' pings, and growth past an auto-compact window is not modeled".into(),
            ],
            uncertainty,
            sources: SOURCES
                .iter()
                .chain(PRICE_SOURCES.iter())
                .map(|s| s.to_string())
                .collect(),
        },
        bucket: "main",
        lifetime_ms: lifetime.as_ref().map(|l| l.ms),
        interval_ms: interval,
        ping_input_tokens: new_tokens,
        ping_output_tokens: output_tokens,
        returns,
        finals,
        unpriced,
        idle_compaction_savings_usd: idle_savings,
        margin_usd: margin.filter(|_| !periods.is_empty()),
        scenarios,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CacheContext, ExposureScan, ScanCoverage};
    use crate::test_support::{ClaudeUsage, claude_usage, make_profile, make_request, prices};

    const HOUR: Millis = 60 * MINUTE;

    fn scan(profile: Profile, requests: Vec<Request>) -> Scan {
        Scan {
            snapshot_started_at: String::new(),
            snapshot_finished_at: String::new(),
            since: None,
            roots: vec![],
            profiles: vec![profile],
            snapshots: vec![],
            requests,
            events: vec![],
            exposure: ExposureScan::default(),
            cache_context: CacheContext::default(),
            coverage: ScanCoverage::default(),
        }
    }

    fn one_hour() -> Profile {
        let mut profile = make_profile("profile", "root");
        profile.ttl.main.value = Some(Ttl::OneHour);
        profile
    }

    /// A main request with a large warm prefix, as in a long session.
    fn turn(session: &str, time: Millis) -> Request {
        let mut r = make_request();
        r.id = format!("{session}:{time}");
        r.session = session.into();
        r.time = time;
        r.end_time = time;
        r.parts[0].usage = claude_usage(ClaudeUsage {
            read: 100_000,
            write: 1000,
            output: 500,
            ttl: Ttl::OneHour,
            input: 0,
        });
        r
    }

    fn run(scan: &Scan, now: Millis) -> KeepaliveCheck {
        let mut checks = keepalive_checks(&KeepaliveOptions {
            scan,
            catalog: &prices(),
            ttls: &[],
            idle: &[],
            since: None,
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
        assert_eq!(check.scenarios.len(), 1);
        let s = &check.scenarios[0];
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
        assert_eq!(check.base.action, Action::Keep);
        assert_eq!(check.base.recommended.as_deref(), Some("off"));
        assert!(check.base.break_even.unwrap().contains("close within"));
    }

    #[test]
    fn gaps_inside_the_lifetime_have_nothing_to_rescue() {
        let requests = vec![turn("a", 0), turn("a", 55 * MINUTE), turn("a", 70 * MINUTE)];
        let check = run(&scan(one_hour(), requests), 2 * HOUR);
        // The 55m gap still pays one unneeded ping; no cap rescues anything.
        assert_eq!((check.returns, check.finals), (1, 1));
        assert!(check.scenarios.is_empty() && check.margin_usd.is_none());
        assert_eq!(check.base.action, Action::Keep);
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
    fn codex_prices_only_documented_lifetimes() {
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
            part.usage.write30m = Some(1000);
            part.usage.write12h = Some(0);
            r
        };
        let requests = vec![
            codex("gpt-6.1-sol", "a", 0),
            codex("gpt-6.1-sol", "a", HOUR),
            codex("gpt-5.4", "b", 0),
        ];
        let check = run(&scan(profile, requests), 2 * HOUR);
        assert_eq!(check.lifetime_ms, Some(30 * MINUTE));
        assert_eq!(check.interval_ms, Some(25 * MINUTE));
        assert_eq!((check.returns, check.finals, check.unpriced), (1, 2, 1));
        assert_eq!(check.idle_compaction_savings_usd, None);
        // Pings at 25m and 50m keep the cache warm past the 1h return.
        assert_eq!(check.scenarios[0].cap_ms, 50 * MINUTE);
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
                turn("a", 0),
                turn("a", 30 * MINUTE),
                turn("a", 150 * MINUTE),
            ]
        };
        let win = line(scan(one_hour(), session()), 30 * DAY);
        assert!(
            win.starts_with("Keepalive pings: off. Pinging idle sessions every 50m for up to 1h 40m would save $"),
            "{win}"
        );
        assert!(win.ends_with("but a /loop task pings on a fixed schedule with no idle cap, and stops only when cancelled or after seven days."), "{win}");
        let mut requests = session();
        requests.extend((0..20).map(|i| turn(&format!("gone{i}"), i * MINUTE)));
        let lose = line(scan(one_hour(), requests), 30 * DAY);
        assert!(
            lose.starts_with("Keepalive pings: off. Pinging idle sessions every 50m would cost $"),
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
        assert!(check.scenarios.is_empty());

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
            pricing: None,
        });
        assert!(run(&compacted, 3 * HOUR).scenarios.is_empty());
    }

    #[test]
    fn ping_turns_cost_every_later_read_and_unrescued_returns() {
        let rescued = |extra: Vec<Request>| {
            let mut requests = vec![turn("a", 0), turn("a", 2 * HOUR)];
            requests.extend(extra);
            run(&scan(one_hour(), requests), 3 * HOUR).scenarios[0].avoided_usd
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

    #[test]
    fn steps_price_every_value_across_a_tier_boundary() {
        let line = Steps::new(4, |k| Some(1.0 + k as f64)).unwrap();
        assert!(matches!(line, Steps::Line { .. }));
        assert_eq!(line.sum(3), 6.0);
        let tiered = Steps::new(4, |k| Some(if k < 2 { 1.0 } else { 3.0 })).unwrap();
        assert!(matches!(tiered, Steps::Exact(_)));
        assert_eq!((tiered.sum(4), tiered.at(2)), (8.0, 3.0));
    }

    #[test]
    fn durations_read_plainly() {
        assert_eq!(duration(50 * MINUTE), "50m");
        assert_eq!(duration(4 * MINUTE + 10_000), "4m 10s");
        assert_eq!(duration(2 * DAY + 3 * HOUR), "2d 3h");
    }
}
