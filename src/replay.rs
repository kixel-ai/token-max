//! Claude cache-TTL checks, compaction summary charges, and the shared report every check joins.

use std::collections::{HashMap, HashSet};

use indexmap::{IndexMap, IndexSet};

use crate::background::{BackgroundInput, background_checks};
use crate::bash::{BashInput, bash_checks};
use crate::cache::{CacheBoundaries, observed_ttl};
use crate::compaction::{CompactionOptions, compaction_checks, js_number, skipped_lines};
use crate::context::{ContextInput, context_costs};
use crate::estimates::estimate_usage;
use crate::exposure::calibrate;
use crate::fast::{SpeedInput, speed_checks};
use crate::idle::{IdleOptions, idle_checks};
use crate::model::{
    Accounting, Action, Agent, Api, BillingPart, Bucket, CacheReplayEvidence, CheckBase,
    CheckResult, CompactionCharge, Cost, Coverage, EventKind, Evidence, ExposureSummary, Harness,
    HistoricalCost, Millis, PartKind, PerTtl, PriceSource, Profile, Report, Request,
    RequestEvidence, Scan, StructuralEvent, Ttl, TtlCheck, TtlSensitivity, UnlinkedSensitivity,
    Usage, UsageEstimate, WindowBasisKind,
};
use crate::prices::{
    Catalog, PRICE_SOURCES, add_costs, paired_savings, price_part, price_request, server_cost,
    unknown_cost, zero_cost,
};
use crate::safe::{ScanError, identity, iso};
use crate::scenarios::{AttemptScenario, ReplayOptions, ReplayTtls, cache_bucket, replay_parts};

const TTL_SOURCE: &str = "https://code.claude.com/docs/en/prompt-caching (2026-11-07)";
const MINUTE: Millis = 60_000;

fn in_bucket(r: &Request, p: &BillingPart, selected: Bucket) -> bool {
    cache_bucket(r, p).is_none_or(|b| b == selected)
}

/// Whether a Claude client version has TTL controls (v2.1.242 or later), read as JavaScript
/// `Number` integers.
fn version_supported(version: Option<&str>) -> bool {
    let Some(version) = version else {
        return false;
    };
    let parts: Vec<Option<f64>> = version.split('.').map(js_number).collect();
    let [Some(major), Some(minor), Some(patch)] = parts[..] else {
        return false;
    };
    if [major, minor, patch]
        .iter()
        .any(|v| !v.is_finite() || v.fract() != 0.0)
    {
        return false;
    }
    major > 2.0 || (major == 2.0 && (minor > 1.0 || (minor == 1.0 && patch >= 242.0)))
}

/// JavaScript's `toFixed(1)`, which rounds exact binary halves away from zero.
fn to_fixed1(value: f64) -> String {
    // An exact tie at one decimal is an odd multiple of 1/4; Rust would round it to even.
    let quarters = value.abs() * 4.0;
    if quarters.fract() == 0.0 && quarters % 2.0 == 1.0 {
        return format!("{:.1}", value + 0.05_f64.copysign(value));
    }
    format!("{value:.1}")
}

fn proxy_request(event: &StructuralEvent, previous: Option<&Request>) -> Option<Request> {
    let (Some(pre), Some(post)) = (event.pre_tokens, event.post_tokens) else {
        return None;
    };
    let (model, tier, speed, geo) = match (&event.pricing, previous.and_then(|r| r.parts.last())) {
        (Some(p), _) => (&p.model, &p.tier, &p.speed, &p.geo),
        (None, Some(p)) => (&p.model, &p.tier, &p.speed, &p.geo),
        (None, None) => return None,
    };
    Some(Request {
        evidence: RequestEvidence::Estimated,
        id: identity(&event.id),
        harness: event.harness,
        session: event.session.clone(),
        time: event.time,
        end_time: event.time,
        agent: Agent::Subagent,
        model: model.clone(),
        provider: event
            .pricing
            .as_ref()
            .and_then(|p| p.provider.clone())
            .or_else(|| previous.and_then(|r| r.provider.clone())),
        api: Api::Messages,
        source: Some("compaction".into()),
        trigger: event.trigger.clone(),
        reference: None,
        turn: None,
        version: event.version.clone(),
        client: event.client.clone(),
        cwd: event.cwd.clone(),
        profiles: event.profiles.clone(),
        locations: vec![event.location.clone()],
        accounting: Accounting::Individual,
        attempt: None,
        server_tools: Some(IndexMap::new()),
        requested_speed: None,
        parts: vec![BillingPart {
            model: model.clone(),
            tier: tier.clone(),
            speed: speed.clone(),
            geo: geo.clone(),
            kind: PartKind::Compaction,
            usage: Usage {
                input: Some(pre),
                output: Some(post),
                read: Some(0),
                write: Some(0),
                write5m: Some(0),
                write1h: Some(0),
                write30m: None,
                write12h: None,
                reasoning: None,
            },
        }],
        issues: vec![
            "summary model/tier/provider assumed from preceding metadata in its own file; public API equivalent if endpoint is unrecorded".into(),
        ],
    })
}

/// Pre-window context requests followed by selected requests, addressed by one index.
struct Requests<'s> {
    context: &'s mut [Request],
    selected: &'s mut [Request],
}

impl Requests<'_> {
    fn len(&self) -> usize {
        self.context.len() + self.selected.len()
    }
    fn get(&self, i: usize) -> &Request {
        match i.checked_sub(self.context.len()) {
            Some(j) => &self.selected[j],
            None => &self.context[i],
        }
    }
    fn get_mut(&mut self, i: usize) -> &mut Request {
        match i.checked_sub(self.context.len()) {
            Some(j) => &mut self.selected[j],
            None => &mut self.context[i],
        }
    }
}

/// One boundary's conversation aliases (as session groups) and explicitly linked request.
struct Plan {
    groups: Vec<usize>,
    explicit: Option<usize>,
}

fn has_compaction(r: &Request) -> bool {
    r.parts.iter().any(|p| p.kind == PartKind::Compaction)
}

/// Associate summaries with recorded usage or estimate each unmatched boundary once.
///
/// A linked request whose parts record no summary iteration is converted to compaction parts in
/// `scan`, so later checks attribute it to the outside-main bucket.
pub fn compaction_charges(scan: &mut Scan, catalog: &Catalog) -> Vec<CompactionCharge> {
    let Scan {
        requests,
        events,
        cache_context,
        ..
    } = scan;
    let mut all = Requests {
        context: &mut cache_context.requests,
        selected: requests,
    };
    let events: &[StructuralEvent] = events;
    let selected: HashSet<&str> = events.iter().map(|e| e.id.as_str()).collect();
    let mut boundaries: Vec<&StructuralEvent> = cache_context
        .boundaries
        .iter()
        .chain(events)
        .filter(|e| e.kind == EventKind::Compaction)
        .collect();
    boundaries.sort_by_key(|e| e.time);

    // Each request has one session and a unique id, so session groups never overlap and the
    // union of a boundary's alias groups needs no deduplication.
    let times: Vec<Millis> = (0..all.len()).map(|i| all.get(i).time).collect();
    let (groups, plans) = {
        let mut group_of: HashMap<&str, usize> = HashMap::new();
        let mut groups: Vec<Vec<usize>> = vec![];
        let mut by_id: HashMap<&str, usize> = HashMap::new();
        for i in 0..all.len() {
            let r = all.get(i);
            let g = *group_of.entry(r.session.as_str()).or_insert_with(|| {
                groups.push(vec![]);
                groups.len() - 1
            });
            groups[g].push(i);
            by_id.insert(r.id.as_str(), i);
        }
        // Stable, so equal times keep their order, as a stable sort of the merged aliases would.
        for group in &mut groups {
            group.sort_by_key(|&i| times[i]);
        }
        let plans: Vec<Plan> = boundaries
            .iter()
            .map(|e| {
                let mut aliases: IndexSet<usize> = IndexSet::new();
                for origin in &e.origins {
                    if let Some(&g) = group_of.get(origin.session.as_str()) {
                        aliases.insert(g);
                    }
                }
                Plan {
                    groups: aliases.into_iter().collect(),
                    explicit: e.request.as_deref().and_then(|id| by_id.get(id).copied()),
                }
            })
            .collect();
        (groups, plans)
    };

    let mut result = vec![];
    let mut claimed = vec![false; all.len()];
    // The latest boundary time in each session group; candidates must follow it.
    let mut last: Vec<Option<Millis>> = vec![None; groups.len()];
    for (e, plan) in boundaries.iter().zip(&plans) {
        // Requests at or before the boundary end each group's prefix.
        let ends: Vec<usize> = plan
            .groups
            .iter()
            .map(|&g| groups[g].partition_point(|&i| times[i] <= e.time))
            .collect();
        let mut linked = plan.explicit;
        let mut pairing = "explicit request identity";
        if linked.is_none() {
            let mut candidates = 0;
            let mut candidate = None;
            // Each group's range starts after its previous boundary, so the ranges scanned for
            // one session never overlap across boundaries.
            for (&g, &end) in plan.groups.iter().zip(&ends) {
                let group = &groups[g];
                let start = last[g].map_or(0, |t| group.partition_point(|&i| times[i] <= t));
                for &i in &group[start.min(end)..end] {
                    if has_compaction(all.get(i)) && !claimed[i] {
                        candidates += 1;
                        candidate = Some(i);
                    }
                }
            }
            if candidates == 1 {
                linked = candidate;
                pairing =
                    "estimated sequence attribution: one recorded summary since preceding boundary";
            }
        }
        for &g in &plan.groups {
            last[g] = Some(e.time);
        }
        let assumptions = [
            "boundary time and duration do not establish summary generation time",
            "precomputed does not prove idle compaction; no missing final boundary proves no summary",
        ];
        if let Some(i) = linked {
            let r = all.get_mut(i);
            if !has_compaction(r) {
                for p in &mut r.parts {
                    p.kind = PartKind::Compaction;
                }
            }
            let r = all.get(i);
            let cost = if claimed[i] {
                zero_cost()
            } else {
                let costs: Vec<Cost> = r
                    .parts
                    .iter()
                    .filter(|p| p.kind == PartKind::Compaction)
                    .map(|p| price_part(catalog, r, p))
                    .collect();
                add_costs(&costs)
            };
            claimed[i] = true;
            result.push(CompactionCharge {
                event: e.id.clone(),
                request: Some(r.id.clone()),
                evidence: Evidence::Measured,
                cost,
                ttl_cost: PerTtl {
                    five_minutes: zero_cost(),
                    one_hour: zero_cost(),
                },
                assumptions: assumptions
                    .into_iter()
                    .chain([
                        pairing,
                        "recorded usage already included in request costs; linked costs allocated to the first boundary only, do not add again",
                    ])
                    .map(String::from)
                    .collect(),
            });
            continue;
        }
        // The latest request at or before the boundary; on equal times the later alias wins,
        // as the last entry of a stable time sort over the aliases in order.
        let mut preceding: Option<usize> = None;
        for (&g, &end) in plan.groups.iter().zip(&ends) {
            if let Some(&i) = end.checked_sub(1).map(|j| &groups[g][j])
                && preceding.is_none_or(|p| times[i] >= times[p])
            {
                preceding = Some(i);
            }
        }
        let proxy = if e.harness == Harness::Claude {
            proxy_request(e, preceding.map(|i| all.get(i)))
        } else {
            None
        };
        let cost = match &proxy {
            Some(proxy) => price_request(catalog, proxy),
            None => unknown_cost("summary usage/model or preTokens/postTokens proxy unavailable"),
        };
        result.push(CompactionCharge {
            event: e.id.clone(),
            request: None,
            evidence: if proxy.is_some() {
                Evidence::Estimated
            } else {
                Evidence::Unknown
            },
            ttl_cost: PerTtl {
                five_minutes: cost.clone(),
                one_hour: cost.clone(),
            },
            cost,
            assumptions: assumptions
                .into_iter()
                .chain([
                    "preTokens charged at ordinary input; postTokens is a summary-output proxy, not the full resumed context",
                    "warm cache reads can lower input cost; cold writes can raise it up to 2x input before unrecorded system/tool/instruction overhead",
                    "no cache-write split or generation timestamp: the same summary proxy is included in both outside-main TTL scenarios",
                ])
                .map(String::from)
                .collect(),
        });
    }
    result.retain(|charge| selected.contains(charge.event.as_str()));
    result
}

