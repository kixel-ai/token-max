//! Codex turn-end compaction check: replay compacting after a turn's final response at
//! candidate thresholds, under the limit the window line recommends, and compare them with
//! the current threshold.

use std::collections::BTreeSet;

use crate::collections::{HashMap, HashSet, IndexMap, IndexSet};
use crate::compaction::{
    Lockstep, Pass, Simulator, Stream, Trigger, TurnEnds, WindowEvents, WindowOptions,
    compaction_samples, is_conversation, scenarios, skipped_lines,
};
use crate::controls::TURN_END_KEY;
use crate::discovery::locale_compare;
use crate::model::{
    Action, CheckBase, CompactionCharge, CompactionCheck, EventKind, Evidence, Harness, Id, Millis,
    PerBucket, Profile, Request, Scan, StructuralEvent, Text, TurnEndCheck, UsableWindow,
    UsageEstimate, WindowScenario,
};
use crate::prices::{Catalog, PRICE_SOURCES, Sum, TIER_SENSITIVITY, server_cost, tier_sensitive};
use crate::scenarios::{PartReplay, ReplayOptions, ReplayTtls};
use crate::version::codex_at_least;

const SOURCES: [&str; 3] = [
    "https://developers.openai.com/codex/config-reference (2026-11-23)",
    "https://github.com/openai/codex/blob/rust-v0.171.0/codex-rs/core/src/session/context_window.rs (2026-11-23)",
    "https://developers.openai.com/api/docs/guides/compaction (2026-11-07)",
];

/// The Codex release that added the setting, v0.156.0.
const SINCE_MINOR: u64 = 156;

/// Inputs to the turn-end checks, which run after the window decisions.
pub struct TurnEndOptions<'a> {
    pub scan: &'a Scan,
    pub catalog: &'a Catalog,
    pub summaries: &'a [CompactionCharge],
    pub estimates: &'a IndexMap<Text, UsageEstimate>,
    /// Window checks whose recommended limits the replay applies.
    pub windows: &'a [&'a CompactionCheck],
    pub since: Option<Millis>,
    pub now: Millis,
}

/// Compare Codex's turn-end compaction thresholds, one line for each settings file.
pub fn turn_end_checks(options: &TurnEndOptions) -> Vec<TurnEndCheck> {
    let codex = options
        .scan
        .profiles
        .iter()
        .filter(|p| p.harness == Harness::Codex && p.turn_end.is_some());
    // Profiles share a line when the threshold and the window the replay applies match too.
    let owner = |p: &Profile| {
        let control = p.turn_end.as_ref().expect("filtered above");
        format!(
            "{}\n{}\n{:?}\n{}",
            control.owner,
            control.percent,
            control.unresolved,
            crate::rows::compaction_owner(p)
        )
    };
    crate::rows::groups(codex, owner)
        .iter()
        .filter_map(|group| check(options, group))
        .collect()
}

/// The largest whole percentage of `usable` at or below `tokens`, which compacts a turn
/// that ends at `tokens` or more.
fn percent_of(tokens: i64, usable: i64) -> i64 {
    (i128::from(tokens) * 100 / i128::from(usable)) as i64
}

