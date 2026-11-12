//! Rate-only fast-mode premiums at the current profile's speed setting.

use crate::collections::IndexSet;

use crate::controls::claude_fast_support;
use crate::model::{
    Action, BillingPart, CheckBase, Cost, CurrentPremium, Evidence, Harness, Millis,
    ObservedPremium, PartKind, Request, RequestEvidence, Scan, SpeedCheck, SpeedControl, Text,
};
use crate::prices::{Catalog, add_costs, price_components, unknown_cost, zero_cost};

const SOURCES: [&str; 9] = [
    "https://code.claude.com/docs/en/fast-mode (2026-11-07)",
    "https://code.claude.com/docs/en/settings-reference#fastmode (2026-11-07)",
    "https://learn.chatgpt.com/docs/agent-configuration/speed (2026-11-07)",
    "https://learn.chatgpt.com/docs/config-file/config-reference (2026-11-07)",
    "https://learn.chatgpt.com/docs/config-file/config-advanced#profiles (2026-11-07)",
    "https://developers.openai.com/api/docs/guides/fast-mode (2026-11-07)",
    "https://developers.openai.com/api/docs/guides/ultrafast-mode (2026-11-07)",
    "https://developers.openai.com/api/docs/pricing (2026-11-07)",
    "https://developers.openai.com/api/docs/models/gpt-5.6 (2026-11-07)",
];

const DAY: f64 = 86_400_000.0;

/// Whether a part was billed at a premium speed; None when the record cannot say.
fn premium(r: &Request, p: &BillingPart) -> Option<bool> {
    let tier = p.tier.as_deref().unwrap_or("");
    let standard_tier = matches!(tier, "standard" | "default");
    if r.harness == Harness::Claude {
        if !standard_tier {
            return None;
        }
        return match p.speed.as_deref() {
            Some("fast") => Some(true),
            Some("standard") => Some(false),
            _ => None,
        };
    }
    if matches!(tier, "fast" | "priority" | "ultrafast") {
        Some(true)
    } else if standard_tier {
        Some(false)
    } else {
        None
    }
}

fn with_speed(p: &BillingPart, tier: Option<&str>, speed: Option<&str>) -> BillingPart {
    BillingPart {
        tier: tier.map(Text::from),
        speed: speed.map(Text::from),
        ..p.clone()
    }
}

/// The premium of `p` over the same part at standard speed.
fn difference(catalog: &Catalog, r: &Request, p: &BillingPart) -> Cost {
    if r.evidence == RequestEvidence::Unknown
        || r.issues.iter().any(|s| {
            s.contains("no verified request delta")
                || s.contains("usage is absent")
                || s.contains("no linked assistant usage")
        })
    {
        return unknown_cost("no verified billed usage for speed comparison");
    }
    let billed = price_components(catalog, r, p);
    let normal = price_components(
        catalog,
        r,
        &with_speed(p, Some("default"), Some("standard")),
    );
    let pairs = [
        (&billed.cache, &normal.cache),
        (&billed.ordinary, &normal.ordinary),
        (&billed.output, &normal.output),
    ];
    let deltas: Vec<Cost> = pairs
        .into_iter()
        .map(|(a, b)| {
            // A matched missing category contributes unknown to both sides. Only the
            // identically covered numeric categories contribute a known difference.
            let sorted = |c: &Cost| {
                let mut v = c.unknown.clone();
                v.sort();
                v
            };
            let shared_coverage = sorted(a) == sorted(b);
            let both = a.total_usd.zip(b.total_usd);
            let union = |x: &[String], y: &[String]| -> Vec<String> {
                x.iter()
                    .chain(y)
                    .cloned()
                    .collect::<IndexSet<_>>()
                    .into_iter()
                    .collect()
            };
            Cost {
                known_usd: if both.is_some() || shared_coverage {
                    a.known_usd - b.known_usd
                } else {
                    0.0
                },
                total_usd: both.map(|(a, b)| a - b),
                unknown: union(&a.unknown, &b.unknown),
                assumptions: union(&a.assumptions, &b.assumptions),
            }
        })
        .collect();
    add_costs(&deltas)
}

/// Inputs for the fast-mode checks.
pub struct SpeedInput<'a> {
    /// Profiles with their current speed controls, and the recorded requests.
    pub scan: &'a Scan,
    /// Public prices.
    pub catalog: &'a Catalog,
    /// The explicit window start; None uses each card's first request.
    pub since: Option<Millis>,
    /// The window end, which monthly values scale to.
    pub now: Millis,
}

