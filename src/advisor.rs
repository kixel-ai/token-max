//! Claude Code's advisor: the `advisorModel` set now, what stops it loading, what its recorded
//! calls cost, and the advisor `vibes` suggests one rank above the main-session default.

use serde_json::Value;

use crate::collections::IndexMap;
use crate::controls::{Env, SettingsScope, js_string, scoped_environment};
use crate::model::{
    AdvisorControl, AdvisorUse, Estimate, Harness, Millis, PartKind, Profile, Scan, Share,
};
use crate::prices::{Catalog, Sample, add_costs, price_part, request_evidence};
use crate::safe::ScanError;

/// Claude Code's page on the advisor tool.
pub(crate) const DOCS: &str = "https://code.claude.com/docs/en/advisor";

/// The setting that names the advisor model.
const KEY: &str = "advisorModel";

/// Providers with no advisor tool, by the variable that selects each.
const UNSUPPORTED: [(&str, &str); 4] = [
    ("CLAUDE_CODE_USE_BEDROCK", "Amazon Bedrock"),
    ("CLAUDE_CODE_USE_MANTLE", "Amazon Bedrock's Mantle endpoint"),
    ("CLAUDE_CODE_USE_VERTEX", "Google Cloud's Agent Platform"),
    ("CLAUDE_CODE_USE_FOUNDRY", "Microsoft Foundry"),
];

/// Providers that serve the advisor without the feature flag Claude Code fetches.
const FLAGLESS: [&str; 2] = [
    "CLAUDE_CODE_USE_ANTHROPIC_AWS",
    "CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD",
];

/// Variables that stop Claude Code fetching its feature flags, the advisor's among them, and
/// whether any non-empty value does (rather than only a true one).
const FLAG_OFF: [(&str, bool); 5] = [
    ("DISABLE_TELEMETRY", true),
    ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", true),
    ("DO_NOT_TRACK", false),
    ("CLAUDE_CODE_DISABLE_FEATURE_FLAGS", false),
    ("DISABLE_GROWTHBOOK", false),
];

