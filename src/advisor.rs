//! Claude Code's advisor: the `advisorModel` set now, what stops it loading, what its recorded
//! calls cost, and the advisor `vibes` suggests one rank above the main-session default.

use serde_json::Value;

use crate::collections::IndexMap;
use crate::controls::{Env, SettingsScope, env_flag, js_string, scoped_environment};
use crate::model::{
    AdvisorControl, AdvisorLine, AdvisorUse, Cost, Harness, Millis, PartKind, Profile, Request,
    Scan, Share,
};
use crate::prices::{Catalog, Sample, add_costs, floored, price_part, request_evidence};

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
/// `user_file` is where an advisor would be set when no scope sets one. A value that is no
/// model name Claude Code recognizes is shown as set and blocked, by its kind alone, since it
/// could be anything, a pasted credential included, and only the advisor card reads it.
pub fn control(scopes: &[SettingsScope], env: &Env, user_file: &str) -> AdvisorControl {
    // A null outranks the files below it, as settings merge.
    let (model, owner, invalid) = match crate::effort::highest(scopes, KEY) {
        Some((Value::Null, file)) => (None, format!("{file}:{KEY}"), false),
        Some((Value::String(m), file)) => match m.trim() {
            "" => (None, format!("{file}:{KEY}"), false),
            m if recognized(m) => (Some(m.to_string()), format!("{file}:{KEY}"), false),
            _ => (
                Some("an unknown name".into()),
                format!("{file}:{KEY}"),
                true,
            ),
        },
        Some((value, file)) => {
            let kind = match value {
                Value::Bool(_) => "a boolean",
                Value::Number(_) => "a number",
                Value::Array(_) => "a list",
                _ => "an object",
            };
            (Some(kind.into()), format!("{file}:{KEY}"), true)
        }
        None => (None, format!("{user_file}:{KEY}"), false),
    };
    let environment = scoped_environment(scopes, env);
    let value = |key: &str| {
        let value = js_string(environment.values.get(key));
        (environment.values.contains_key(key) && !value.is_empty()).then_some(value)
    };
    let on = |key: &str| environment.values.get(key).and_then(env_flag) == Some(true);
    // Where a variable is set, as a reader would look for it.
    let named = |key: &str| {
        let owner = &environment.owners[key];
        match owner.strip_suffix(&format!(":env.{key}")) {
            Some(file) => format!("{key}, set in {file},"),
            None => format!("{key}, set in the environment,"),
        }
    };
    let flagless = FLAGLESS.iter().any(|key| on(key));
    let blocked = if invalid {
        Some(format!("{KEY} is no model name Claude Code recognizes"))
    } else if on("CLAUDE_CODE_DISABLE_ADVISOR_TOOL") {
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
    AdvisorControl {
        model,
        owner,
        blocked,
        gateway,
    }
}

/// What puts a profile on an advisor line: the `advisorModel` owner, and the main-session
/// default and control the line's card reads. None without an advisor control.
pub(crate) fn owner(p: &Profile) -> Option<String> {
    let a = p.advisor.as_ref()?;
    let main = p
        .effort
        .as_ref()
        .map(|e| (&e.model, &e.model_owner, &e.unresolved));
    Some(format!(
        "{}\nmain {main:?}\n{:?} {:?} {:?}",
        a.owner, a.model, a.blocked, a.gateway
    ))
}

/// A canonical Claude model ID's family and version, such as `("opus", (5, 5))` for
/// `claude-opus-5-5`; None for an alias or another name.
fn parse(id: &str) -> Option<(&str, (u32, u32))> {
    let mut parts = id.strip_prefix("claude-")?.split('-');
    let family = parts.next()?;
    let major = parts.next()?.parse().ok()?;
    // A dated snapshot, such as `claude-sonnet-5-20260101`, has no minor version.
    let minor = parts
        .next()
        .filter(|n| n.len() <= 2)
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    Some((family, (major, minor)))
}

/// An advisor name as Claude Code resolves it: lowercased, without a `[1m]` or `[2m]`
/// context-window suffix, which it drops before checking the advisor.
fn normalize(name: &str) -> String {
    let name = name.trim().to_ascii_lowercase();
    match name
        .strip_suffix("[1m]")
        .or_else(|| name.strip_suffix("[2m]"))
    {
        Some(base) => base.to_string(),
        None => name,
    }
}

/// Whether `name` is, in full, an alias or a Claude model ID of a family that can advise:
/// `claude-<family>-<major>`, then an optional one- or two-digit minor and an optional
/// eight-digit date, in any case and with an optional context-window suffix.
fn recognized(name: &str) -> bool {
    let name = normalize(name);
    let family = |f: &str| ["fable", "opus", "sonnet", "haiku"].contains(&f);
    if family(&name) {
        return true;
    }
    let Some(rest) = name.strip_prefix("claude-") else {
        return false;
    };
    let parts: Vec<&str> = rest.split('-').collect();
    let digits = |s: &str, len: std::ops::RangeInclusive<usize>| {
        len.contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
    };
    match parts.as_slice() {
        [f, major, tail @ ..] if family(f) && digits(major, 1..=2) => match tail {
            [] => true,
            [n] => digits(n, 1..=2) || digits(n, 8..=8),
            [minor, date] => digits(minor, 1..=2) && digits(date, 8..=8),
            _ => false,
        },
        _ => false,
    }
}

/// Whether Claude Code pairs `main`, a canonical model ID, with `advisor`, by its pairing
/// table (https://code.claude.com/docs/en/advisor#choose-an-advisor-model); None when the
/// table can't say. An alias resolves to a version that changes with Claude Code releases,
/// so only Fable's rule, which takes no other family, judges one.
pub(crate) fn accepts(main: &str, advisor: &str) -> Option<bool> {
    let (family, version) = parse(main)?;
    let advisor = normalize(advisor);
    if family == "fable" {
        if advisor == "fable" {
            return Some(true);
        }
        return Some(matches!(parse(&advisor), Some(("fable", a)) if a >= version));
    }
    let (advisor, at) = parse(&advisor)?;
    // The oldest version of each family the main model takes as its advisor, by row.
    let minimum = |opus, sonnet, haiku| match advisor {
        "fable" => Some((0, 0)),
        "opus" => opus,
        "sonnet" => sonnet,
        "haiku" => haiku,
        _ => None,
    };
    let any = Some((0, 0));
    let min = match (family, version) {
        ("haiku", (4, 5)) | ("sonnet", (4, 6)) => minimum(any, any, Some((5, 5))),
        ("opus", (4, 6)) => minimum(any, Some((5, 0)), Some((5, 5))),
        ("sonnet", (5, 0)) | ("haiku", (5, 5)) => minimum(Some((4, 7)), Some((5, 0)), Some((5, 5))),
        ("opus", (4, 7) | (4, 8)) => minimum(Some((4, 7)), Some((5, 5)), None),
        ("sonnet", (5, 5)) => minimum(Some((5, 0)), Some((5, 5)), None),
        ("opus", (5, 0) | (5, 5)) => minimum(Some((5, 0)), None, None),
        _ => return None,
    };
    Some(min.is_some_and(|min| at >= min))
}

/// The advisor calls of each line of Claude profiles that share an advisor owner, in the
/// window from `since`, else from the first request: one line per card, then one per group
/// of T3 instances that read no card's settings file, then one per removed T3 instance, then
/// one per set of lines that share requests.
pub fn advisor_use(scan: &Scan, catalog: &Catalog, since: Option<Millis>) -> Vec<AdvisorUse> {
    let advised = scan.profiles.iter().filter(|p| p.advisor.is_some());
    // A T3 instance has no card of its own, but its threads read its settings file, so its
    // calls count on the first line whose advisorModel that file owns. A line whose owner is
    // another file, such as a project's, is a setting the threads may never read.
    let file = |p: &Profile| crate::rows::canonical(&format!("{}:", p.settings_file));
    let mut t3: Vec<&Profile> = scan
        .profiles
        .iter()
        .filter(|p| p.harness == Harness::Claude && p.t3.is_some())
        .collect();
    // Each line's shown profiles, whose calls it counts, and its kind.
    let mut lines: Vec<(Vec<String>, Vec<String>, AdvisorLine)> = vec![];
    let mut owners: Vec<String> = vec![];
    for line in crate::rows::groups(advised, |p| owner(p).expect("filtered on an advisor")) {
        let control = line.lead.advisor.as_ref().expect("filtered on an advisor");
        let owned = control
            .owner
            .strip_suffix(&format!(":{KEY}"))
            .map(|f| crate::rows::canonical(&format!("{f}:")));
        let mut ids = line.ids.clone();
        // Launch settings token-max can't resolve may replace the file, and a disabled
        // instance keeps a line naming it, as `rows::owner_file` does.
        t3.retain(|p| {
            let apart =
                p.t3.as_ref()
                    .is_some_and(|t| t.settings_unresolved || !t.enabled);
            let mine = !apart && owned.as_ref() == Some(&file(p));
            if mine {
                ids.push(p.id.clone());
            }
            !mine
        });
        owners.push(control.owner.clone());
        lines.push((line.ids, ids, AdvisorLine::Settings));
    }
    // The rest read a file no card does, as in a scan of only `--t3-dir`.
    let unresolved = |p: &Profile| p.t3.as_ref().is_some_and(|t| t.settings_unresolved);
    // Unresolved launch settings leave the file unknown, and the instance on a line of its own.
    for line in crate::rows::groups(t3, |p| format!("{}:", p.settings_file)) {
        let owner = (!unresolved(line.lead)).then(|| format!("{}:{KEY}", line.lead.settings_file));
        owners.push(owner.clone().unwrap_or_default());
        lines.push((line.ids.clone(), line.ids, AdvisorLine::T3 { owner }));
    }
    // A removed instance's retained requests name its old key, which no profile has.
    for state in &scan.t3 {
        for instance in state.removed.keys() {
            let key = crate::t3::profile_key(&state.path, instance);
            owners.push(String::new());
            lines.push((vec![key.clone()], vec![key], AdvisorLine::Removed));
        }
    }
    let mut tallies: Vec<Tally> = lines.iter().map(|_| Tally::default()).collect();
    let mut shared: Vec<u64> = vec![0; lines.len()];
    // Requests on several lines, by the lines they name.
    let mut overlaps: IndexMap<Vec<usize>, Tally> = IndexMap::default();
    for r in scan.requests.iter().filter(|r| {
        r.harness == Harness::Claude
            && since.is_none_or(|s| r.time >= s)
            && r.parts.iter().any(|p| p.kind == PartKind::Advisor)
    }) {
        let on: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, (_, ids, _))| r.profiles.iter().any(|p| ids.contains(p)))
            .map(|(i, _)| i)
            .collect();
        // A T3 line says its threads made its calls, so a request T3 could not settle, such
        // as a native session in a scan of only `--t3-dir`, counts as unknown instead.
        let unsettled = |i: usize| {
            matches!(lines[i].2, AdvisorLine::T3 { .. })
                && r.issues.iter().any(|i| i.starts_with(crate::t3::UNSETTLED))
        };
        match on.as_slice() {
            [] => {}
            [i] if !unsettled(*i) => tallies[*i].add(catalog, r),
            _ => {
                for &i in &on {
                    shared[i] += 1;
                }
                overlaps.entry(on).or_default().add(catalog, r);
            }
        }
    }
    let shown: Vec<Vec<String>> = lines
        .iter()
        .map(|(profiles, ..)| profiles.clone())
        .collect();
    let mut result: Vec<AdvisorUse> = lines
        .into_iter()
        .zip(tallies)
        .zip(&shared)
        .filter(|(((_, _, line), tally), shared)| {
            *line == AdvisorLine::Settings || tally.requests > 0 || **shared > 0
        })
        .map(|(((profiles, _, line), tally), &shared)| tally.finish(profiles, line, shared))
        .collect();
    for (on, tally) in overlaps {
        let mut profiles: Vec<String> = vec![];
        for &i in &on {
            for id in &shown[i] {
                if !profiles.contains(id) {
                    profiles.push(id.clone());
                }
            }
        }
        let owners = on
            .iter()
            .map(|&i| owners[i].clone())
            .filter(|o| !o.is_empty())
            .collect();
        result.push(tally.finish(profiles, AdvisorLine::Shared { owners }, 0));
    }
    result
}

