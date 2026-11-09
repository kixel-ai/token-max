//! Background generation: Claude prompt suggestions, session recaps and goal check-ins, and
//! Codex automatic recaps.
//!
//! Claude Code 2.1.319 sends suggestions and recaps as forks of the main conversation with
//! `skipTranscript` and `skipCacheWrite`, so transcripts never record them. Their cost is
//! estimated at each recorded turn where Claude Code would have sent one: the conversation
//! prefix at its cache-read or expired-input rate, the last response and instruction as new
//! input, and a short reply.

use indexmap::IndexSet;

use crate::cache::{observed_ttl, ttl_milliseconds};
use crate::model::{
    Action, BackgroundCheck, BackgroundControl, BackgroundFeature, BillingPart, CheckBase, Cost,
    Coverage, EventKind, Evidence, Harness, Millis, PartKind, Profile, Request, RequestEvidence,
    Scan, Ttl, Usage,
};
use crate::prices::{
    Catalog, add_costs, paired_savings, price_components, unit_rates, unknown_cost, zero_cost,
};

const SOURCES: [&str; 8] = [
    "https://code.claude.com/docs/en/costs#background-token-usage (2026-11-08)",
    "https://code.claude.com/docs/en/interactive-mode#prompt-suggestions (2026-11-08)",
    "https://code.claude.com/docs/en/interactive-mode#session-recap (2026-11-08)",
    "https://code.claude.com/docs/en/env-vars (2026-11-08)",
    "https://learn.chatgpt.com/docs/config-file/config-reference (2026-11-08)",
    "https://code.claude.com/docs/en/settings (2026-11-08)",
    "https://code.claude.com/docs/en/settings-reference#env (2026-11-08)",
    "https://learn.chatgpt.com/docs/config-file/config-advanced (2026-11-08)",
];

const DAY: f64 = 86_400_000.0;
const MINUTE: Millis = 60_000;
/// A recap starts once the terminal has been unfocused this long after a completed turn.
const RECAP_DELAY: Millis = 3 * MINUTE;
/// Claude Code skips a suggestion when the last response wrote or sent more uncached tokens than this.
const COLD: i64 = 10_000;
/// The suggestion instruction is about 1,400 characters in Claude Code 2.1.319.
const SUGGESTION_PROMPT: i64 = 350;
/// A suggestion is a few words; thinking, if the fork inherits it, is not recorded.
const SUGGESTION_OUTPUT: i64 = 50;
/// The recap instruction is about 330 characters.
const RECAP_PROMPT: i64 = 100;
/// Recaps are capped at 400 characters.
const RECAP_OUTPUT: i64 = 100;

/// Inputs for the background generation checks.
pub struct BackgroundInput<'a> {
    pub scan: &'a Scan,
    pub catalog: &'a Catalog,
    /// The explicit window start; None uses each profile's first request.
    pub since: Option<Millis>,
    /// The window end, which monthly values scale to.
    pub now: Millis,
    /// Recommend turning a feature off when it cost more than this per recorded use.
    pub max_cost_per_use_usd: f64,
}

/// One check per distinct owning control, charging only its applicable requests.
pub fn background_checks(input: &BackgroundInput<'_>) -> Vec<BackgroundCheck> {
    let mut result = vec![];
    for profile in &input.scan.profiles {
        let requests: Vec<&Request> = input
            .scan
            .requests
            .iter()
            .filter(|r| {
                r.agent == crate::model::Agent::Main
                    && r.profiles.contains(&profile.id)
                    && input.since.is_none_or(|s| r.time >= s)
            })
            .collect();
        let mut ends = turn_ends(input.scan, profile);
        // Earlier history only establishes eligibility; charge turns inside the window.
        let selected: IndexSet<&str> = requests.iter().map(|r| r.id.as_str()).collect();
        ends.retain(|e| selected.contains(e.request.id.as_str()));
        let mut groups: indexmap::IndexMap<&BackgroundControl, (Vec<&Request>, Vec<&TurnEnd<'_>>)> =
            indexmap::IndexMap::new();
        for &request in &requests {
            for control in controls(profile, request) {
                groups.entry(control).or_default().0.push(request);
            }
        }
        for end in &ends {
            for control in controls(profile, end.request) {
                groups.entry(control).or_default().1.push(end);
            }
        }
        if requests.is_empty() {
            // Earlier requests still identify current project owners, without billing them.
            for r in input
                .scan
                .cache_context
                .requests
                .iter()
                .chain(&input.scan.requests)
                .filter(|r| {
                    r.agent == crate::model::Agent::Main && r.profiles.contains(&profile.id)
                })
            {
                for control in controls(profile, r) {
                    groups.entry(control).or_default();
                }
            }
            if groups.is_empty() {
                let unscoped = profile
                    .background_directories
                    .get("")
                    .unwrap_or(&profile.background);
                groups.extend(unscoped.iter().map(|c| (c, (vec![], vec![]))));
            }
        }
        for (control, (requests, ends)) in groups {
            result.push(check(input, profile, control, &requests, &ends));
        }
    }
    result
}

/// Discovery keeps absent working directories separate from the profile defaults.
fn controls<'a>(profile: &'a Profile, request: &Request) -> &'a [BackgroundControl] {
    profile
        .background_directories
        .get(request.cwd.as_deref().unwrap_or_default())
        .map_or(&profile.background, Vec::as_slice)
}