/// Resolve the advisor from `scopes`, lowest precedence first, and the terminal `env`.
/// `user_file` is where an advisor would be set when no scope sets one.
pub fn control(
    scopes: &[SettingsScope],
    env: &Env,
    user_file: &str,
) -> Result<AdvisorControl, ScanError> {
    let set = scopes.iter().rev().find_map(|s| {
        s.config
            .get(KEY)
            .filter(|v| !v.is_null())
            .map(|v| (v, s.file))
    });
    let (model, owner) = match set {
        Some((Value::String(m), file)) => (
            Some(m.trim().to_string()).filter(|m| !m.is_empty()),
            format!("{file}:{KEY}"),
        ),
        Some((_, file)) => {
            return Err(ScanError::at(
                format!("invalid {KEY}"),
                crate::model::Location {
                    file: file.into(),
                    line: 1,
                },
            ));
        }
        None => (None, format!("{user_file}:{KEY}")),
    };
    let environment = scoped_environment(scopes, env);
    let value = |key: &str| {
        let value = js_string(environment.values.get(key));
        (environment.values.contains_key(key) && !value.is_empty()).then_some(value)
    };
    let on = |key: &str| {
        value(key).is_some_and(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
    };
    // Where a variable is set, as a reader would look for it.
    let named = |key: &str| {
        let owner = &environment.owners[key];
        match owner.strip_suffix(&format!(":env.{key}")) {
            Some(file) => format!("{key}, set in {file},"),
            None => format!("{key}, set in the environment,"),
        }
    };
    let flagless = FLAGLESS.iter().any(|key| on(key));
    let blocked = if on("CLAUDE_CODE_DISABLE_ADVISOR_TOOL") {
        Some(format!(
            "{} turns the advisor tool off",
            named("CLAUDE_CODE_DISABLE_ADVISOR_TOOL")
        ))
    } else if let Some((key, provider)) = UNSUPPORTED.iter().find(|(key, _)| on(key)) {
        Some(format!(
            "{} sends requests to {provider}, which has no advisor tool",
            named(key)
        ))
    } else if flagless {
        None
    } else {
        FLAG_OFF
            .iter()
            .find(|(key, any)| if *any { value(key).is_some() } else { on(key) })
            .map(|(key, _)| {
                format!(
                    "{} turns off the feature-flag fetch the advisor needs",
                    named(key)
                )
            })
    };
    let gateway =
        (!flagless && value("ANTHROPIC_BASE_URL").is_some()).then(|| named("ANTHROPIC_BASE_URL"));
    Ok(AdvisorControl {
        model,
        owner,
        blocked,
        gateway,
    })
}

/// What puts a profile on an advisor line: the `advisorModel` owner, and the main-session
/// default and control the line's card reads. None without an advisor control.
pub(crate) fn owner(p: &Profile) -> Option<String> {
    let a = p.advisor.as_ref()?;
    let main = p.effort.as_ref().map(|e| (&e.model, &e.unresolved));
    Some(format!(
        "{}\nmain {main:?}\n{:?} {:?} {:?}",
        a.owner, a.model, a.blocked, a.gateway
    ))
}

/// The advisor calls of each line of Claude profiles that share an advisor owner, in the
/// window from `since`, else from the first request.
pub fn advisor_use(scan: &Scan, catalog: &Catalog, since: Option<Millis>) -> Vec<AdvisorUse> {
    let advised = scan.profiles.iter().filter(|p| p.advisor.is_some());
    // A T3 instance has no card of its own, but its threads read the same settings file, so
    // its calls count on the first line of that file.
    let file = |p: &Profile| crate::rows::canonical(&format!("{}:", p.settings_file));
    let mut t3: Vec<&Profile> = scan
        .profiles
        .iter()
        .filter(|p| p.harness == Harness::Claude && p.t3.is_some())
        .collect();
    crate::rows::groups(advised, |p| owner(p).expect("filtered on an advisor"))
        .iter()
        .map(|line| {
            let lead = file(line.lead);
            let mut ids = line.ids.clone();
            t3.retain(|p| {
                let mine = file(p) == lead;
                if mine {
                    ids.push(p.id.clone());
                }
                !mine
            });
            let (mut requests, mut calls) = (0, 0);
            let mut models: IndexMap<String, u64> = IndexMap::default();
            let mut sample = Sample::default();
            // What each partly priced request is known to cost, which its extrapolated
            // share never falls below.
            let mut floors: Vec<f64> = vec![];
            for r in scan.requests.iter().filter(|r| {
                r.harness == Harness::Claude
                    && r.profiles.iter().any(|p| ids.contains(p))
                    && since.is_none_or(|s| r.time >= s)
            }) {
                let parts: Vec<_> = r
                    .parts
                    .iter()
                    .filter(|p| p.kind == PartKind::Advisor)
                    .collect();
                if parts.is_empty() {
                    continue;
                }
                requests += 1;
                calls += parts.len() as u64;
                for p in &parts {
                    let model = p.model.as_deref().unwrap_or("an unrecorded model");
                    *models.entry(model.to_string()).or_default() += 1;
                }
                let costs: Vec<_> = parts.iter().map(|p| price_part(catalog, r, p)).collect();
                let cost = request_evidence(r, add_costs(&costs));
                if cost.total_usd.is_none() {
                    floors.push(cost.known_usd.to_f64());
                }
                sample.add_cost(&cost);
            }
            let mut models: Vec<Share> = models
                .into_iter()
                .map(|(name, count)| Share { name, count })
                .collect();
            models.sort_by_key(|m| std::cmp::Reverse(m.count));
            AdvisorUse {
                profiles: line.ids.clone(),
                requests,
                calls,
                models,
                estimate: sample.estimate(requests).map(|e| {
                    let mean = e.usd / requests as f64;
                    let raise: f64 = floors.iter().map(|&k| (k - mean).max(0.0)).sum();
                    Estimate {
                        usd: e.usd + raise,
                        ..e
                    }
                }),
            }
        })
        .collect()
}

/// What `vibes` suggests as the advisor for a main model, by Claude Code's ranking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pick {
    /// The alias one rank above the main model, which `advisorModel` takes.
    Advisor(&'static str),
    /// A Fable main model, which nothing ranks above.
    Top,
    /// A main model Claude Code attaches no advisor to.
    Unsupported,
}

/// The advisor one rank above `main`, a canonical Claude model ID such as `claude-opus-5-5`:
/// Opus above Haiku and Sonnet, and Fable above Opus.
pub(crate) fn pick(main: &str) -> Pick {
    let mut parts = main.strip_prefix("claude-").unwrap_or_default().split('-');
    let family = parts.next().unwrap_or_default();
    let mut number = || parts.next().and_then(|n| n.parse::<u32>().ok());
    let version = (number(), number().unwrap_or(0));
    let (Some(major), minor) = version else {
        return Pick::Unsupported;
    };
    // Claude Code takes an advisor on Opus and Sonnet 4.6 or later and Haiku 4.5 or later.
    let at_least = |m: u32, n: u32| (major, minor) >= (m, n);
    match family {
        "fable" => Pick::Top,
        "opus" if at_least(4, 6) => Pick::Advisor("fable"),
        "sonnet" if at_least(4, 6) => Pick::Advisor("opus"),
        "haiku" if at_least(4, 5) => Pick::Advisor("opus"),
        _ => Pick::Unsupported,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BillingPart, EffortControl};
    use crate::test_support::{make_profile, make_request, prices};
    use serde_json::{Map, json};

    fn config(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    fn resolve(user: Value, project: Value, env: &[(&str, &str)]) -> AdvisorControl {
        let (user, project) = (config(user), config(project));
        let scopes = [
            SettingsScope {
                config: &user,
                file: "/u/settings.json",
            },
            SettingsScope {
                config: &project,
                file: "/p/.claude/settings.json",
            },
        ];
        let env = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        control(&scopes, &env, "/u/settings.json").unwrap()
    }

    #[test]
    fn the_highest_scope_names_the_advisor_and_an_unset_one_is_set_in_the_user_file() {
        let a = resolve(json!({"advisorModel": "opus"}), json!({}), &[]);
        assert_eq!(
            (a.model.as_deref(), a.owner.as_str()),
            (Some("opus"), "/u/settings.json:advisorModel")
        );
        let a = resolve(
            json!({"advisorModel": "opus"}),
            json!({"advisorModel": "fable"}),
            &[],
        );
        assert_eq!(a.model.as_deref(), Some("fable"));
        assert_eq!(a.owner, "/p/.claude/settings.json:advisorModel");
        // Null is unset; an empty name sets no advisor where it is written.
        let a = resolve(json!({"advisorModel": null}), json!({}), &[]);
        assert_eq!((a.model, a.blocked), (None, None));
        let a = resolve(json!({}), json!({"advisorModel": " "}), &[]);
        assert_eq!(a.model, None);
        assert_eq!(a.owner, "/p/.claude/settings.json:advisorModel");
        let user = config(json!({"advisorModel": 1}));
        let scopes = [SettingsScope {
            config: &user,
            file: "/u/settings.json",
        }];
        assert!(control(&scopes, &Env::default(), "/u/settings.json").is_err());
    }

    #[test]
    fn what_stops_the_advisor_names_its_variable_and_where_it_is_set() {
        let blocked = |user: Value, env: &[(&str, &str)]| resolve(user, json!({}), env).blocked;
        assert_eq!(blocked(json!({}), &[]), None);
        // Telemetry's opt-out takes any value, even 0; DO_NOT_TRACK is a plain boolean.
        assert_eq!(
            blocked(json!({}), &[("DISABLE_TELEMETRY", "0")]).as_deref(),
            Some(
                "DISABLE_TELEMETRY, set in the environment, turns off the feature-flag fetch the advisor needs"
            )
        );
        assert_eq!(blocked(json!({}), &[("DO_NOT_TRACK", "0")]), None);
        assert!(blocked(json!({}), &[("DO_NOT_TRACK", "true")]).is_some());
        // A settings file's empty value unsets an exported one.
        assert_eq!(
            blocked(
                json!({"env": {"CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": ""}}),
                &[("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")]
            ),
            None
        );
        assert_eq!(
            blocked(
                json!({"env": {"CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1"}}),
                &[]
            )
            .as_deref(),
            Some(
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC, set in /u/settings.json, turns off the feature-flag fetch the advisor needs"
            )
        );
        // Turning the tool off outranks a provider, which outranks the flag.
        assert_eq!(
            blocked(
                json!({"env": {"CLAUDE_CODE_USE_BEDROCK": "1", "CLAUDE_CODE_DISABLE_ADVISOR_TOOL": "1"}}),
                &[("DISABLE_TELEMETRY", "1")]
            )
            .as_deref(),
            Some(
                "CLAUDE_CODE_DISABLE_ADVISOR_TOOL, set in /u/settings.json, turns the advisor tool off"
            )
        );
        assert_eq!(
            blocked(
                json!({"env": {"CLAUDE_CODE_USE_VERTEX": "1"}}),
                &[("DISABLE_TELEMETRY", "1")]
            )
            .as_deref(),
            Some(
                "CLAUDE_CODE_USE_VERTEX, set in /u/settings.json, sends requests to Google Cloud's Agent Platform, which has no advisor tool"
            )
        );
        // Claude Platform on AWS serves the advisor with no fetched flag, and is no gateway.
        let a = resolve(
            json!({"env": {"CLAUDE_CODE_USE_ANTHROPIC_AWS": "1", "ANTHROPIC_BASE_URL": "https://gw"}}),
            json!({}),
            &[("DISABLE_TELEMETRY", "1")],
        );
        assert_eq!((a.blocked, a.gateway), (None, None));
        let a = resolve(
            json!({}),
            json!({}),
            &[("ANTHROPIC_BASE_URL", "https://gw")],
        );
        assert_eq!(
            a.gateway.as_deref(),
            Some("ANTHROPIC_BASE_URL, set in the environment,")
        );
    }

    #[test]
    fn the_pick_is_one_rank_above_a_main_model_that_takes_an_advisor() {
        for (main, expected) in [
            ("claude-haiku-4-5", Pick::Advisor("opus")),
            ("claude-haiku-5-5", Pick::Advisor("opus")),
            ("claude-sonnet-4-6", Pick::Advisor("opus")),
            ("claude-sonnet-5", Pick::Advisor("opus")),
            ("claude-opus-4-6", Pick::Advisor("fable")),
            ("claude-opus-5-5", Pick::Advisor("fable")),
            ("claude-fable-5-2", Pick::Top),
            ("claude-fable-6", Pick::Top),
            ("claude-sonnet-4-5", Pick::Unsupported),
            ("claude-opus-4-5", Pick::Unsupported),
            ("claude-haiku-4", Pick::Unsupported),
            ("claude-3-5-sonnet", Pick::Unsupported),
            ("claude-mystery-9", Pick::Unsupported),
            ("gpt-6.2-sol", Pick::Unsupported),
        ] {
            assert_eq!(pick(main), expected, "{main}");
        }
    }

    #[test]
    fn only_advisor_iterations_count_and_profiles_with_another_main_default_split() {
        let advised = |id: &str, main: &str| {
            let mut p = make_profile(id, "/synthetic/claude");
            p.settings_file = "/synthetic/claude/settings.json".into();
            p.advisor = Some(AdvisorControl {
                model: Some("opus".into()),
                owner: "/synthetic/claude/settings.json:advisorModel".into(),
                blocked: None,
                gateway: None,
            });
            p.effort = Some(EffortControl {
                model: Some(main.into()),
                model_owner: "/synthetic/claude/settings.json:model".into(),
                level: None,
                owner: String::new(),
                effective: None,
                lowered: None,
                caps: vec![],
                default: None,
                unresolved: None,
            });
            p
        };
        let request = |profile: &str, advisors: usize| {
            let mut r = make_request();
            r.profiles = vec![profile.into()].into();
            for _ in 0..advisors {
                r.parts.push(BillingPart {
                    kind: PartKind::Advisor,
                    model: Some("claude-opus-5-5".into()),
                    ..r.parts[0].clone()
                });
            }
            r
        };
        let mut scan = Scan {
            snapshot_started_at: String::new(),
            snapshot_finished_at: String::new(),
            since: None,
            roots: vec![],
            profiles: vec![
                advised("a", "claude-sonnet-5-5"),
                advised("b", "claude-haiku-5-5"),
            ],
            snapshots: vec![],
            requests: vec![request("a", 2), request("a", 0), request("b", 1)],
            events: vec![],
            exposure: Default::default(),
            cache_context: Default::default(),
            coverage: Default::default(),
            subagents: vec![],
            t3: vec![],
            sessions: Default::default(),
            defaults: false,
        };
        let catalog = prices();
        let lines = advisor_use(&scan, &catalog, None);
        assert_eq!(lines.len(), 2);
        let a = &lines[0];
        assert_eq!((a.requests, a.calls), (1, 2));
        assert_eq!(
            a.models,
            [Share {
                name: "claude-opus-5-5".into(),
                count: 2
            }]
        );
        // The two advisor iterations alone, not the executor's.
        let r = request("a", 2);
        let advisor = price_part(&catalog, &r, &r.parts[1]).total_usd.unwrap();
        assert_eq!(a.estimate.unwrap().usd, (advisor + advisor).to_f64());
        assert_eq!(lines[1].calls, 1);
        // The same main default shares one line.
        scan.profiles[1] = advised("b", "claude-sonnet-5-5");
        let lines = advisor_use(&scan, &catalog, None);
        assert_eq!(lines.len(), 1);
        assert_eq!((lines[0].requests, lines[0].calls), (2, 3));
        // A T3 instance on the same settings file has no line, but its calls count on this one.
        let mut t3 = crate::test_support::make_t3_profile("t3", "/synthetic/claude");
        t3.settings_file = "/synthetic/claude/settings.json".into();
        scan.profiles.push(t3);
        scan.requests.push(request("t3", 1));
        let lines = advisor_use(&scan, &catalog, None);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].profiles, ["a", "b"]);
        assert_eq!((lines[0].requests, lines[0].calls), (3, 4));
    }

    #[test]
    fn a_partly_priced_call_never_extrapolates_below_its_known_charge() {
        let mut p = make_profile("a", "/synthetic/claude");
        p.advisor = Some(AdvisorControl {
            model: None,
            owner: "/synthetic/claude/settings.json:advisorModel".into(),
            blocked: None,
            gateway: None,
        });
        let catalog = prices();
        let advisor = |input: i64, output: Option<i64>| {
            let mut r = make_request();
            r.profiles = vec!["a".into()].into();
            let mut part = r.parts[0].clone();
            part.kind = PartKind::Advisor;
            part.usage.input = Some(input);
            part.usage.output = output;
            r.parts.push(part);
            r
        };
        // A small call priced in full, and a large one whose output is not recorded.
        let small = advisor(10, Some(1));
        let large = advisor(10_000_000, None);
        let known = price_part(&catalog, &large, &large.parts[1]);
        assert_eq!(known.total_usd, None);
        let floor = known.known_usd.to_f64();
        let priced = price_part(&catalog, &small, &small.parts[1])
            .total_usd
            .unwrap()
            .to_f64();
        assert!(floor > priced);
        let scan = Scan {
            snapshot_started_at: String::new(),
            snapshot_finished_at: String::new(),
            since: None,
            roots: vec![],
            profiles: vec![p],
            snapshots: vec![],
            requests: vec![small, large],
            events: vec![],
            exposure: Default::default(),
            cache_context: Default::default(),
            coverage: Default::default(),
            subagents: vec![],
            t3: vec![],
            sessions: Default::default(),
            defaults: false,
        };
        let estimate = advisor_use(&scan, &catalog, None)[0].estimate.unwrap();
        assert!(
            (estimate.usd - (priced + floor)).abs() < 1e-9,
            "{estimate:?}"
        );
    }
}