/// The default window start: the latest observed write-TTL transition, unless cohorts disagree.
struct Window {
    change: Option<Millis>,
    mixed: bool,
}

fn default_since(requests: &[&Request], selected: Bucket) -> Window {
    let mut cohorts: HashMap<String, (Ttl, Option<Millis>)> = HashMap::new();
    let mut ambiguous = false;
    for r in requests {
        for p in &r.parts {
            if cache_bucket(r, p) != Some(selected) {
                continue;
            }
            let Some(ttl) = observed_ttl(p) else {
                continue;
            };
            // Sources/agent kinds can legitimately have different policies concurrently.
            // Missing source cannot establish a transition within the outside-main bucket.
            if selected == Bucket::Subagent && r.source.is_none() {
                ambiguous = true;
            }
            let agent = match r.agent {
                Agent::Main => "main",
                Agent::Subagent => "subagent",
                Agent::Unknown => "unknown",
            };
            let kind = match p.kind {
                PartKind::Message => "message",
                PartKind::Compaction => "compaction",
                PartKind::Advisor => "advisor",
                PartKind::Unknown => "unknown",
            };
            let key = format!(
                "{}:{agent}:{kind}:{}",
                r.source.as_deref().unwrap_or(""),
                p.model.as_deref().unwrap_or("")
            );
            let change = match cohorts.get(&key) {
                Some(&(previous, _)) if previous != ttl => Some(r.time),
                Some(&(_, change)) => change,
                None => None,
            };
            cohorts.insert(key, (ttl, change));
        }
    }
    let ttls: HashSet<Ttl> = cohorts.values().map(|&(ttl, _)| ttl).collect();
    if ttls.len() > 1 || ambiguous {
        return Window {
            change: None,
            mixed: true,
        };
    }
    Window {
        change: cohorts.values().filter_map(|&(_, change)| change).max(),
        mixed: false,
    }
}

/// One cache replay of a profile's requests, restricted to the checked bucket's selected parts.
struct Replay<'a> {
    comparison: Cost,
    /// Cache cost per selected request part, keyed by request id and part index.
    items: IndexMap<(&'a str, usize), Cost>,
    fixed: Cost,
    predecessors: HashSet<String>,
    evidence: Vec<CacheReplayEvidence>,
}

/// Shared inputs for every TTL check in one report.
struct TtlInput<'a> {
    scan: &'a Scan,
    catalog: &'a Catalog,
    /// Every pre-window and selected boundary, indexed once for all replays.
    boundaries: &'a CacheBoundaries,
    summaries: &'a [CompactionCharge],
    estimates: &'a IndexMap<String, UsageEstimate>,
    since: Option<Millis>,
    now: Millis,
}

/// Replays one profile's time-ordered Claude requests for one bucket and window.
struct Replayer<'i, 'a> {
    input: &'i TtlInput<'a>,
    requests: &'i [&'a Request],
    bucket: Bucket,
    since: Millis,
}

impl<'a> Replayer<'_, 'a> {
    fn run(&self, ttl: Ttl, cold_prefixes: bool, attempt_scenario: AttemptScenario) -> Replay<'a> {
        let options = ReplayOptions {
            estimates: self.input.estimates,
            catalog: self.input.catalog,
            ttls: ReplayTtls::all(Some(ttl)),
            since: self.since,
            cold_prefixes,
            estimated_parts: None,
            attempt_scenario,
        };
        let mut predecessors = HashSet::new();
        let mut evidence = vec![];
        let mut items = IndexMap::new();
        let mut fixed = vec![];
        let replayed = replay_parts(
            self.requests.iter().copied(),
            [self.input.boundaries],
            &options,
        );
        for item in replayed {
            if !item.selected || !in_bucket(item.request, &item.p, self.bucket) {
                continue;
            }
            if let Some(e) = item.evidence {
                for predecessor in &e.predecessors {
                    predecessors.insert(predecessor.request.clone());
                }
                evidence.push(e);
            }
            // Replay clones each part, so only its recorded index identifies it.
            items.insert((item.request.id.as_str(), item.part), item.components.cache);
            fixed.push(item.components.ordinary);
            fixed.push(item.components.output);
        }
        Replay {
            comparison: add_costs(items.values()),
            items,
            fixed: add_costs(&fixed),
            predecessors,
            evidence,
        }
    }
}

