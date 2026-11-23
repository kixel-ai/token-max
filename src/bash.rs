//! Claude's foreground Bash timeout cap beside the cache TTL, and what long waits under it cost.

use crate::collections::HashMap;

use crate::cache::{observed_ttl, ttl_milliseconds};
use crate::model::{
    Action, BashCap, BashCapControl, BashWaits, Bucket, Cost, EventKind, Harness, Id, Millis,
    Profile, Request, RequestEvidence, Scan, StructuralEvent, Ttl, TtlCheck, Usd,
};
use crate::prices::{Catalog, Sample, add_costs, known_cost, unit_rates, unknown_cost, zero_cost};

/// Claude Code's pages on the timeout variables, the Bash tool and the cache's lifetime.
pub(crate) const SOURCES: [&str; 3] = [
    "https://code.claude.com/docs/en/env-vars",
    "https://code.claude.com/docs/en/tools-reference",
    "https://code.claude.com/docs/en/prompt-caching",
];

/// The cap suggested under a cache TTL: a guessed margin, not a measured one.
///
/// The TTL runs from the request that started the call, so the cap leaves room
/// for that request's output and the reply after the wait.
fn safe_cap(ttl: Ttl) -> Millis {
    match ttl {
        Ttl::FiveMinutes => 4 * 60_000,
        Ttl::OneHour => 55 * 60_000,
    }
}

/// Inputs for the Bash cap check.
pub struct BashInput<'a> {
    /// Profiles with their current Bash controls, the recorded requests and tool events.
    pub scan: &'a Scan,
    /// Public prices.
    pub catalog: &'a Catalog,
    /// TTL checks, whose main-agent verdict sets the TTL the cap must stay under.
    pub ttls: &'a [&'a TtlCheck],
    /// The explicit window start; None uses the first request.
    pub since: Option<Millis>,
}

/// One cap per line of Claude profiles that share the cap's owners, compared with the
/// main-agent cache TTL.
pub fn bash_caps(input: &BashInput<'_>) -> Vec<BashCap> {
    let capped = input.scan.profiles.iter().filter(|p| p.bash.is_some());
    crate::rows::groups(capped, |p| owner(p).expect("filtered on a cap"))
        .iter()
        .map(|line| check(input, line, line.lead.bash.as_ref().expect("a cap")))
        .collect()
}

/// The owners that put a profile on a Bash cap line: both timeout variables' and the main
/// TTL's the cap is compared with. None without a cap.
pub(crate) fn owner(p: &Profile) -> Option<String> {
    let bash = p.bash.as_ref()?;
    Some(format!(
        "{}\n{}\n{}",
        bash.owner,
        bash.default_owner,
        crate::rows::ttl_owner(p, crate::model::Bucket::Main)
    ))
}

/// The main-agent TTL the cap must stay under, and where it came from.
fn evaluation_ttl(
    ttls: &[&TtlCheck],
    profile: &Profile,
    requests: &[&Request],
) -> (Option<Ttl>, &'static str) {
    // No one TTL holds across projects that set it differently, so no fallback stands in.
    if profile.ttl.main.varies {
        return (
            None,
            "recorded projects set the main-agent cache TTL differently",
        );
    }
    let verdict = ttls
        .iter()
        .find(|t| t.base.profiles.contains(&profile.id) && t.bucket == Bucket::Main)
        .filter(|t| t.base.action != Action::Advisory)
        .and_then(|t| t.base.recommended);
    if let Some(ttl) = verdict {
        return (Some(ttl), "the main-agent cache TTL this report recommends");
    }
    if let Some(ttl) = profile.ttl.main.one().value {
        return (Some(ttl), "the current main-agent cache TTL setting");
    }
    let observed = requests
        .iter()
        .rev()
        .filter(|r| r.agent == crate::model::Agent::Main)
        .find_map(|r| r.parts.iter().rev().find_map(observed_ttl));
    match observed {
        Some(ttl) => (
            Some(ttl),
            "the cache lifetime the latest main-agent request wrote",
        ),
        None => (None, "the main-agent cache TTL is unresolved"),
    }
}

