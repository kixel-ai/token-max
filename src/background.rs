//! Background generation: Claude prompt suggestions, session recaps and goal check-ins, and
//! Codex automatic recaps.
//!
//! Claude Code 2.1.319 sends suggestions and recaps as forks of the main conversation with
//! `skipTranscript` and `skipCacheWrite`, so transcripts never record them. Their cost is
//! estimated at each recorded turn where Claude Code would have sent one: a cache read of the
//! conversation, the last response and the instruction as new input, and a short reply.

use indexmap::IndexSet;

use crate::model::{
    Action, BackgroundCheck, BackgroundControl, BackgroundFeature, BillingPart, CheckBase, Cost,
    Coverage, EventKind, Evidence, Harness, Millis, PartKind, Profile, Request, RequestEvidence,
    Scan, Ttl, Usage,
};
use crate::prices::{
    Catalog, add_costs, paired_savings, price_components, unit_rates, unknown_cost, zero_cost,
};

const SOURCES: [&str; 5] = [
    "https://code.claude.com/docs/en/costs#background-token-usage (2026-11-08)",
    "https://code.claude.com/docs/en/interactive-mode#prompt-suggestions (2026-11-08)",
    "https://code.claude.com/docs/en/interactive-mode#session-recap (2026-11-08)",
    "https://code.claude.com/docs/en/env-vars (2026-11-08)",
    "https://learn.chatgpt.com/docs/config-file/config-reference (2026-11-08)",
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

/// One check per background feature of every profile.
pub fn background_checks(input: &BackgroundInput<'_>) -> Vec<BackgroundCheck> {
    let mut result = vec![];
    for profile in &input.scan.profiles {
        let requests: Vec<&Request> = input
            .scan
            .requests
            .iter()
            .filter(|r| r.profiles.contains(&profile.id) && input.since.is_none_or(|s| r.time >= s))
            .collect();
        let ends = turn_ends(input.scan, profile, &requests);
        for control in &profile.background {
            result.push(check(input, profile, control, &requests, &ends));
        }
    }
    result
}

/// A main-conversation response that finished a turn.
struct TurnEnd<'a> {
    request: &'a Request,
    /// Whether it came from the interactive terminal, where suggestions and recaps run.
    interactive: bool,
    /// Main-conversation responses in the session so far, including this one.
    responses: usize,
    /// User turns in the session so far.
    turns: usize,
    /// When the next user turn started.
    prompt: Option<Millis>,
    /// The next main-conversation request, which a cache refresh could have kept warm.
    next: Option<&'a Request>,
}

