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
    Action, CheckBase, CompactionCharge, CompactionCheck, Cost, EventKind, Evidence, Harness, Id,
    Millis, PerBucket, Profile, Request, Scan, StructuralEvent, Text, TurnEndCheck, UsableWindow,
    UsageEstimate, WindowScenario,
};
use crate::prices::{
    Catalog, PRICE_SOURCES, Signs, Sum, TIER_SENSITIVITY, TIER_SIGNS, server_cost, tier_sensitive,
};
use crate::scenarios::{PartReplay, ReplayOptions, ReplayTtls};
use crate::version::Needs;

const SOURCES: [&str; 3] = [
    "https://developers.openai.com/codex/config-reference (2026-11-23)",
    "https://github.com/openai/codex/blob/rust-v0.171.0/codex-rs/core/src/session/context_window.rs (2026-11-23)",
    "https://developers.openai.com/api/docs/guides/compaction (2026-11-07)",
];

/// The Codex release that added the setting, v0.156.0.
const TURN_END: Needs = Needs::codex(156, 0);

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

/// A threshold that is set, with no selected request to replay it against.
fn unmeasured(
    group: &crate::rows::Group<'_>,
    control: &crate::model::TurnEndControl,
    since: Option<Millis>,
) -> Option<TurnEndCheck> {
    if control.unset {
        return None;
    }
    let mut base = CheckBase::unmeasured(group, control.percent, control.owner.clone(), since);
    base.sources = SOURCES.iter().map(|s| s.to_string()).collect();
    base.uncertainty.extend(control.unresolved.clone());
    Some(TurnEndCheck {
        base,
        unset: false,
        usable: vec![],
        window: None,
        window_offered: false,
        baseline_cost: Cost::default(),
        scenarios: vec![],
        usage_estimates: vec![],
    })
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
    let Some(first_selected) = selected.iter().map(|r| r.time).min() else {
        return unmeasured(group, control, options.since);
    };
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

    // The limit is the window line's recommendation, as `doctor` applies it first.
    let window_check = options
        .windows
        .iter()
        .find(|c| c.base.profiles.contains(&profile.id));
    let offered = window_check
        .filter(|c| c.base.action == Action::Change)
        .and_then(|c| c.base.recommended);
    let window = offered.or(profile.compaction.default.value);
    let index = WindowEvents::new(events.iter().copied());
    let turns = TurnEnds::new(
        events.iter().copied(),
        usable.iter().map(|u| (u.model.clone(), u.tokens)).collect(),
        index.carried(&requests),
    );
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
    let acts: Vec<(bool, Signs)> = runs[0].iter().map(|r| (r.acts, r.signs)).collect();
    let scenarios = scenarios(&percents, runs);
    let acting: Vec<(&WindowScenario, Signs)> = scenarios
        .iter()
        .zip(acts)
        .filter_map(|(s, (acts, signs))| acts.then_some((s, signs)))
        .collect();
    let conditional = scenarios
        .iter()
        .any(|s| tier_sensitive(&s.baseline_comparison_cost) || tier_sensitive(&s.comparison_cost));
    let mut best: Option<(&WindowScenario, Signs)> = None;
    for &(s, signs) in acting.iter().filter(|(s, _)| s.savings_usd.is_some()) {
        // The higher threshold wins a tie, since it summarizes less.
        if best.is_none_or(|(b, _)| s.savings_usd >= b.savings_usd) {
            best = Some((s, signs));
        }
    }
    let winning = best.filter(|(b, _)| b.savings_usd.is_some_and(|s| s > 0.0));
    let every_priced = acting.iter().all(|(s, _)| s.savings_usd.is_some());
    let losing = best.is_some_and(|(b, _)| b.savings_usd.is_some_and(|s| s < 0.0)) && every_priced;
    // Priced at an assumed tier, a verdict holds only when no tier could reverse it.
    let settled = !conditional
        || match winning {
            Some((_, signs)) => signs.never_loses(),
            None => acting.iter().all(|(_, signs)| signs.never_saves()),
        };
    let winning = winning.map(|(w, _)| w);
    // A tie, or a threshold that could not be priced beside none that wins, gives no verdict.
    let recommended = winning
        .map(|w| w.window)
        .or(losing.then_some(current))
        .filter(|_| settled);
    // Each profile's next session runs its latest install, and one file sets it for all.
    let versions = crate::version::latest(&group.members, requests.iter().copied());
    let supported = !versions.is_empty() && TURN_END.unmet(versions.iter().copied()).is_none();
    let applicable = control.unresolved.is_none() && supported;
    let upgrade = TURN_END
        .upgrade(versions.iter().copied())
        .filter(|_| control.unresolved.is_none());
    let savings = winning
        .and_then(|w| w.savings_usd)
        .or(recommended.map(|_| 0.0))
        .filter(|_| settled);
    let margin = winning.and_then(|w| w.margin).filter(|_| settled);

    let mut uncertainty: IndexSet<String> = [
        "compaction loses detail; re-reading files, repeated tool calls or changed reasoning may erase savings; that work is unmeasured, not priced at zero",
        "which turns compact and what they resume to are estimated from token counts and recorded compactions, not observed counterfactual behavior; no samples means affected thresholds remain unpriced",
    ]
    .map(String::from)
    .into_iter()
    .collect();
    if !settled {
        uncertainty.insert(TIER_SENSITIVITY.into());
    }
    uncertainty.extend(control.unresolved.clone());
    if !supported {
        uncertainty.insert(format!(
            "current client support for {TURN_END_KEY} is not established (requires {TURN_END})"
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
        .or(best.map(|(b, _)| b))
        .or_else(|| scenarios.iter().find(|s| s.window == current))
        .map(|s| s.coverage);
    let after_window = match (offered, window) {
        (Some(window), _) => {
            format!("the auto-compact window line's recommended {window}-token limit")
        }
        (None, Some(window)) => format!("the current {window}-token auto-compact limit"),
        (None, None) => "no auto-compact limit, which no setting names".into(),
    };
    let state = if current == 0 {
        "turn-end compaction off".to_string()
    } else {
        format!("turn-end compaction at {current}%")
    };
    let mut assumptions: Vec<String> = vec![
                "a turn's end compacts when its final request's input and output reach the threshold's share of the model's usable window, the window Codex last recorded for the model, or the auto-compact limit, which any threshold but off also compacts at; the limit still applies before each request in the same replay, so a turn's end that compacts first spares the limit's compaction before the next request".into(),
                "candidate thresholds are the current one and, as percentages of each model's usable window, its catalog price tiers and the auto-compact limit; this is not an exhaustive optimizer over every percentage".into(),
                "a turn-end summary replaces only the context the turn finished with, so what the next turn adds stays, and a next request on another model, agent, provider or speed leaves it unpriced; it reads that context while its cache is warm: what the turn's final request read from the cache is read again, and the rest is ordinary input; output and the resumed prefix use cohort medians from linked billed summaries and first subsequent requests".into(),
                "the next request starts from the resumed prefix, cold, and later requests reuse it within the documented 30m billed Codex lifetime; a session's final turn pays for its summary and gets nothing back".into(),
                "only added compactions are modeled; every observed compaction is retained once, a turn followed by one adds none, and a threshold above the current one or off cannot be replayed, since the detail recorded summaries dropped is gone".into(),
                "ordinary requests, outputs, tools and incremental growth are held fixed until each observed boundary; proportional quality or behavioral improvements are not assumed".into(),
                "cold-summary sensitivity charges the whole summary as ordinary input or a 12h billed write; larger-summary sensitivity uses observed cohort maxima; neither sensitivity vetoes the aggregate estimate".into(),
                "a threshold no recorded turn reached decides nothing, and a tie with the current threshold gives no verdict".into(),
            ];
    if window.is_none() {
        assumptions.push("with no auto-compact limit set, Codex's built-in limit is not recorded: the compactions it caused are in the transcripts and stay charged, a turn the session carried on from without one ended under it, and a session's final turn under its threshold is decided only when it ended with no more context than some request the model carried into a next request on the same model".into());
    }
    if conditional && recommended.is_some() {
        assumptions.push(TIER_SIGNS.into());
    }
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
            assumptions,
            uncertainty: uncertainty.into_iter().collect(),
            sources: SOURCES.iter().chain(&PRICE_SOURCES).map(|s| s.to_string()).collect(),
            upgrade,
        },
        unset: control.unset,
        usable,
        window,
        window_offered: offered.is_some(),
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
            file: None,
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
            reading: None,
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

    /// Add a session of one turn on `model`, whose usable window is `usable`.
    fn other_model(scan: &mut Scan, model: &str, usable: Option<i64>, time: Millis) {
        let mut request = at(&format!("only-{model}"), model, time, usage(1000, 0, 10));
        request.model = Some(model.into());
        request.parts[0].model = request.model.clone();
        scan.requests.push(request);
        let mut end = turn_end(model, time + 1000);
        end.meta = std::sync::Arc::new(crate::model::EventMeta {
            model: Some(model.into()),
            usable_window: usable,
            ..(*end.meta).clone()
        });
        scan.events.push(end);
    }

    /// `scan`'s report with its turn-end line replaced by `check`.
    fn report_of(scan: &Scan, check: TurnEndCheck) -> crate::model::Report {
        let mut report = report(scan);
        let line = report
            .checks
            .iter()
            .position(|c| matches!(c, CheckResult::TurnEnd(_)))
            .unwrap();
        report.checks[line] = CheckResult::TurnEnd(check);
        report
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
        // Any threshold but off also compacts a turn that ends at the limit, so 100% does
        // what 90% does, and the higher of the two wins the tie.
        let whole = at_percent(&c, 100);
        assert_eq!((whole.added.len(), whole.affected_requests), (1, 4));
        assert_eq!(whole.savings_usd, on.savings_usd);
        assert_eq!(
            (c.base.action, c.base.recommended),
            (Action::Change, Some(100))
        );
        near(c.base.savings_usd, 0.066);
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

    /// `scan` as native Codex records it: no service tier or speed on any request or summary.
    fn untiered(mut scan: Scan) -> Scan {
        for p in scan.requests.iter_mut().flat_map(|r| &mut r.parts) {
            p.tier = None;
            p.speed = None;
        }
        for e in &mut scan.events {
            let mut meta = (*e.meta).clone();
            meta.pricing = meta.pricing.map(|p| {
                std::sync::Arc::new(Pricing {
                    tier: None,
                    speed: None,
                    ..(*p).clone()
                })
            });
            e.meta = std::sync::Arc::new(meta);
        }
        scan
    }

    #[test]
    fn an_assumed_tier_decides_only_when_every_session_goes_one_way() {
        // A cold return saves at any tier, and a warm one loses at any tier.
        let cold = check(&untiered(data(HOUR, limit())));
        assert_eq!(
            (cold.base.action, cold.base.recommended),
            (Action::Change, Some(100))
        );
        near(cold.base.savings_usd, 0.066);
        assert!(cold.base.assumptions.iter().any(|s| s == TIER_SIGNS));
        let warm = check(&untiered(data(10 * MINUTE, limit())));
        assert_eq!(
            (warm.base.action, warm.base.recommended),
            (Action::Keep, Some(0))
        );
        assert!(warm.base.assumptions.iter().any(|s| s == TIER_SIGNS));
        let options = crate::report::ReportOptions {
            details: true,
            ..Default::default()
        };
        let text = crate::report::rendered(&report_of(&data(HOUR, limit()), cold), options);
        assert!(
            text.contains("dollars are at an assumed service tier"),
            "{text}"
        );

        // A summary priced at the sample's requested fast tier scales apart from the
        // session's requests at the default tier, so its cost cannot pool with their saving.
        let mut fast_sample = untiered(data(HOUR, limit()));
        fast_sample.requests[0].requested_speed = Some(crate::model::RequestedSpeed {
            tier: Some("fast".into()),
            speed: None,
        });
        let c = check(&fast_sample);
        assert_eq!(
            usd(&at_percent(&c, 90).added[0].summary_cost).map(|s| (s * 1e6).round() / 1e6),
            Some(0.764)
        );
        assert_eq!(
            (c.base.action, c.base.recommended),
            (Action::Advisory, None)
        );
        assert!(c.base.uncertainty.iter().any(|s| s == TIER_SENSITIVITY));

        // Both sessions together lose $0.3636 - $0.066 at recorded tiers, so off stays; at
        // an assumed tier, a dearer one on the cold session could reverse that.
        let mut both = data(HOUR, limit());
        let returned = data(10 * MINUTE, limit());
        for mut r in returned.requests {
            if &*r.session == "session" {
                r.id = format!("warm-{}", r.id).into();
                r.session = "warm".into();
                both.requests.push(r);
            }
        }
        both.requests.sort_by_key(|r| r.time);
        both.events.push(turn_end("warm", HOUR + 1000));
        both.events
            .push(turn_end("warm", HOUR + 10 * MINUTE + 3 * MINUTE + 1000));
        let n = both.requests.len() as u64;
        both.coverage = ScanCoverage {
            requests: n,
            source_known: n,
            tier_known: n,
            ..ScanCoverage::default()
        };
        let recorded = check(&both);
        assert_eq!(
            (recorded.base.action, recorded.base.recommended),
            (Action::Keep, Some(0))
        );
        let assumed = check(&untiered(both));
        assert_eq!(
            (assumed.base.action, assumed.base.recommended),
            (Action::Advisory, None)
        );
        assert_eq!(assumed.base.savings_usd, None);
        assert!(
            assumed
                .base
                .uncertainty
                .iter()
                .any(|s| s == TIER_SENSITIVITY)
        );
        assert!(!assumed.base.assumptions.iter().any(|s| s == TIER_SIGNS));
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
        assert_eq!((c.window, c.window_offered), (Some(20_000), true));
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
            (Action::Change, Some(100))
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
        // Its 181k tokens are past the limit, which 100% compacts at a turn's end too.
        near(at_percent(&c, 100).savings_usd, -0.382);
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
    fn a_final_turn_under_an_unrecorded_built_in_limit_is_undecided() {
        // No limit is set, so Codex's built-in one applies and no transcript records it.
        // The 272k price tier caps at 100%, and the final turn ends at 181k, under it:
        // whether it compacts depends on a limit no request shows it to be under.
        let mut s = data(HOUR, json!({}));
        s.requests.truncate(3);
        s.events.truncate(2);
        let c = check(&s);
        assert_eq!(c.window, None);
        let whole = at_percent(&c, 100);
        assert_eq!(whole.savings_usd, None);
        assert!(
            whole
                .comparison_cost
                .unknown
                .iter()
                .any(|u| u.contains("Codex's built-in limit")),
            "{:?}",
            whole.comparison_cost.unknown
        );
        assert_eq!(c.base.recommended, None);

        // A session that carried 190k tokens into a request on another model shows nothing:
        // Codex checked that request against the other model's limit.
        let mut switched = s.clone();
        switched
            .requests
            .push(at("big", "other", 2 * HOUR, usage(189_000, 0, 1000)));
        let mut next = at(
            "bigger",
            "other",
            2 * HOUR + MINUTE,
            usage(190_000, 189_000, 10),
        );
        next.model = Some("gpt-6.1-luna".into());
        next.parts[0].model = next.model.clone();
        switched.requests.push(next);
        let c = check(&switched);
        assert_eq!(at_percent(&c, 100).savings_usd, None);
        assert_eq!(c.base.recommended, None);

        // Another session carried 190k tokens into its next request with no compaction
        // between, so the built-in limit is above 181k and the final turn compacts nothing.
        s.requests
            .push(at("big", "other", 2 * HOUR, usage(189_000, 0, 1000)));
        s.requests.push(at(
            "bigger",
            "other",
            2 * HOUR + MINUTE,
            usage(190_000, 189_000, 10),
        ));
        let c = check(&s);
        let whole = at_percent(&c, 100);
        assert!(whole.added.is_empty());
        assert_eq!(whole.comparison_cost.unknown, Vec::<String>::new());
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
            (Action::Advisory, Some(100))
        );
        assert!(
            c.base
                .uncertainty
                .iter()
                .any(|u| u.contains("Codex 0.156.0"))
        );
        let upgrade = c.base.upgrade.expect("only the version holds it back");
        assert_eq!(
            (upgrade.needs, upgrade.ran.as_deref(), upgrade.command),
            (TURN_END, Some("0.155.0"), None)
        );

        let mut shared = data(HOUR, limit());
        shared.profiles[0].turn_end.as_mut().unwrap().unresolved =
            Some("a named profile can set it".into());
        for r in &mut shared.requests {
            r.version = Some("0.155.0".into());
        }
        let c = check(&shared);
        assert_eq!(
            (c.base.action, c.base.recommended),
            (Action::Advisory, Some(100))
        );
        // An upgrade alone would not let it apply, so none is advised.
        assert_eq!(c.base.upgrade, None);

        // Another profile on the same file last ran an old install, though the latest
        // request overall came from a supported one.
        let mut grouped = data(HOUR, limit());
        let mut second = grouped.profiles[0].clone();
        second.id = "second".into();
        grouped.profiles.push(second);
        let resume = grouped
            .requests
            .iter_mut()
            .find(|r| &*r.id == "resume")
            .unwrap();
        resume.profiles = vec!["second".into()].into();
        resume.version = Some("0.155.0".into());
        let c = check(&grouped);
        assert_eq!(c.base.action, Action::Advisory);
        let upgrade = c
            .base
            .upgrade
            .expect("the second profile's install is too old");
        assert_eq!(upgrade.ran.as_deref(), Some("0.155.0"));
    }

    #[test]
    fn a_threshold_in_force_replays_only_lower_ones() {
        // A 190k limit is 95% of the session's usable window and 90% of another model's
        // 211k. The session's turn ends at 181k, under the limit, so only 90% compacts it.
        let mut user = json!({ "model_auto_compact_token_limit": 190_000 });
        user[TURN_END_KEY] = json!(95);
        let mut scan = data(HOUR, user);
        other_model(&mut scan, "gpt-6.1-terra", Some(211_000), 2 * HOUR);
        let c = check(&scan);
        let percents: Vec<i64> = c.scenarios.iter().map(|s| s.window).collect();
        assert_eq!(percents, [90, 95]);
        assert_eq!((c.base.current, c.unset), (Some(95), false));
        assert_eq!(
            (c.base.action, c.base.recommended),
            (Action::Change, Some(90))
        );
        assert!(c.base.baseline.starts_with("turn-end compaction at 95%"));

        // At the limit's own percentage, nothing lower is a candidate.
        let mut user = limit();
        user[TURN_END_KEY] = json!(90);
        let c = check(&data(HOUR, user.clone()));
        assert_eq!(c.scenarios.len(), 1);
        assert_eq!(
            (c.base.action, c.base.recommended),
            (Action::Advisory, None)
        );

        // A threshold above the limit's percentage compacts the turns the limit's does, so
        // the lower one decides nothing.
        user[TURN_END_KEY] = json!(95);
        let c = check(&data(HOUR, user));
        let percents: Vec<i64> = c.scenarios.iter().map(|s| s.window).collect();
        assert_eq!(percents, [90, 95]);
        assert_eq!(at_percent(&c, 90).savings_usd, Some(0.0));
        assert_eq!(
            (c.base.action, c.base.recommended),
            (Action::Advisory, None)
        );
    }

    #[test]
    fn an_undecided_turn_end_withholds_the_keep_verdict() {
        // After a warm return 90% loses to the 95% in force. The 190k limit is 95% of the
        // session's usable window, 92% of a second model's 206k and 90% of a third's 211k.
        // No turn ends between 92% and the limit, so 92% replays as 95% does.
        let mut user = json!({ "model_auto_compact_token_limit": 190_000 });
        user[TURN_END_KEY] = json!(95);
        let mut scan = data(10 * MINUTE, user);
        other_model(&mut scan, "gpt-6.1-terra", Some(206_000), 2 * HOUR);
        other_model(&mut scan, "gpt-6.1-vega", Some(211_000), 2 * HOUR + MINUTE);
        let c = check(&scan);
        let percents: Vec<i64> = c.scenarios.iter().map(|s| s.window).collect();
        assert_eq!(percents, [90, 92, 95]);
        assert_eq!(
            (c.base.action, c.base.recommended),
            (Action::Keep, Some(95))
        );

        // A turn that ends on a model with no recorded usable window might compact at 92%
        // and not at 95%, so 92% is no longer known to replay the same and nothing is kept.
        other_model(&mut scan, "gpt-6.1-luna", None, 3 * HOUR);
        let mut after = at(
            "after",
            "gpt-6.1-luna",
            3 * HOUR + MINUTE,
            usage(1000, 0, 10),
        );
        after.model = Some("gpt-6.1-luna".into());
        after.parts[0].model = after.model.clone();
        scan.requests.push(after);
        let c = check(&scan);
        assert_eq!(
            (c.base.action, c.base.recommended),
            (Action::Advisory, None)
        );

        // With no threshold that compacts a turn, the line says a turn end was undecided
        // rather than that none had enough context.
        scan.requests.retain(|r| &*r.session != "gpt-6.1-vega");
        scan.events.retain(|e| &*e.session != "gpt-6.1-vega");
        scan.requests.retain(|r| &*r.id != "after");
        let c = check(&scan);
        assert_eq!(c.scenarios.len(), 2);
        let options = crate::report::ReportOptions {
            details: true,
            ..Default::default()
        };
        let text = crate::report::rendered(&report_of(&scan, c), options);
        assert!(
            text.contains("whether its end compacts is unknown"),
            "{text}"
        );
        assert!(!text.contains("o recorded turn ended"), "{text}");
    }

    #[test]
    fn a_turn_end_is_decided_once_from_the_context_it_finished_with() {
        // No limit is set and the 272k price tier caps at 100%. The turn ends at 102k, and
        // the return's first part alone fills the usable window.
        let mut s = data(HOUR, json!({}));
        s.requests[2].parts[0].usage = usage(100_000, 0, 2000);
        let mut second = s.requests[3].parts[0].clone();
        second.usage = usage(1000, 0, 10);
        s.requests[3].parts[0].usage = usage(199_000, 0, 1000);
        s.requests[3].parts.push(second);
        let c = check(&s);
        let whole = at_percent(&c, 100);
        assert!(whole.added.is_empty());
        assert_eq!(whole.comparison_cost.unknown, Vec::<String>::new());
    }

    #[test]
    fn samples_that_disagree_leave_the_summary_unpriced() {
        let mut s = data(HOUR, limit());
        let mut summary = at("summary-2", "sample-2", -1000, usage(100_000, 0, 2000));
        summary.parts[0].kind = PartKind::Compaction;
        summary.parts[0].tier = Some("priority".into());
        let mut boundary = event("boundary-2", "sample-2", 0, EventKind::Compaction);
        boundary.request = Some("summary-2".into());
        s.requests.push(summary);
        s.requests
            .push(at("resume-2", "sample-2", 1000, usage(20_000, 0, 10)));
        s.events.push(boundary);
        let c = check(&s);
        let on = at_percent(&c, 90);
        assert_eq!(on.added.len(), 1);
        assert_eq!(usd(&on.added[0].summary_cost), None);
        assert_eq!(on.savings_usd, None);
        assert_eq!(c.base.recommended, None);
    }

    #[test]
    fn doctor_names_the_line_and_offers_the_config_edit() {
        let scan = data(HOUR, limit());
        let report = report_of(&scan, check(&scan));
        let fixes = crate::doctor::fixes(&report, false);
        let fix = fixes
            .iter()
            .find(|f| f.name == "Turn-end compaction")
            .expect("a change is offered");
        assert_eq!(fix.value, "off (default) -> 100%");
        let crate::doctor::How::Edit(patch) = &fix.how else {
            panic!("{:?} is not an edit", fix.how);
        };
        assert_eq!((patch.file.as_str(), patch.harness), (FILE, Harness::Codex));
        assert_eq!(patch.edits.len(), 1);
        assert_eq!(patch.edits[0].path, [TURN_END_KEY]);
        assert_eq!(patch.edits[0].value, json!(100));

        let text = crate::report::rendered(&report, crate::report::ReportOptions::default());
        let line = text
            .lines()
            .find(|l| l.contains("Turn-end compaction"))
            .expect("the line is shown");
        assert!(line.contains("off (default) -> 100%"), "{line}");
        assert!(
            text.contains(&format!("\n  EDITS  {FILE}\n"))
                && text.contains(&format!("\n    {TURN_END_KEY} = 100\n")),
            "{text}"
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
            fix.evidence.contains("under the 180k auto-compact limit,"),
            "{}",
            fix.evidence
        );
        assert!(!fix.evidence.contains("assumes"), "{}", fix.evidence);

        // The window line's edit is its own question, so a saving replayed under the limit
        // it offers says it rests on that edit.
        let mut offered = check(&scan);
        offered.window_offered = true;
        let report = report_of(&scan, offered);
        let mut fixes = crate::doctor::fixes(&report, false);
        let listing =
            crate::report::format_report(&report, &fixes, crate::report::ReportOptions::default());
        crate::doctor::explain(&mut fixes, &listing);
        let fix = fixes
            .iter()
            .find(|f| f.name == "Turn-end compaction")
            .unwrap();
        assert!(
            fix.evidence.contains(
                "under the 180k auto-compact limit the Auto-compact window line recommends, "
            ) && fix
                .evidence
                .contains("The saving assumes that limit is set too."),
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

    /// The report of a scan whose only requests are earlier history.
    fn unmeasured_report(user: Value) -> crate::model::Report {
        let mut scan = data(HOUR, user);
        scan.cache_context.requests = std::mem::take(&mut scan.requests);
        report(&scan)
    }

    /// The window and turn-end lines of that report.
    fn unmeasured_lines(user: Value) -> Vec<CheckResult> {
        unmeasured_report(user)
            .checks
            .into_iter()
            .filter(|c| matches!(c, CheckResult::Compaction(_) | CheckResult::TurnEnd(_)))
            .collect()
    }

    #[test]
    fn set_values_keep_their_lines_when_no_request_is_selected() {
        let user = json!({
            "model_auto_compact_token_limit": 180_000,
            TURN_END_KEY: 80,
        });
        let lines = unmeasured_lines(user);
        let [CheckResult::Compaction(window), CheckResult::TurnEnd(turn)] = &lines[..] else {
            panic!("expected a window line and a turn-end line, got {lines:?}");
        };
        assert_eq!(window.base.current, Some(180_000));
        assert_eq!(turn.base.current, Some(80));
        for base in [&window.base, &turn.base] {
            assert_eq!(
                (
                    base.action,
                    base.evidence,
                    base.recommended,
                    base.savings_usd
                ),
                (Action::Advisory, Evidence::Unknown, None, None)
            );
            assert_eq!(base.requests, 0);
            assert!(base.owner.starts_with(FILE));
        }
        assert!(window.scenarios.is_empty() && turn.scenarios.is_empty());

        // --details shows no replay field for a limit nothing replayed.
        let user = json!({
            "model_auto_compact_token_limit": 180_000,
            TURN_END_KEY: 80,
        });
        let text = crate::report::rendered(
            &unmeasured_report(user),
            crate::report::ReportOptions {
                details: true,
                ..Default::default()
            },
        );
        assert!(text.contains("Turn-end compaction"));
        for field in [
            "Usable window",
            "Auto-compact limit replayed with it",
            "Current-threshold baseline",
            "Current-window baseline",
            "Existing summaries",
        ] {
            assert!(!text.contains(field), "{field} shown without a replay");
        }
    }

    #[test]
    fn a_default_gets_no_line_when_no_request_is_selected() {
        assert!(unmeasured_lines(json!({})).is_empty());
        // Only the value that is set gets one.
        let lines = unmeasured_lines(json!({ TURN_END_KEY: 0 }));
        let [CheckResult::TurnEnd(turn)] = &lines[..] else {
            panic!("expected only the turn-end line, got {lines:?}");
        };
        assert_eq!(turn.base.current, Some(0));
    }
}