/// The cache rewrite after a wait: prefix the next request wrote instead of reading.
fn miss(catalog: &Catalog, issuing: Option<&Request>, next: Option<&Request>) -> Cost {
    let (Some(issuing), Some(next)) = (issuing, next) else {
        return unknown_cost("no request linked before and after the wait");
    };
    let prefix = issuing.parts.last().and_then(|p| p.usage.input);
    let Some(p) = next.parts.first() else {
        return unknown_cost("the request after the wait has no usage");
    };
    let (Some(prefix), Some(read), Some(write)) = (prefix, p.usage.read, p.usage.write) else {
        return unknown_cost("cache usage around the wait is not recorded");
    };
    if next.evidence == RequestEvidence::Unknown {
        return unknown_cost("the request after the wait has no verified usage");
    }
    let rewritten = write.min((prefix - read).max(0));
    if rewritten == 0 {
        return zero_cost();
    }
    let Some(rates) = unit_rates(catalog, next, p) else {
        return unknown_cost("cache rates for the request after the wait are unknown");
    };
    let write_rate = match observed_ttl(p) {
        Some(Ttl::FiveMinutes) => rates.write5m,
        Some(Ttl::OneHour) => rates.write1h,
        None => return unknown_cost("the write lifetime after the wait is not recorded"),
    };
    let usd = Usd::from_f64(rewritten as f64 * (write_rate - rates.read));
    known_cost(usd)
}