/// Compare speed alone at the current profile baseline, independently of TTL/window recommendations.
pub fn speed_checks(input: &SpeedInput<'_>) -> Vec<SpeedCheck> {
    let SpeedInput {
        scan,
        catalog,
        since,
        now,
    } = *input;
    let mut result = vec![];
    for profile in &scan.profiles {
        let Some(speed) = &profile.speed else {
            continue;
        };
        let requests: Vec<&Request> = scan
            .requests
            .iter()
            .filter(|r| r.profiles.contains(&profile.id) && since.is_none_or(|s| r.time >= s))
            .collect();
        // Requests grouped by the control that applies at their working directory, in first-seen order.
        let mut groups: Vec<(&SpeedControl, Vec<&Request>)> = vec![];
        for &r in &requests {
            let control = r
                .cwd
                .as_deref()
                .filter(|cwd| !cwd.is_empty())
                .and_then(|cwd| profile.speed_directories.as_ref()?.get(cwd))
                .unwrap_or(speed);
            match groups.iter_mut().find(|(c, _)| *c == control) {
                Some((_, group)) => group.push(r),
                None => groups.push((control, vec![r])),
            }
        }
        if requests.is_empty() {
            groups.push((speed, vec![]));
        }
        for (control, requests) in groups {
            result.push(check(catalog, &profile.id, control, &requests, since, now));
        }
    }
    result
}