/// Compare 5m and 1h cache lifetimes for one profile bucket over its time-ordered requests.
fn ttl_check(
    input: &TtlInput,
    profile: &Profile,
    requests: &[&Request],
    bucket: Bucket,
) -> Option<TtlCheck> {
    let TtlInput {
        scan,
        summaries,
        estimates,
        now,
        ..
    } = *input;
    let relevant: Vec<&Request> = requests
        .iter()
        .copied()
        .filter(|r| r.parts.iter().any(|p| in_bucket(r, p, bucket)))
        .collect();
    let boundaries: Vec<&StructuralEvent> = if bucket == Bucket::Subagent {
        scan.events
            .iter()
            .filter(|e| {
                e.harness == Harness::Claude
                    && e.kind == EventKind::Compaction
                    && e.profiles.contains(&profile.id)
            })
            .collect()
    } else {
        vec![]
    };
    if relevant.is_empty() && boundaries.is_empty() {
        return None;
    }
    let window = match input.since {
        None => default_since(requests, bucket),
        Some(_) => Window {
            change: None,
            mixed: false,
        },
    };
    let change = window.change;
    let since = input.since.or(change).unwrap_or_else(|| {
        relevant
            .iter()
            .map(|r| r.time)
            .chain(boundaries.iter().map(|e| e.time))
            .min()
            .expect("a request or boundary")
    });
    let selected: Vec<&Request> = relevant
        .iter()
        .copied()
        .filter(|r| r.time >= since)
        .collect();
    if selected.is_empty() && !boundaries.iter().any(|e| e.time >= since) {
        return None;
    }
    let summary_ids: HashSet<&str> = boundaries
        .iter()
        .filter(|e| e.time >= since)
        .map(|e| e.id.as_str())
        .collect();
    let unmatched: Vec<&CompactionCharge> = summaries
        .iter()
        .filter(|s| summary_ids.contains(s.event.as_str()) && s.request.is_none())
        .collect();
    let summary_cost = PerTtl {
        five_minutes: add_costs(unmatched.iter().map(|s| &s.ttl_cost.five_minutes)),
        one_hour: add_costs(unmatched.iter().map(|s| &s.ttl_cost.one_hour)),
    };
    let replayer = Replayer {
        input,
        requests,
        bucket,
        since,
    };
    let replay = |ttl, attempt| replayer.run(ttl, false, attempt);
    let short = replay(Ttl::FiveMinutes, AttemptScenario::Point);
    let long = replay(Ttl::OneHour, AttemptScenario::Point);
    let comparison_cost = PerTtl {
        five_minutes: short.comparison.clone(),
        one_hour: long.comparison.clone(),
    };
    let servers: Vec<Cost> = selected.iter().map(|r| server_cost(r)).collect();
    let fixed_cost = add_costs(
        [&short.fixed]
            .into_iter()
            .chain(&servers)
            .chain([&summary_cost.five_minutes]),
    );
    let cost = PerTtl {
        five_minutes: add_costs([&comparison_cost.five_minutes, &fixed_cost]),
        one_hour: add_costs([&comparison_cost.one_hour, &fixed_cost]),
    };
    let cold_comparison = replayer
        .run(Ttl::OneHour, true, AttemptScenario::Point)
        .comparison;
    let cold = add_costs([&cold_comparison, &fixed_cost]);
    let predecessor =
        |r: &Request| short.predecessors.contains(&r.id) || long.predecessors.contains(&r.id);
    let usage_estimates: Vec<UsageEstimate> = requests
        .iter()
        .filter(|r| {
            (r.time >= since && r.parts.iter().any(|p| in_bucket(r, p, bucket))) || predecessor(r)
        })
        .filter_map(|r| estimates.get(&r.id).cloned())
        .collect();
    let unlinked = usage_estimates
        .iter()
        .any(|e| e.part.is_some())
        .then(|| UnlinkedSensitivity {
            no_billed_attempt: PerTtl {
                five_minutes: replay(Ttl::FiveMinutes, AttemptScenario::NoBilledAttempt).comparison,
                one_hour: replay(Ttl::OneHour, AttemptScenario::NoBilledAttempt).comparison,
            },
            cold_cache: PerTtl {
                five_minutes: replay(Ttl::FiveMinutes, AttemptScenario::ColdCache).comparison,
                one_hour: replay(Ttl::OneHour, AttemptScenario::ColdCache).comparison,
            },
        });
    let benefit = match (
        cold_comparison.total_usd,
        comparison_cost.one_hour.total_usd,
    ) {
        (Some(cold), Some(hour)) => Some(cold - hour),
        _ => None,
    };
    let break_even = match (
        benefit,
        cold_comparison.total_usd,
        comparison_cost.five_minutes.total_usd,
    ) {
        (Some(benefit), Some(cold), Some(short)) if benefit > 0.0 => Some((cold - short) / benefit),
        _ => None,
    };
    let current = profile.ttl.get(bucket);
    // Positive when 1h is cheaper than 5m on the parts both replays price.
    let not_replayed = unknown_cost("part not replayed");
    let paired = paired_savings(
        short
            .items
            .iter()
            .map(|(key, baseline)| (baseline, long.items.get(key).unwrap_or(&not_replayed))),
        skipped_lines(scan, &profile.id),
    );
    let recommended = if !paired.decided {
        None
    } else if paired.usd > 0.0 {
        Some(Ttl::OneHour)
    } else if paired.usd < 0.0 {
        Some(Ttl::FiveMinutes)
    } else {
        current.value
    };
    let recent_version = relevant
        .last()
        .and_then(|r| r.version.as_deref())
        .or_else(|| requests.last().and_then(|r| r.version.as_deref()));
    let supported = version_supported(recent_version);
    let applies =
        supported && current.value.is_some() && current.reason != "prompt caching is disabled";
    let action = match recommended {
        Some(recommended) if applies => {
            if Some(recommended) != current.value {
                Action::Change
            } else {
                Action::Keep
            }
        }
        _ => Action::Advisory,
    };
    let savings = match (current.value, recommended) {
        (Some(current), Some(recommended)) if current == recommended => Some(0.0),
        (Some(Ttl::FiveMinutes), Some(_)) => Some(paired.usd),
        (Some(_), Some(_)) => Some(-paired.usd),
        _ => None,
    };
    let profiles: IndexSet<&str> = relevant
        .iter()
        .flat_map(|r| &r.profiles)
        .chain(boundaries.iter().flat_map(|e| &e.profiles))
        .map(String::as_str)
        .collect();
    let mut uncertainty: IndexSet<&str> = [
        "gap replay assumes the same requests, stable prefixes, and no extra work; shared caches and invalidations can change the outcome",
        "unknown source identifiers are never inferred from idle time",
        "unknown part/agent attribution is conditional in both possible buckets; per-check request counts and fixed costs are not additive",
        "current profile settings are applied to history; project/CLI settings and agent frontmatter may override user defaults",
        "unknown client lifetime and unlogged discarded summaries are not measured zero",
    ]
    .into_iter()
    .collect();
    if !supported {
        uncertainty.insert(
            "current supported client version is not established; recorded latest version is absent or predates TTL controls",
        );
    }
    if current.value.is_none() {
        uncertainty.insert(&current.reason);
    }
    if profiles.len() > 1 {
        uncertainty.insert(
            "shared history has ambiguous profile provenance; this is a conditional replay for the named current profile, not assigned historical spend",
        );
    }
    for estimate in &usage_estimates {
        uncertainty.insert(&estimate.reason);
        uncertainty.extend(estimate.assumptions.iter().map(String::as_str));
    }
    uncertainty.extend(selected.iter().flat_map(|r| &r.issues).map(String::as_str));
    uncertainty.extend(
        requests
            .iter()
            .filter(|r| predecessor(r))
            .flat_map(|r| &r.issues)
            .map(String::as_str),
    );
    uncertainty.extend(short.evidence.iter().map(|e| e.reason.as_str()));
    uncertainty.extend(long.evidence.iter().map(|e| e.reason.as_str()));
    uncertainty.extend(cost.five_minutes.unknown.iter().map(String::as_str));
    uncertainty.extend(cost.one_hour.unknown.iter().map(String::as_str));
    let uncertainty: Vec<String> = uncertainty.into_iter().map(String::from).collect();
    Some(TtlCheck {
        base: CheckBase {
            profile: profile.id.clone(),
            history_profiles: profiles.into_iter().map(String::from).collect(),
            evidence: if recommended.is_some() {
                Evidence::Estimated
            } else {
                Evidence::Unknown
            },
            action,
            current: current.value,
            recommended,
            owner: current.owner.clone(),
            since: Some(since),
            until: Some(now),
            window_basis: if input.since.is_some() {
                "explicit --since; current settings applied to history"
            } else if change.is_some() {
                "since latest observed write-TTL transition; not proof of a setting change"
            } else if window.mixed {
                "available history; mixed agent/source TTLs do not establish a shared setting transition"
            } else {
                "available history; no recorded setting change, current settings applied to history"
            }
            .into(),
            baseline: format!("current {} TTL for {}", bucket.as_str(), profile.root),
            requests: selected.len() as u64,
            coverage: Some(Coverage {
                priced: paired.priced,
                total: paired.total,
            }),
            savings_usd: savings,
            monthly_savings_usd: savings
                .filter(|_| now > since)
                .map(|s| s * 30.0 * 24.0 * 60.0 * MINUTE as f64 / (now - since) as f64),
            break_even: break_even.map(|b| {
                format!(
                    "1h breaks even at {}% of modeled warm-prefix reuse in 5-60 minute gaps",
                    to_fixed1(b * 100.0)
                )
            }),
            assumptions: [
                current.reason.as_str(),
                "latest recorded client version is assumed to remain applicable to current profile controls",
                "1h writes use documented 2x input; first visible request keeps observed reads from prefixes warmed before the window",
                "reuse conversions are limited to the previously cached prefix; new input stays a write",
                "ordinary input, output, server fees and unmatched summary proxies are held identical under fixed behavior and cancel; comparisonCost contains cache charges only, even when fixedCost is unknown",
                "unlinked-attempt estimates are comparison-only; original usage/coverage and historical spend remain unknown, never excluded or assigned zero billing",
                "cache evidence retains known, estimated and unknown predecessor witnesses, including pre-window attempts and possible cohorts with missing metadata; request counts and charges cover only the selected window",
                "per-check savings share baselines and must not be summed",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            uncertainty,
            sources: [TTL_SOURCE]
                .into_iter()
                .chain(PRICE_SOURCES)
                .map(String::from)
                .collect(),
        },
        window_basis_kind: if input.since.is_some() {
            WindowBasisKind::Explicit
        } else if change.is_some() {
            WindowBasisKind::TtlTransition
        } else {
            WindowBasisKind::Available
        },
        bucket,
        one_hour_savings_usd: paired.usd,
        cost,
        summary_cost,
        comparison_cost,
        fixed_cost,
        usage_estimates,
        cache_evidence: short.evidence.into_iter().chain(long.evidence).collect(),
        sensitivity: TtlSensitivity {
            cold_prefix1h_usd: cold.total_usd,
            cold_prefix1h_comparison_usd: cold_comparison.total_usd,
            warm_reuse_break_even_fraction: break_even,
            unlinked,
        },
    })
}

/// Inputs to one report.
pub struct AnalyzeOptions<'a> {
    /// The scanned histories; the report keeps them, with linked summaries marked as compaction.
    pub scan: Scan,
    /// Public prices.
    pub catalog: &'a Catalog,
    /// The comparison window start; defaults to the scan's billing window.
    pub since: Option<Millis>,
    /// The window end, which monthly values scale to.
    pub now: Millis,
    /// Recommend turning off context that cost more than this per use.
    pub max_cost_per_use_usd: f64,
}