fn check(
    input: &BashInput<'_>,
    line: &crate::rows::Group<'_>,
    control: &BashCapControl,
) -> BashCap {
    let profile = line.lead;
    let BashInput {
        scan,
        catalog,
        ttls,
        since,
    } = *input;
    let selected = |time: Millis| since.is_none_or(|s| time >= s);
    let mut requests: Vec<&Request> = scan
        .requests
        .iter()
        .filter(|r| r.harness == Harness::Claude && line.covers(&r.profiles) && selected(r.time))
        .collect();
    requests.sort_by_key(|r| r.time);
    let by_id: HashMap<&str, &Request> = requests.iter().map(|r| (&*r.id, *r)).collect();
    let mut sessions: HashMap<&str, Vec<&Request>> = HashMap::default();
    for &r in &requests {
        sessions.entry(&r.session).or_default().push(r);
    }
    let (ttl, ttl_basis) = evaluation_ttl(ttls, profile, &requests);

    let mine = |e: &&StructuralEvent| line.covers(&e.profiles) && selected(e.time);
    let mut ends: HashMap<(&str, &Id), &StructuralEvent> = HashMap::default();
    for e in scan
        .events
        .iter()
        .filter(mine)
        .filter(|e| e.kind == EventKind::ToolEnd)
    {
        if let Some(id) = &e.tool_id {
            ends.entry((&e.session, id))
                .and_modify(|end| {
                    if e.time < end.time {
                        *end = e;
                    }
                })
                .or_insert(e);
        }
    }
    let (mut calls, mut long, mut timed_out, mut polling) = (0, 0, 0, 0);
    let mut costs = vec![];
    let mut priced = crate::collections::HashSet::default();
    for start in scan.events.iter().filter(mine) {
        let Some(shell) = start
            .shell
            .filter(|s| start.kind == EventKind::ToolStart && !s.background)
        else {
            continue;
        };
        let Some(end) = start
            .tool_id
            .as_ref()
            .and_then(|id| ends.get(&(start.session.as_ref(), id)))
        else {
            continue;
        };
        calls += 1;
        let issuing = start
            .request
            .as_deref()
            .and_then(|id| by_id.get(id).copied());
        // The cache lifetime the starting request wrote, else the profile's.
        let lifetime = issuing
            .and_then(|r| r.parts.iter().rev().find_map(observed_ttl))
            .or(ttl);
        let began = issuing.map_or(start.time, |r| r.time);
        if lifetime.is_none_or(|t| end.time - began < ttl_milliseconds(t)) {
            continue;
        }
        long += 1;
        timed_out += u64::from(end.shell.is_some_and(|s| s.timed_out));
        polling += u64::from(shell.polls);
        let next = sessions.get(start.session.as_ref()).and_then(|list| {
            list.get(list.partition_point(|r| r.time < end.time))
                .copied()
        });
        // Parallel waits can share one next request, whose rewrite counts once.
        if next.is_none_or(|r| priced.insert(&*r.id)) {
            costs.push(miss(catalog, issuing, next));
        }
    }
    let miss_cost = add_costs(&costs);
    let mut sample = Sample::default();
    for cost in &costs {
        sample.add_cost(cost);
    }
    let miss_estimate = sample.estimate(costs.len() as u64);

    // A request T3 could not settle may have run under another owner's cap.
    let unresolved = control
        .unresolved
        .clone()
        .or_else(|| {
            (!crate::t3::settled(&scan.profiles, &line.ids)).then(|| crate::t3::UNOWNED.into())
        })
        .or_else(|| ttl.is_none().then(|| ttl_basis.into()));
    let suggested = ttl.map(safe_cap).filter(|_| unresolved.is_none());
    let cap = suggested.unwrap_or(control.value);
    let mut replacement = format!("env.BASH_MAX_TIMEOUT_MS: \"{cap}\"");
    // A default above the cap raises the cap again, so it comes down too.
    if suggested.is_some_and(|cap| control.default_timeout > cap) {
        replacement += &format!(" and env.BASH_DEFAULT_TIMEOUT_MS: \"{cap}\"");
    }
    BashCap {
        profile: profile.id.clone(),
        profiles: line.ids.clone(),
        current: control.value,
        suggested,
        owner: control.owner.clone(),
        ttl,
        ttl_basis: ttl_basis.into(),
        unresolved,
        replacement,
        waits: BashWaits {
            calls,
            long,
            timed_out,
            polling,
            miss_cost,
            miss_estimate,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::CheckResult;
    use crate::prices::unit_rates;
    use crate::reader::scan;
    use crate::replay::{AnalyzeOptions, analyze};
    use crate::report::{ReportOptions, advice, rendered as format_report};
    use crate::test_support::{Workspace, prices};
    use serde_json::{Value, json};

    const MIN: Millis = 60_000;
    /// 2026-11-01T00:00:00Z.
    const START: Millis = 1_793_491_200_000;

    fn at(minute: f64) -> String {
        jiff::Timestamp::from_millisecond(START + (minute * MIN as f64) as Millis)
            .unwrap()
            .to_string()
    }

    /// One main-agent request reading `read` and writing `write` 5m tokens, optionally starting a Bash call.
    fn assistant(minute: f64, id: &str, read: i64, write: i64, bash: Option<Value>) -> Value {
        let mut content = vec![json!({"type": "text", "text": "SYNTHETIC_PRIVATE_REPLY"})];
        if let Some(input) = bash {
            content.push(json!({"type": "tool_use", "id": format!("tool-{id}"), "name": "Bash", "input": input}));
        }
        json!({
            "type": "assistant",
            "sessionId": "session",
            "timestamp": at(minute),
            "version": "2.1.319",
            "message": {
                "id": id,
                "model": "claude-opus-5-5",
                "content": content,
                "usage": {
                    "input_tokens": 0,
                    "output_tokens": 10,
                    "cache_read_input_tokens": read,
                    "cache_creation_input_tokens": write,
                    "cache_creation": {"ephemeral_5m_input_tokens": write, "ephemeral_1h_input_tokens": 0},
                    "service_tier": "standard",
                    "speed": "standard",
                    "inference_geo": "global",
                },
            },
        })
    }

    fn result(minute: f64, id: &str, timed_out: bool) -> Value {
        let mut outcome = json!({"stdout": "SYNTHETIC_PRIVATE_OUTPUT", "interrupted": false});
        if timed_out {
            outcome["timedOutAfterMs"] = json!(360_000);
        }
        json!({
            "type": "user",
            "sessionId": "session",
            "timestamp": at(minute),
            "message": {"content": [{"type": "tool_result", "tool_use_id": format!("tool-{id}"), "content": "SYNTHETIC_PRIVATE_OUTPUT"}]},
            "toolUseResult": outcome,
        })
    }

    fn report(settings: &str) -> crate::model::Report {
        let w = Workspace::new();
        std::fs::write(w.claude.join("settings.json"), settings).unwrap();
        let records = [
            // A polling loop that waits 6 minutes, past the 5m cache, then times out.
            assistant(
                0.0,
                "poll",
                0,
                100_000,
                Some(
                    json!({"command": "until test -f SYNTHETIC_PRIVATE; do sleep 30; done", "timeout": 360_000}),
                ),
            ),
            result(6.0, "poll", true),
            // The next request rewrites the expired prefix, then starts a short build.
            assistant(
                6.1,
                "build",
                0,
                100_500,
                Some(json!({"command": "cargo build"})),
            ),
            result(7.0, "build", false),
            assistant(7.1, "after", 100_500, 200, None),
            // The model does not wait on background commands.
            assistant(
                8.0,
                "background",
                100_700,
                100,
                Some(json!({"command": "sleep 900", "run_in_background": true})),
            ),
            result(30.0, "background", false),
        ];
        let lines: String = records.iter().map(|r| format!("{r}\n")).collect();
        std::fs::write(w.claude.join("projects/project/main.jsonl"), lines).unwrap();
        let data = scan(&w.scan_options()).unwrap();
        let mut report = analyze(AnalyzeOptions {
            scan: data,
            catalog: &prices(),
            since: None,
            now: START + 60 * MIN,
            max_cost_per_use_usd: 1.0,
        })
        .unwrap();
        // Compare with the current TTL alone; the TTL check's own verdict is tested separately.
        report.bash = bash_caps(&BashInput {
            scan: &report.scan,
            catalog: &prices(),
            ttls: &[],
            since: None,
        });
        report
    }

    fn bash(report: &crate::model::Report) -> &BashCap {
        &report.bash[0]
    }

    /// The cap's card as `vibes` shows it.
    fn card(report: &crate::model::Report) -> crate::vibes::Item {
        let items = advice(report, ReportOptions::default());
        let mut caps = items
            .into_iter()
            .filter(|i| i.name.starts_with("Bash timeout cap"));
        let card = caps.next().expect("a cap card");
        assert!(caps.next().is_none());
        card
    }

    #[test]
    fn a_main_ttl_projects_set_differently_has_no_fallback() {
        let mut profile = crate::test_support::make_profile("p", "root");
        profile.ttl.main.value = Some(Ttl::OneHour);
        profile.ttl.main.varies = true;
        let (ttl, basis) = evaluation_ttl(&[], &profile, &[]);
        assert_eq!(ttl, None);
        assert!(basis.contains("differently"));
    }

    #[test]
    fn the_ttl_check_verdict_sets_the_compared_ttl() {
        let w = Workspace::new();
        std::fs::write(w.claude.join("settings.json"), r#"{"promptCacheTtl":"5m"}"#).unwrap();
        let records = [
            assistant(
                0.0,
                "poll",
                0,
                100_000,
                Some(json!({"command": "sleep 360"})),
            ),
            result(6.0, "poll", true),
            assistant(6.1, "next", 0, 100_500, None),
        ];
        let lines: String = records.iter().map(|r| format!("{r}\n")).collect();
        std::fs::write(w.claude.join("projects/project/main.jsonl"), lines).unwrap();
        let report = analyze(AnalyzeOptions {
            scan: scan(&w.scan_options()).unwrap(),
            catalog: &prices(),
            since: None,
            now: START + 60 * MIN,
            max_cost_per_use_usd: 1.0,
        })
        .unwrap();
        let verdict = report
            .checks
            .iter()
            .find_map(|c| match c {
                CheckResult::Ttl(t) if t.bucket == Bucket::Main => Some(t),
                _ => None,
            })
            .unwrap();
        assert_ne!(verdict.base.action, Action::Advisory);
        let check = &report.bash[0];
        assert_eq!(check.ttl, verdict.base.recommended);
        assert!(check.ttl_basis.contains("recommends"));
    }

    #[test]
    fn a_default_cap_past_the_cache_ttl_is_advice_that_prices_the_long_wait() {
        let report = report(r#"{"promptCacheTtl":"5m"}"#);
        let check = bash(&report);
        assert_eq!(check.current, 600_000);
        assert_eq!(check.suggested, Some(240_000));
        assert_eq!(check.ttl, Some(Ttl::FiveMinutes));
        assert!(
            check
                .owner
                .ends_with("settings.json:env.BASH_MAX_TIMEOUT_MS")
        );
        let w = &check.waits;
        assert_eq!((w.calls, w.long, w.timed_out, w.polling), (2, 1, 1, 1));
        let next = report
            .scan
            .requests
            .iter()
            .find(|r| r.time == START + 6 * MIN + 6000)
            .unwrap();
        let rates = unit_rates(&prices(), next, &next.parts[0]).unwrap();
        let expected = 100_000.0 * (rates.write5m - rates.read);
        assert!((w.miss_cost.total_usd.unwrap().to_f64() - expected).abs() < 1e-9);
        // The cap is advice with an edit, and no line or verdict of the measured report.
        let text = format_report(
            &report,
            ReportOptions {
                details: true,
                ..Default::default()
            },
        );
        assert!(!text.contains("Bash timeout cap"), "{text}");
        assert!(!text.contains("BASH_MAX_TIMEOUT_MS"), "{text}");
        assert!(
            crate::doctor::fixes(&report, false)
                .iter()
                .all(|f| !f.name.contains("Bash"))
        );
        let card = card(&report);
        assert_eq!(card.value, "10m -> 4m");
        assert!(!card.quiet);
        let Some(crate::doctor::How::Edit(patch)) = &card.how else {
            panic!("{:?}", card.how)
        };
        assert_eq!(patch.edits.len(), 1);
        assert_eq!(
            patch.edits[0].text(Harness::Claude),
            r#""env": {"BASH_MAX_TIMEOUT_MS": "240000"}"#
        );
        // The miss cost is the window's total, like the wait count, not a 30-day rate.
        assert!(
            card.text.contains(&format!(
                "1 foreground Bash wait outlasted the cache (1 hit the timeout, 1 polled in a loop); the cache misses cost {} over the window.",
                crate::report::about(expected)
            )),
            "{}",
            card.text
        );
        // It says the saving is a guess, and that a lowered cap kills nothing.
        for said in [
            "What a lower cap saves is not measured, and the 4m margin is a guess.",
            "A command past the cap is not killed: it keeps running in the background",
            "keep the cap if yours do",
        ] {
            assert!(card.text.contains(said), "{}", card.text);
        }
        assert!(card.sources[0].contains("The maintainer's pick"));
        assert!(!format!("{report:?}").contains("SYNTHETIC_PRIVATE"));
        assert!(!format!("{card:?}").contains("SYNTHETIC_PRIVATE"));
    }

    #[test]
    fn parallel_waits_count_the_shared_rewrite_once() {
        let w = Workspace::new();
        std::fs::write(w.claude.join("settings.json"), r#"{"promptCacheTtl":"5m"}"#).unwrap();
        let mut first = assistant(0.0, "a", 0, 100_000, Some(json!({"command": "sleep 360"})));
        first["message"]["content"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "tool_use", "id": "tool-b", "name": "Bash", "input": {"command": "sleep 360"}}));
        let records = [
            first,
            result(6.0, "a", true),
            result(6.0, "b", true),
            assistant(6.1, "next", 0, 100_500, None),
        ];
        let lines: String = records.iter().map(|r| format!("{r}\n")).collect();
        std::fs::write(w.claude.join("projects/project/main.jsonl"), lines).unwrap();
        let data = scan(&w.scan_options()).unwrap();
        let checks = bash_caps(&BashInput {
            scan: &data,
            catalog: &prices(),
            ttls: &[],
            since: None,
        });
        let waits = &checks[0].waits;
        assert_eq!((waits.long, waits.timed_out), (2, 2));
        let next = data.requests.iter().find(|r| r.time > START).unwrap();
        let rates = unit_rates(&prices(), next, &next.parts[0]).unwrap();
        let expected = 100_000.0 * (rates.write5m - rates.read);
        assert!((waits.miss_cost.total_usd.unwrap().to_f64() - expected).abs() < 1e-9);
    }

    #[test]
    fn any_cap_but_the_margin_is_moved_to_it_and_a_larger_default_timeout_comes_down_too() {
        // Exactly the margin: a card with nothing to change, which `doctor` offers nothing for.
        let kept = report(r#"{"promptCacheTtl":"5m","env":{"BASH_MAX_TIMEOUT_MS":"240000"}}"#);
        assert_eq!(bash(&kept).suggested, Some(240_000));
        let item = card(&kept);
        assert_eq!((item.value.as_str(), item.quiet), ("", true));
        assert_eq!(item.how, None);
        assert!(
            item.text
                .starts_with("Capped at 4m, the margin suggested under the 5m cache."),
            "{}",
            item.text
        );
        assert!(
            item.text
                .ends_with("settings.json:env.BASH_MAX_TIMEOUT_MS.")
        );
        // Under the TTL but past the margin, which a threshold at the TTL would keep.
        let near = report(r#"{"promptCacheTtl":"5m","env":{"BASH_MAX_TIMEOUT_MS":"270000"}}"#);
        assert_eq!(card(&near).value, "270s -> 4m");
        // A low cap is raised, and says what that trades.
        let low = report(r#"{"promptCacheTtl":"5m","env":{"BASH_MAX_TIMEOUT_MS":"120000"}}"#);
        let item = card(&low);
        assert_eq!(item.value, "2m -> 4m");
        assert!(
            item.text.contains(
                "Raising it to 4m trades longer foreground waits for fewer background check-ins."
            ),
            "{}",
            item.text
        );
        // The default timeout raises the effective cap, so both come down in the one edit.
        let both = report(
            r#"{"promptCacheTtl":"5m","env":{"BASH_MAX_TIMEOUT_MS":"200000","BASH_DEFAULT_TIMEOUT_MS":"300000"}}"#,
        );
        let check = bash(&both);
        assert_eq!(check.current, 300_000);
        assert!(
            check
                .replacement
                .contains("BASH_DEFAULT_TIMEOUT_MS: \"240000\"")
        );
        let Some(crate::doctor::How::Edit(patch)) = card(&both).how else {
            panic!("an edit")
        };
        assert_eq!(patch.edits.len(), 2);
        // Under a 1h cache the margin is 55m.
        let hour = report(r#"{"promptCacheTtl":"1h"}"#);
        assert_eq!(card(&hour).value, "10m -> 55m");
    }

    #[test]
    fn an_unset_main_ttl_falls_back_to_the_recorded_write_lifetime() {
        let report = report("{}");
        let check = bash(&report);
        assert_eq!(check.ttl, Some(Ttl::FiveMinutes));
        assert_eq!(check.suggested, Some(240_000));
    }

    #[test]
    fn an_unresolved_ttl_suggests_no_cap_and_says_why() {
        let mut report = report("{}");
        for cap in &mut report.bash {
            cap.ttl = None;
            cap.ttl_basis = "the main-agent cache TTL is unresolved".into();
            cap.unresolved = Some(cap.ttl_basis.clone());
            cap.suggested = None;
        }
        let item = card(&report);
        assert_eq!((item.value.as_str(), item.quiet), ("", true));
        assert_eq!(item.how, None);
        assert!(
            item.text.starts_with(
                "Capped at 10m. No cap is suggested: the main-agent cache TTL is unresolved."
            ),
            "{}",
            item.text
        );
    }
}