fn check(options: &TurnEndOptions, group: &crate::rows::Group<'_>) -> Option<TurnEndCheck> {
    let TurnEndOptions {
        scan, catalog, now, ..
    } = *options;
    let profile = group.lead;
    let control = profile.turn_end.as_ref()?;
    let in_profile = |profiles: &[String]| group.covers(profiles);
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
    let selected_ids: HashSet<&str> = scan.requests.iter().map(|r| &*r.id).collect();
    let selected: Vec<&Request> = requests
        .iter()
        .copied()
        .filter(|r| selected_ids.contains(&*r.id))
        .collect();
    let first_selected = selected.iter().map(|r| r.time).min()?;
    let first_boundary = scan
        .events
        .iter()
        .filter(|e| in_profile(&e.profiles) && e.kind == EventKind::Compaction)
        .map(|e| e.time)
        .min();
    let since = options
        .since
        .unwrap_or_else(|| first_boundary.map_or(first_selected, |b| first_selected.min(b)));
    let models: Vec<Option<Text>> = requests
        .iter()
        .flat_map(|r| r.parts.iter().filter(|p| is_conversation(p)))
        .map(|p| p.model.clone())
        .collect::<IndexSet<_>>()
        .into_iter()
        .collect();
    // Each model's usable window as its latest turn recorded it.
    let mut latest: HashMap<Option<Text>, (Millis, i64)> = HashMap::default();
    for e in &events {
        if let Some(window) = e.usable_window
            && latest.get(&e.model).is_none_or(|(time, _)| *time <= e.time)
        {
            latest.insert(e.model.clone(), (e.time, window));
        }
    }
    let usable: Vec<UsableWindow> = models
        .iter()
        .filter_map(|model| {
            Some(UsableWindow {
                model: model.clone(),
                tokens: latest.get(model)?.1,
            })
        })
        .collect();
    let turns = TurnEnds::new(
        events.iter().copied(),
        usable.iter().map(|u| (u.model.clone(), u.tokens)).collect(),
    );

    // The limit is the window line's recommendation, as `doctor` applies it first.
    let window_check = options
        .windows
        .iter()
        .find(|c| c.base.profiles.contains(&profile.id));
    let window = match window_check {
        Some(c) if c.base.action == Action::Change => c.base.recommended,
        _ => profile.compaction.default.value,
    };
    let index = WindowEvents::new(events.iter().copied());
    let calibration = compaction_samples(&requests, &events, options.summaries);
    let window_options = WindowOptions {
        events: &index,
        estimates: options.estimates,
        catalog,
        samples: &calibration,
        since,
        ttls: PerBucket {
            main: None,
            subagent: None,
        },
    };

    // Charges no threshold changes: summaries the transcripts record without a request and
    // server tools.
    let summary_ids: HashSet<&Id> = events
        .iter()
        .filter(|e| e.time >= since)
        .map(|e| &e.id)
        .collect();
    let mut fixed = Sum::new();
    for s in options.summaries {
        if summary_ids.contains(&s.event) && s.request.is_none() {
            fixed.add(&s.cost);
        }
    }
    for r in selected.iter().filter(|r| r.time >= since) {
        fixed.add(&server_cost(r));
    }
    let fixed = fixed.cost();

    // Thresholds where the cost of a turn's end can change: each model's price tiers and the
    // auto-compact limit, each as a percentage of that model's usable window.
    let current = control.percent;
    let mut candidates: BTreeSet<i64> = BTreeSet::from([current]);
    let mut providers: IndexMap<&Option<Text>, IndexSet<&str>> = IndexMap::default();
    for r in &requests {
        for p in r.parts.iter().filter(|p| is_conversation(p)) {
            if let (Some(provider), Some(_)) = (&r.provider, &p.model) {
                providers.entry(&p.model).or_default().insert(provider);
            }
        }
    }
    for u in &usable {
        let mut sizes: Vec<i64> = window.into_iter().collect();
        if let (Some(model), Some(providers)) = (&u.model, providers.get(&u.model)) {
            for provider in providers {
                // Catalog sizes are whole token counts; a fraction would compare like its floor.
                sizes.extend(catalog.tier_sizes(provider, model).map(|s| s as i64));
            }
        }
        candidates.extend(
            sizes
                .into_iter()
                .map(|size| percent_of(size, u.tokens).min(100)),
        );
    }
    // Recorded turn-end compactions cannot be undone, so only a lower threshold replays.
    let ceiling = if current > 0 { current } else { 100 };
    let percents: Vec<i64> = candidates
        .into_iter()
        .filter(|&p| p == current || (1..=ceiling).contains(&p))
        .collect();

    let last: HashMap<&str, usize> = requests
        .iter()
        .enumerate()
        .map(|(i, r)| (&*r.session, i))
        .collect();
    let no_estimates = IndexMap::default();
    let replay_options = ReplayOptions {
        estimates: &no_estimates,
        catalog,
        ttls: ReplayTtls::default(),
        since,
        cold_prefixes: false,
        estimated_parts: None,
        attempt_scenario: Default::default(),
    };
    let lockstep = Lockstep {
        requests: &requests,
        last: &last,
        catalog,
        fixed: &fixed,
        // A limit the replay cannot model leaves every threshold beside it unpriced.
        unknown: profile.compaction.default.comparison_unknown.as_deref(),
        skipped: skipped_lines(scan, &group.ids),
    };
    let replay = || PartReplay::new([&index.cache], &replay_options);
    let (runs, current_cost) = lockstep.runs(
        Some(current),
        &percents,
        |percent, scenario| {
            Stream::new(
                // One pass replays the limit and the threshold in time order, so a turn's end
                // that compacts first spares the limit's compaction before the next request.
                Simulator::new(
                    &[],
                    Some(options.estimates),
                    Pass {
                        trigger: Trigger::TurnEnd {
                            percent,
                            turns: &turns,
                            window,
                        },
                        larger: scenario == 1,
                        cold: scenario == 2,
                    },
                    &window_options,
                ),
                replay(),
            )
        },
        replay,
    );
    // A threshold whose turn ends compact where the current one's do replays as it does, so
    // it decides nothing. One that could not decide or price a turn end still acts.
    let acts: Vec<bool> = runs[0].iter().map(|r| r.acts).collect();
    let scenarios = scenarios(&percents, runs);
    let acting: Vec<&WindowScenario> = scenarios
        .iter()
        .zip(acts)
        .filter_map(|(s, acts)| acts.then_some(s))
        .collect();
    let conditional = scenarios
        .iter()
        .any(|s| tier_sensitive(&s.baseline_comparison_cost) || tier_sensitive(&s.comparison_cost));
    let mut best: Option<&WindowScenario> = None;
    for &s in acting.iter().filter(|s| s.savings_usd.is_some()) {
        // The higher threshold wins a tie, since it summarizes less.
        if best.is_none_or(|b| s.savings_usd >= b.savings_usd) {
            best = Some(s);
        }
    }
    let winning = best.filter(|b| b.savings_usd.is_some_and(|s| s > 0.0));
    let every_priced = acting.iter().all(|s| s.savings_usd.is_some());
    let losing = best.is_some_and(|b| b.savings_usd.is_some_and(|s| s < 0.0)) && every_priced;
    // A tie, or a threshold that could not be priced beside none that wins, gives no verdict.
    let recommended = winning
        .map(|w| w.window)
        .or(losing.then_some(current))
        .filter(|_| !conditional);
    let recent = selected.iter().max_by_key(|r| r.time);
    let supported = recent.is_some_and(|r| codex_at_least(r.version.as_deref(), SINCE_MINOR, 0));
    let applicable = control.unresolved.is_none() && supported;
    let savings = winning
        .and_then(|w| w.savings_usd)
        .or(recommended.map(|_| 0.0))
        .filter(|_| !conditional);
    let margin = winning.and_then(|w| w.margin).filter(|_| !conditional);

    let mut uncertainty: IndexSet<String> = [
        "compaction loses detail; re-reading files, repeated tool calls or changed reasoning may erase savings; that work is unmeasured, not priced at zero",
        "which turns compact and what they resume to are estimated from token counts and recorded compactions, not observed counterfactual behavior; no samples means affected thresholds remain unpriced",
    ]
    .map(String::from)
    .into_iter()
    .collect();
    if conditional {
        uncertainty.insert(TIER_SENSITIVITY.into());
    }
    uncertainty.extend(control.unresolved.clone());
    if !supported {
        uncertainty.insert(format!(
            "current client support for {TURN_END_KEY} is not established (requires Codex v0.{SINCE_MINOR}.0)"
        ));
    }
    let missing: Vec<&str> = models
        .iter()
        .filter(|m| !usable.iter().any(|u| &u.model == *m))
        .map(|m| m.as_deref().unwrap_or("an unrecorded model"))
        .collect();
    if !missing.is_empty() {
        uncertainty.insert(format!(
            "no usable context window is recorded for {}, so its turns stay unpriced",
            missing.join(", ")
        ));
    }
    let profiles: IndexSet<&String> = selected.iter().flat_map(|r| r.profiles.iter()).collect();
    if profiles.iter().any(|p| !group.ids.contains(p)) {
        uncertainty.insert("shared history has ambiguous profile attribution".into());
    }
    for s in &scenarios {
        uncertainty.extend(s.comparison_cost.unknown.iter().cloned());
        uncertainty.extend(s.cost.unknown.iter().cloned());
    }
    for issue in selected.iter().flat_map(|r| &r.issues) {
        uncertainty.insert(issue.to_string());
    }
    let coverage = winning
        .or(best)
        .or_else(|| scenarios.iter().find(|s| s.window == current))
        .map(|s| s.coverage);
    let after_window = match (window_check, window) {
        (Some(c), Some(window)) if c.base.action == Action::Change => {
            format!("the auto-compact window line's recommended {window}-token limit")
        }
        (_, Some(window)) => format!("the current {window}-token auto-compact limit"),
        (_, None) => "no auto-compact limit, which no setting names".into(),
    };
    let state = if current == 0 {
        "turn-end compaction off".to_string()
    } else {
        format!("turn-end compaction at {current}%")
    };
    Some(TurnEndCheck {
        base: CheckBase {
            profile: profile.id.clone(),
            profiles: group.ids.clone(),
            history_profiles: profiles.into_iter().cloned().collect(),
            evidence: if recommended.is_none() {
                Evidence::Unknown
            } else {
                Evidence::Estimated
            },
            action: match recommended {
                Some(r) if applicable && r == current => Action::Keep,
                Some(_) if applicable => Action::Change,
                _ => Action::Advisory,
            },
            current: Some(current),
            recommended,
            owner: control.owner.clone(),
            since: Some(since),
            until: Some(now),
            window_basis: if options.since.is_some() {
                "explicit --since; current controls applied to history"
            } else {
                "available selected history; current controls applied to history"
            }
            .into(),
            baseline: format!(
                "{state}, applied to recorded requests under {after_window}; observed summaries retained once; savings are incremental and checks must not be summed"
            ),
            requests: selected.iter().filter(|r| r.time >= since).count() as u64,
            coverage,
            savings_usd: savings,
            margin,
            monthly_savings_usd: savings
                .filter(|_| now > since)
                .map(|s| crate::report::monthly(s, Some(since), Some(now))),
            break_even: savings.filter(|&s| s > 0.0).map(|s| {
                format!(
                    "${s:.4} of additional re-read or changed-work cost erases the modeled saving over this window"
                )
            }),
            assumptions: vec![
                "a turn's end compacts when its final request's input and output reach the threshold's share of the model's usable window, the window Codex last recorded for the model; the usual auto-compact limit still applies in the same replay, so a turn's end that compacts first spares the limit's compaction before the next request".into(),
                "candidate thresholds are the current one and, as percentages of each model's usable window, its catalog price tiers and the auto-compact limit; this is not an exhaustive optimizer over every percentage".into(),
                "a turn-end summary replaces only the context the turn finished with, so what the next turn adds stays, and a next request on another model, agent, provider or speed leaves it unpriced; it reads that context while its cache is warm: what the turn's final request read from the cache is read again, and the rest is ordinary input; output and the resumed prefix use cohort medians from linked billed summaries and first subsequent requests".into(),
                "the next request starts from the resumed prefix, cold, and later requests reuse it within the documented 30m billed Codex lifetime; a session's final turn pays for its summary and gets nothing back".into(),
                "only added compactions are modeled; every observed compaction is retained once, a turn followed by one adds none, and a threshold above the current one or off cannot be replayed, since the detail recorded summaries dropped is gone".into(),
                "ordinary requests, outputs, tools and incremental growth are held fixed until each observed boundary; proportional quality or behavioral improvements are not assumed".into(),
                "cold-summary sensitivity charges the whole summary as ordinary input or a 12h billed write; larger-summary sensitivity uses observed cohort maxima; neither sensitivity vetoes the aggregate estimate".into(),
                "a threshold no recorded turn reached decides nothing, and a tie with the current threshold gives no verdict".into(),
            ],
            uncertainty: uncertainty.into_iter().collect(),
            sources: SOURCES.iter().chain(&PRICE_SOURCES).map(|s| s.to_string()).collect(),
        },
        unset: control.unset,
        usable,
        window,
        baseline_cost: current_cost.expect("the current threshold always replays"),
        scenarios,
        usage_estimates: requests
            .iter()
            .filter(|r| r.time >= since)
            .filter_map(|r| options.estimates.get(&r.id).cloned())
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controls::turn_end_control;
    use crate::controls::{CompactionSources, Env, SettingsScope, compaction_controls};
    use crate::model::{
        Api, CacheContext, CheckResult, ExposureScan, IdleAttribution, Location, Origin, PartKind,
        Pricing, ScanCoverage, Usage,
    };
    use crate::replay::{AnalyzeOptions, analyze};
    use crate::test_support::{make_profile, make_request, prices};
    use serde_json::{Map, Value, json};

    const MINUTE: Millis = 60_000;
    const HOUR: Millis = 60 * MINUTE;
    const NOW: Millis = 10 * HOUR;
    const MODEL: &str = "gpt-6.1-sol";
    const FILE: &str = "/home/.codex/config.toml";

    /// Codex usage of `input` tokens, `read` of them cached and the rest written.
    fn usage(input: i64, read: i64, output: i64) -> Usage {
        let write = input - read;
        crate::reader::usage(
            &json!({
                "input_tokens": input,
                "cached_input_tokens": read,
                "cache_write_input_tokens": write,
                "input_tokens_details": {
                    "cache_write_tokens_details": { "ttl_30m_tokens": write, "ttl_12h_tokens": 0 },
                },
                "output_tokens": output,
            }),
            Harness::Codex,
        )
        .unwrap()
    }

    fn at(id: &str, session: &str, time: Millis, usage: Usage) -> Request {
        let mut r = Request {
            id: id.into(),
            time,
            end_time: time,
            ..make_request()
        };
        r.harness = Harness::Codex;
        r.api = Api::Responses;
        r.session = session.into();
        r.provider = Some("openai".into());
        r.model = Some(MODEL.into());
        r.version = Some("0.171.0".into());
        r.parts[0].model = r.model.clone();
        r.parts[0].usage = usage;
        r
    }

    fn event(id: &str, session: &str, time: Millis, kind: EventKind) -> StructuralEvent {
        let location = Location {
            file: "synthetic".into(),
            line: 1,
        };
        StructuralEvent {
            id: id.into(),
            origins: vec![Origin {
                session: session.into(),
                location: location.clone(),
            }]
            .into(),
            time,
            kind,
            location,
            request: None,
            tool_id: None,
            settings: Default::default(),
            idle_attribution: IdleAttribution::Unknown,
            shell: None,
            meta: crate::model::EventMeta {
                harness: Harness::Codex,
                session: session.into(),
                profiles: vec!["profile".into()].into(),
                model: Some(MODEL.into()),
                version: Some("0.171.0".into()),
                client: None,
                cwd: None,
                trigger: None,
                pre_tokens: None,
                post_tokens: None,
                duration_ms: None,
                usable_window: Some(200_000),
                tool: None,
                t3: None,
                pricing: Some(std::sync::Arc::new(Pricing {
                    provider: Some("openai".into()),
                    model: Some(MODEL.into()),
                    tier: Some("standard".into()),
                    speed: Some("standard".into()),
                    geo: Some("global".into()),
                })),
            }
            .into(),
        }
    }

    fn turn_end(session: &str, time: Millis) -> StructuralEvent {
        event(
            &format!("end-{session}-{time}"),
            session,
            time,
            EventKind::TurnEnd,
        )
    }

    fn config(user: Value) -> Map<String, Value> {
        user.as_object().cloned().unwrap()
    }

    /// A Codex profile whose config holds `user`.
    fn profile(user: Value) -> Profile {
        let user = config(user);
        let mut p = make_profile("profile", "/home/.codex");
        p.harness = Harness::Codex;
        p.settings_file = FILE.into();
        p.idle = None;
        p.compaction = compaction_controls(&CompactionSources {
            harness: Harness::Codex,
            user: &user,
            managed: &Map::new(),
            env: &Env::default(),
            file: FILE,
            managed_file: "managed.json",
            scopes: None,
        })
        .unwrap();
        p.turn_end = Some(
            turn_end_control(
                &[SettingsScope {
                    config: &user,
                    file: FILE,
                }],
                FILE,
            )
            .unwrap(),
        );
        p
    }

    /// One recorded compaction, whose 2k-token summary resumes to a 20k-token prefix, and a
    /// session under a 180k-token limit, 90% of its 200k usable window: a turn that ends
    /// with 181k tokens of context, then, `gap` later, a turn of four requests.
    fn data(gap: Millis, user: Value) -> Scan {
        let mut summary = at("summary", "sample", -1000, usage(100_000, 0, 2000));
        summary.parts[0].kind = PartKind::Compaction;
        let mut boundary = event("boundary", "sample", 0, EventKind::Compaction);
        boundary.request = Some("summary".into());
        let mut requests = vec![
            summary,
            at("resume", "sample", 1000, usage(20_000, 0, 10)),
            at("first", "session", HOUR, usage(179_000, 0, 2000)),
            at("return", "session", HOUR + gap, usage(180_000, 0, 1000)),
        ];
        if gap < 30 * MINUTE {
            requests[3].parts[0].usage = usage(180_000, 179_000, 1000);
        }
        for i in 1..=3 {
            requests.push(at(
                &format!("later-{i}"),
                "session",
                HOUR + gap + i * MINUTE,
                usage(180_000, 180_000, 1000),
            ));
        }
        let n = requests.len() as u64;
        Scan {
            subagents: vec![],
            t3: vec![],
            sessions: Default::default(),
            defaults: false,
            snapshot_started_at: crate::safe::iso(0),
            snapshot_finished_at: crate::safe::iso(0),
            since: None,
            roots: vec![],
            profiles: vec![profile(user)],
            snapshots: vec![],
            requests,
            events: vec![
                boundary,
                turn_end("session", HOUR + 1000),
                turn_end("session", HOUR + gap + 3 * MINUTE + 1000),
            ],
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

    fn limit() -> Value {
        json!({ "model_auto_compact_token_limit": 180_000 })
    }

    fn report(scan: &Scan) -> crate::model::Report {
        analyze(AnalyzeOptions {
            scan: scan.clone(),
            catalog: &prices(),
            since: None,
            now: NOW,
            max_cost_per_use_usd: 1.0,
        })
        .unwrap()
    }

    /// The line under the current auto-compact limit, whatever its own line recommends.
    fn check(scan: &Scan) -> TurnEndCheck {
        let catalog = prices();
        let mut scan = scan.clone();
        let summaries = crate::replay::compaction_charges(&mut scan, &catalog);
        let estimates = crate::estimates::estimate_usage(&scan);
        let mut checks = turn_end_checks(&TurnEndOptions {
            scan: &scan,
            catalog: &catalog,
            summaries: &summaries,
            estimates: &estimates,
            windows: &[],
            since: None,
            now: NOW,
        });
        assert_eq!(checks.len(), 1);
        checks.remove(0)
    }

    fn at_percent(check: &TurnEndCheck, percent: i64) -> &WindowScenario {
        check
            .scenarios
            .iter()
            .find(|s| s.window == percent)
            .unwrap()
    }

    fn near(actual: Option<f64>, expected: f64) {
        let actual = actual.expect("priced");
        assert!(
            (actual - expected).abs() < 1e-9,
            "{actual} is not {expected}"
        );
    }

    fn usd(cost: &crate::model::Cost) -> Option<f64> {
        cost.total_usd.map(crate::model::Usd::to_f64)
    }

    // gpt-6.1-sol costs $2 a million input tokens, $0.10 read, $2.50 written and $10 output.
    #[test]
    fn a_cold_return_makes_the_summary_pay_and_the_limit_and_tiers_are_the_candidates() {
        let c = check(&data(HOUR, limit()));
        // The 180k limit is 90% of the usable window; the 272k price tier is beyond it.
        let percents: Vec<i64> = c.scenarios.iter().map(|s| s.window).collect();
        assert_eq!(percents, [0, 90, 100]);
        assert_eq!((c.base.current, c.unset), (Some(0), true));
        assert_eq!(c.window, Some(180_000));
        let on = at_percent(&c, 90);
        // The summary reads the turn's 181k tokens as ordinary input, since its only
        // request read nothing from the cache, and writes 2k: $0.362 + $0.02.
        assert_eq!(on.added.len(), 1);
        assert_eq!(
            (on.added[0].input_tokens, on.added[0].prefix_tokens),
            (181_000, 20_000)
        );
        near(usd(&on.added[0].summary_cost), 0.382);
        // Off, the return writes 180k tokens and three requests read them: $0.45 + $0.054.
        // On, it writes the 20k prefix and they read it: $0.05 + $0.006. The session's last
        // turn ends at 21k tokens, under the threshold, so it adds no summary.
        near(on.savings_usd, 0.504 - 0.056 - 0.382);
        assert_eq!(on.affected_requests, 4);
        assert_eq!(
            (c.base.action, c.base.recommended),
            (Action::Change, Some(90))
        );
        near(c.base.savings_usd, 0.066);
        // Nothing reaches the whole usable window, so 100% decides nothing.
        assert_eq!(at_percent(&c, 100).affected_requests, 0);
        assert!(c.base.baseline.starts_with(
            "turn-end compaction off, applied to recorded requests under the current 180000-token auto-compact limit"
        ));
    }

    #[test]
    fn a_warm_return_keeps_it_off_and_a_warm_summary_reads_the_cache() {
        let c = check(&data(10 * MINUTE, limit()));
        // Off, the return reads 179k tokens and writes 1k, then three requests read 180k:
        // $0.0204 + $0.054. On costs the same $0.056 as after a cold return, and the summary.
        near(at_percent(&c, 90).savings_usd, 0.0744 - 0.056 - 0.382);
        assert_eq!((c.base.action, c.base.recommended), (Action::Keep, Some(0)));
        assert_eq!(c.base.savings_usd, Some(0.0));

        // A turn whose last request read 170k tokens from the cache summarizes them at the
        // read rate: 170k read, 11k ordinary and 2k written.
        let mut warm = data(HOUR, limit());
        warm.requests[2].parts[0].usage = usage(179_000, 170_000, 2000);
        let c = check(&warm);
        near(
            usd(&at_percent(&c, 90).added[0].summary_cost),
            0.017 + 0.022 + 0.02,
        );
        // The cold-summary sensitivity charges all 181k as a 12h write instead.
        let cold = at_percent(&c, 90).sensitivity.cold_summary_savings_usd;
        assert!(cold < at_percent(&c, 90).savings_usd);
    }

    #[test]
    fn the_window_lines_recommendation_replays_first() {
        // The window line finds a 20k-token limit cheaper, which is 10% of the usable window
        // and compacts the return before a turn-end threshold could.
        let report = report(&data(HOUR, limit()));
        let c = report
            .checks
            .iter()
            .find_map(|c| match c {
                CheckResult::TurnEnd(c) => Some(c),
                _ => None,
            })
            .expect("a Codex profile has a turn-end line");
        assert_eq!(c.window, Some(20_000));
        let percents: Vec<i64> = c.scenarios.iter().map(|s| s.window).collect();
        assert_eq!(percents, [0, 10, 100]);
        assert!(
            c.base
                .baseline
                .contains("under the auto-compact window line's recommended 20000-token limit")
        );
    }

    #[test]
    fn a_turn_end_summary_leaves_what_the_next_turn_added() {
        // The turn ends with 150k of input and 44.5k of output, 194.5k of context, past 97%
        // of the usable window. The next turn's first request carries 500 tokens more.
        let mut s = data(HOUR, json!({ "model_auto_compact_token_limit": 195_000 }));
        s.requests[2].parts[0].usage = usage(150_000, 0, 44_500);
        s.requests[3].parts[0].usage = usage(195_000, 0, 1000);
        for r in &mut s.requests[4..] {
            r.parts[0].usage = usage(195_000, 195_000, 1000);
        }
        let c = check(&s);
        let on = at_percent(&c, 97);
        assert_eq!(on.added[0].input_tokens, 194_500);
        // Off, the return writes 195k tokens and three requests read them: $0.4875 + $0.0585.
        // On, the 20k prefix and the 500 new tokens are written and read: $0.05125 +
        // $0.00615, after a summary of 194.5k ordinary tokens and 2k of output: $0.409.
        near(on.savings_usd, 0.546 - 0.0574 - 0.409);
    }

    #[test]
    fn a_turn_end_that_compacts_first_spares_the_limits_compaction() {
        // The next turn opens over the 180k limit, so off compacts there: a cold summary of
        // 185k tokens, $0.37 + $0.02, then the 20k prefix written and read, $0.05 + $0.006.
        let mut s = data(HOUR, limit());
        s.requests[3].parts[0].usage = usage(185_000, 0, 1000);
        for r in &mut s.requests[4..] {
            r.parts[0].usage = usage(185_000, 185_000, 1000);
        }
        let c = check(&s);
        let off = at_percent(&c, 0);
        assert_eq!(off.added.len(), 1);
        assert_eq!(off.added[0].input_tokens, 185_000);
        // At 90% the turn's end summarizes its 181k tokens first, $0.382, the next request
        // opens at 24k, under the limit, and that is written and read: $0.06 + $0.0072.
        let on = at_percent(&c, 90);
        assert_eq!(on.added.len(), 1);
        assert_eq!(on.added[0].input_tokens, 181_000);
        near(on.savings_usd, 0.446 - 0.4492);
        assert_eq!((c.base.action, c.base.recommended), (Action::Keep, Some(0)));

        // A warm turn's end and the limit can summarize the same 181k tokens to the same
        // prefix before the same request, and still differ: the limit's summary reads them
        // cold, $0.382, and the turn's end reads 170k from the cache, $0.059.
        s.requests[2].parts[0].usage = usage(179_000, 170_000, 2000);
        s.requests[3].parts[0].usage = usage(181_000, 0, 1000);
        for r in &mut s.requests[4..] {
            r.parts[0].usage = usage(181_000, 181_000, 1000);
        }
        let c = check(&s);
        near(at_percent(&c, 90).savings_usd, 0.382 - 0.059);
        assert_eq!(
            (c.base.action, c.base.recommended),
            (Action::Change, Some(90))
        );
    }

    #[test]
    fn a_model_switch_after_the_turn_or_a_limit_the_replay_cannot_model_stays_unpriced() {
        // The summary runs on the model that finished the turn, which the next request's
        // model cannot calibrate, so the threshold is unresolved rather than untouched.
        let mut switched = data(HOUR, limit());
        for r in &mut switched.requests[3..] {
            r.model = Some("gpt-6.1-luna".into());
            r.parts[0].model = r.model.clone();
        }
        let c = check(&switched);
        let on = at_percent(&c, 90);
        assert!(on.added.is_empty());
        assert_eq!(on.savings_usd, None);
        assert_eq!(
            (c.base.action, c.base.recommended),
            (Action::Advisory, None)
        );
        assert!(
            c.base
                .uncertainty
                .iter()
                .any(|u| u.contains("changed after a turn whose end compacts")),
            "{:?}",
            c.base.uncertainty
        );

        let mut user = limit();
        user["model_auto_compact_token_limit_scope"] = json!("body_after_prefix");
        let c = check(&data(HOUR, user));
        assert!(c.scenarios.iter().all(|s| s.savings_usd.is_none()));
        assert_eq!(
            (c.base.action, c.base.recommended),
            (Action::Advisory, None)
        );
    }

    #[test]
    fn a_final_turn_pays_for_its_summary_and_gets_nothing_back() {
        let mut s = data(HOUR, limit());
        s.requests.truncate(3);
        s.events.truncate(2);
        let c = check(&s);
        let on = at_percent(&c, 90);
        assert_eq!(on.added.len(), 1);
        assert_eq!(on.added[0].prefix_tokens, 0);
        assert_eq!(&*on.added[0].request, "first");
        near(on.savings_usd, -0.382);
        assert_eq!((c.base.action, c.base.recommended), (Action::Keep, Some(0)));

        // A turn that never ended, such as one interrupted, compacts nothing.
        s.events.truncate(1);
        let c = check(&s);
        assert!(at_percent(&c, 90).added.is_empty());
        assert_eq!(
            (c.base.action, c.base.recommended),
            (Action::Advisory, None)
        );
    }

    #[test]
    fn a_recorded_compaction_after_the_turn_stands_in_for_the_modeled_one() {
        let mut s = data(HOUR, limit());
        s.events.push(event(
            "recorded",
            "session",
            HOUR + 2000,
            EventKind::Compaction,
        ));
        // The session resumes from the recorded summary, far under the threshold.
        for r in &mut s.requests[3..] {
            r.parts[0].usage = usage(30_000, 0, 1000);
        }
        let c = check(&s);
        assert!(at_percent(&c, 90).added.is_empty());
        assert_eq!(c.base.recommended, None);
    }

    #[test]
    fn no_verdict_without_a_turn_at_a_threshold_a_usable_window_or_a_sample() {
        // Every turn ends under 90% of the usable window.
        let mut small = data(HOUR, limit());
        for r in &mut small.requests[2..] {
            r.parts[0].usage = usage(170_000, 0, 2000);
        }
        let c = check(&small);
        assert!(at_percent(&c, 90).added.is_empty());
        assert_eq!(
            (c.base.action, c.base.recommended),
            (Action::Advisory, None)
        );

        // No transcript records the usable window, so only the current threshold replays.
        let mut unrecorded = data(HOUR, limit());
        for e in &mut unrecorded.events {
            e.usable_window = None;
        }
        let c = check(&unrecorded);
        assert!(c.usable.is_empty());
        assert_eq!(c.scenarios.len(), 1);
        assert_eq!(
            (c.base.action, c.base.recommended),
            (Action::Advisory, None)
        );

        // No recorded compaction calibrates a summary, so the threshold stays unpriced.
        let mut uncalibrated = data(HOUR, limit());
        uncalibrated.events.remove(0);
        let c = check(&uncalibrated);
        assert_eq!(at_percent(&c, 90).savings_usd, None);
        assert_eq!(
            (c.base.action, c.base.recommended),
            (Action::Advisory, None)
        );
    }

    #[test]
    fn an_unsupported_client_or_another_file_that_sets_it_stays_advisory() {
        let mut old = data(HOUR, limit());
        for r in &mut old.requests {
            r.version = Some("0.155.0".into());
        }
        let c = check(&old);
        assert_eq!(
            (c.base.action, c.base.recommended),
            (Action::Advisory, Some(90))
        );
        assert!(c.base.uncertainty.iter().any(|u| u.contains("v0.156.0")));

        let mut shared = data(HOUR, limit());
        shared.profiles[0].turn_end.as_mut().unwrap().unresolved =
            Some("a named profile can set it".into());
        let c = check(&shared);
        assert_eq!(
            (c.base.action, c.base.recommended),
            (Action::Advisory, Some(90))
        );
    }

    #[test]
    fn a_threshold_in_force_replays_only_lower_ones() {
        let mut user = limit();
        user[TURN_END_KEY] = json!(95);
        let c = check(&data(HOUR, user.clone()));
        let percents: Vec<i64> = c.scenarios.iter().map(|s| s.window).collect();
        assert_eq!(percents, [90, 95]);
        assert_eq!((c.base.current, c.unset), (Some(95), false));
        assert_eq!(
            (c.base.action, c.base.recommended),
            (Action::Change, Some(90))
        );
        assert!(c.base.baseline.starts_with("turn-end compaction at 95%"));

        // At the limit's own percentage, nothing lower is a candidate.
        user[TURN_END_KEY] = json!(90);
        let c = check(&data(HOUR, user));
        assert_eq!(c.scenarios.len(), 1);
        assert_eq!(
            (c.base.action, c.base.recommended),
            (Action::Advisory, None)
        );
    }

    #[test]
    fn doctor_names_the_line_and_offers_the_config_edit() {
        let scan = data(HOUR, limit());
        let mut report = report(&scan);
        let line = report
            .checks
            .iter()
            .position(|c| matches!(c, CheckResult::TurnEnd(_)))
            .unwrap();
        report.checks[line] = CheckResult::TurnEnd(check(&scan));
        let fixes = crate::doctor::fixes(&report, false);
        let fix = fixes
            .iter()
            .find(|f| f.name == "Turn-end compaction")
            .expect("a change is offered");
        assert_eq!(fix.value, "off (default) -> 90%");
        let crate::doctor::How::Edit(patch) = &fix.how else {
            panic!("{:?} is not an edit", fix.how);
        };
        assert_eq!((patch.file.as_str(), patch.harness), (FILE, Harness::Codex));
        assert_eq!(patch.edits.len(), 1);
        assert_eq!(patch.edits[0].path, [TURN_END_KEY]);
        assert_eq!(patch.edits[0].value, json!(90));

        let text = crate::report::rendered(&report, crate::report::ReportOptions::default());
        let line = text
            .lines()
            .find(|l| l.contains("Turn-end compaction"))
            .expect("the line is shown");
        assert!(line.contains("off (default) -> 90%"), "{line}");
        assert!(
            line.contains(&format!("{TURN_END_KEY} = 90 in {FILE}")),
            "{line}"
        );
        let mut explained = fixes.clone();
        let listing =
            crate::report::format_report(&report, &fixes, crate::report::ReportOptions::default());
        crate::doctor::explain(&mut explained, &listing);
        let fix = explained
            .iter()
            .find(|f| f.name == "Turn-end compaction")
            .unwrap();
        assert!(
            fix.evidence.contains("net of 1 added summary"),
            "{}",
            fix.evidence
        );
        assert!(
            fix.evidence.contains("under the 180k auto-compact limit"),
            "{}",
            fix.evidence
        );
    }

    #[test]
    fn the_setting_is_a_whole_percentage_the_user_file_owns() {
        let resolve = |system: Value, user: Value| {
            let (system, user) = (config(system), config(user));
            turn_end_control(
                &[
                    SettingsScope {
                        config: &system,
                        file: "/etc/codex/config.toml",
                    },
                    SettingsScope {
                        config: &user,
                        file: FILE,
                    },
                ],
                FILE,
            )
        };
        let unset = resolve(json!({}), json!({})).unwrap();
        assert_eq!((unset.percent, unset.unset), (0, true));
        assert_eq!(unset.owner, format!("{FILE}:{TURN_END_KEY}"));
        let system = resolve(json!({ TURN_END_KEY: 60 }), json!({})).unwrap();
        assert_eq!((system.percent, system.unset), (60, false));
        let user = resolve(json!({ TURN_END_KEY: 60 }), json!({ TURN_END_KEY: 0 })).unwrap();
        assert_eq!((user.percent, user.unset), (0, false));
        for invalid in [json!(101), json!(-1), json!(50.5), json!("50")] {
            let error = resolve(json!({}), json!({ TURN_END_KEY: invalid })).unwrap_err();
            assert!(error.to_string().contains("turn-end compaction threshold"));
        }
    }
}
