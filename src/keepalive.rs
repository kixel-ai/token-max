//! Keepalive check: price pinging idle sessions just under the cache lifetime, for each cap
//! on how long to keep pinging, against leaving them idle and idle compaction.

use indexmap::IndexMap;

use crate::cache::{CODEX_CACHE_MILLISECONDS, observed_ttl, ttl_milliseconds};
use crate::model::{
    Action, Agent, BillingPart, Bucket, CheckBase, Coverage, Evidence, Harness, IdleCheck,
    KeepaliveCheck, KeepaliveScenario, Millis, PartKind, Profile, Request, Scan, Ttl, TtlCheck,
    Usage,
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
    fn usage(self, read: i64, write: i64, output: i64) -> Usage {
        let (write5m, write1h, write30m, write12h) = match self {
            Cache::Claude(Ttl::FiveMinutes) => (Some(write), Some(0), None, None),
            Cache::Claude(Ttl::OneHour) => (Some(0), Some(write), None, None),
            Cache::Codex => (None, None, Some(write), Some(0)),
        };
        Usage {
            input: Some(read + write),
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

/// One idle gap or final period, with its costs per ping and per avoided miss.
struct Period {
    /// Pings that fit before the return; None for a final period.
    room: Option<u64>,
    /// Pings needed so the return reads a warm cache; 0 when it already did.
    needed: u64,
    /// The first ping's cost and how much each later ping adds, as context grows.
    first: f64,
    growth: f64,
    /// A rewrite of the resumed prefix minus a read of it.
    miss: f64,
}

impl Period {
    /// Total cost of the first `n` pings.
    fn pings(&self, n: u64) -> f64 {
        let n = n as f64;
        n * self.first + self.growth * n * (n - 1.0) / 2.0
    }
    /// Pings sent under a cap of `cap` pings.
    fn sent(&self, cap: u64) -> u64 {
        self.room.map_or(cap, |room| room.min(cap))
    }
    /// Net saving under a cap of `cap` pings; finals count only when `open`.
    fn net(&self, cap: u64, open: bool) -> f64 {
        if self.room.is_none() && !open {
            return 0.0;
        }
        let sent = self.sent(cap);
        let avoided = if self.needed > 0 && cap >= self.needed {
            // The return also reads every ping's turn.
            self.miss - sent as f64 * self.growth
        } else {
            0.0
        };
        avoided - self.pings(sent)
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
                && message(r).next().is_some()
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

    let mut sessions: IndexMap<&str, Vec<&Request>> = IndexMap::new();
    for &r in &requests {
        sessions.entry(r.session.as_str()).or_default().push(r);
    }
    let mut periods: Vec<Period> = vec![];
    let (mut returns, mut finals, mut unpriced) = (0u64, 0u64, 0u64);
    if let (Some(lifetime), Some(interval)) = (&lifetime, interval) {
        for list in sessions.values() {
            for (i, &a) in list.iter().enumerate() {
                let b = list.get(i + 1).copied();
                let gap = b.map(|b| (b.time - a.end_time).max(0));
                if gap.is_some_and(|g| g <= interval) {
                    continue;
                }
                match b {
                    Some(_) => returns += 1,
                    None => finals += 1,
                }
                let priced = (|| {
                    let part = message(a).next_back()?;
                    if let (Cache::Codex, Some(model)) = (lifetime.cache, &part.model)
                        && write_billing(model) != Some(true)
                    {
                        // Older models' retention is not a documented lifetime.
                        return None;
                    }
                    let context = part.usage.input? + part.usage.output?;
                    let (new, output) = (new_tokens?, output_tokens?);
                    let price = |r: &Request, p: &BillingPart, usage: Usage| {
                        price_part(catalog, r, &BillingPart { usage, ..p.clone() }).total_usd
                    };
                    let cache = lifetime.cache;
                    let first = price(a, part, cache.usage(context, new, output))?;
                    let second = price(a, part, cache.usage(context + new, new, output))?;
                    let (room, needed, miss) = match (b, gap) {
                        (Some(b), Some(gap)) => {
                            let resumed = message(b).next()?;
                            let prefix = context.min(resumed.usage.input?);
                            let miss = price(b, resumed, cache.usage(0, prefix, 0))?
                                - price(b, resumed, cache.usage(prefix, 0, 0))?;
                            let needed = if gap > lifetime.ms {
                                ((gap - lifetime.ms + interval - 1) / interval) as u64
                            } else {
                                0
                            };
                            (Some(((gap - 1) / interval) as u64), needed, miss)
                        }
                        _ => (None, 0, 0.0),
                    };
                    Some(Period {
                        room,
                        needed,
                        first,
                        growth: second - first,
                        miss,
                    })
                })();
                match priced {
                    Some(p) => periods.push(p),
                    None => unpriced += 1,
                }
            }
        }
    }

    let longest = interval.map_or(0, |i| (LONGEST_CAP / i) as u64);
    let mut caps: Vec<u64> = periods
        .iter()
        .map(|p| p.needed)
        .filter(|&n| n > 0 && n <= longest)
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
                if p.room.is_some() {
                    pings += sent;
                } else {
                    final_pings += sent;
                }
                ping_cost += p.pings(sent);
                if p.needed > 0 && cap >= p.needed {
                    rescued += 1;
                    avoided += p.miss - sent as f64 * p.growth;
                }
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
            None => p.pings(p.sent(1)),
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
            // No return outlasted the cache, so pings could only add cost.
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
        let finals: Vec<&Period> = periods.iter().filter(|p| p.room.is_none()).collect();
        // The most pings each such session can send before pinging stops paying.
        let open = (0..cap)
            .take_while(|&n| finals.iter().map(|p| p.pings(n)).sum::<f64>() <= spare)
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
                "each ping reads the session's last context and adds the median recorded request's new input and output, a full-turn proxy; later pings and the return read that growth".into(),
                "a return within the last ping plus the lifetime reads its resumed prefix instead of rewriting it; misses are priced at the lifetime's write rate".into(),
                "pinging prevents idle compaction while it runs, so caps are priced with idle compaction off and compete with it rather than adding to it".into(),
                "caps are the ping counts that rescue each recorded return, up to the seven-day expiry of recurring Claude Code tasks".into(),
                "context tiers are priced at each period's first and second ping; growth past an auto-compact window is not modeled".into(),
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
    fn durations_read_plainly() {
        assert_eq!(duration(50 * MINUTE), "50m");
        assert_eq!(duration(4 * MINUTE + 10_000), "4m 10s");
        assert_eq!(duration(2 * DAY + 3 * HOUR), "2d 3h");
    }
}
