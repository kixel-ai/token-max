//! Claude Code's advisor: the `advisorModel` set now, what stops it loading, and the advisor
//! `vibes` suggests one rank above the main-session default.

use serde_json::Value;

use crate::controls::{Env, SettingsScope, env_flag, js_string, scoped_environment};
use crate::model::{AdvisorControl, Profile};
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
    // A null outranks the files below it, as settings merge.
    let (model, owner) = match crate::effort::highest(scopes, KEY) {
        Some((Value::Null, file)) => (None, format!("{file}:{KEY}")),
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

/// Where a Claude model or alias, such as `opus` or `claude-sonnet-5-5`, ranks among the
/// families an advisor pairs across: Haiku, Sonnet, Opus, then Fable. None for any other name.
pub(crate) fn rank(model: &str) -> Option<usize> {
    let model = model.to_ascii_lowercase();
    ["haiku", "sonnet", "opus", "fable"]
        .iter()
        .position(|family| model.contains(family))
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
}