/// A main-conversation response that finished a turn.
struct TurnEnd<'a> {
    request: &'a Request,
    /// Whether it came from the interactive terminal, where suggestions and recaps run.
    interactive: bool,
    /// Whether the enabled suggestion feature can run after this turn.
    suggestion: bool,
    /// Whether a recap can run after this turn, including its spacing from earlier recaps.
    recap: bool,
    /// The cache lifetime the last response wrote, else the current profile's lifetime.
    ttl: Ttl,
    /// The next main-conversation request, which a cache refresh could have kept warm.
    next: Option<&'a Request>,
}

impl TurnEnd<'_> {
    /// The latest live-prefix read when the selected background features run.
    fn cache_touch(&self, suggestions: bool, recaps: bool) -> Millis {
        let mut touch = self.request.time;
        let lifetime = ttl_milliseconds(self.ttl);
        if suggestions && self.suggestion && self.request.end_time - touch < lifetime {
            touch = self.request.end_time;
        }
        let recap_time = self.request.end_time + RECAP_DELAY;
        if recaps && self.recap && recap_time - touch < lifetime {
            touch = recap_time;
        }
        touch
    }
}

/// Every turn-ending main-conversation response of a Claude profile, in session order,
/// counting responses and prompts from before the scanned window too.
fn turn_ends<'a>(scan: &'a Scan, profile: &Profile) -> Vec<TurnEnd<'a>> {
    if profile.harness != Harness::Claude {
        return vec![];
    }
    let mut sessions: indexmap::IndexMap<&str, Vec<&'a Request>> = indexmap::IndexMap::new();
    for r in scan.cache_context.requests.iter().chain(&scan.requests) {
        if r.harness == Harness::Claude
            && r.agent == crate::model::Agent::Main
            && r.profiles.contains(&profile.id)
        {
            sessions.entry(&r.session).or_default().push(r);
        }
    }
    let mut starts: indexmap::IndexMap<&str, Vec<Millis>> = indexmap::IndexMap::new();
    for e in scan.cache_context.prompts.iter().chain(&scan.events) {
        if e.kind == EventKind::TurnStart && e.profiles.contains(&profile.id) {
            starts.entry(&e.session).or_default().push(e.time);
        }
    }
    let mut result = vec![];
    for (session, mut main) in sessions {
        main.sort_by_key(|r| r.time);
        let mut prompts = starts.get(session).cloned().unwrap_or_default();
        prompts.sort_unstable();
        let mut recap_turn = 0;
        for (i, &r) in main.iter().enumerate() {
            let enabled = |feature| {
                controls(profile, r)
                    .iter()
                    .any(|c| c.feature == feature && c.value == Some(true))
            };
            let suggestions = enabled(BackgroundFeature::PromptSuggestions);
            let recaps = enabled(BackgroundFeature::SessionRecaps);
            let next = main.get(i + 1).copied();
            let prompt = prompts.iter().copied().find(|&t| t > r.time);
            // A later request before the next prompt continues the same turn.
            let ends = match (next, prompt) {
                (None, _) => true,
                (Some(next), Some(prompt)) => next.time >= prompt,
                (Some(_), None) => false,
            };
            if !ends {
                continue;
            }
            let interactive = r.client.as_deref() == Some("cli")
                && r.source
                    .as_deref()
                    .is_none_or(|s| s.starts_with("repl_main_thread"));
            let turns = prompts.iter().filter(|&&t| t <= r.time).count();
            let cold = last_message(r)
                .and_then(uncached)
                .is_some_and(|(ordinary, _, write, output)| ordinary + output + write > COLD);
            let suggestion = suggestions && interactive && i >= 1 && !cold;
            let recap = recaps
                && interactive
                && turns >= 3
                && turns >= recap_turn + 2
                && prompt.is_some_and(|p| p - r.end_time >= RECAP_DELAY);
            if recap {
                recap_turn = turns;
            }
            result.push(TurnEnd {
                request: r,
                interactive,
                suggestion,
                recap,
                ttl: last_message(r)
                    .and_then(observed_ttl)
                    .or_else(|| {
                        r.cwd.as_ref().and_then(|cwd| {
                            profile
                                .idle_directories
                                .as_ref()?
                                .get(cwd)?
                                .ttl
                                .as_ref()?
                                .main
                                .value
                        })
                    })
                    .or(profile.ttl.main.value)
                    .unwrap_or(Ttl::FiveMinutes),
                next,
            });
        }
    }
    result
}

