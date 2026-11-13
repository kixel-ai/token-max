//! Whether Claude's foreground Bash timeout cap lets the prompt cache expire, and what long waits cost.

use crate::collections::{HashMap, IndexSet};

use crate::cache::{observed_ttl, ttl_milliseconds};
use crate::model::{
    Action, BashCapCheck, BashCapControl, BashWaits, Bucket, CheckBase, Cost, EventKind, Evidence,
    Harness, Id, Millis, Profile, Request, RequestEvidence, Scan, StructuralEvent, Ttl, TtlCheck,
    Usd,
};
use crate::prices::{Catalog, add_costs, unit_rates, unknown_cost, zero_cost};

const SOURCES: [&str; 3] = [
    "https://code.claude.com/docs/en/env-vars (2026-11-08)",
    "https://code.claude.com/docs/en/tools-reference (2026-11-08)",
    "https://code.claude.com/docs/en/prompt-caching (2026-11-08)",
];

/// The largest cap that still leaves the cache warm when the model replies.
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
    /// The window end.
    pub now: Millis,
}

/// One check per Claude profile, comparing the current cap with the main-agent cache TTL.
pub fn bash_checks(input: &BashInput<'_>) -> Vec<BashCapCheck> {
    input
        .scan
        .profiles
        .iter()
        .filter_map(|profile| Some(check(input, profile, profile.bash.as_ref()?)))
        .collect()
}