/// One line's advisor requests as they are read.
#[derive(Default)]
struct Tally {
    requests: u64,
    calls: u64,
    models: IndexMap<String, u64>,
    sample: Sample,
    /// What each partly priced request is known to cost, which its extrapolated share never
    /// falls below.
    floors: Vec<f64>,
    total: Vec<Cost>,
}

impl Tally {
    /// Count `r`'s advisor iterations and price them alone, at the model each names.
    fn add(&mut self, catalog: &Catalog, r: &Request) {
        let parts: Vec<_> = r
            .parts
            .iter()
            .filter(|p| p.kind == PartKind::Advisor)
            .collect();
        self.requests += 1;
        self.calls += parts.len() as u64;
        for p in &parts {
            let model = p.model.as_deref().unwrap_or("an unrecorded model");
            *self.models.entry(model.to_string()).or_default() += 1;
        }
        let costs: Vec<_> = parts.iter().map(|p| price_part(catalog, r, p)).collect();
        let cost = request_evidence(r, add_costs(&costs));
        if cost.total_usd.is_none() {
            self.floors.push(cost.known_usd.to_f64());
        }
        self.sample.add_cost(&cost);
        self.total.push(cost);
    }

    fn finish(self, profiles: Vec<String>, line: AdvisorLine, shared: u64) -> AdvisorUse {
        let mut models: Vec<Share> = self
            .models
            .into_iter()
            .map(|(name, count)| Share { name, count })
            .collect();
        models.sort_by_key(|m| std::cmp::Reverse(m.count));
        let requests = self.requests;
        let floors = self.floors;
        AdvisorUse {
            profiles,
            line,
            requests,
            calls: self.calls,
            models,
            cost: add_costs(&self.total),
            estimate: floored(self.sample.estimate(requests), requests, &floors),
            shared,
        }
    }
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
    let Some((family, version)) = parse(main) else {
        return Pick::Unsupported;
    };
    // Claude Code takes an advisor on Opus and Sonnet 4.6 or later and Haiku 4.5 or later.
    let at_least = |m: u32, n: u32| version >= (m, n);
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
        control(&scopes, &env, "/u/settings.json")
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
        // Null is unset, and outranks the files below it; an empty name sets no advisor
        // where it is written.
        let a = resolve(json!({"advisorModel": null}), json!({}), &[]);
        assert_eq!((a.model, a.blocked), (None, None));
        let a = resolve(
            json!({"advisorModel": "opus"}),
            json!({"advisorModel": null}),
            &[],
        );
        assert_eq!(
            (a.model, a.owner.as_str()),
            (None, "/p/.claude/settings.json:advisorModel")
        );
        let a = resolve(json!({}), json!({"advisorModel": " "}), &[]);
        assert_eq!(a.model, None);
        assert_eq!(a.owner, "/p/.claude/settings.json:advisorModel");
        // A value that is no model name stops the advisor rather than the scan.
        let a = resolve(json!({"advisorModel": false}), json!({}), &[]);
        assert_eq!(
            (a.model.as_deref(), a.blocked.as_deref()),
            (
                Some("a boolean"),
                Some("advisorModel is no model name Claude Code recognizes")
            )
        );
        let a = resolve(
            json!({"advisorModel": {"key": "sk-secret"}}),
            json!({}),
            &[],
        );
        assert_eq!(a.model.as_deref(), Some("an object"));
        // So is a string naming no model, which could be a pasted credential.
        let a = resolve(json!({"advisorModel": "sk-ant-secret"}), json!({}), &[]);
        assert_eq!(a.model.as_deref(), Some("an unknown name"));
        assert!(a.blocked.is_some());
        for name in [
            "claude-opus-5-5sk-ant-secret",
            "claude-opus-5-5-sk",
            "claude-opus-x",
            "claude-opus-5-5[3m]",
        ] {
            let a = resolve(json!({"advisorModel": name}), json!({}), &[]);
            assert_eq!(a.model.as_deref(), Some("an unknown name"), "{name}");
        }
        for name in [
            "Opus",
            "claude-opus-5",
            "claude-sonnet-5-5",
            "claude-sonnet-5-20260101",
            "claude-haiku-5-5-20260101",
            // Claude Code lowercases a name and drops a context-window suffix.
            "Claude-Fable-5-2",
            "claude-opus-5-5[1m]",
            "Opus[1M]",
            "claude-sonnet-5-5[2m]",
        ] {
            let a = resolve(json!({"advisorModel": name}), json!({}), &[]);
            assert_eq!((a.model.as_deref(), a.blocked), (Some(name), None));
        }
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
    fn pairings_follow_claude_codes_table_and_leave_aliases_unjudged() {
        for (main, advisor, accepted) in [
            // A Fable main takes only a Fable advisor of its version or later.
            ("claude-fable-5-2", "fable", Some(true)),
            ("claude-fable-5-2", "claude-fable-5-2", Some(true)),
            ("claude-fable-5-2", "claude-fable-5-1", Some(false)),
            ("claude-fable-5-1", "claude-fable-5-2", Some(true)),
            ("claude-fable-5", "claude-fable-5-1", Some(true)),
            ("claude-fable-5-2", "opus", Some(false)),
            ("claude-fable-5-2", "claude-opus-5-5", Some(false)),
            // In any case, and with a context-window suffix, as Claude Code reads it.
            ("claude-fable-5-2", "Claude-Fable-5-2", Some(true)),
            ("claude-fable-5-2", "Fable[1m]", Some(true)),
            ("claude-fable-5-2", "claude-fable-5-1[1M]", Some(false)),
            ("claude-opus-5-5", "Claude-Opus-5[1m]", Some(true)),
            // Each other row's oldest accepted version per family, and Haiku 4.5 advising none.
            ("claude-haiku-4-5", "claude-sonnet-4-6", Some(true)),
            ("claude-haiku-4-5", "claude-haiku-4-5", Some(false)),
            ("claude-sonnet-4-6", "claude-haiku-5-5", Some(true)),
            ("claude-opus-4-6", "claude-sonnet-4-6", Some(false)),
            ("claude-opus-4-6", "claude-sonnet-5", Some(true)),
            ("claude-sonnet-5", "claude-opus-4-6", Some(false)),
            ("claude-haiku-5-5", "claude-opus-4-7", Some(true)),
            ("claude-opus-4-8", "claude-sonnet-5", Some(false)),
            ("claude-opus-4-7", "claude-sonnet-5-5", Some(true)),
            ("claude-opus-4-7", "claude-haiku-5-5", Some(false)),
            ("claude-sonnet-5-5", "claude-opus-4-8", Some(false)),
            ("claude-sonnet-5-5", "claude-opus-5", Some(true)),
            ("claude-opus-5-5", "claude-opus-5", Some(true)),
            ("claude-opus-5-5", "claude-sonnet-5-5", Some(false)),
            ("claude-opus-5-5", "claude-fable-5", Some(true)),
            ("claude-opus-5-5", "claude-sonnet-5-20260101", Some(false)),
            // An alias's version moves with Claude Code, and a row the table lacks is unknown.
            ("claude-opus-5-5", "sonnet", None),
            ("claude-opus-5-6", "claude-sonnet-5-5", None),
            ("claude-mystery-9", "claude-opus-5-5", None),
        ] {
            assert_eq!(accepts(main, advisor), accepted, "{main} {advisor}");
        }
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
                picked: None,
                provider: None,
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
        // Unless its launch settings, unresolved, may replace that file.
        scan.profiles[2].t3.as_mut().unwrap().settings_unresolved = true;
        let lines = advisor_use(&scan, &catalog, None);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].profiles, ["t3"]);
        scan.profiles[2].t3.as_mut().unwrap().settings_unresolved = false;
        // Only a line whose advisorModel the instance's own file owns takes its calls, not
        // one a project's file sets.
        scan.profiles[2].settings_file = "/synthetic/other/settings.json".into();
        let lines = advisor_use(&scan, &catalog, None);
        assert_eq!(lines.len(), 2);
        assert_eq!((lines[0].requests, lines[0].calls), (2, 3));
        assert_eq!(lines[1].profiles, ["t3"]);
        // An instance whose settings file no native line reads, as in a scan of only
        // `--t3-dir`, gets a line of its own rather than losing its calls.
        scan.profiles.drain(..2);
        let lines = advisor_use(&scan, &catalog, None);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].profiles, ["t3"]);
        assert_eq!(
            lines[0].line,
            AdvisorLine::T3 {
                owner: Some("/synthetic/other/settings.json:advisorModel".into())
            }
        );
        assert_eq!((lines[0].requests, lines[0].calls), (1, 1));
        // Unresolved launch settings may replace that file, so the line names no setting.
        scan.profiles[0].t3.as_mut().unwrap().settings_unresolved = true;
        let lines = advisor_use(&scan, &catalog, None);
        assert_eq!(lines[0].line, AdvisorLine::T3 { owner: None });
        scan.profiles[0].t3.as_mut().unwrap().settings_unresolved = false;
        // A request T3 could not settle, such as a native session, is not the line's threads'.
        let mut unsettled = request("t3", 2);
        unsettled
            .issues
            .push(format!("{}: synthetic", crate::t3::UNSETTLED).into());
        scan.requests.push(unsettled);
        let lines = advisor_use(&scan, &catalog, None);
        assert_eq!(lines.len(), 2);
        assert_eq!((lines[0].requests, lines[0].shared), (1, 1));
        assert!(matches!(lines[1].line, AdvisorLine::Shared { .. }));
        assert_eq!((lines[1].requests, lines[1].calls), (1, 2));
        scan.requests.pop();
        // A removed instance's retained requests keep its old key, and a line of their own.
        let gone = crate::t3::profile_key("/state", "gone");
        scan.t3.push(crate::model::T3State {
            path: "/state".into(),
            status: "selected".into(),
            exclusions: vec![],
            metadata: String::new(),
            snapshot_started_at: None,
            snapshot_finished_at: None,
            ambiguous_requests: 0,
            unresolved: IndexMap::default(),
            removed: IndexMap::from_iter([("gone".into(), 1)]),
            unsupported: 0,
            missing: 0,
            instances: Default::default(),
            text_generation: None,
        });
        scan.requests.push(request(&gone, 1));
        let lines = advisor_use(&scan, &catalog, None);
        assert_eq!(lines.len(), 2);
        assert_eq!(
            (&lines[1].profiles, &lines[1].line, lines[1].calls),
            (&vec![gone], &AdvisorLine::Removed, 1)
        );
        // With no calls, neither has a line.
        scan.requests.clear();
        assert!(advisor_use(&scan, &catalog, None).is_empty());
    }

    #[test]
    fn a_request_on_two_lines_counts_once_on_a_shared_line() {
        let advised = |id: &str, root: &str| {
            let mut p = make_profile(id, root);
            p.settings_file = format!("{root}/settings.json");
            p.advisor = Some(AdvisorControl {
                model: None,
                owner: format!("{root}/settings.json:advisorModel"),
                blocked: None,
                gateway: None,
            });
            p
        };
        let request = |profiles: &[&str]| {
            let mut r = make_request();
            r.profiles = profiles
                .iter()
                .map(|&p| p.into())
                .collect::<Vec<_>>()
                .into();
            r.parts.push(BillingPart {
                kind: PartKind::Advisor,
                model: Some("claude-opus-5-5".into()),
                ..r.parts[0].clone()
            });
            r
        };
        let scan = Scan {
            snapshot_started_at: String::new(),
            snapshot_finished_at: String::new(),
            since: None,
            roots: vec![],
            profiles: vec![advised("a", "/synthetic/a"), advised("b", "/synthetic/b")],
            snapshots: vec![],
            // Two config roots read one projects directory: both name the second request.
            requests: vec![request(&["a"]), request(&["a", "b"]), request(&["b"])],
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
        let counts: Vec<_> = lines
            .iter()
            .map(|u| (u.profiles.clone(), u.requests, u.calls, u.shared))
            .collect();
        assert_eq!(
            counts,
            [
                (vec!["a".to_string()], 1, 1, 1),
                (vec!["b".to_string()], 1, 1, 1),
                (vec!["a".to_string(), "b".to_string()], 1, 1, 0),
            ]
        );
        assert_eq!(
            lines[2].line,
            AdvisorLine::Shared {
                owners: vec![
                    "/synthetic/a/settings.json:advisorModel".into(),
                    "/synthetic/b/settings.json:advisorModel".into()
                ]
            }
        );
        // Priced once in all: the three lines sum to the three requests.
        let one = lines[0].estimate.unwrap().usd;
        let total: f64 = lines.iter().map(|u| u.estimate.unwrap().usd).sum();
        assert!((total - 3.0 * one).abs() < 1e-9, "{total} {one}");
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