/// The last sampled message of a request, whose context a fork reuses.
fn last_message(r: &Request) -> Option<&BillingPart> {
    if r.evidence == RequestEvidence::Unknown {
        return None;
    }
    r.parts.iter().rev().find(|p| p.kind == PartKind::Message)
}

/// Uncached input, cache writes and output of a recorded part, when all are known.
fn uncached(p: &BillingPart) -> Option<(i64, i64, i64, i64)> {
    let u = &p.usage;
    let (input, read, write, output) = (u.input?, u.read?, u.write?, u.output?);
    Some((input - read - write, read, write, output))
}

/// A fork reads the prefix while it is warm, else sends it as ordinary input without writing cache.
fn fork_cost(catalog: &Catalog, end: &TurnEnd<'_>, feature: BackgroundFeature) -> Cost {
    let r = end.request;
    let (prompt, output, delay) = match feature {
        BackgroundFeature::PromptSuggestions => (SUGGESTION_PROMPT, SUGGESTION_OUTPUT, 0),
        BackgroundFeature::SessionRecaps => (RECAP_PROMPT, RECAP_OUTPUT, RECAP_DELAY),
        _ => unreachable!("only suggestions and recaps have estimable requests"),
    };
    let Some((part, (ordinary, read, write, last))) =
        last_message(r).and_then(|p| Some((p, uncached(p)?)))
    else {
        return unknown_cost("the turn's last response has no recorded usage");
    };
    let fresh = ordinary + last + prompt;
    let touch = if feature == BackgroundFeature::SessionRecaps {
        end.cache_touch(true, false)
    } else {
        r.time
    };
    let cached = r.end_time + delay - touch < ttl_milliseconds(end.ttl);
    let usage = Usage {
        input: Some(read + write + fresh),
        output: Some(output),
        read: Some(if cached { read + write } else { 0 }),
        write: Some(0),
        write5m: Some(0),
        write1h: Some(0),
        ..Usage::default()
    };
    price_components(
        catalog,
        r,
        &BillingPart {
            usage,
            ..part.clone()
        },
    )
    .total()
}

/// The rewrite the next request would pay without this feature's cache refreshes.
fn request_warming(catalog: &Catalog, end: &TurnEnd<'_>, feature: BackgroundFeature) -> Cost {
    let Some(next) = end.next else {
        return zero_cost();
    };
    if next.cwd != end.request.cwd {
        return zero_cost();
    }
    let lifetime = ttl_milliseconds(end.ttl);
    let on = end.cache_touch(true, true);
    let off = end.cache_touch(
        feature != BackgroundFeature::PromptSuggestions,
        feature != BackgroundFeature::SessionRecaps,
    );
    if next.time - on >= lifetime || next.time - off < lifetime {
        return zero_cost();
    }
    let Some(part) = next
        .parts
        .iter()
        .find(|p| p.kind == PartKind::Message)
        .filter(|_| next.evidence != RequestEvidence::Unknown)
    else {
        return unknown_cost("the request after the recap has no recorded usage");
    };
    // Only reads of this turn's prefix in the same model, speed and provider cohort.
    let Some(source) = last_message(end.request) else {
        return unknown_cost("the turn's last response has no recorded usage");
    };
    if (&part.model, &part.speed, &next.provider)
        != (&source.model, &source.speed, &end.request.provider)
    {
        return zero_cost();
    }
    let prefix = match (source.usage.read, source.usage.write) {
        (Some(read), Some(write)) => read + write,
        _ => return unknown_cost("the turn's last response has no recorded cache usage"),
    };
    let read = part.usage.read.map(|read| read.min(prefix));
    let (Some(read), Some(rates)) = (read, unit_rates(catalog, next, part)) else {
        return unknown_cost("the request after the recap has no priced cache reads");
    };
    let write = match end.ttl {
        Ttl::FiveMinutes => rates.write5m,
        Ttl::OneHour => rates.write1h,
    };
    Cost {
        known_usd: read as f64 * (write - rates.read),
        total_usd: Some(read as f64 * (write - rates.read)),
        ..zero_cost()
    }
}

/// The extra ordinary input a recap would pay without a suggestion refreshing its prefix.
fn recap_warming(catalog: &Catalog, end: &TurnEnd<'_>) -> Cost {
    let time = end.request.end_time + RECAP_DELAY;
    let lifetime = ttl_milliseconds(end.ttl);
    if !end.recap
        || time - end.cache_touch(true, false) >= lifetime
        || time - end.request.time < lifetime
    {
        return zero_cost();
    }
    let Some(part) = last_message(end.request) else {
        return unknown_cost("the turn's last response has no recorded usage");
    };
    let (Some(read), Some(write), Some(rates)) = (
        part.usage.read,
        part.usage.write,
        unit_rates(catalog, end.request, part),
    ) else {
        return unknown_cost("the recap's cached prefix has no priced usage");
    };
    let usd = (read + write) as f64 * (rates.ordinary - rates.read);
    Cost {
        known_usd: usd,
        total_usd: Some(usd),
        ..zero_cost()
    }
}