/// Build the shared report without summing savings from independent checks.
pub fn analyze(options: AnalyzeOptions) -> Result<Report, ScanError> {
    let AnalyzeOptions {
        mut scan,
        catalog,
        since,
        now,
        max_cost_per_use_usd,
    } = options;
    if let (Some(scanned), Some(since)) = (scan.since, since)
        && since < scanned
    {
        return Err(ScanError::new(
            "comparison window predates the scanned billing window",
        ));
    }
    let since = since.or(scan.since);
    let compactions = compaction_charges(&mut scan, catalog);
    let scan = scan;
    let estimates = estimate_usage(&scan);
    let mut groups: IndexMap<String, Vec<&Request>> = IndexMap::new();
    for r in &scan.requests {
        let mut profiles: Vec<&str> = r.profiles.iter().map(String::as_str).collect();
        // JavaScript's default sort compares UTF-16 code units.
        profiles.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
        groups.entry(profiles.join(":")).or_default().push(r);
    }
    let historical = groups
        .into_iter()
        .map(|(key, requests)| {
            let costs: Vec<Cost> = requests.iter().map(|r| price_request(catalog, r)).collect();
            HistoricalCost {
                bucket: if requests[0].profiles.len() > 1 {
                    format!("shared:{key}")
                } else {
                    key
                },
                profiles: requests[0].profiles.clone(),
                requests: requests.len() as u64,
                evidence: "estimated",
                cost: add_costs(&costs),
            }
        })
        .collect();
    let boundaries = CacheBoundaries::new(scan.cache_context.boundaries.iter().chain(&scan.events));
    let input = TtlInput {
        scan: &scan,
        catalog,
        boundaries: &boundaries,
        summaries: &compactions,
        estimates: &estimates,
        since,
        now,
    };
    let mut ttls = vec![];
    for profile in scan
        .profiles
        .iter()
        .filter(|p| p.harness == Harness::Claude)
    {
        let mut requests: Vec<&Request> = scan
            .cache_context
            .requests
            .iter()
            .chain(&scan.requests)
            .filter(|r| r.harness == Harness::Claude && r.profiles.contains(&profile.id))
            .collect();
        requests.sort_by_key(|r| r.time);
        for bucket in Bucket::ALL {
            ttls.extend(ttl_check(&input, profile, &requests, bucket));
        }
    }
    let ttl_refs: Vec<&TtlCheck> = ttls.iter().collect();
    let windows = compaction_checks(&CompactionOptions {
        scan: &scan,
        catalog,
        summaries: &compactions,
        estimates: &estimates,
        ttls: &ttl_refs,
        since,
        now,
    });
    let window_refs: Vec<_> = windows.iter().collect();
    let idle = idle_checks(&IdleOptions {
        scan: &scan,
        catalog,
        summaries: &compactions,
        estimates: &estimates,
        ttls: &ttl_refs,
        windows: &window_refs,
        since,
        now,
    });
    let speed = speed_checks(&SpeedInput {
        scan: &scan,
        catalog,
        since,
        now,
    });
    let background = background_checks(&BackgroundInput {
        scan: &scan,
        catalog,
        since,
        now,
        max_cost_per_use_usd,
    });
    let bash = bash_checks(&BashInput {
        scan: &scan,
        catalog,
        ttls: &ttl_refs,
        since,
        now,
    });
    let checks = ttls
        .into_iter()
        .map(CheckResult::Ttl)
        .chain(windows.into_iter().map(CheckResult::Compaction))
        .chain(idle.into_iter().map(CheckResult::Idle))
        .chain(speed.into_iter().map(CheckResult::Speed))
        .chain(background.into_iter().map(CheckResult::Background))
        .chain(bash.into_iter().map(CheckResult::BashCap))
        .collect();
    let exposure = ExposureSummary {
        calibration: calibrate(&scan.exposure, &scan.requests),
        assumptions: [
            "component tokens are characters times a per-model ratio calibrated against recorded request totals",
            "Codex records no complete request text to calibrate against, so its components and tool output use OpenAI's rule of thumb of 4 characters per token",
            "each component is priced at the cache state of its position in the prompt prefix: tools, system, then message attachments in arrival order",
            "tool output stays in the conversation until it compacts; output the previous request held lies within its content, and newer output follows its input and reply",
            "Claude Code caches through the last message, so a Claude request's content ends at its cached reads plus writes",
            "removing a component is assumed to change nothing else; costs of different components can be added only as an upper bound on the saving",
        ]
        .into_iter()
        .map(String::from)
        .collect(),
    };
    let context = context_costs(&ContextInput {
        scan: &scan.exposure,
        requests: &scan.requests,
        profiles: &scan.profiles,
        catalog,
        max_cost_per_use_usd,
    });
    Ok(Report {
        schema_version: 1,
        generated_at: iso(now),
        prices: PriceSource {
            source: catalog.source.clone(),
            fetched_at: catalog.fetched_at.clone(),
            offline: catalog.offline,
        },
        historical,
        compactions,
        exposure,
        context,
        max_cost_per_use_usd,
        checks,
        assumptions: [
            "host-local read-only analysis; network access is only for public prices",
            "measured usage is replayed at present public API rates, not historical invoices or subscription savings",
            "compaction summary proxies are separate from recorded request charges",
            "unlogged or unresumed summary generation is unknown; a missing final boundary does not establish zero cost",
            "Claude prompt suggestions and session recaps, and Codex automatic recaps, are not logged; every other check's recorded baseline omits them and their cache refreshes",
            "Codex has no user cache-TTL control; its usage is priced but no TTL change is recommended",
            "no summed per-check or joint savings total",
        ]
        .into_iter()
        .map(String::from)
        .collect(),
        scan,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        Attempt, CacheContext, EstimateEvidence, ExposureScan, IdleAttribution, KnownState,
        Location, Origin, PrefixEvidence, ReplayOutcome, ScanCoverage, Snapshot,
    };
    use crate::reader::{ScanOptions, scan, usage};
    use crate::report::{ReportOptions, format_report};
    use crate::test_support::{
        ClaudeUsage, Workspace, claude_usage, make_profile, make_request, prices,
    };
    use serde_json::{Value, json};

    const MIN: Millis = 60_000;
    /// A fixed report time for tests whose assertions do not depend on it.
    const NOW: Millis = 1_800_000_000_000;
    const M5: Ttl = Ttl::FiveMinutes;
    const H1: Ttl = Ttl::OneHour;
    const UNLINKED: &str = "api request has no linked assistant usage; failed/in-flight/hidden request billing is unknown";

    fn data(requests: Vec<Request>, events: Vec<StructuralEvent>) -> Scan {
        let mut profile = make_profile("profile", "synthetic");
        profile.aliases = vec!["synthetic".into()];
        profile.histories = vec![];
        for (bucket, key) in [
            (Bucket::Main, "promptCacheTtl"),
            (Bucket::Subagent, "subagentPromptCacheTtl"),
        ] {
            let ttl = profile.ttl.get_mut(bucket);
            ttl.owner = format!("settings.json:{key}");
            ttl.reason = "current user setting".into();
        }
        let window = &mut profile.compaction.default;
        window.owner = "synthetic".into();
        window.reason = "unknown".into();
        window.applicable = false;
        let n = requests.len() as u64;
        Scan {
            snapshot_started_at: iso(0),
            snapshot_finished_at: iso(0),
            since: None,
            roots: vec![],
            profiles: vec![profile],
            snapshots: vec![],
            requests,
            events,
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

    /// The report with only its TTL checks, so rendered details cover only what these tests vary.
    fn report_at(scan: &Scan, since: Option<Millis>, now: Millis) -> Report {
        let mut report = analyze(AnalyzeOptions {
            scan: scan.clone(),
            catalog: &prices(),
            since,
            now,
            max_cost_per_use_usd: 1.0,
        })
        .unwrap();
        report.checks.retain(|c| matches!(c, CheckResult::Ttl(_)));
        report
    }

    fn ttls(report: &Report) -> Vec<&TtlCheck> {
        report
            .checks
            .iter()
            .filter_map(|c| match c {
                CheckResult::Ttl(c) => Some(c),
                _ => None,
            })
            .collect()
    }

    /// The first TTL check of a report.
    fn check(scan: &Scan, since: Option<Millis>, now: Millis) -> TtlCheck {
        ttls(&report_at(scan, since, now))[0].clone()
    }

    fn bucket_check(report: &Report, bucket: Bucket) -> &TtlCheck {
        ttls(report)
            .into_iter()
            .find(|c| c.bucket == bucket)
            .unwrap()
    }

    fn at(id: &str, time: Millis) -> Request {
        Request {
            id: id.into(),
            time,
            end_time: time,
            ..make_request()
        }
    }

    fn with(mut r: Request, u: ClaudeUsage) -> Request {
        r.parts[0].usage = claude_usage(u);
        r
    }

    fn u(write: i64, read: i64, ttl: Ttl) -> ClaudeUsage {
        ClaudeUsage {
            write,
            read,
            ttl,
            ..ClaudeUsage::default()
        }
    }

    fn total(cost: &Cost) -> Option<f64> {
        cost.total_usd
    }

    fn retime(r: &mut Request, time: Millis) {
        r.time = time;
        r.end_time = time;
    }

    fn empty_usage() -> Usage {
        usage(&json!({}), Harness::Claude).unwrap()
    }

    fn boundary(id: &str, session: &str, time: Millis) -> StructuralEvent {
        let location = Location {
            file: "synthetic".into(),
            line: 1,
        };
        StructuralEvent {
            id: id.into(),
            origins: vec![Origin {
                session: session.into(),
                location: location.clone(),
            }],
            harness: Harness::Claude,
            session: session.into(),
            time,
            kind: EventKind::Compaction,
            profiles: vec!["profile".into()],
            location,
            request: None,
            model: None,
            version: Some("2.1.314".into()),
            client: None,
            cwd: None,
            trigger: Some("auto".into()),
            precomputed: Some(false),
            pre_tokens: None,
            post_tokens: None,
            duration_ms: None,
            tool: None,
            tool_id: None,
            settings: IndexMap::new(),
            idle_attribution: IdleAttribution::Unknown,
            shell: None,
            pricing: None,
        }
    }

    fn details(report: &Report) -> String {
        format_report(
            report,
            ReportOptions {
                width: None,
                color: false,
                unicode: false,
                details: true,
                host: None,
            },
        )
    }

    fn options(w: &Workspace, since: Option<Millis>) -> ScanOptions {
        ScanOptions {
            since,
            ..w.scan_options()
        }
    }

    fn day(text: &str) -> Millis {
        format!("{text}T00:00:00Z")
            .parse::<jiff::Timestamp>()
            .unwrap()
            .as_millisecond()
    }

    fn jsonl(records: &[Value]) -> String {
        records.iter().map(|r| format!("{r}\n")).collect()
    }

    #[test]
    fn ttl_compares_aggregate_net_cost_in_either_direction() {
        let first = make_request();
        let next = with(at("next", 600_000), u(1100, 0, M5));
        let s = data(vec![first.clone(), next.clone()], vec![]);
        let c = check(&s, None, 1_200_000);
        assert_eq!(c.base.recommended, Some(H1));
        assert_eq!(c.base.action, Action::Change);
        let expected = (1100.0 * 8.0 + 1000.0 * 0.2 + 20.0 * 20.0) / 1e6;
        assert!((total(&c.cost.one_hour).unwrap() - expected).abs() < 1e-10);
        assert!(c.base.savings_usd.unwrap() > 0.0);
        assert!(c.base.monthly_savings_usd.unwrap() > 0.0);
        let fraction = c.sensitivity.warm_reuse_break_even_fraction.unwrap();
        assert!(fraction > 0.0 && fraction < 1.0);
        assert!(c.sensitivity.cold_prefix1h_usd.unwrap() > total(&c.cost.five_minutes).unwrap());
        let mut next = next;
        retime(&mut next, 60_000);
        let mut burst = data(vec![first, next], vec![]);
        burst.profiles[0].ttl.main.value = Some(H1);
        let other = check(&burst, None, 1_200_000);
        assert_eq!(other.base.recommended, Some(M5));
        assert_eq!(other.base.action, Action::Change);
        let tie = data(vec![with(make_request(), u(0, 1000, M5))], vec![]);
        assert_eq!(check(&tie, None, 1).base.action, Action::Keep);
    }

    #[test]
    fn each_iteration_keeps_its_own_replayed_cache_cost() {
        // A write followed by a read of it in the same request: 1h only makes the write dearer.
        let mut r = make_request();
        let part = r.parts[0].clone();
        r.parts = vec![
            BillingPart {
                usage: claude_usage(u(1000, 0, M5)),
                ..part.clone()
            },
            BillingPart {
                usage: claude_usage(u(0, 1000, M5)),
                ..part
            },
        ];
        let mut s = data(vec![r], vec![]);
        s.profiles[0].ttl.main.value = Some(H1);
        let c = check(&s, None, 1);
        assert_eq!(c.base.recommended, Some(M5));
        assert_eq!(c.base.action, Action::Change);
        assert!((c.base.savings_usd.unwrap() - 1000.0 * (8.0 - 5.0) / 1e6).abs() < 1e-10);
    }

    #[test]
    fn unreadable_lines_can_block_a_recommendation() {
        let next = with(at("next", 600_000), u(1100, 0, M5));
        let mut s = data(vec![make_request(), next], vec![]);
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
        let c = check(&s, None, 1_200_000);
        assert_eq!(c.base.recommended, None);
        assert_eq!(c.base.action, Action::Advisory);
        let coverage = c.base.coverage.unwrap();
        assert_eq!(coverage.total, coverage.priced + 10000);
    }

    #[test]
    fn five_minutes_rewrites_a_late_read_and_sessions_do_not_share_cache() {
        let first = with(make_request(), u(1000, 0, H1));
        let read = with(at("read", 600_000), u(100, 1000, H1));
        let result = check(&data(vec![first, read], vec![]), None, 1_200_000);
        assert_eq!(result.base.recommended, Some(H1));
        assert!(total(&result.cost.five_minutes).unwrap() > total(&result.cost.one_hour).unwrap());
        let unshared = Request {
            session: "different".into(),
            ..at("separate", 600_000)
        };
        let c = check(
            &data(vec![make_request(), unshared], vec![]),
            None,
            1_200_000,
        );
        assert_eq!(c.base.recommended, Some(M5));
    }

    #[test]
    fn a_large_returned_period_decides_the_aggregate() {
        let mut requests: Vec<Request> = (0..4)
            .map(|i| {
                let r = Request {
                    id: format!("small-{i}"),
                    session: format!("small-{i}"),
                    ..make_request()
                };
                with(r, u(10, 0, M5))
            })
            .collect();
        requests.push(with(at("big1", 0), u(1_000_000, 0, M5)));
        requests.push(with(at("big2", 600_000), u(1_000_000, 0, M5)));
        let c = check(&data(requests, vec![]), None, 1_200_000);
        assert_eq!(c.base.recommended, Some(H1));
    }

    #[test]
    fn unknown_billing_or_version_stays_advisory() {
        let mut s = data(vec![make_request()], vec![]);
        s.profiles[0].ttl.main.value = None;
        assert_eq!(check(&s, None, 100).base.action, Action::Advisory);
        s.profiles[0].ttl.main.value = Some(H1);
        s.requests[0].version = Some("2.1.100".into());
        assert_eq!(check(&s, None, 100).base.action, Action::Advisory);
        s.requests[0].parts[0].model = Some("private-internal".into());
        let result = check(&s, None, 100);
        assert_eq!(result.base.recommended, None);
        assert_eq!(result.base.evidence, Evidence::Unknown);
    }

    #[test]
    fn unmatched_summary_proxies_are_charged_once_outside_main() {
        let w = Workspace::new();
        w.fixture(w.claude.join("projects/project/main.jsonl"), "claude.jsonl");
        let s = scan(&options(&w, None)).unwrap();
        let r = report_at(&s, None, day("2026-11-02"));
        assert_eq!(r.scan.requests.len(), 2);
        assert_eq!(r.compactions.len(), 1);
        assert_eq!(r.compactions[0].evidence, Evidence::Estimated);
        assert_eq!(total(&r.compactions[0].cost), Some(0.0064));
        let main = bucket_check(&r, Bucket::Main);
        let sub = bucket_check(&r, Bucket::Subagent);
        assert_eq!(total(&main.summary_cost.five_minutes), Some(0.0));
        assert_eq!(total(&sub.summary_cost.five_minutes), Some(0.0064));
        assert_eq!(total(&sub.cost.five_minutes), Some(0.0064));
        assert!(
            r.compactions[0]
                .assumptions
                .join(" ")
                .contains("generation time")
        );
    }

    #[test]
    fn compaction_and_advisor_iterations_replace_top_level_usage() {
        let w = Workspace::new();
        let u = json!({
            "input_tokens": 0,
            "output_tokens": 10,
            "cache_read_input_tokens": 0,
            "cache_creation_input_tokens": 100,
            "cache_creation": {"ephemeral_5m_input_tokens": 100, "ephemeral_1h_input_tokens": 0},
            "service_tier": "standard",
            "speed": "standard",
        });
        let iteration = |kind: &str, model: Option<&str>| {
            let mut i = u.clone();
            i["type"] = json!(kind);
            if let Some(model) = model {
                i["model"] = json!(model);
            }
            i
        };
        let mut top = u.clone();
        top["inference_geo"] = json!("global");
        top["server_tool_use"] = json!({});
        top["iterations"] = json!([
            iteration("message", None),
            iteration("compaction", None),
            iteration("advisor_message", Some("claude-haiku-5-5")),
        ]);
        let records = [
            json!({
                "type": "assistant",
                "sessionId": "s",
                "timestamp": "2026-11-01T00:00:00Z",
                "version": "2.1.314",
                "requestRef": "summary-api",
                "message": {"id": "m", "model": "claude-opus-5-5", "usage": top},
            }),
            json!({
                "type": "system",
                "subtype": "compact_boundary",
                "uuid": "b",
                "sessionId": "s",
                "timestamp": "2026-11-01T00:00:01Z",
                "requestRef": "summary-api",
                "compactMetadata": {"preTokens": 1000, "postTokens": 100},
            }),
        ];
        std::fs::write(
            w.claude.join("projects/project/main.jsonl"),
            jsonl(&records),
        )
        .unwrap();
        let s = scan(&options(&w, None)).unwrap();
        assert_eq!(s.requests[0].parts.len(), 3);
        let r = report_at(&s, None, day("2026-11-02"));
        assert_eq!(r.compactions[0].request.as_ref(), Some(&s.requests[0].id));
        assert_eq!(r.compactions[0].evidence, Evidence::Measured);
        assert_eq!(total(&r.compactions[0].ttl_cost.five_minutes), Some(0.0));
        let sub = bucket_check(&r, Bucket::Subagent);
        assert_eq!(total(&sub.summary_cost.five_minutes), Some(0.0));
        assert!(total(&r.historical[0].cost).unwrap() < 0.002);
    }

    #[test]
    fn default_windows_follow_observed_ttl_changes_unless_explicit() {
        let changed = with(at("change", 600_000), u(1000, 0, H1));
        let s = data(vec![make_request(), changed], vec![]);
        assert_eq!(check(&s, None, 1_200_000).base.since, Some(600_000));
        assert_eq!(check(&s, Some(0), 1_200_000).base.since, Some(0));
    }

    #[test]
    fn scenario_invariant_unknown_server_fees_do_not_veto() {
        let first = Request {
            server_tools: Some(IndexMap::from([("code_execution_requests".into(), 1)])),
            ..make_request()
        };
        let mut next = Request {
            id: "later".into(),
            time: 600_000,
            end_time: 600_000,
            ..first.clone()
        };
        let s = data(vec![first.clone(), next.clone()], vec![]);
        let report = report_at(&s, None, 1_200_000);
        let result = ttls(&report)[0];
        assert_eq!(result.base.action, Action::Change);
        assert_eq!(result.base.recommended, Some(H1));
        assert_eq!(total(&result.cost.five_minutes), None);
        assert_eq!(total(&result.cost.one_hour), None);
        assert!(result.base.savings_usd.unwrap() > 0.0);
        assert!(
            details(&report)
                .to_lowercase()
                .contains("identical fixed costs remain unknown")
        );
        // Unknown request usage is not scenario-invariant and must still block a winner.
        next.parts[0].usage.write = None;
        next.parts[0].usage.read = None;
        let c = check(&data(vec![first, next], vec![]), None, 1_200_000);
        assert_eq!(c.base.recommended, None);
    }

    #[test]
    fn mixed_outside_main_agent_ttls_keep_the_available_window() {
        let first = Request {
            agent: Agent::Subagent,
            source: Some("agent:builtin:general-purpose".into()),
            ..make_request()
        };
        let helper = Request {
            session: "helper-session".into(),
            agent: Agent::Subagent,
            source: Some("session_title".into()),
            ..at("helper", 600_000)
        };
        let helper = with(helper, u(1000, 0, H1));
        let again = Request {
            id: "again".into(),
            time: 1_200_000,
            end_time: 1_200_000,
            ..first.clone()
        };
        let result = check(&data(vec![first, helper, again], vec![]), None, 1_800_000);
        assert_eq!(result.base.since, Some(0));
        assert_eq!(result.base.requests, 3);
        assert!(result.base.window_basis.to_lowercase().contains("mixed"));
    }

    #[test]
    fn a_selected_boundary_keeps_its_pre_window_pricing_context() {
        let w = Workspace::new();
        let file = w.claude.join("projects/project/main.jsonl");
        w.fixture(&file, "claude.jsonl");
        let text = std::fs::read_to_string(&file).unwrap();
        let text = text.replacen("2026-11-01T00:20:00Z", "2026-11-02T00:20:00Z", 1);
        std::fs::write(&file, text).unwrap();
        let since = day("2026-11-02");
        let s = scan(&options(&w, Some(since))).unwrap();
        assert_eq!(s.requests.len(), 0);
        let result = report_at(&s, Some(since), day("2026-11-03"));
        assert_eq!(total(&result.compactions[0].cost), Some(0.0064));
    }

    #[test]
    fn an_unknown_unmatched_proxy_cancels_without_hiding_its_cost() {
        let first = Request {
            agent: Agent::Subagent,
            source: Some("agent:builtin:general-purpose".into()),
            ..make_request()
        };
        let next = Request {
            id: "next".into(),
            time: 600_000,
            end_time: 600_000,
            ..first.clone()
        };
        let event = StructuralEvent {
            client: Some("sdk-ts".into()),
            precomputed: Some(true),
            ..boundary("unpriced-summary", "other-session", 300_000)
        };
        let result = check(&data(vec![first, next], vec![event]), None, 1_200_000);
        assert_eq!(result.base.recommended, Some(H1));
        assert_eq!(result.base.action, Action::Change);
        assert_eq!(total(&result.summary_cost.five_minutes), None);
        assert_eq!(total(&result.fixed_cost), None);
        assert_eq!(total(&result.cost.one_hour), None);
        assert!(result.base.savings_usd.unwrap() > 0.0);
    }

    #[test]
    fn unknown_comparison_reports_missing_linked_usage_first() {
        let mut r = Request {
            evidence: RequestEvidence::Unknown,
            issues: vec![UNLINKED.into()],
            ..make_request()
        };
        r.parts[0].usage = empty_usage();
        r.parts[0].tier = None;
        let c = check(&data(vec![r], vec![]), None, 1_200_000);
        assert_eq!(c.base.recommended, None);
        assert!(
            c.base
                .uncertainty
                .join(" ")
                .contains("no linked assistant usage")
        );
        assert!(
            c.comparison_cost
                .five_minutes
                .unknown
                .join(" ")
                .contains("usage is not recorded")
        );
    }

    struct Bracket {
        first: Request,
        attempt: Request,
        after: Request,
    }

    fn bracketed_attempt() -> Bracket {
        let first = at("before", 0);
        let mut attempt = Request {
            evidence: RequestEvidence::Unknown,
            reference: Some("attempt-ref".into()),
            attempt: Some(Attempt {
                max_output_tokens: Some(100),
            }),
            server_tools: None,
            issues: vec![UNLINKED.into()],
            ..at("attempt", 600_000)
        };
        attempt.parts[0] = BillingPart {
            usage: empty_usage(),
            tier: None,
            speed: None,
            geo: None,
            ..first.parts[0].clone()
        };
        let after = at("after", 1_200_000);
        Bracket {
            first,
            attempt,
            after,
        }
    }

    #[test]
    fn unlinked_attempts_use_matching_bracket_evidence() {
        let Bracket {
            first,
            attempt,
            after,
        } = bracketed_attempt();
        let s = data(vec![first.clone(), attempt, after.clone()], vec![]);
        let report = report_at(&s, None, 1_800_000);
        let c = ttls(&report)[0];
        assert_eq!(c.base.recommended, Some(H1));
        assert_eq!(c.base.action, Action::Change);
        assert_eq!(c.base.requests, 3);
        assert_eq!(c.usage_estimates.len(), 1);
        assert_eq!(c.usage_estimates[0].basis, vec![first.id, after.id]);
        let part = c.usage_estimates[0].part.as_ref().unwrap();
        assert_eq!(part.usage.input, Some(1000));
        assert_eq!(part.usage.output, None);
        let recorded = &report.scan.requests[1];
        assert_eq!(recorded.parts[0].usage.input, None);
        assert_eq!(recorded.evidence, RequestEvidence::Unknown);
        assert_eq!(total(&report.historical[0].cost), None);
        assert_eq!(total(&c.cost.five_minutes), None);
        assert!(c.base.savings_usd.unwrap() > 0.0);
        // A losing cold-cache sensitivity must accompany, not veto, the point estimate.
        let unlinked = c.sensitivity.unlinked.as_ref().unwrap();
        assert!(
            total(&unlinked.cold_cache.one_hour).unwrap()
                > total(&unlinked.cold_cache.five_minutes).unwrap()
        );
        assert!(total(&unlinked.no_billed_attempt.five_minutes).unwrap() > 0.0);
        assert!(details(&report).lines().any(|line| {
            let line = line.to_lowercase();
            line.find("unlinked")
                .is_some_and(|i| line[i..].contains("estimated"))
        }));
    }

    #[test]
    fn growing_context_retries_support_an_estimate_within_the_cache_lifetime() {
        let mut requests: Vec<Request> = (0..20)
            .map(|i| {
                let r = at(&format!("measured-{i}"), i * 1_200_000);
                with(r, u(100_000 + i * 1000, 0, M5))
            })
            .collect();
        let mut attempt = bracketed_attempt().attempt;
        let before = requests.last().unwrap().clone();
        retime(&mut attempt, before.time + 300);
        let mut after = with(at("retry", before.time + 3300), u(1000, 119_000, M5));
        requests.push(attempt.clone());
        let mut all = requests.clone();
        all.push(after.clone());
        let report = report_at(&data(all, vec![]), None, after.time + 1000);
        let c = ttls(&report)[0];
        let estimate = &c.usage_estimates[0];
        assert_eq!(estimate.evidence, EstimateEvidence::Estimated);
        assert_eq!(estimate.basis, vec![before.id.clone(), after.id.clone()]);
        let part = estimate.part.as_ref().unwrap();
        assert_eq!(part.usage.input, Some(119_000));
        assert_eq!(part.usage.read, Some(119_000));
        assert_eq!(part.usage.write, Some(0));
        assert_eq!(part.usage.output, None);
        assert_eq!(c.base.recommended, Some(H1));
        assert!(c.sensitivity.unlinked.is_some());
        let recorded = report
            .scan
            .requests
            .iter()
            .find(|r| r.id == attempt.id)
            .unwrap();
        assert_eq!(recorded.evidence, RequestEvidence::Unknown);
        assert_eq!(recorded.parts[0].usage.input, None);
        assert_eq!(total(&report.historical[0].cost), None);
        retime(&mut after, before.time + 300_000);
        let now = after.time + 1000;
        requests.push(after);
        let expired = check(&data(requests, vec![]), None, now);
        assert_eq!(
            expired.usage_estimates[0].evidence,
            EstimateEvidence::Unknown
        );
        // One unpriced part among twenty clearly 1h-favoring parts cannot flip the winner.
        assert_eq!(expired.base.recommended, Some(H1));
        let coverage = expired.base.coverage.unwrap();
        assert!(coverage.priced < coverage.total);
    }

    #[test]
    fn retry_estimates_never_take_the_following_requests_own_writes() {
        let Bracket {
            mut first,
            mut attempt,
            mut after,
        } = bracketed_attempt();
        retime(&mut attempt, 300);
        retime(&mut after, 3300);
        after.parts[0].usage = claude_usage(u(100, 1100, M5));
        let s = |first: &Request, after: &Request| {
            data(vec![first.clone(), attempt.clone(), after.clone()], vec![])
        };
        let report = report_at(&s(&first, &after), None, 60_000);
        let part = ttls(&report)[0].usage_estimates[0].part.clone().unwrap();
        assert_eq!(part.usage.input, Some(1100));
        assert_eq!(part.usage.read, Some(1000));
        assert_eq!(part.usage.write, Some(100));
        assert_eq!(report.scan.requests[2].parts[0].usage.write, Some(100));
        after.parts[0].usage = claude_usage(u(300, 900, M5));
        let invalidated = check(&s(&first, &after), None, 60_000);
        assert_eq!(
            invalidated.usage_estimates[0].evidence,
            EstimateEvidence::Unknown
        );
        let coverage = invalidated.base.coverage.unwrap();
        assert!(coverage.priced < coverage.total);
        first.parts[0].usage = claude_usage(ClaudeUsage {
            input: 1000,
            write: 0,
            ..ClaudeUsage::default()
        });
        after.parts[0].usage = claude_usage(ClaudeUsage {
            input: 1200,
            write: 200,
            ..ClaudeUsage::default()
        });
        let no_read_evidence = check(&s(&first, &after), None, 60_000);
        assert_eq!(
            no_read_evidence.usage_estimates[0].evidence,
            EstimateEvidence::Unknown
        );
    }

    #[test]
    fn pre_window_estimated_seeds_keep_provenance_without_charges() {
        let first = with(at("first", 0), u(0, 1000, M5));
        let mut attempt = bracketed_attempt().attempt;
        retime(&mut attempt, 240_000);
        let after = Request {
            id: "after".into(),
            time: 480_000,
            end_time: 480_000,
            ..first.clone()
        };
        let other = Request {
            session: "other".into(),
            ..at("other", 60_000)
        };
        let other = with(other, u(10, 0, M5));
        let transition = Request {
            id: "transition".into(),
            time: 420_000,
            end_time: 420_000,
            ..other.clone()
        };
        let transition = with(transition, u(10, 0, H1));
        let s = data(
            vec![first, other, attempt.clone(), transition, after],
            vec![],
        );
        let c = check(&s, None, 600_000);
        assert_eq!(c.base.since, Some(420_000));
        assert_eq!(c.base.requests, 2);
        assert_eq!(c.usage_estimates[0].request, attempt.id);
        assert!((total(&c.comparison_cost.five_minutes).unwrap() - 0.00025).abs() < 1e-10);
        assert_eq!(c.base.recommended, Some(M5));
        let none = &c.sensitivity.unlinked.as_ref().unwrap().no_billed_attempt;
        assert!(total(&none.one_hour).unwrap() < total(&none.five_minutes).unwrap());
        assert!(c.base.assumptions.join(" ").contains("pre-window"));
    }

    struct PreWindow {
        first: Request,
        attempt: Request,
        after: Request,
        other: Request,
        transition: Request,
    }

    fn pre_window_attempt(speed: Option<&str>) -> PreWindow {
        let Bracket {
            mut first,
            mut attempt,
            mut after,
        } = bracketed_attempt();
        first.parts[0].usage = claude_usage(u(1000, 0, H1));
        retime(&mut attempt, 17 * MIN);
        attempt.parts[0].speed = speed.map(String::from);
        retime(&mut after, 20 * MIN);
        after.parts[0].usage = claude_usage(u(200, 1000, H1));
        let mut other = Request {
            session: "other".into(),
            source: Some("other".into()),
            ..at("other", 60_000)
        };
        other.parts[0] = BillingPart {
            usage: claude_usage(u(10, 0, M5)),
            ..first.parts[0].clone()
        };
        let mut transition = Request {
            id: "transition".into(),
            time: 18 * MIN,
            end_time: 18 * MIN,
            ..other.clone()
        };
        transition.parts[0].usage = claude_usage(u(10, 0, H1));
        PreWindow {
            first,
            attempt,
            after,
            other,
            transition,
        }
    }

    #[test]
    fn unsupported_pre_window_attempts_cannot_choose_winners_from_missing_speed() {
        for speed in [None, Some("standard")] {
            let p = pre_window_attempt(speed);
            let s = data(
                vec![
                    p.first,
                    p.other,
                    p.attempt.clone(),
                    p.transition,
                    p.after.clone(),
                ],
                vec![],
            );
            let c = check(&s, None, 21 * MIN);
            assert_eq!(c.base.since, Some(18 * MIN));
            assert_eq!(c.base.requests, 2);
            assert_eq!(c.base.recommended, None, "{speed:?}");
            assert_eq!(c.base.action, Action::Advisory);
            assert_eq!(total(&c.comparison_cost.five_minutes), None);
            assert!(total(&c.comparison_cost.one_hour).is_some());
            assert_eq!(c.usage_estimates[0].request, p.attempt.id);
            assert_eq!(c.usage_estimates[0].evidence, EstimateEvidence::Unknown);
            assert!(c.cache_evidence.iter().any(|e| {
                e.request == p.after.id
                    && e.ttl == M5
                    && e.outcome == ReplayOutcome::Unknown
                    && e.predecessors
                        .iter()
                        .any(|q| q.request == p.attempt.id && q.state == KnownState::Unknown)
            }));
        }
    }

    #[test]
    fn measured_missing_speed_keeps_ambiguity_across_cohorts() {
        let mut p = pre_window_attempt(None);
        p.attempt.evidence = RequestEvidence::Measured;
        p.attempt.attempt = None;
        p.attempt.parts[0].usage = claude_usage(u(0, 1000, H1));
        let mut fast_before = Request {
            id: "fast-before".into(),
            time: 30_000,
            end_time: 30_000,
            ..p.first.clone()
        };
        fast_before.parts[0].speed = Some("fast".into());
        let mut fast_after = Request {
            id: "fast-after".into(),
            time: 21 * MIN,
            end_time: 21 * MIN,
            ..p.after.clone()
        };
        fast_after.parts[0].speed = Some("fast".into());
        let s = data(
            vec![
                p.first,
                fast_before,
                p.other,
                p.attempt.clone(),
                p.transition,
                p.after.clone(),
                fast_after.clone(),
            ],
            vec![],
        );
        let c = check(&s, None, 22 * MIN);
        assert_eq!(c.base.recommended, None);
        for request in [&p.after.id, &fast_after.id] {
            assert!(c.cache_evidence.iter().any(|e| {
                &e.request == request
                    && e.outcome == ReplayOutcome::Unknown
                    && e.predecessors.iter().any(|q| q.request == p.attempt.id)
            }));
        }
    }

    #[test]
    fn unknown_pre_window_state_cancels_when_charges_do_not_depend_on_it() {
        let mut p = pre_window_attempt(None);
        p.after.parts[0].usage = claude_usage(u(1500, 0, H1));
        let s = data(
            vec![p.first, p.other, p.attempt.clone(), p.transition, p.after],
            vec![],
        );
        let c = check(&s, None, 21 * MIN);
        assert_eq!(c.base.recommended, Some(M5));
        assert_eq!(c.usage_estimates[0].evidence, EstimateEvidence::Unknown);
        assert!(c.cache_evidence.iter().any(|e| {
            e.state == KnownState::Unknown
                && e.outcome == ReplayOutcome::Invariant
                && e.predecessors.iter().any(|q| q.request == p.attempt.id)
        }));
    }

    #[test]
    fn a_later_known_touch_resolves_an_unsupported_predecessor() {
        let mut p = pre_window_attempt(None);
        let reset = Request {
            id: "known-reset".into(),
            time: 18 * MIN,
            end_time: 18 * MIN,
            ..p.after.clone()
        };
        retime(&mut p.transition, 19 * MIN);
        let s = data(
            vec![p.first, p.other, p.attempt, reset, p.transition, p.after],
            vec![],
        );
        let report = report_at(&s, None, 21 * MIN);
        let c = ttls(&report)[0];
        assert_eq!(c.base.since, Some(19 * MIN));
        assert_eq!(c.base.recommended, Some(M5));
        assert_eq!(c.usage_estimates.len(), 0);
        assert_eq!(total(&report.historical[0].cost), None);
    }

    #[test]
    fn unsupported_attempts_block_only_their_comparison() {
        let Bracket {
            first,
            attempt,
            after,
        } = bracketed_attempt();
        let after = with(after, u(1200, 0, M5));
        let sub = Request {
            agent: Agent::Subagent,
            source: Some("agent:builtin:general-purpose".into()),
            session: "child".into(),
            ..at("sub", 0)
        };
        let s = data(vec![first, attempt, after, sub], vec![]);
        let report = report_at(&s, None, 1_800_000);
        let main = bucket_check(&report, Bucket::Main);
        let outside = bucket_check(&report, Bucket::Subagent);
        assert_eq!(main.base.recommended, None);
        assert_eq!(main.base.requests, 3);
        assert_eq!(main.usage_estimates[0].evidence, EstimateEvidence::Unknown);
        assert!(main.usage_estimates[0].reason.contains("matching"));
        assert_eq!(outside.base.recommended, Some(M5));
        assert_eq!(report.scan.requests.len(), 4);
    }

    #[test]
    fn bracketing_cannot_cross_changes() {
        for change in ["source", "model", "client", "profiles", "boundary"] {
            let Bracket {
                first,
                attempt,
                mut after,
            } = bracketed_attempt();
            let mut events = vec![];
            match change {
                "source" => after.source = Some("different-source".into()),
                "model" => after.parts[0].model = Some("claude-haiku-5-5".into()),
                "client" => after.client = Some("changed-entrypoint".into()),
                "profiles" => after.profiles = vec!["profile".into(), "alias".into()],
                _ => events.push(boundary("boundary", "session", 900_000)),
            }
            let report = report_at(&data(vec![first, attempt, after], events), None, 1_800_000);
            let c = bucket_check(&report, Bucket::Main);
            let coverage = c.base.coverage.unwrap();
            assert!(coverage.priced < coverage.total, "{change}");
            assert_eq!(
                c.usage_estimates[0].evidence,
                EstimateEvidence::Unknown,
                "{change}"
            );
        }
    }

    #[test]
    fn unknown_output_and_ordinary_input_cancel() {
        let mut first = make_request();
        first.parts[0].usage.output = None;
        first.parts[0].usage.input = None;
        let s = data(vec![first, at("after", 600_000)], vec![]);
        let c = check(&s, None, 1_200_000);
        assert_eq!(c.base.recommended, Some(H1));
        assert_eq!(total(&c.fixed_cost), None);
        assert!(total(&c.comparison_cost.one_hour).is_some());
    }

    #[test]
    fn estimation_needs_both_adjacent_measured_brackets() {
        for layout in ["first", "last", "consecutive", "mixed"] {
            let Bracket {
                first,
                attempt,
                after,
            } = bracketed_attempt();
            let other = Request {
                id: "other".into(),
                session: if layout == "mixed" {
                    "unbracketed-session".into()
                } else {
                    attempt.session.clone()
                },
                time: 900_000,
                end_time: 900_000,
                ..attempt.clone()
            };
            let requests = match layout {
                "first" => vec![attempt, after],
                "last" => vec![first, attempt],
                _ => vec![first, attempt, other, after],
            };
            let n = requests.len() as u64;
            let c = check(&data(requests, vec![]), None, 1_800_000);
            // Two-part histories cannot outweigh their unpriced half; the mixed history can.
            let expected = if layout == "mixed" { Some(H1) } else { None };
            assert_eq!(c.base.recommended, expected, "{layout}");
            let coverage = c.base.coverage.unwrap();
            assert!(coverage.priced < coverage.total, "{layout}");
            assert_eq!(c.base.requests, n, "{layout}");
            let estimated = c
                .usage_estimates
                .iter()
                .filter(|e| e.evidence == EstimateEvidence::Estimated)
                .count();
            assert_eq!(estimated, usize::from(layout == "mixed"), "{layout}");
        }
    }

    #[test]
    fn unknown_rates_for_a_cache_free_part_cancel_but_an_unknown_tier_does_not() {
        let mut first = at("before", 0);
        let after = at("after", 600_000);
        let mut fixed = Request {
            session: "separate".into(),
            ..at("fixed", 0)
        };
        fixed.parts[0].model = Some("private-internal".into());
        fixed.parts[0].usage = Usage {
            input: None,
            output: None,
            ..claude_usage(u(0, 0, M5))
        };
        let s = data(vec![first.clone(), after.clone(), fixed], vec![]);
        let result = check(&s, None, 1_200_000);
        assert_eq!(result.base.recommended, Some(H1));
        assert_eq!(total(&result.fixed_cost), None);
        first.parts[0].model = Some("claude-haiku-5-5".into());
        first.parts[0].usage.input = None;
        let c = check(&data(vec![first, after], vec![]), None, 1_200_000);
        assert_eq!(c.base.recommended, None);
    }

    #[test]
    fn unknown_kinds_span_both_buckets_while_known_agents_stay_separate() {
        let mut first = at("first", 0);
        first.parts[0].kind = PartKind::Compaction;
        first.parts[0].usage = claude_usage(u(100_000, 0, H1));
        let mut middle = Request {
            id: "middle".into(),
            time: 17 * MIN,
            end_time: 17 * MIN,
            ..first.clone()
        };
        middle.parts[0].kind = PartKind::Unknown;
        middle.parts[0].usage = claude_usage(u(0, 100_000, H1));
        let mut after = Request {
            id: "after".into(),
            time: 20 * MIN,
            end_time: 20 * MIN,
            ..first.clone()
        };
        after.parts[0].usage = claude_usage(u(2000, 100_000, H1));
        let run = |first: &Request, middle: &Request, after: &Request| {
            let s = data(vec![first.clone(), middle.clone(), after.clone()], vec![]);
            check(&s, Some(19 * MIN), NOW)
        };
        let c = run(&first, &middle, &after);
        assert_eq!(c.bucket, Bucket::Subagent);
        assert_eq!(c.base.recommended, None);
        assert!(c.cache_evidence.iter().any(|e| {
            e.predecessors.iter().any(|q| {
                q.request == middle.id
                    && !q.cohort_known
                    && q.prefix_evidence == PrefixEvidence::Measured
            })
        }));
        middle.parts[0].kind = PartKind::Compaction;
        assert_eq!(run(&first, &middle, &after).base.recommended, Some(M5));
        // Message cohorts in the same conversation cannot renew another agent's cache.
        for r in [&mut first, &mut middle, &mut after] {
            r.parts[0].kind = PartKind::Message;
        }
        middle.agent = Agent::Subagent;
        assert_eq!(run(&first, &middle, &after).base.recommended, Some(H1));
        middle.agent = Agent::Unknown;
        assert_eq!(run(&first, &middle, &after).base.recommended, None);
    }

    #[test]
    fn unresolved_part_attribution_stays_unpriced_in_each_bucket() {
        let mut request = make_request();
        request.parts[0].kind = PartKind::Unknown;
        let report = report_at(&data(vec![request], vec![]), None, NOW);
        assert_eq!(ttls(&report).len(), 2);
        for c in ttls(&report) {
            assert_eq!(total(&c.comparison_cost.five_minutes), None);
            assert!(
                c.comparison_cost
                    .five_minutes
                    .unknown
                    .join(" ")
                    .contains("bucket is not established")
            );
        }
        assert_eq!(total(&report.historical[0].cost), None);
        assert!(
            report.historical[0]
                .cost
                .unknown
                .join(" ")
                .contains("iteration semantics")
        );
    }

    #[test]
    fn estimated_prefix_provenance_stays_explicit_when_its_cohort_is_incomplete() {
        let Bracket {
            mut first,
            mut attempt,
            mut after,
        } = bracketed_attempt();
        first.parts[0].speed = None;
        after.parts[0].speed = None;
        retime(&mut attempt, 60 * MIN);
        retime(&mut after, 65 * MIN);
        let s = data(vec![first, attempt.clone(), after], vec![]);
        let c = check(&s, Some(64 * MIN), NOW);
        assert_eq!(c.usage_estimates[0].evidence, EstimateEvidence::Estimated);
        assert!(c.cache_evidence.iter().any(|e| {
            e.predecessors.iter().any(|q| {
                q.request == attempt.id
                    && q.state == KnownState::Unknown
                    && q.prefix_evidence == PrefixEvidence::Estimated
                    && !q.cohort_known
                    && q.reason.contains("estimated")
            })
        }));
    }

    #[test]
    fn borrowed_ttl_splits_never_establish_observed_warmth() {
        for ttl in Ttl::ALL {
            let mut results = vec![];
            for split in [true, false] {
                let Bracket {
                    mut first,
                    mut attempt,
                    mut after,
                } = bracketed_attempt();
                for r in [&mut first, &mut after] {
                    r.parts[0].usage = claude_usage(u(1000, 100_000, ttl));
                    if !split {
                        r.parts[0].usage.write5m = None;
                        r.parts[0].usage.write1h = None;
                    }
                }
                retime(&mut attempt, 30 * MIN);
                retime(&mut after, 31 * MIN);
                let c = check(&data(vec![first, attempt, after], vec![]), None, 32 * MIN);
                assert_eq!(c.usage_estimates[0].evidence, EstimateEvidence::Estimated);
                assert_eq!(c.base.recommended, Some(H1));
                results.push(c.comparison_cost);
            }
            assert_eq!(results[0], results[1], "{ttl:?}");
        }
    }

    #[test]
    fn unknown_subagent_kinds_cannot_affect_the_main_bucket() {
        let first = with(at("main-first", 0), u(50_000, 0, M5));
        let after = with(at("main-after", 20 * MIN), u(1000, 50_000, M5));
        let mut child = Request {
            agent: Agent::Subagent,
            ..at("child", 60_000)
        };
        child.parts[0].kind = PartKind::Unknown;
        child.parts[0].usage = claude_usage(u(20_000, 0, M5));
        let baseline = check(&data(vec![first.clone(), after.clone()], vec![]), None, NOW);
        let report = report_at(&data(vec![first, child, after], vec![]), None, NOW);
        let main = bucket_check(&report, Bucket::Main);
        assert_eq!(main.base.requests, 2);
        assert_eq!(main.comparison_cost, baseline.comparison_cost);
        assert_eq!(main.base.recommended, Some(M5));
        let sub = bucket_check(&report, Bucket::Subagent);
        assert_eq!(sub.base.recommended, None);
    }

    // Explicit reader windows, compared against the same history scanned whole.

    fn common(kind: &str, minute: i64) -> Value {
        json!({
            "version": "2.1.314",
            "entrypoint": "sdk-ts",
            "sessionId": "synthetic",
            "querySource": "sdk",
            "type": kind,
            "timestamp": iso(minute * MIN),
        })
    }

    fn assistant(minute: i64, id: &str, read: i64, write: i64) -> Value {
        let mut record = common("assistant", minute);
        record["message"] = json!({
            "id": id,
            "content": [{"type": "text", "text": "SYNTHETIC_PRIVATE_WINDOW"}],
            "model": "claude-opus-5-5",
            "usage": {
                "input_tokens": 0,
                "output_tokens": 10,
                "cache_read_input_tokens": read,
                "cache_creation_input_tokens": write,
                "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": write},
                "service_tier": "standard",
                "speed": "standard",
                "inference_geo": "global",
            },
        });
        record
    }

    fn api_attempt(minute: i64) -> Value {
        let mut record = common("api-request", minute);
        record["id"] = json!("attempt");
        record["params"] = json!({"model": "claude-opus-5-5"});
        record
    }

    fn compact_boundary(minute: i64, uuid: &str, metadata: Value) -> Value {
        let mut record = common("system", minute);
        record["subtype"] = json!("compact_boundary");
        record["uuid"] = json!(uuid);
        record["compactMetadata"] = metadata;
        record
    }

    fn profile_with(records: &[Value]) -> Workspace {
        let w = Workspace::new();
        std::fs::write(w.claude.join("settings.json"), r#"{"promptCacheTtl":"5m"}"#).unwrap();
        std::fs::write(w.claude.join("projects/project/main.jsonl"), jsonl(records)).unwrap();
        w
    }

    #[test]
    fn explicit_windows_keep_pre_window_cache_evidence_without_billing_it() {
        for mode in ["known", "unsupported", "estimated"] {
            let last = if mode == "estimated" {
                assistant(20, "last", 0, 100_000)
            } else {
                assistant(20, "last", 100_000, 2000)
            };
            let mut records = vec![assistant(0, "first", 0, 100_000)];
            if mode != "known" {
                records.push(api_attempt(if mode == "estimated" { 10 } else { 17 }));
            }
            records.push(last);
            let w = profile_with(&records);
            let since = 18 * MIN;
            let full = check(&scan(&options(&w, None)).unwrap(), Some(since), 21 * MIN);
            let selected = scan(&options(&w, Some(since))).unwrap();
            let report = report_at(&selected, Some(since), 21 * MIN);
            let c = ttls(&report)[0];
            assert_eq!(c.comparison_cost, full.comparison_cost, "{mode}");
            assert_eq!(c.cache_evidence, full.cache_evidence, "{mode}");
            assert_eq!(c.usage_estimates, full.usage_estimates, "{mode}");
            assert_eq!(selected.requests.len(), 1);
            assert_eq!(selected.coverage.requests, 1);
            assert_eq!(selected.since, Some(since));
            let inherited = check(&selected, None, 21 * MIN);
            assert_eq!(inherited.comparison_cost, c.comparison_cost);
            assert_eq!(inherited.base.since, Some(since));
            let error = analyze(AnalyzeOptions {
                scan: selected.clone(),
                catalog: &prices(),
                since: Some(since - 1),
                now: NOW,
                max_cost_per_use_usd: 1.0,
            })
            .unwrap_err();
            assert!(
                error
                    .message
                    .contains("predates the scanned billing window")
            );
            let context = if mode == "known" { 1 } else { 2 };
            assert_eq!(selected.cache_context.requests.len(), context);
            assert_eq!(report.historical[0].requests, 1);
            assert_eq!(c.base.requests, 1);
            match mode {
                "known" => assert_eq!(c.base.recommended, Some(H1)),
                "unsupported" => {
                    assert_eq!(c.base.recommended, None);
                    assert_eq!(c.usage_estimates[0].evidence, EstimateEvidence::Unknown);
                }
                _ => {
                    assert_eq!(c.usage_estimates[0].evidence, EstimateEvidence::Estimated);
                    assert!(c.sensitivity.unlinked.is_some());
                }
            }
        }
    }

    #[test]
    fn pre_window_boundaries_reset_context_while_invalid_usage_stays_unknown() {
        for with_boundary in [false, true] {
            let mut invalid = assistant(0, "invalid", 0, 100_000);
            invalid["message"] = json!({
                "id": "invalid",
                "model": "claude-opus-5-5",
                "usage": {"input_tokens": "invalid synthetic counter"},
            });
            let mut records = vec![invalid];
            if with_boundary {
                let metadata = json!({"preTokens": "unselected invalid proxy"});
                records.push(compact_boundary(18, "boundary", metadata));
            }
            records.push(assistant(20, "last", 100_000, 2000));
            let w = profile_with(&records);
            let selected = scan(&options(&w, Some(19 * MIN))).unwrap();
            let context = &selected.cache_context;
            assert_eq!(context.requests[0].evidence, RequestEvidence::Unknown);
            assert_eq!(context.requests[0].parts[0].usage.read, None);
            assert_eq!(context.boundaries.len(), usize::from(with_boundary));
            assert_eq!(selected.events.len(), 0);
            let c = check(&selected, Some(19 * MIN), 21 * MIN);
            assert_eq!(
                total(&c.comparison_cost.five_minutes).is_none(),
                !with_boundary
            );
            if with_boundary {
                assert_eq!(c.usage_estimates.len(), 0);
            } else {
                assert!(
                    c.base
                        .uncertainty
                        .join(" ")
                        .contains("pre-window usage is invalid")
                );
                assert!(c.cache_evidence.iter().any(|e| {
                    e.predecessors
                        .iter()
                        .any(|q| q.prefix_evidence == PrefixEvidence::Unknown)
                }));
            }
        }
    }

    #[test]
    fn a_request_linked_across_the_window_belongs_only_to_cache_context() {
        let mut last = assistant(20, "last", 100_000, 2000);
        last["requestRef"] = json!("attempt");
        let w = profile_with(&[api_attempt(17), last]);
        let selected = scan(&options(&w, Some(18 * MIN))).unwrap();
        assert_eq!(selected.requests.len(), 0);
        assert_eq!(selected.coverage.requests, 0);
        let context = &selected.cache_context.requests;
        assert_eq!(context.len(), 1);
        assert_eq!(context[0].time, 17 * MIN);
        assert_eq!(context[0].evidence, RequestEvidence::Measured);
        let report = report_at(&selected, Some(18 * MIN), NOW);
        assert_eq!(ttls(&report).len(), 0);
        assert_eq!(report.historical.len(), 0);
    }

    #[test]
    fn pre_window_boundaries_keep_summary_claims_and_explicit_links() {
        for explicit in [false, true] {
            let mut summary = assistant(0, "old-summary", 0, 0);
            summary["querySource"] = json!(if explicit { "sdk" } else { "compact" });
            let mut iteration = summary["message"]["usage"].clone();
            iteration["type"] = json!(if explicit { "message" } else { "compaction" });
            summary["message"]["usage"]["iterations"] = json!([iteration]);
            let boundary = |minute: i64| {
                let mut metadata = json!({"preTokens": 1000, "postTokens": 100});
                if explicit && minute == 1 {
                    metadata["requestId"] = json!("old-summary");
                }
                compact_boundary(minute, &format!("boundary-{minute}"), metadata)
            };
            let w = profile_with(&[
                summary,
                boundary(1),
                assistant(2, "middle", 0, 1000),
                boundary(15),
                assistant(20, "last", 0, 1000),
            ]);
            let since = 10 * MIN;
            let complete = report_at(&scan(&options(&w, None)).unwrap(), Some(since), NOW);
            let selected = scan(&options(&w, Some(since))).unwrap();
            let report = report_at(&selected, Some(since), NOW);
            let ids: HashSet<&str> = selected.events.iter().map(|e| e.id.as_str()).collect();
            let expected: Vec<CompactionCharge> = complete
                .compactions
                .iter()
                .filter(|c| ids.contains(c.event.as_str()))
                .cloned()
                .collect();
            assert_eq!(report.compactions, expected);
            assert_eq!(report.compactions[0].evidence, Evidence::Estimated);
            assert_eq!(report.compactions[0].request, None);
            let full_sub = bucket_check(&complete, Bucket::Subagent);
            let sub = bucket_check(&report, Bucket::Subagent);
            assert_eq!(total(&sub.summary_cost.five_minutes), Some(0.006));
            assert_eq!(sub.summary_cost, full_sub.summary_cost);
            assert_eq!(sub.fixed_cost, full_sub.fixed_cost);
            assert_eq!(sub.cost, full_sub.cost);
            if explicit {
                let linked = &report.scan.cache_context.requests[0];
                assert_eq!(linked.parts[0].kind, PartKind::Compaction);
            }
        }
    }

    #[test]
    fn versions_and_fixed_decimals_follow_javascript() {
        assert!(version_supported(Some("2.1.242")));
        assert!(version_supported(Some("3.0.0")));
        assert!(version_supported(Some("2.1.0x100")));
        assert!(!version_supported(Some("2.1.241")));
        assert!(!version_supported(Some("2.1")));
        assert!(!version_supported(Some("2.1.242a")));
        assert!(!version_supported(Some("2.1.242.5")));
        assert!(!version_supported(None));
        assert_eq!(to_fixed1(0.25), "0.3");
        assert_eq!(to_fixed1(-0.25), "-0.3");
        assert_eq!(to_fixed1(0.35), "0.3");
        assert_eq!(to_fixed1(12.04), "12.0");
        assert_eq!(to_fixed1(99.95), "100.0");
    }
}