/// Every turn-ending main-conversation response of a Claude profile, in session order.
fn turn_ends<'a>(scan: &Scan, profile: &Profile, requests: &[&'a Request]) -> Vec<TurnEnd<'a>> {
    if profile.harness != Harness::Claude {
        return vec![];
    }
    let mut sessions: indexmap::IndexMap<&str, Vec<&'a Request>> = indexmap::IndexMap::new();
    for &r in requests {
        if r.harness == Harness::Claude && r.agent == crate::model::Agent::Main {
            sessions.entry(&r.session).or_default().push(r);
        }
    }
    let mut starts: indexmap::IndexMap<&str, Vec<Millis>> = indexmap::IndexMap::new();
    for e in &scan.events {
        if e.kind == EventKind::TurnStart && e.profiles.contains(&profile.id) {
            starts.entry(&e.session).or_default().push(e.time);
        }
    }
    let mut result = vec![];
    for (session, mut main) in sessions {
        main.sort_by_key(|r| r.time);
        let mut prompts = starts.get(session).cloned().unwrap_or_default();
        prompts.sort_unstable();
        for (i, &r) in main.iter().enumerate() {
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
            result.push(TurnEnd {
                request: r,
                interactive: r.client.as_deref() == Some("cli")
                    && r.source
                        .as_deref()
                        .is_none_or(|s| s.starts_with("repl_main_thread")),
                responses: i + 1,
                turns: prompts.iter().filter(|&&t| t <= r.time).count(),
                prompt,
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

/// One forked request after `r`: its cached prefix read back, the last response and
/// `prompt` as new input, and `output` tokens.
fn fork_cost(catalog: &Catalog, r: &Request, prompt: i64, output: i64) -> Cost {
    let Some((part, (ordinary, read, write, last))) =
        last_message(r).and_then(|p| Some((p, uncached(p)?)))
    else {
        return unknown_cost("the turn's last response has no recorded usage");
    };
    let fresh = ordinary + last + prompt;
    let usage = Usage {
        input: Some(read + write + fresh),
        output: Some(output),
        read: Some(read + write),
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

/// The rewrite the next request would pay if a recap had not refreshed the cache.
fn lost_warming(catalog: &Catalog, profile: &Profile, end: &TurnEnd<'_>) -> Cost {
    let Some(next) = end.next else {
        return zero_cost();
    };
    let ttl = match last_message(end.request).and_then(|p| p.usage.write1h) {
        Some(w) if w > 0 => Ttl::OneHour,
        _ => profile.ttl.main.value.unwrap_or(Ttl::FiveMinutes),
    };
    let lifetime = match ttl {
        Ttl::FiveMinutes => 5 * MINUTE,
        Ttl::OneHour => 60 * MINUTE,
    };
    let gap = next.time - end.request.end_time;
    if gap <= lifetime || gap > RECAP_DELAY + lifetime {
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
    let (Some(read), Some(rates)) = (part.usage.read, unit_rates(catalog, next, part)) else {
        return unknown_cost("the request after the recap has no priced cache reads");
    };
    let write = match ttl {
        Ttl::FiveMinutes => rates.write5m,
        Ttl::OneHour => rates.write1h,
    };
    Cost {
        known_usd: read as f64 * (write - rates.read),
        total_usd: Some(read as f64 * (write - rates.read)),
        ..zero_cost()
    }
}

/// The estimated (on, off) cost of each turn where the feature would have run.
fn generations(
    catalog: &Catalog,
    profile: &Profile,
    feature: BackgroundFeature,
    ends: &[TurnEnd<'_>],
) -> Vec<(Cost, Cost)> {
    let mut result = vec![];
    for end in ends.iter().filter(|e| e.interactive) {
        match feature {
            BackgroundFeature::PromptSuggestions => {
                if end.responses < 2 {
                    continue;
                }
                let cold = last_message(end.request).and_then(uncached).map(
                    |(ordinary, _, write, output)| {
                        ordinary + output > COLD || ordinary + output + write > COLD
                    },
                );
                if cold == Some(true) {
                    continue;
                }
                result.push((
                    fork_cost(catalog, end.request, SUGGESTION_PROMPT, SUGGESTION_OUTPUT),
                    zero_cost(),
                ));
            }
            BackgroundFeature::SessionRecaps => {
                let away = end
                    .prompt
                    .is_some_and(|p| p - end.request.end_time >= RECAP_DELAY);
                if end.turns < 3 || !away {
                    continue;
                }
                result.push((
                    fork_cost(catalog, end.request, RECAP_PROMPT, RECAP_OUTPUT),
                    lost_warming(catalog, profile, end),
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
    ends: &[TurnEnd<'_>],
) -> BackgroundCheck {
    let feature = control.feature;
    let measurable = matches!(
        feature,
        BackgroundFeature::PromptSuggestions | BackgroundFeature::SessionRecaps
    );
    let pairs = if measurable && control.value {
        generations(input.catalog, profile, feature, ends)
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
        (false, _, _) => Action::Keep,
        (true, false, _) | (true, true, None) => Action::Advisory,
        (true, true, Some(usd)) if usd > input.max_cost_per_use_usd => Action::Change,
        (true, true, Some(_)) => Action::Keep,
    };
    let other = ends.iter().filter(|e| !e.interactive).count();
    let mut uncertainty = vec![];
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
    let cost = add_costs(pairs.iter().map(|(on, _)| on));
    let lost = add_costs(pairs.iter().map(|(_, off)| off));
    uncertainty.extend(cost.unknown.iter().cloned());
    uncertainty.extend(lost.unknown.iter().cloned());
    let history_profiles: IndexSet<&String> = requests.iter().flat_map(|r| &r.profiles).collect();
    BackgroundCheck {
        base: CheckBase {
            profile: profile.id.clone(),
            history_profiles: history_profiles.into_iter().cloned().collect(),
            evidence: if measurable {
                Evidence::Estimated
            } else {
                Evidence::Guidance
            },
            action,
            current: Some(control.value),
            recommended: match action {
                Action::Change => Some(false),
                Action::Keep => Some(control.value),
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
            baseline: "current profile with this feature on, holding recorded work and all other settings constant".into(),
            requests: requests.len() as u64,
            coverage: (measurable && control.value).then_some(Coverage {
                priced: paired.priced,
                total: paired.total,
            }),
            savings_usd: if measurable && control.value { estimate } else { None },
            monthly_savings_usd: if measurable && control.value {
                month(estimate)
            } else {
                None
            },
            break_even: None,
            assumptions: [
                "the client does not log these requests; each is estimated at a recorded turn where it would have run",
                "a fork reads the conversation's cached prefix and sends the last response and its instruction as new input, without writing cache",
                "recaps refresh the cache; a request that arrived within that refresh would rewrite its cached prefix with the feature off",
                "uses are not recorded, so the whole cost counts as one use against --max-cost-per-use",
            ]
            .map(String::from)
            .into(),
            uncertainty,
            sources: SOURCES.map(String::from).into(),
        },
        feature,
        off: control.off.clone(),
        generations: measurable.then_some(pairs.len() as u64),
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
            value,
            minutes: None,
            owner: "/synthetic/.claude/settings.json:key".into(),
            off: "\"key\": false".into(),
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
    fn suggestions_follow_every_warm_interactive_turn_after_the_first() {
        let s = turns(&[(BackgroundFeature::PromptSuggestions, true)]);
        let c = &run(&s, 1.0)[0];
        assert_eq!(c.generations, Some(3));
        let one = fork_cost(
            &prices(),
            &s.requests[0],
            SUGGESTION_PROMPT,
            SUGGESTION_OUTPUT,
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
        let recap = fork_cost(&prices(), &s.requests[2], RECAP_PROMPT, RECAP_OUTPUT);
        let net = recap.total_usd.unwrap() - lost;
        assert!((c.base.savings_usd.unwrap() - net).abs() < 1e-12);
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
        assert_eq!(checks[1].base.action, Action::Advisory);
        assert_eq!(checks[1].base.evidence, Evidence::Guidance);
        assert_eq!(checks[1].generations, None);
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
            &s.requests[0],
            SUGGESTION_PROMPT,
            SUGGESTION_OUTPUT,
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