/// Later input costs avoided by this feature's cache reads, with other features held constant.
fn lost_warming(catalog: &Catalog, end: &TurnEnd<'_>, feature: BackgroundFeature) -> Cost {
    let recap = if feature == BackgroundFeature::PromptSuggestions {
        recap_warming(catalog, end)
    } else {
        zero_cost()
    };
    add_costs(&[request_warming(catalog, end, feature), recap])
}

/// The estimated (on, off) cost of each turn where the feature would have run.
fn generations(
    catalog: &Catalog,
    feature: BackgroundFeature,
    ends: &[&TurnEnd<'_>],
) -> Vec<(Cost, Cost)> {
    let mut result = vec![];
    for end in ends.iter().filter(|e| e.interactive) {
        match feature {
            BackgroundFeature::PromptSuggestions => {
                if !end.suggestion {
                    continue;
                }
                result.push((
                    fork_cost(catalog, end, feature),
                    lost_warming(catalog, end, feature),
                ));
            }
            BackgroundFeature::SessionRecaps => {
                if !end.recap {
                    continue;
                }
                result.push((
                    fork_cost(catalog, end, feature),
                    lost_warming(catalog, end, feature),
                ));
            }
            BackgroundFeature::GoalCheckIns | BackgroundFeature::AutoRecaps => {}
        }
    }
    result
}

fn check(
    input: &BackgroundInput<'_>,
    profile: &Profile,
    control: &BackgroundControl,
    requests: &[&Request],
    ends: &[&TurnEnd<'_>],
) -> BackgroundCheck {
    let feature = control.feature;
    let measurable = matches!(
        feature,
        BackgroundFeature::PromptSuggestions | BackgroundFeature::SessionRecaps
    );
    let enabled = control.value == Some(true);
    let resolved = control.value.is_some();
    let pairs = if measurable && enabled {
        generations(input.catalog, feature, ends)
    } else {
        vec![]
    };
    let paired = paired_savings(pairs.iter().map(|(on, off)| (on, off)), 0);
    // Scale the measured net cost up to every turn where it ran, as context costs do.
    let estimate = (paired.priced > 0)
        .then(|| paired.usd * paired.total as f64 / paired.priced as f64)
        .or((paired.total == 0).then_some(0.0));
    let start = input.since.or(requests.iter().map(|r| r.time).min());
    let month = |v: Option<f64>| match (v, start) {
        (Some(v), Some(start)) if input.now > start => {
            Some((v * 30.0 * DAY) / (input.now - start) as f64)
        }
        _ => v,
    };
    // A suggestion or recap use is not recorded, so its whole cost counts as one use.
    let action = match (control.value, measurable, estimate) {
        (None, _, _) => Action::Advisory,
        (Some(false), _, _) => Action::Keep,
        (Some(true), false, _) | (Some(true), true, None) => Action::Advisory,
        (Some(true), true, Some(usd)) if usd > input.max_cost_per_use_usd => Action::Change,
        (Some(true), true, Some(_)) => Action::Keep,
    };
    let other = ends.iter().filter(|e| !e.interactive).count();
    let mut uncertainty = vec![control.reason.clone()];
    match feature {
        BackgroundFeature::PromptSuggestions => {
            uncertainty.push("assumes the terminal was focused after every eligible turn and suggestions were not throttled after many went unused".to_string());
            if other > 0 {
                uncertainty.push(format!("{other} turns from SDK or desktop clients are excluded; suggestions run there only when the client asks for them"));
            }
        }
        BackgroundFeature::SessionRecaps => uncertainty.push(
            "assumes the terminal was unfocused during every gap of three minutes or more; a final turn with no later prompt is not counted".into(),
        ),
        BackgroundFeature::GoalCheckIns => uncertainty.push(
            "check-in turns are recorded as ordinary task notifications and cannot be told apart".into(),
        ),
        BackgroundFeature::AutoRecaps => uncertainty.push(
            "Codex does not record automatic recap requests in its session files".into(),
        ),
    }
    let (cost, lost) = if !resolved || (enabled && !measurable) {
        let unknown = unknown_cost(if resolved {
            "these requests are not recorded"
        } else {
            &control.reason
        });
        (unknown.clone(), unknown)
    } else {
        (
            add_costs(pairs.iter().map(|(on, _)| on)),
            add_costs(pairs.iter().map(|(_, off)| off)),
        )
    };
    uncertainty.extend(cost.unknown.iter().cloned());
    uncertainty.extend(lost.unknown.iter().cloned());
    let history_profiles: IndexSet<&String> = requests.iter().flat_map(|r| &r.profiles).collect();
    let directories: IndexSet<Option<String>> = if requests.is_empty() {
        input
            .scan
            .cache_context
            .requests
            .iter()
            .chain(&input.scan.requests)
            .filter(|r| {
                r.agent == crate::model::Agent::Main
                    && r.profiles.contains(&profile.id)
                    && controls(profile, r).contains(control)
            })
            .map(|r| r.cwd.clone())
            .collect()
    } else {
        requests.iter().map(|r| r.cwd.clone()).collect()
    };
    BackgroundCheck {
        base: CheckBase {
            profile: profile.id.clone(),
            history_profiles: history_profiles.into_iter().cloned().collect(),
            evidence: if measurable && resolved {
                Evidence::Estimated
            } else {
                Evidence::Guidance
            },
            action,
            current: control.value,
            recommended: match action {
                Action::Change => Some(false),
                Action::Keep => control.value,
                Action::Advisory => None,
            },
            owner: control.owner.clone(),
            since: start,
            until: Some(input.now),
            window_basis: if input.since.is_none() {
                "available selected usage; current settings applied to history"
            } else {
                "explicit --since; current settings applied to history"
            }
            .into(),
            baseline: "current owning control applied only to its recorded directories, holding recorded work and all other settings constant; not a measurement of historical enablement".into(),
            requests: requests.len() as u64,
            coverage: (measurable && enabled).then_some(Coverage {
                priced: paired.priced,
                total: paired.total,
            }),
            savings_usd: if measurable && enabled { estimate } else { None },
            monthly_savings_usd: if measurable && enabled {
                month(estimate)
            } else {
                None
            },
            break_even: None,
            assumptions: [
                "the client does not log these requests; each is estimated at a recorded turn where it would have run",
                "a fork sends the conversation's prefix as a cache read before expiry or ordinary input after expiry, plus the last response and its instruction as new input, without writing cache",
                "recaps require two new user turns after the previous modeled recap, including recaps before the analysis window",
                "live-prefix reads by enabled suggestions and recaps refresh the cache; turning one feature off keeps the other's modeled requests and refreshes, including any extra recap input or later cache rewrite",
                "uses are not recorded, so the whole cost counts as one use against --max-cost-per-use",
            ]
            .map(String::from)
            .into(),
            uncertainty,
            sources: SOURCES.map(String::from).into(),
        },
        feature,
        directories: directories.into_iter().collect(),
        reason: control.reason.clone(),
        off: control.off.clone(),
        off_file: control.off_file.clone(),
        generations: (measurable && resolved).then_some(pairs.len() as u64),
        cost,
        lost_warming: lost,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CacheContext, ExposureScan, Location, ScanCoverage, StructuralEvent};
    use crate::test_support::{ClaudeUsage, claude_usage, make_profile, make_request, prices};

    fn control(feature: BackgroundFeature, value: bool) -> BackgroundControl {
        BackgroundControl {
            feature,
            value: Some(value),
            reason: "synthetic current controls applied to history".into(),
            minutes: None,
            owner: "/synthetic/.claude/settings.json:key".into(),
            off: "\"key\": false".into(),
            off_file: Some("/synthetic/.claude/settings.json".into()),
        }
    }

    fn prompt(time: Millis) -> StructuralEvent {
        StructuralEvent {
            id: format!("prompt-{time}"),
            origins: vec![],
            harness: Harness::Claude,
            session: "session".into(),
            time,
            kind: EventKind::TurnStart,
            profiles: vec!["profile".into()],
            location: Location {
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
            settings: Default::default(),
            idle_attribution: crate::model::IdleAttribution::Unknown,
            shell: None,
            pricing: None,
        }
    }

    /// An interactive response at `time` minutes with `read` cached tokens.
    fn response(time: Millis, read: i64) -> Request {
        let mut r = make_request();
        r.id = format!("r{time}");
        r.time = time * MINUTE;
        r.end_time = r.time + 1000;
        r.client = Some("cli".into());
        r.source = Some("repl_main_thread".into());
        r.parts[0].usage = claude_usage(ClaudeUsage {
            read,
            write: 100,
            input: 10,
            output: 20,
            ..ClaudeUsage::default()
        });
        r
    }

    fn scan(
        features: &[(BackgroundFeature, bool)],
        requests: Vec<Request>,
        prompts: &[Millis],
    ) -> Scan {
        let mut profile = make_profile("profile", "/synthetic/.claude");
        profile.background = features.iter().map(|&(f, v)| control(f, v)).collect();
        Scan {
            snapshot_started_at: String::new(),
            snapshot_finished_at: String::new(),
            since: None,
            roots: vec![],
            profiles: vec![profile],
            snapshots: vec![],
            requests,
            events: prompts.iter().map(|&t| prompt(t * MINUTE)).collect(),
            exposure: ExposureScan::default(),
            cache_context: CacheContext::default(),
            coverage: ScanCoverage::default(),
        }
    }

    fn run(scan: &Scan, max: f64) -> Vec<BackgroundCheck> {
        background_checks(&BackgroundInput {
            scan,
            catalog: &prices(),
            since: None,
            now: 30 * 24 * 60 * MINUTE,
            max_cost_per_use_usd: max,
        })
    }

    /// Four one-response turns: prompts at 0, 1, 2 and 10 minutes.
    fn turns(features: &[(BackgroundFeature, bool)]) -> Scan {
        let requests = [0, 1, 2, 10].map(|t| response(t, 100_000)).into();
        scan(features, requests, &[0, 1, 2, 10])
    }

    #[test]
    fn distinct_project_controls_charge_only_their_requests_and_keep_unknown_scopes_advisory() {
        let mut s = turns(&[(BackgroundFeature::PromptSuggestions, true)]);
        let mut on = control(BackgroundFeature::PromptSuggestions, true);
        on.owner = "/synthetic/a/.claude/settings.json:promptSuggestionEnabled".into();
        on.off_file = Some("/synthetic/a/.claude/settings.json".into());
        let mut off = on.clone();
        off.value = Some(false);
        off.owner = "/synthetic/b/.claude/settings.json:promptSuggestionEnabled".into();
        off.off_file = Some("/synthetic/b/.claude/settings.json".into());
        let mut unknown = on.clone();
        unknown.value = None;
        unknown.reason =
            "recorded working directory is absent; current ownership unresolved".into();
        s.profiles[0].background_directories.extend([
            ("/synthetic/a".into(), vec![on]),
            ("/synthetic/b".into(), vec![off]),
            (String::new(), vec![unknown]),
        ]);
        s.requests[0].cwd = Some("/synthetic/a".into());
        s.requests[1].cwd = Some("/synthetic/a".into());
        s.requests[2].cwd = Some("/synthetic/b".into());
        let checks = run(&s, 0.0);
        assert_eq!(checks.len(), 3);
        assert_eq!(
            (checks[0].base.requests, checks[0].generations),
            (2, Some(1))
        );
        assert_eq!(checks[0].base.action, Action::Change);
        assert_eq!(checks[0].directories, [Some("/synthetic/a".into())]);
        assert_eq!(
            (checks[1].base.requests, checks[1].generations),
            (1, Some(0))
        );
        assert_eq!(checks[1].base.action, Action::Keep);
        assert_eq!(checks[2].base.requests, 1);
        assert_eq!(checks[2].base.current, None);
        assert_eq!(checks[2].base.action, Action::Advisory);
        assert_eq!(checks[2].cost.total_usd, None);
        assert_eq!(checks[2].base.savings_usd, None);
        assert_eq!(checks[2].generations, None);
        assert_eq!(checks[2].directories, [None]);
        s.cache_context.requests = std::mem::take(&mut s.requests);
        let empty = run(&s, 0.0);
        assert_eq!(empty.len(), 3);
        assert!(empty.iter().all(|c| c.base.requests == 0));
        assert_eq!(empty[0].base.current, Some(true));
        assert_eq!(empty[0].generations, Some(0));
        assert_eq!(empty[0].directories, [Some("/synthetic/a".into())]);
    }

    #[test]
    fn suggestions_follow_every_warm_interactive_turn_after_the_first() {
        let s = turns(&[(BackgroundFeature::PromptSuggestions, true)]);
        let c = &run(&s, 1.0)[0];
        assert_eq!(c.generations, Some(3));
        let one = fork_cost(
            &prices(),
            &turn_ends(&s, &s.profiles[0])[0],
            BackgroundFeature::PromptSuggestions,
        );
        let expected = one.total_usd.unwrap() * 3.0;
        assert!((c.cost.total_usd.unwrap() - expected).abs() < 1e-12);
        assert!((c.base.savings_usd.unwrap() - expected).abs() < 1e-12);
        assert_eq!(c.base.action, Action::Keep);
        // Any cost above the per-use limit turns it off, since uses are not recorded.
        let c = &run(&s, expected / 2.0)[0];
        assert_eq!(
            (c.base.action, c.base.recommended),
            (Action::Change, Some(false))
        );
    }

    #[test]
    fn suggestions_skip_cold_caches_and_non_interactive_clients() {
        let mut s = turns(&[(BackgroundFeature::PromptSuggestions, true)]);
        s.requests[1].parts[0].usage = claude_usage(ClaudeUsage {
            write: 20_000,
            ..ClaudeUsage::default()
        });
        s.requests[2].client = Some("sdk-ts".into());
        let c = &run(&s, 1.0)[0];
        assert_eq!(c.generations, Some(1));
        assert!(
            c.base
                .uncertainty
                .iter()
                .any(|u| u.starts_with("1 turns from SDK"))
        );
    }

    #[test]
    fn a_tool_loop_ends_its_turn_once() {
        // Two requests answer the prompt at 0; suggestions need two responses.
        let s = scan(
            &[(BackgroundFeature::PromptSuggestions, true)],
            vec![response(0, 1000), response(1, 1000)],
            &[0],
        );
        assert_eq!(run(&s, 1.0)[0].generations, Some(1));
    }

    #[test]
    fn recaps_need_three_turns_and_a_three_minute_gap() {
        let s = turns(&[(BackgroundFeature::SessionRecaps, true)]);
        let c = &run(&s, 1.0)[0];
        // Only the gap from 2 to 10 minutes follows a third turn and lasts three minutes.
        assert_eq!(c.generations, Some(1));
        // The recap at 5 minutes kept the 5m cache warm for the prompt at 10.
        let rates = unit_rates(&prices(), &s.requests[3], &s.requests[3].parts[0]).unwrap();
        let lost = 100_000.0 * (rates.write5m - rates.read);
        assert!((c.lost_warming.total_usd.unwrap() - lost).abs() < 1e-12);
        let recap = fork_cost(
            &prices(),
            &turn_ends(&s, &s.profiles[0])[2],
            BackgroundFeature::SessionRecaps,
        );
        let net = recap.total_usd.unwrap() - lost;
        assert!((c.base.savings_usd.unwrap() - net).abs() < 1e-12);
        // A request on another model reads a cache the recap never touched.
        let mut other = s.clone();
        other.requests[3].parts[0].model = Some("claude-sonnet-5".into());
        assert_eq!(run(&other, 1.0)[0].lost_warming.total_usd, Some(0.0));
    }

    #[test]
    fn streamed_responses_can_expire_the_cache_before_background_generation() {
        for (feature, duration, prompt, output) in [
            (BackgroundFeature::PromptSuggestions, 6, 350, 50),
            (BackgroundFeature::SessionRecaps, 4, 100, 100),
        ] {
            let mut s = turns(&[(feature, true)]);
            for i in [0, 1, 3] {
                s.requests[i].client = Some("sdk-ts".into());
            }
            let r = &mut s.requests[2];
            r.end_time = r.time + duration * MINUTE;
            let expected = price_components(
                &prices(),
                r,
                &BillingPart {
                    usage: claude_usage(ClaudeUsage {
                        input: 100_100 + 10 + 20 + prompt,
                        output,
                        write: 0,
                        ..ClaudeUsage::default()
                    }),
                    ..r.parts[0].clone()
                },
            )
            .total()
            .total_usd
            .unwrap();
            let c = &run(&s, 1.0)[0];
            assert_eq!(c.generations, Some(1));
            assert!((c.cost.total_usd.unwrap() - expected).abs() < 1e-12);
            assert_eq!(c.lost_warming.total_usd, Some(0.0));
        }
    }

    #[test]
    fn recaps_skip_the_next_turn_even_when_the_previous_recap_is_before_the_window() {
        let times = [0, 1, 2, 6, 10, 14];
        let mut s = scan(
            &[(BackgroundFeature::SessionRecaps, true)],
            times.map(|t| response(t, 1000)).into(),
            &times,
        );
        // Eligible gaps after turns three, four and five allow recaps on three and five.
        assert_eq!(run(&s, 1.0)[0].generations, Some(2));
        let window = |s: &Scan| {
            background_checks(&BackgroundInput {
                scan: s,
                catalog: &prices(),
                since: Some(6 * MINUTE),
                now: 30 * 24 * 60 * MINUTE,
                max_cost_per_use_usd: 1.0,
            })[0]
                .generations
        };
        assert_eq!(window(&s), Some(1));
        s.cache_context.requests = s.requests.drain(..3).collect();
        let (earlier, events) = s.events.drain(..).partition(|e| e.time < 6 * MINUTE);
        (s.cache_context.prompts, s.events) = (earlier, events);
        assert_eq!(window(&s), Some(1));
    }

    #[test]
    fn recap_warming_uses_request_start_and_only_a_live_prefix_can_refresh() {
        for (next, expected_reads) in [(8, 100_000), (11, 0)] {
            let mut s = scan(
                &[(BackgroundFeature::SessionRecaps, true)],
                vec![
                    response(0, 100_000),
                    response(1, 100_000),
                    response(2, 100_000),
                    response(next, 100_000),
                ],
                &[0, 1, 2, next],
            );
            s.requests[2].end_time = 3 * MINUTE;
            let rates = unit_rates(&prices(), &s.requests[3], &s.requests[3].parts[0]).unwrap();
            let expected = expected_reads as f64 * (rates.write5m - rates.read);
            let c = &run(&s, 1.0)[0];
            assert_eq!(c.generations, Some(1));
            assert!((c.lost_warming.total_usd.unwrap() - expected).abs() < 1e-12);
        }
    }

    #[test]
    fn suggestion_refreshes_keep_recaps_warm_and_reduce_recaps_warming_credit() {
        let features = [
            (BackgroundFeature::PromptSuggestions, true),
            (BackgroundFeature::SessionRecaps, true),
        ];
        let mut s = scan(
            &features,
            vec![
                response(0, 100_000),
                response(1, 100_000),
                response(2, 100_000),
                response(9, 100_000),
            ],
            &[0, 1, 2, 9],
        );
        let warm_recap = run(&s, 1.0)[1].cost.total_usd.unwrap();
        // The suggestion at minute five keeps the prefix warm for the recap at eight.
        s.requests[2].end_time = 5 * MINUTE;
        let checks = run(&s, 1.0);
        assert!((checks[1].cost.total_usd.unwrap() - warm_recap).abs() < 1e-12);
        // The prompt at nine is still inside the suggestion's lifetime without a recap.
        assert_eq!(checks[1].lost_warming.total_usd, Some(0.0));
        // Without suggestions the recap loses its read, and the next request loses warming.
        let rates = unit_rates(&prices(), &s.requests[2], &s.requests[2].parts[0]).unwrap();
        let extra =
            100_100.0 * (rates.ordinary - rates.read) + 100_000.0 * (rates.write5m - rates.read);
        assert!((checks[0].lost_warming.total_usd.unwrap() - extra).abs() < 1e-12);
    }

    #[test]
    fn recap_warming_starts_when_the_other_features_refresh_expires() {
        for (next, reads) in [(7, 0), (8, 100_000), (11, 0)] {
            let mut s = scan(
                &[
                    (BackgroundFeature::PromptSuggestions, true),
                    (BackgroundFeature::SessionRecaps, true),
                ],
                vec![
                    response(0, 100_000),
                    response(1, 100_000),
                    response(2, 100_000),
                    response(next, 100_000),
                ],
                &[0, 1, 2, next],
            );
            s.requests[2].end_time = 3 * MINUTE;
            let rates = unit_rates(&prices(), &s.requests[3], &s.requests[3].parts[0]).unwrap();
            let expected = reads as f64 * (rates.write5m - rates.read);
            assert!((run(&s, 1.0)[1].lost_warming.total_usd.unwrap() - expected).abs() < 1e-12);
        }
    }

    #[test]
    fn history_before_the_window_counts_toward_eligibility() {
        let features = [
            (BackgroundFeature::PromptSuggestions, true),
            (BackgroundFeature::SessionRecaps, true),
        ];
        let window = |s: &Scan| {
            background_checks(&BackgroundInput {
                scan: s,
                catalog: &prices(),
                since: Some(2 * MINUTE),
                now: 30 * 24 * 60 * MINUTE,
                max_cost_per_use_usd: 1.0,
            })
            .iter()
            .map(|c| c.generations)
            .collect::<Vec<_>>()
        };
        // A narrower analysis window over a full scan.
        let mut s = turns(&features);
        assert_eq!(window(&s), [Some(2), Some(1)]);
        // A scan cut at 2 minutes keeps earlier responses and prompts as context.
        s.cache_context.requests = s.requests.drain(..2).collect();
        let (earlier, events) = s.events.drain(..).partition(|e| e.time < 2 * MINUTE);
        (s.cache_context.prompts, s.events) = (earlier, events);
        assert_eq!(window(&s), [Some(2), Some(1)]);
        // With only one earlier prompt, the turn at 2 minutes is the second: no recap.
        s.cache_context.prompts.truncate(1);
        assert_eq!(window(&s), [Some(2), Some(0)]);
    }

    #[test]
    fn off_features_and_unmeasured_features() {
        let s = turns(&[
            (BackgroundFeature::PromptSuggestions, false),
            (BackgroundFeature::GoalCheckIns, true),
        ]);
        let checks = run(&s, 1.0);
        assert_eq!(checks[0].base.action, Action::Keep);
        assert_eq!(checks[0].base.savings_usd, None);
        assert_eq!(checks[0].cost.total_usd, Some(0.0));
        assert_eq!(checks[1].base.action, Action::Advisory);
        assert_eq!(checks[1].base.evidence, Evidence::Guidance);
        assert_eq!(checks[1].generations, None);
        assert_eq!(checks[1].cost.total_usd, None);
    }

    #[test]
    fn unpriced_turns_scale_the_estimate_or_leave_it_unknown() {
        let mut s = turns(&[(BackgroundFeature::PromptSuggestions, true)]);
        s.requests[1].evidence = RequestEvidence::Unknown;
        let c = &run(&s, 100.0)[0];
        let coverage = c.base.coverage.unwrap();
        assert_eq!((coverage.priced, coverage.total), (2, 3));
        let one = fork_cost(
            &prices(),
            &turn_ends(&s, &s.profiles[0])[0],
            BackgroundFeature::PromptSuggestions,
        );
        assert!((c.base.savings_usd.unwrap() - one.total_usd.unwrap() * 3.0).abs() < 1e-12);
        for r in &mut s.requests {
            r.evidence = RequestEvidence::Unknown;
        }
        let c = &run(&s, 100.0)[0];
        assert_eq!(
            (c.base.action, c.base.savings_usd),
            (Action::Advisory, None)
        );
    }
}
