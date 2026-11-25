//! Claude Code's advisor: the `advisorModel` set now, what stops it loading, and the advisor
//! `vibes` suggests one rank above the main-session default.

use serde_json::Value;

use crate::controls::{
    Env, SettingsScope, claude_flags_off, claude_provider, env_flag, js_string, scoped_environment,
};
use crate::model::{AdvisorControl, Profile};

/// Claude Code's page on the advisor tool.
pub(crate) const DOCS: &str = "https://code.claude.com/docs/en/advisor";

/// The setting that names the advisor model.
const KEY: &str = "advisorModel";

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
    let provider = claude_provider(&environment);
    // A provider Anthropic runs serves the advisor without the flag; the others have none.
    let flagless = provider.is_some_and(|p| p.pricing.is_none());
    let blocked = if invalid {
        Some(format!("{KEY} is no model name Claude Code recognizes"))
    } else if on("CLAUDE_CODE_DISABLE_ADVISOR_TOOL") {
        Some(format!(
            "{} turns the advisor tool off",
            named("CLAUDE_CODE_DISABLE_ADVISOR_TOOL")
        ))
    } else if let Some(p) = provider.filter(|p| p.pricing.is_some()) {
        Some(format!(
            "{} sends requests to {}, which has no advisor tool",
            named(p.variable),
            p.name
        ))
    } else if flagless {
        None
    } else {
        claude_flags_off(&environment).map(|key| {
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

/// What puts profiles on one advisor card: the `advisorModel` owner, and the main-session
/// default and control the card reads. None without an advisor control.
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
}