fn check(
    catalog: &Catalog,
    profile: &str,
    control: &SpeedControl,
    requests: &[&Request],
    since: Option<Millis>,
    now: Millis,
) -> SpeedCheck {
    let (mut premium_parts, mut standard_parts, mut unknown_parts) = (0, 0, 0);
    let (mut priced_parts, mut estimate_unknown) = (0, 0);
    let mut observed: Vec<Cost> = vec![];
    let mut estimated: Vec<Cost> = vec![];
    for &r in requests {
        if r.parts.is_empty() {
            unknown_parts += 1;
            observed.push(unknown_cost("request parts are absent"));
            estimate_unknown += 1;
            estimated.push(unknown_cost("request parts are absent"));
        }
        for p in &r.parts {
            match premium(r, p) {
                Some(true) => {
                    premium_parts += 1;
                    observed.push(difference(catalog, r, p));
                }
                Some(false) => {
                    standard_parts += 1;
                    observed.push(zero_cost());
                }
                None => {
                    unknown_parts += 1;
                    observed.push(unknown_cost(
                        "billed response speed/tier is unknown; requested controls are not billing evidence",
                    ));
                }
            }
            let value = match control.value.as_deref() {
                Some(value) if control.resolved => value,
                _ => {
                    estimate_unknown += 1;
                    estimated.push(unknown_cost("current speed ownership is unresolved"));
                    continue;
                }
            };
            if value == "standard" {
                estimated.push(zero_cost());
                continue;
            }
            // Claude only enables fast mode on supported Opus; other models stay standard.
            if r.harness == Harness::Claude
                && claude_fast_support(p.model.as_deref()) == Some(false)
            {
                estimated.push(zero_cost());
                continue;
            }
            if matches!(p.kind, PartKind::Advisor | PartKind::Unknown) {
                estimate_unknown += 1;
                estimated.push(unknown_cost(
                    "current speed ownership of advisor/unknown iteration is not established",
                ));
                continue;
            }
            let current = if r.harness == Harness::Claude {
                with_speed(p, Some("standard"), Some("fast"))
            } else {
                with_speed(p, Some(value), None)
            };
            let delta = difference(catalog, r, &current);
            if delta.total_usd.is_none() {
                estimate_unknown += 1;
            } else {
                priced_parts += 1;
            }
            estimated.push(delta);
        }
    }
    let start = since.or(requests.first().map(|r| r.time));
    let month = |v: Option<f64>| match (v, start) {
        (Some(v), Some(start)) if now > start => Some((v * 30.0 * DAY) / (now - start) as f64),
        _ => None,
    };
    let observed_cost = if observed.is_empty() {
        unknown_cost("no selected billed usage")
    } else {
        add_costs(&observed)
    };
    let current_cost = if estimated.is_empty() {
        unknown_cost("no selected usage for current-setting estimate")
    } else {
        add_costs(&estimated)
    };
    let action = match control.value.as_deref() {
        Some(_) if !control.resolved => Action::Advisory,
        None => Action::Advisory,
        Some("standard") => Action::Keep,
        Some(_) => Action::Change,
    };
    let change = action == Action::Change;
    let history_profiles: IndexSet<&String> =
        requests.iter().flat_map(|r| r.profiles.iter()).collect();
    let mut uncertainty = vec![
        control.reason.clone(),
        "account/server policy, availability, cloud-managed defaults, launch overrides and live session toggles not observed in local files may override these defaults".into(),
    ];
    uncertainty.extend(observed_cost.unknown.iter().cloned());
    uncertainty.extend(current_cost.unknown.iter().cloned());
    SpeedCheck {
        base: CheckBase {
            profile: profile.to_string(),
            history_profiles: history_profiles.into_iter().cloned().collect(),
            evidence: Evidence::Guidance,
            action,
            current: control.value.clone(),
            recommended: control.resolved.then(|| "standard".to_string()),
            owner: control.owner.clone(),
            since: start,
            until: Some(now),
            window_basis: if since.is_none() {
                "available selected usage; current files applied to history, no current session override inferred"
            } else {
                "explicit --since; current files applied to history"
            }
            .into(),
            baseline: "current profile speed, holding recorded model, work, tokens, cache categories, geography, context tiers and all other settings constant; independent of TTL/window/idle recommendations".into(),
            requests: requests.len() as u64,
            coverage: None,
            savings_usd: if change { current_cost.total_usd } else { None },
            monthly_savings_usd: if change { month(current_cost.total_usd) } else { None },
            break_even: None,
            assumptions: [
                "maintainer cost preference: fast and ultrafast off; setting warning does not require historical billing metadata",
                "rate-only API-price equivalent at present public prices, not an invoice, subscription bill, purchased-credit or rate-limit multiplier",
                "recorded cache categories and context tiers held constant; Claude fast/standard have separate caches, so switching cold-cache effects are excluded",
                "fixed behavior and work; latency value is not modeled; no summed per-check savings",
                "current local files and active-profile environment supply defaults; historical session overrides never establish current session state",
            ]
            .map(String::from)
            .into(),
            uncertainty,
            sources: SOURCES.map(String::from).into(),
        },
        bucket: "all",
        replacement: control.replacement.clone(),
        reason: control.reason.clone(),
        observed: ObservedPremium {
            premium_parts,
            standard_parts,
            unknown_parts,
            monthly_premium_usd: month(observed_cost.total_usd),
            premium: observed_cost,
        },
        current_estimate: CurrentPremium {
            premium: current_cost,
            priced_parts,
            unknown_parts: estimate_unknown,
        },
        usage_estimates: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Api, CacheContext, ExposureScan, ScanCoverage, Ttl};
    use crate::test_support::{ClaudeUsage, claude_usage, make_profile, make_request, prices};

    const DAY_MS: Millis = 86_400_000;

    fn control(value: Option<&str>, resolved: bool) -> SpeedControl {
        SpeedControl {
            value: value.map(String::from),
            owner: "/synthetic/.claude/settings.json:fastMode".into(),
            replacement: "fastMode: false".into(),
            resolved,
            reason: "synthetic fastMode preference".into(),
        }
    }

    fn scan(speed: SpeedControl, requests: Vec<Request>) -> Scan {
        let mut profile = make_profile("profile", "/synthetic/.claude");
        profile.speed = Some(speed);
        Scan {
            subagents: vec![],
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
            t3: vec![],
        }
    }

    fn checks(scan: &Scan, since: Option<Millis>, now: Millis) -> Vec<SpeedCheck> {
        speed_checks(&SpeedInput {
            scan,
            catalog: &prices(),
            since,
            now,
        })
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-12
    }

    /// One fast 5m part in the US and one standard 1h part, with server fees.
    fn premium_history() -> Scan {
        let mut r = make_request();
        let usage = ClaudeUsage {
            input: 100,
            write: 200,
            read: 300,
            output: 50,
            ..ClaudeUsage::default()
        };
        let mut fast = r.parts[0].clone();
        fast.speed = Some("fast".into());
        fast.geo = Some("us".into());
        fast.usage = claude_usage(usage);
        let mut standard = r.parts[0].clone();
        standard.usage = claude_usage(ClaudeUsage {
            ttl: Ttl::OneHour,
            ..usage
        });
        r.parts = vec![fast, standard];
        r.server_tools = Some(
            [("web_search_requests".to_string(), 999)]
                .into_iter()
                .collect(),
        );
        scan(control(Some("fast"), true), vec![r])
    }

    #[test]
    fn rate_only_premium_preserves_cache_lifetimes_and_geography() {
        let s = premium_history();
        let result = &checks(&s, None, 30 * DAY_MS)[0];
        assert_eq!(result.base.action, Action::Change);
        assert_eq!(result.base.evidence, Evidence::Guidance);
        // Fast doubles every rate; the US part carries the 1.1 geography multiplier.
        let observed = ((100.0 * 4.0 + 200.0 * 5.0 + 300.0 * 0.2 + 50.0 * 20.0) * 1.1) / 1e6;
        assert!(close(result.observed.premium.total_usd.unwrap(), observed));
        let estimate = observed + (100.0 * 4.0 + 200.0 * 8.0 + 300.0 * 0.2 + 50.0 * 20.0) / 1e6;
        assert!(close(result.base.savings_usd.unwrap(), estimate));
        assert_eq!(
            result.observed.monthly_premium_usd,
            result.observed.premium.total_usd
        );
        assert_eq!(
            (
                result.observed.premium_parts,
                result.observed.standard_parts,
                result.current_estimate.priced_parts
            ),
            (1, 1, 2)
        );

        // Old billed premiums never recommend changing a current standard setting.
        let mut standard = s.clone();
        standard.profiles[0].speed = Some(control(Some("standard"), true));
        let keep = &checks(&standard, None, DAY_MS)[0];
        assert_eq!(keep.base.action, Action::Keep);
        assert_eq!(keep.base.savings_usd, None);
        assert!(keep.observed.premium.total_usd.unwrap() > 0.0);

        let mut open = s;
        open.profiles[0].speed = Some(control(Some("fast"), false));
        let advisory = &checks(&open, None, DAY_MS)[0];
        assert_eq!(advisory.base.action, Action::Advisory);
        assert_eq!(advisory.base.recommended, None);
        assert_eq!(advisory.current_estimate.premium.total_usd, None);
    }

    #[test]
    fn unknown_speeds_price_gaps_and_windows_never_become_zero() {
        let mut r = make_request();
        r.time = 100;
        r.parts[0].speed = None;
        let mut s = scan(control(Some("fast"), true), vec![r]);
        let result = &checks(&s, Some(50), 200)[0];
        assert_eq!(result.observed.unknown_parts, 1);
        assert_eq!(result.observed.premium.total_usd, None);
        assert_eq!(result.observed.premium.known_usd, 0.0);
        assert!(result.current_estimate.premium.total_usd.unwrap() > 0.0);

        let excluded = &checks(&s, Some(150), 200)[0];
        assert_eq!(excluded.base.requests, 0);
        assert_eq!(excluded.base.savings_usd, None);
        assert_eq!(excluded.base.action, Action::Change);

        s.requests[0].parts[0].speed = Some("fast".into());
        s.requests[0].parts[0].model = Some("claude-unpriced-model".into());
        s.requests[0].model = Some("claude-unpriced-model".into());
        let unpriced = &checks(&s, None, 200)[0];
        assert_eq!(unpriced.observed.premium.total_usd, None);
        assert_eq!(unpriced.base.action, Action::Change);
    }

    /// Codex requests at two directories whose project controls differ.
    fn directories() -> Scan {
        let request = |id: &str, cwd: &str| {
            let mut r = make_request();
            r.id = id.into();
            r.cwd = Some(cwd.into());
            r.harness = Harness::Codex;
            r.provider = Some("openai".into());
            r.api = Api::Responses;
            r.parts[0].model = Some("gpt-6.1-sol".into());
            r.parts[0].tier = Some("default".into());
            r.parts[0].speed = None;
            r
        };
        let mut s = scan(
            control(Some("fast"), true),
            vec![request("a", "a"), request("b", "b"), request("c", "")],
        );
        s.profiles[0].harness = Harness::Codex;
        s.profiles[0].speed_directories = Some(
            [
                ("a".to_string(), control(Some("ultrafast"), true)),
                ("b".to_string(), control(Some("standard"), true)),
            ]
            .into_iter()
            .collect(),
        );
        s
    }

    #[test]
    fn distinct_project_controls_get_separate_cards() {
        let cards = checks(&directories(), None, 100);
        let summary: Vec<_> = cards
            .iter()
            .map(|c| (c.base.action, c.base.current.as_deref(), c.base.requests))
            .collect();
        // The empty working directory falls back to the profile's own control.
        assert_eq!(
            summary,
            [
                (Action::Change, Some("ultrafast"), 1),
                (Action::Keep, Some("standard"), 1),
                (Action::Change, Some("fast"), 1),
            ]
        );
        assert!(cards[0].base.savings_usd.unwrap() > cards[2].base.savings_usd.unwrap());
    }

    /// The TypeScript fast-mode tests that read real settings files through `scan`, then `speed_checks` or `analyze`.
    mod end_to_end {
        use super::super::{SpeedInput, speed_checks};
        use crate::controls::{Env, SettingsScope, speed_controls};
        use crate::discovery::DiscoveryOptions;
        use crate::model::*;
        use crate::prices::Catalog;
        use crate::reader::{ScanOptions, scan};
        use crate::replay::{AnalyzeOptions, analyze};
        use crate::report::{ReportOptions, format_report};
        use crate::test_support::{ClaudeUsage, Workspace, claude_usage, make_request, prices};
        use serde_json::{Value, json};
        use std::path::Path;

        const DAY_MS: Millis = 86_400_000;

        fn text(path: &Path) -> String {
            // Windows paths joined from "a/b" keep the slash; the code under test normalizes it.
            let path: std::path::PathBuf = if cfg!(windows) {
                path.components().collect()
            } else {
                path.to_path_buf()
            };
            path.to_string_lossy().into_owned()
        }

        fn write(path: impl AsRef<Path>, contents: &str) {
            std::fs::write(path, contents).unwrap();
        }

        /// The TypeScript `workspace().options`: this home, no environment, a checkout-local managed file.
        fn options(w: &Workspace) -> DiscoveryOptions {
            DiscoveryOptions {
                home: text(&w.home),
                env: Env::default(),
                claude_dirs: vec![],
                codex_dirs: vec![],
                t3_dirs: vec![],
                managed_settings_file: text(&w.managed_settings_file()),
                codex_system_config_file: text(&w.home.join("system.toml")),
                codex_requirements_file: text(&w.home.join("requirements.toml")),
            }
        }

        fn scanned(discovery: DiscoveryOptions) -> Scan {
            scan(&ScanOptions {
                discovery,
                since: None,
            })
            .unwrap()
        }

        fn claude_dirs(w: &Workspace) -> DiscoveryOptions {
            DiscoveryOptions {
                claude_dirs: vec![text(&w.claude)],
                ..options(w)
            }
        }

        fn codex_dirs(w: &Workspace) -> DiscoveryOptions {
            DiscoveryOptions {
                codex_dirs: vec![text(&w.codex)],
                ..options(w)
            }
        }

        fn checks(
            scan: &Scan,
            catalog: &Catalog,
            since: Option<Millis>,
            now: Millis,
        ) -> Vec<SpeedCheck> {
            speed_checks(&SpeedInput {
                scan,
                catalog,
                since,
                now,
            })
        }

        /// The TypeScript `control(harness, config)`: a user scope over an empty managed scope.
        fn control(harness: Harness, config: Value) -> SpeedControl {
            let config = config.as_object().cloned().unwrap();
            let managed = serde_json::Map::new();
            speed_controls(
                harness,
                &[
                    SettingsScope {
                        config: &config,
                        file: "user",
                    },
                    SettingsScope {
                        config: &managed,
                        file: "managed",
                    },
                ],
                &Env::default(),
            )
            .unwrap()
        }

        fn claude(scan: &Scan) -> &Profile {
            scan.profiles
                .iter()
                .find(|p| p.harness == Harness::Claude)
                .unwrap()
        }

        fn day(text: &str) -> Millis {
            format!("{text}T00:00:00Z")
                .parse::<jiff::Timestamp>()
                .unwrap()
                .as_millisecond()
        }

        fn close(a: f64, b: f64) -> bool {
            (a - b).abs() < 1e-12
        }

        fn request(profile: &Profile) -> Request {
            Request {
                profiles: vec![profile.id.clone()].into(),
                ..make_request()
            }
        }

        // Ported from fast.test.ts: "rate-only premium preserves Claude 5m/1h reads/writes, geography and fixed fees".
        #[test]
        fn rate_only_premium_preserves_claude_cache_lifetimes_geography_and_fixed_fees() {
            let w = Workspace::new();
            write(
                w.claude.join("settings.json"),
                &json!({ "fastMode": true, "model": "opus" }).to_string(),
            );
            let mut s = scanned(options(&w));
            let usage = ClaudeUsage {
                input: 100,
                write: 200,
                read: 300,
                output: 50,
                ..ClaudeUsage::default()
            };
            let base = make_request().parts[0].clone();
            let r = Request {
                parts: vec![
                    BillingPart {
                        speed: Some("fast".into()),
                        geo: Some("us".into()),
                        usage: claude_usage(usage),
                        ..base.clone()
                    },
                    BillingPart {
                        speed: Some("standard".into()),
                        usage: claude_usage(ClaudeUsage {
                            ttl: Ttl::OneHour,
                            ..usage
                        }),
                        ..base
                    },
                ],
                server_tools: Some(
                    [("web_search_requests".to_string(), 999)]
                        .into_iter()
                        .collect(),
                ),
                ..request(claude(&s))
            };
            s.requests = vec![r.clone()];
            let report = analyze(AnalyzeOptions {
                scan: s.clone(),
                catalog: &prices(),
                since: None,
                now: 30 * DAY_MS,
                max_cost_per_use_usd: 1.0,
            })
            .unwrap();
            let result = report
                .checks
                .iter()
                .find_map(|c| match c {
                    CheckResult::Speed(c) => Some(c),
                    _ => None,
                })
                .unwrap();
            assert_eq!(result.base.action, Action::Change);
            assert_eq!(result.base.evidence, Evidence::Guidance);
            let observed = ((100.0 * 4.0 + 200.0 * 5.0 + 300.0 * 0.2 + 50.0 * 20.0) * 1.1) / 1e6;
            assert!(close(result.observed.premium.total_usd.unwrap(), observed));
            let estimate = observed + (100.0 * 4.0 + 200.0 * 8.0 + 300.0 * 0.2 + 50.0 * 20.0) / 1e6;
            assert!(close(result.base.savings_usd.unwrap(), estimate));
            assert_eq!(
                result.observed.monthly_premium_usd,
                result.observed.premium.total_usd
            );
            assert_eq!(result.observed.premium_parts, 1);
            assert_eq!(result.observed.standard_parts, 1);
            assert_eq!(result.current_estimate.priced_parts, 2);
            let json = serde_json::to_string(&report).unwrap();
            assert!(format_report(&report, ReportOptions::default()).contains("Fast mode ["));
            let details = format_report(
                &report,
                ReportOptions {
                    details: true,
                    ..ReportOptions::default()
                },
            );
            for expected in [
                "Observed response-tier premium",
                "Current-setting premium estimate",
                "cache-switch effects excluded",
                "rate-only",
                "fastMode: false",
            ] {
                assert!(details.contains(expected), "{expected}");
            }
            assert_eq!(serde_json::to_string(&report).unwrap(), json);
            let profile = s
                .profiles
                .iter_mut()
                .find(|p| p.harness == Harness::Claude)
                .unwrap();
            profile.speed = Some(control(Harness::Claude, json!({ "fastMode": false })));
            let cards = checks(&s, &prices(), None, DAY_MS);
            let standard = cards
                .iter()
                .find(|c| c.base.profile == r.profiles[0])
                .unwrap();
            assert_eq!(
                standard.base.action,
                Action::Keep,
                "old billed premiums must not recommend changing current standard speed"
            );
            assert_eq!(standard.base.savings_usd, None);
            assert!(standard.observed.premium.total_usd.unwrap() > 0.0);
        }

        // Ported from fast.test.ts: "current-setting warning survives missing billing metadata; historical unknowns and prices stay unknown".
        #[test]
        fn current_setting_warning_survives_missing_billing_metadata() {
            let w = Workspace::new();
            write(
                w.codex.join("config.toml"),
                "model = \"gpt-6.1-sol\"\nservice_tier = \"ultrafast\"\n",
            );
            w.fixture(w.codex.join("sessions/main.jsonl"), "codex.jsonl");
            let mut s = scanned(codex_dirs(&w));
            let mut c = prices();
            let now = day("2026-11-02");
            let check = |s: &Scan, c: &Catalog| checks(s, c, None, now).remove(0);
            assert_eq!(
                s.requests[0].parts[0].tier, None,
                "native Codex usage records do not contain billed tiers"
            );
            assert_eq!(
                s.requests[0]
                    .requested_speed
                    .as_ref()
                    .unwrap()
                    .tier
                    .as_deref(),
                Some("standard")
            );
            let first = check(&s, &c);
            assert_eq!(first.base.action, Action::Change);
            assert_eq!(first.observed.premium.total_usd, None);
            assert!(
                first.base.savings_usd.unwrap() > 0.0,
                "current config estimate may use measured tokens without inventing an observed premium"
            );
            c.clear_models();
            let unpriced = check(&s, &c);
            assert_eq!(unpriced.base.action, Action::Change);
            assert_eq!(unpriced.base.savings_usd, None);
            assert_eq!(unpriced.current_estimate.premium.total_usd, None);
            s.requests = vec![];
            let empty = check(&s, &c);
            assert_eq!(empty.base.action, Action::Change);
            assert_eq!(empty.base.savings_usd, None);
        }

        // Ported from fast.test.ts: "active-profile environment, managed policy and current project ownership suppress stale preferences".
        #[test]
        fn active_profile_environment_managed_policy_and_project_ownership_suppress_stale_preferences()
         {
            let w = Workspace::new();
            write(
                w.claude.join("settings.json"),
                r#"{"model":"opus","fastMode":true}"#,
            );
            let file = w.claude.join("projects/project/main.jsonl");
            w.fixture(&file, "claude.jsonl");
            let env_scan = scanned(DiscoveryOptions {
                env: [
                    ("CLAUDE_CONFIG_DIR".to_string(), text(&w.claude)),
                    ("CLAUDE_CODE_DISABLE_FAST_MODE".to_string(), "1".to_string()),
                ]
                .into_iter()
                .collect(),
                ..options(&w)
            });
            assert_eq!(
                claude(&env_scan).speed.as_ref().unwrap().value.as_deref(),
                Some("standard")
            );
            let managed_file = w.managed_settings_file();
            write(&managed_file, r#"{"fastMode":false}"#);
            let managed = scanned(options(&w));
            assert_eq!(
                claude(&managed).speed.as_ref().unwrap().owner,
                format!("{}:fastMode", text(&managed_file))
            );
            write(&managed_file, "{}");
            std::fs::create_dir(w.home.join(".git")).unwrap();
            std::fs::create_dir_all(w.home.join(".claude-project/.claude")).unwrap();
            let cwd = text(&w.home.join(".claude-project"));
            // A repository rooted at home keeps the local file at the session directory.
            let local = w.home.join(".claude-project/.claude/settings.local.json");
            write(&local, r#"{"fastMode":false}"#);
            let rows: String = std::fs::read_to_string(&file)
                .unwrap()
                .trim_end()
                .split('\n')
                .map(|line| {
                    let mut row: Value = serde_json::from_str(line).unwrap();
                    row["cwd"] = json!(cwd);
                    format!("{row}\n")
                })
                .collect();
            write(&file, &rows);
            let project = scanned(options(&w));
            let id = &claude(&project).id;
            let cards = checks(&project, &prices(), None, day("2026-11-02"));
            let result = cards.iter().find(|c| &c.base.profile == id).unwrap();
            assert_eq!(result.base.action, Action::Keep);
            assert_eq!(result.base.owner, format!("{}:fastMode", text(&local)));
        }

        // Ported from fast.test.ts: "Codex legacy/named profiles and project selection stay advisory; local requirements can block premium".
        #[test]
        fn codex_legacy_named_profiles_stay_advisory_and_local_requirements_block_premium() {
            let w = Workspace::new();
            let config = w.codex.join("config.toml");
            write(
                &config,
                "model=\"gpt-6.1-sol\"\nservice_tier=\"fast\"\n[profiles.special]\nservice_tier=\"default\"",
            );
            let requirements = w.home.join("requirements.toml");
            let speed = || {
                scanned(DiscoveryOptions {
                    codex_requirements_file: text(&requirements),
                    ..codex_dirs(&w)
                })
                .profiles[0]
                    .speed
                    .clone()
                    .unwrap()
            };
            assert!(!speed().resolved);
            write(&config, "model=\"gpt-6.1-sol\"\nservice_tier=\"fast\"");
            let named = w.codex.join("special.config.toml");
            write(&named, "service_tier=\"default\"");
            assert!(speed().reason.contains("--profile selection"));
            std::fs::remove_file(&named).unwrap();
            write(&requirements, "[features]\nfast_mode=false");
            assert_eq!(speed().value.as_deref(), Some("standard"));
            write(&config, "model=\"gpt-6.1-sol\"\nservice_tier=\"ultrafast\"");
            write(&requirements, "[features]\nultrafast_mode=false");
            assert_eq!(speed().value.as_deref(), Some("standard"));
        }

        // Ported from fast.test.ts: "unknown speeds, price gaps and explicit windows never turn missing premiums into zero".
        #[test]
        fn unknown_speeds_price_gaps_and_explicit_windows_never_turn_missing_premiums_into_zero() {
            let w = Workspace::new();
            write(
                w.claude.join("settings.json"),
                r#"{"model":"opus","fastMode":true}"#,
            );
            let mut s = scanned(claude_dirs(&w));
            let mut r = Request {
                time: 100,
                ..request(&s.profiles[0])
            };
            r.parts[0].speed = None;
            s.requests = vec![r];
            let mut c = prices();
            let result = checks(&s, &c, Some(50), 200).remove(0);
            assert_eq!(result.observed.unknown_parts, 1);
            assert_eq!(result.observed.premium.total_usd, None);
            assert_eq!(result.observed.premium.known_usd, 0.0);
            assert!(result.current_estimate.premium.total_usd.unwrap() > 0.0);
            let excluded = checks(&s, &c, Some(150), 200).remove(0);
            assert_eq!(excluded.base.requests, 0);
            assert_eq!(excluded.base.savings_usd, None);
            s.requests[0].parts[0].speed = Some("fast".into());
            c.clear_models();
            let unknown = checks(&s, &c, None, 200).remove(0);
            assert_eq!(unknown.observed.premium.total_usd, None);
            assert_eq!(unknown.base.action, Action::Change);
        }

        // Ported from fast.test.ts: "unsupported Opus history has no hypothetical premium and unknown iteration ownership stays unpriced".
        #[test]
        fn unsupported_opus_history_has_no_hypothetical_premium_and_unknown_ownership_stays_unpriced()
         {
            let w = Workspace::new();
            write(
                w.claude.join("settings.json"),
                r#"{"model":"opus","fastMode":true}"#,
            );
            let mut s = scanned(claude_dirs(&w));
            s.requests = vec![request(&s.profiles[0])];
            let c = prices();
            for model in [
                "claude-opus-4-7",
                "claude-opus-4-1",
                "claude-opus-4-6",
                "claude-sonnet-5",
            ] {
                s.requests[0].parts[0].model = Some(model.into());
                let result = checks(&s, &c, None, 100).remove(0);
                assert_eq!(
                    result.current_estimate.premium.total_usd,
                    Some(0.0),
                    "{model}"
                );
                assert_eq!(result.base.action, Action::Change);
            }
            s.requests[0].parts = vec![BillingPart {
                kind: PartKind::Advisor,
                ..make_request().parts[0].clone()
            }];
            assert_eq!(
                checks(&s, &c, None, 100)[0]
                    .current_estimate
                    .premium
                    .total_usd,
                None
            );
        }

        // Ported from fast.test.ts: "known distinct project controls get separate cards and nearest Codex scope remains advisory owner".
        #[test]
        fn known_distinct_project_controls_get_separate_cards_and_nearest_codex_scope_owns() {
            let w = Workspace::new();
            let root = w.home.join("repo");
            let nested = root.join("nested");
            for path in [
                root.join(".git"),
                root.join(".codex"),
                nested.join(".codex"),
            ] {
                std::fs::create_dir_all(path).unwrap();
            }
            write(root.join(".codex/config.toml"), "service_tier=\"fast\"");
            let nearest = nested.join(".codex/config.toml");
            write(&nearest, "service_tier=\"default\"");
            let records = [
                json!({
                    "type": "session_meta",
                    "timestamp": "2026-11-01T00:00:00Z",
                    "payload": {
                        "id": "thread",
                        "source": "cli",
                        "model_provider": "openai",
                        "cwd": text(&nested),
                    },
                }),
                json!({
                    "type": "turn_context",
                    "timestamp": "2026-11-01T00:00:00Z",
                    "payload": { "model": "gpt-6.1-sol", "service_tier": "fast" },
                }),
                json!({
                    "type": "token_usage_record",
                    "timestamp": "2026-11-01T00:00:01Z",
                    "payload": {
                        "response_id": "r",
                        "usage": {
                            "input_tokens": 100,
                            "output_tokens": 1,
                            "cached_input_tokens": 0,
                            "cache_write_input_tokens": 0,
                        },
                    },
                }),
            ];
            let jsonl: String = records.iter().map(|r| format!("{r}\n")).collect();
            write(w.codex.join("sessions/main.jsonl"), &jsonl);
            let mut s = scanned(codex_dirs(&w));
            assert_eq!(
                s.profiles[0].speed_directories.as_ref().unwrap()[&text(&nested)].owner,
                text(&nearest)
            );
            let c = prices();
            assert_eq!(
                checks(&s, &c, None, day("2026-11-02"))[0].base.action,
                Action::Advisory
            );
            let p = &mut s.profiles[0];
            p.speed_directories = Some(
                [
                    (
                        "a".to_string(),
                        control(
                            Harness::Codex,
                            json!({ "model": "gpt-6.1-sol", "service_tier": "fast" }),
                        ),
                    ),
                    (
                        "b".to_string(),
                        control(Harness::Codex, json!({ "service_tier": "default" })),
                    ),
                ]
                .into_iter()
                .collect(),
            );
            let p = p.clone();
            s.requests = vec![
                Request {
                    id: "a".into(),
                    cwd: Some("a".into()),
                    ..request(&p)
                },
                Request {
                    id: "b".into(),
                    cwd: Some("b".into()),
                    ..request(&p)
                },
            ];
            let cards: Vec<_> = checks(&s, &c, None, 100)
                .iter()
                .map(|card| (card.base.action, card.base.requests))
                .collect();
            assert_eq!(cards, [(Action::Change, 1), (Action::Keep, 1)]);
        }

        // Ported from fast.test.ts: "managed blocks survive named/legacy alternatives while a separately allowed premium remains unresolved".
        #[test]
        fn managed_blocks_survive_named_alternatives_while_an_allowed_premium_stays_unresolved() {
            let w = Workspace::new();
            let file = w.codex.join("config.toml");
            let named = w.codex.join("review.config.toml");
            let requirements = w.home.join("requirements.toml");
            let speed = || {
                scanned(DiscoveryOptions {
                    codex_requirements_file: text(&requirements),
                    ..codex_dirs(&w)
                })
                .profiles[0]
                    .speed
                    .clone()
                    .unwrap()
            };
            write(&requirements, "[features]\nfast_mode=false");
            write(
                &file,
                "model=\"gpt-6.1-sol\"\nservice_tier=\"fast\"\n[profiles.review]\nservice_tier=\"default\"",
            );
            assert_eq!(speed().value.as_deref(), Some("standard"));
            write(&file, "model=\"gpt-6.1-sol\"\nservice_tier=\"fast\"");
            write(&named, "service_tier=\"default\"");
            let blocked = speed();
            assert_eq!(blocked.value.as_deref(), Some("standard"));
            assert!(blocked.resolved);
            assert!(blocked.owner.contains("requirements.toml:features"));
            write(&named, "service_tier=\"ultrafast\"");
            assert_eq!(speed().value, None);
            write(
                &requirements,
                "[features]\nfast_mode=false\nultrafast_mode=false",
            );
            assert_eq!(speed().value.as_deref(), Some("standard"));
        }

        // Ported from fast.test.ts: "partially priced premium retains covered categories as subtotal while its total remains unknown".
        #[test]
        fn partially_priced_premium_retains_covered_categories_as_a_subtotal() {
            let w = Workspace::new();
            write(
                w.claude.join("settings.json"),
                r#"{"model":"opus","fastMode":true}"#,
            );
            let mut s = scanned(claude_dirs(&w));
            let mut r = request(&s.profiles[0]);
            r.parts[0].speed = Some("fast".into());
            r.parts[0].usage = Usage {
                output: None,
                ..claude_usage(ClaudeUsage {
                    input: 100,
                    read: 300,
                    write: 200,
                    ..ClaudeUsage::default()
                })
            };
            s.requests = vec![r];
            let result = checks(&s, &prices(), None, 100).remove(0);
            let known = (100.0 * 4.0 + 300.0 * 0.2 + 200.0 * 5.0) / 1e6;
            assert_eq!(result.observed.premium.total_usd, None);
            assert!(close(result.observed.premium.known_usd, known));
            assert_eq!(result.base.savings_usd, None);
            assert!(close(result.current_estimate.premium.known_usd, known));
            assert!(result.observed.premium.unknown.join(" ").contains("output"));
        }
    }
}