/// The main-agent TTL the cap must stay under, and where it came from.
fn evaluation_ttl(
    ttls: &[&TtlCheck],
    profile: &Profile,
    requests: &[&Request],
) -> (Option<Ttl>, &'static str) {
    let verdict = ttls
        .iter()
        .find(|t| t.base.profile == profile.id && t.bucket == Bucket::Main)
        .filter(|t| t.base.action != Action::Advisory)
        .and_then(|t| t.base.recommended);
    if let Some(ttl) = verdict {
        return (Some(ttl), "the main-agent cache TTL this report recommends");
    }
    if let Some(ttl) = profile.ttl.main.value {
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
    Cost {
        known_usd: usd,
        total_usd: Some(usd),
        ..zero_cost()
    }
}

fn check(input: &BashInput<'_>, profile: &Profile, control: &BashCapControl) -> BashCapCheck {
    let BashInput {
        scan,
        catalog,
        ttls,
        since,
        now,
    } = *input;
    let selected = |time: Millis| since.is_none_or(|s| time >= s);
    let mut requests: Vec<&Request> = scan
        .requests
        .iter()
        .filter(|r| {
            r.harness == Harness::Claude && r.profiles.contains(&profile.id) && selected(r.time)
        })
        .collect();
    requests.sort_by_key(|r| r.time);
    let by_id: HashMap<&str, &Request> = requests.iter().map(|r| (&*r.id, *r)).collect();
    let mut sessions: HashMap<&str, Vec<&Request>> = HashMap::default();
    for &r in &requests {
        sessions.entry(&r.session).or_default().push(r);
    }
    let (ttl, ttl_basis) = evaluation_ttl(ttls, profile, &requests);

    let mine = |e: &&StructuralEvent| e.profiles.contains(&profile.id) && selected(e.time);
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

    let (action, recommended) = match ttl {
        None => (Action::Advisory, None),
        Some(t) if control.value >= ttl_milliseconds(t) => (Action::Change, Some(safe_cap(t))),
        Some(_) => (Action::Keep, Some(control.value)),
    };
    let mut replacement = format!(
        "env.BASH_MAX_TIMEOUT_MS: \"{}\"",
        recommended.unwrap_or(control.value)
    );
    if let Some(cap) = recommended.filter(|cap| control.default_timeout > *cap) {
        replacement += &format!(" and env.BASH_DEFAULT_TIMEOUT_MS: \"{cap}\"");
    }
    let history_profiles: IndexSet<&String> =
        requests.iter().flat_map(|r| r.profiles.iter()).collect();
    let mut uncertainty = vec![
        "project/local settings and launch environments are not read; only user/managed settings and the active profile's environment".into(),
    ];
    if ttl.is_none() {
        uncertainty.push(ttl_basis.into());
    }
    uncertainty.extend(miss_cost.unknown.iter().cloned());
    BashCapCheck {
        base: CheckBase {
            profile: profile.id.clone(),
            history_profiles: history_profiles.into_iter().cloned().collect(),
            evidence: Evidence::Guidance,
            action,
            current: Some(control.value),
            recommended,
            owner: control.owner.clone(),
            since: since.or(requests.first().map(|r| r.time)),
            until: Some(now),
            window_basis: if since.is_none() {
                "available selected usage; current files applied to history"
            } else {
                "explicit --since; current files applied to history"
            }
            .into(),
            baseline: "current cap and main-agent cache TTL; the recorded waits are measured, not replayed under another cap".into(),
            requests: requests.len() as u64,
            coverage: None,
            savings_usd: None,
            monthly_savings_usd: None,
            break_even: None,
            assumptions: [
                "a foreground Bash wait at or past the cache TTL lets the cache expire before the model replies",
                "a cap under the TTL moves longer commands to the background, where checking on them reads the cache instead of rewriting it",
                "a wait's miss is the prefix the next request wrote instead of read, priced at write minus read rates",
                "the cap is compared with the main-agent TTL; Claude Code 2.1.319 already backgrounds commands after 3 minutes in async subagents with a 5m cache",
            ]
            .map(String::from)
            .into(),
            uncertainty,
            sources: SOURCES.map(String::from).into(),
        },
        bucket: "all",
        ttl,
        ttl_basis: ttl_basis.into(),
        replacement,
        waits: BashWaits {
            calls,
            long,
            timed_out,
            polling,
            miss_cost,
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
    use crate::report::{ReportOptions, format_report};
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
        let checks = bash_checks(&BashInput {
            scan: &report.scan,
            catalog: &prices(),
            ttls: &[],
            since: None,
            now: START + 60 * MIN,
        });
        report
            .checks
            .retain(|c| !matches!(c, CheckResult::BashCap(_)));
        report
            .checks
            .extend(checks.into_iter().map(CheckResult::BashCap));
        report
    }

    fn bash(report: &crate::model::Report) -> &BashCapCheck {
        report
            .checks
            .iter()
            .find_map(|c| match c {
                CheckResult::BashCap(c) => Some(c),
                _ => None,
            })
            .unwrap()
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
        let check = bash(&report);
        assert_eq!(check.ttl, verdict.base.recommended);
        assert!(check.ttl_basis.contains("recommends"));
    }

    #[test]
    fn a_default_cap_past_the_cache_ttl_changes_and_prices_the_long_wait() {
        let report = report(r#"{"promptCacheTtl":"5m"}"#);
        let check = bash(&report);
        assert_eq!(check.base.action, Action::Change);
        assert_eq!(check.base.current, Some(600_000));
        assert_eq!(check.base.recommended, Some(240_000));
        assert_eq!(check.ttl, Some(Ttl::FiveMinutes));
        assert!(
            check
                .base
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
        let text = format_report(&report, ReportOptions::default());
        let line = crate::test_support::report_line(&text, "Bash timeout cap 10m -> 4m");
        assert_eq!(line.mark, "change");
        assert!(
            line.evidence
                .starts_with("A wait that long outlasts the 5m cache."),
            "{text}"
        );
        // The miss cost is the window's total, like the wait count, not a 30-day rate.
        assert!(
            text.contains(&format!(
                "1 foreground Bash wait outlasted the cache (1 hit the timeout, 1 polled in a loop); the cache misses cost {} over the window.",
                crate::report::about(expected)
            )),
            "{text}"
        );
        assert!(!format!("{report:?}").contains("SYNTHETIC_PRIVATE"));
        assert!(!text.contains("SYNTHETIC_PRIVATE"));
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
        let checks = bash_checks(&BashInput {
            scan: &data,
            catalog: &prices(),
            ttls: &[],
            since: None,
            now: START + 60 * MIN,
        });
        let waits = &checks[0].waits;
        assert_eq!((waits.long, waits.timed_out), (2, 2));
        let next = data.requests.iter().find(|r| r.time > START).unwrap();
        let rates = unit_rates(&prices(), next, &next.parts[0]).unwrap();
        let expected = 100_000.0 * (rates.write5m - rates.read);
        assert!((waits.miss_cost.total_usd.unwrap().to_f64() - expected).abs() < 1e-9);
    }

    #[test]
    fn a_cap_under_the_ttl_keeps_and_a_larger_default_timeout_counts() {
        let keep = report(r#"{"promptCacheTtl":"5m","env":{"BASH_MAX_TIMEOUT_MS":"200000"}}"#);
        let check = bash(&keep);
        assert_eq!(check.base.action, Action::Keep);
        assert_eq!(check.base.current, Some(200_000));
        // The default timeout raises the effective cap past the TTL, so both must come down.
        let change = report(
            r#"{"promptCacheTtl":"5m","env":{"BASH_MAX_TIMEOUT_MS":"200000","BASH_DEFAULT_TIMEOUT_MS":"300000"}}"#,
        );
        let check = bash(&change);
        assert_eq!(check.base.action, Action::Change);
        assert_eq!(check.base.current, Some(300_000));
        assert!(
            check
                .replacement
                .contains("BASH_DEFAULT_TIMEOUT_MS: \"240000\"")
        );
    }

    #[test]
    fn an_unset_main_ttl_falls_back_to_the_recorded_write_lifetime() {
        let report = report("{}");
        let check = bash(&report);
        assert_eq!(check.ttl, Some(Ttl::FiveMinutes));
        assert_ne!(check.base.action, Action::Advisory);
    }
}
