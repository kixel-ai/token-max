//! The main session's default reasoning effort: what decides it, the level the session runs
//! at, and what each model and effort pair cost. A `max` default is a change to `xhigh`, a
//! `low` one is worth considering raising, and the rest cite guidance without a verdict.
//! A different effort changes the output tokens, so no cost is repriced.

use std::cmp::Reverse;

use serde_json::{Map, Value};

use crate::collections::IndexMap;
use crate::controls::{Env, SettingsScope, members, scoped_environment};
use crate::model::{
    Action, Agent, CheckBase, EffortCheck, EffortControl, EffortRun, Evidence, Harness, Location,
    Millis, Profile, Request, Scan,
};
use crate::prices::{Catalog, Sum, price_request};
use crate::safe::ScanError;

const SOURCES: [&str; 5] = [
    "https://code.claude.com/docs/en/model-config#adjust-effort-level (2026-11-14)",
    "https://code.claude.com/docs/en/settings-reference#modelsettings (2026-11-14)",
    "https://claude.dev/blog/which-claude-model-should-you-build-with/ (2026-11-14)",
    "https://developers.openai.com/api/docs/guides/reasoning (2026-11-14)",
    "https://learn.chatgpt.com/docs/config-file/config-reference (2026-11-14)",
];

/// Claude Code's effort levels, lowest first.
const LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// A level's rank in [`LEVELS`].
fn rank(level: &str) -> Option<usize> {
    LEVELS.iter().position(|l| *l == level)
}

fn invalid(message: impl Into<String>, file: &str) -> ScanError {
    ScanError::at(
        message,
        Location {
            file: file.into(),
            line: 1,
        },
    )
}

/// How Claude Code treats one model's effort, from its model configuration docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ClaudeModel {
    /// Whether it supports `xhigh`; every model that takes effort supports the rest.
    xhigh: bool,
    default: &'static str,
    /// Whether a top-level `effortLevel` in the user's own file still applies to it.
    legacy_user_level: bool,
}

/// A canonical Claude model's effort support: None when the docs do not list it, and
/// `Some(None)` for a listed model that takes no effort.
fn claude_model(model: &str) -> Option<Option<ClaudeModel>> {
    let takes = |xhigh, default, legacy_user_level| {
        Some(Some(ClaudeModel {
            xhigh,
            default,
            legacy_user_level,
        }))
    };
    match model {
        "claude-fable-5-2" | "claude-opus-5-5" | "claude-sonnet-5-5" | "claude-haiku-5-5" => {
            takes(true, "medium", false)
        }
        "claude-opus-4-7" => takes(true, "xhigh", true),
        "claude-fable-5" | "claude-fable-5-1" | "claude-opus-5" | "claude-sonnet-5"
        | "claude-opus-4-8" => takes(true, "high", true),
        "claude-opus-4-6" | "claude-sonnet-4-6" => takes(false, "high", true),
        "claude-opus-4-5" | "claude-sonnet-4-5" | "claude-haiku-4-5" | "claude-opus-4-1"
        | "claude-opus-4" | "claude-sonnet-4" => Some(None),
        other if other.starts_with("claude-3") => Some(None),
        _ => None,
    }
}

/// A Claude model ID without its `[1m]`, provider prefix, version or date suffix, such as
/// `claude-opus-5-5` for `us.anthropic.claude-opus-5-5-20260101-v1:0`; None for an alias.
fn claude_canonical(model: &str) -> Option<String> {
    let model = model.trim().to_ascii_lowercase();
    let model = model.strip_suffix("[1m]").unwrap_or(&model);
    let start = model.find("claude-")?;
    let mut id = model[start..]
        .split([':', '@'])
        .next()
        .unwrap_or_default()
        .to_string();
    // Bedrock's `-v1`.
    if let Some((head, version)) = id.rsplit_once("-v")
        && !version.is_empty()
        && version.bytes().all(|b| b.is_ascii_digit())
    {
        id = head.into();
    }
    if let Some((head, date)) = id.rsplit_once('-')
        && date.len() == 8
        && date.bytes().all(|b| b.is_ascii_digit())
    {
        id = head.into();
    }
    Some(id)
}

/// Claude Code's model aliases, which name a family rather than a model.
fn claude_alias_family(alias: &str) -> Option<Option<&'static str>> {
    let alias = alias.trim().to_ascii_lowercase();
    match alias.strip_suffix("[1m]").unwrap_or(&alias) {
        "opus" | "opusplan" => Some(Some("opus")),
        "sonnet" => Some(Some("sonnet")),
        "haiku" => Some(Some("haiku")),
        "fable" => Some(Some("fable")),
        "default" | "best" => Some(None),
        _ => None,
    }
}

/// The model the latest main session ran most, so a side request on a small model, such
/// as a title, does not stand for the session's own.
pub(crate) fn latest_model<'a>(requests: &[&'a Request]) -> Option<&'a str> {
    let main = || {
        requests
            .iter()
            .filter(|r| r.agent == Agent::Main && r.model.is_some())
    };
    let session = &main().max_by_key(|r| r.time)?.session;
    let mut counts: IndexMap<&str, usize> = IndexMap::default();
    for r in main().filter(|r| &r.session == session) {
        *counts.entry(r.model.as_deref()?).or_default() += 1;
    }
    // The first seen wins a tie.
    counts
        .iter()
        .rev()
        .max_by_key(|(_, n)| **n)
        .map(|(model, _)| *model)
}

const RECORDED: &str = "the latest recorded main-session model";

/// The setting's value at `key` in the highest scope that sets it, with that scope.
fn highest<'a>(scopes: &[SettingsScope<'a>], key: &str) -> Option<(&'a Value, &'a str)> {
    scopes
        .iter()
        .rev()
        .find_map(|s| s.config.get(key).map(|v| (v, s.file)))
}

/// A Claude level setting's value: a string Claude Code accepts there, or an error.
fn claude_level<'a>(
    value: &'a Value,
    key: &str,
    file: &str,
    max: bool,
) -> Result<&'a str, ScanError> {
    match value.as_str() {
        Some(level) if rank(level).is_some() && (max || level != "max") => Ok(level),
        _ => Err(invalid(format!("invalid {key}"), file)),
    }
}

/// The `modelSettings` entry in `config` for a canonical model, and its key.
fn model_entry<'a>(
    config: &'a Map<String, Value>,
    model: &str,
) -> Option<(&'a str, &'a Map<String, Value>)> {
    members(config.get("modelSettings"))
        .iter()
        .find(|(key, _)| claude_canonical(key).as_deref() == Some(model))
        .map(|(key, entry)| (key.as_str(), members(Some(entry))))
}

/// Resolve a Claude profile's default effort as Claude Code's model configuration docs give
/// it: `CLAUDE_CODE_EFFORT_LEVEL`, then the settings files, then the model's own default,
/// lowered to a supported level and the lowest `maxEffortLevel`. Scopes are lowest first.
pub fn claude_effort(
    scopes: &[SettingsScope],
    user_file: &str,
    env: &Env,
    recorded: Option<&str>,
) -> Result<EffortControl, ScanError> {
    let environment = scoped_environment(scopes, env);
    let unknown =
        |model: Option<String>, model_owner: String, owner: String, why: String| EffortControl {
            model,
            model_owner,
            level: None,
            owner,
            effective: None,
            lowered: None,
            unresolved: Some(why),
        };
    let variable = "CLAUDE_CODE_EFFORT_LEVEL";
    let pinned = environment.values.get(variable);
    let pinned_owner = environment
        .owners
        .get(variable)
        .cloned()
        .unwrap_or_default();
    // The model: ANTHROPIC_MODEL, then the highest settings file's `model`, then the session.
    let (named, model_owner) = match environment.values.get("ANTHROPIC_MODEL") {
        Some(Value::String(m)) => (
            Some(m.as_str()),
            environment.owners["ANTHROPIC_MODEL"].clone(),
        ),
        _ => match highest(scopes, "model") {
            Some((Value::String(m), file)) => (Some(m.as_str()), format!("{file}:model")),
            Some((Value::Null, _)) | None => (recorded, RECORDED.to_string()),
            Some((_, file)) => return Err(invalid("invalid model", file)),
        },
    };
    let recorded_canonical = recorded.and_then(claude_canonical);
    let model = match named {
        None => None,
        Some(m) => match claude_alias_family(m) {
            // An alias names whichever model Claude Code maps it to now; the latest
            // session on that family shows which.
            Some(family) => recorded_canonical
                .clone()
                .filter(|r| family.is_none_or(|f| r.starts_with(&format!("claude-{f}-")))),
            None => claude_canonical(m),
        },
    };
    let default_owner = |model: &str| {
        format!("{user_file}:modelSettings.{model}.effortLevel (the model's default)")
    };
    let Some(model) = model else {
        let why = match named {
            None => {
                "no setting names the model and no main-session request recorded one".to_string()
            }
            Some(m) => {
                format!("the model {m} cannot be resolved to a model in Claude Code's effort table")
            }
        };
        let owner = if pinned.is_some() {
            pinned_owner
        } else {
            user_file.to_string()
        };
        return Ok(unknown(None, model_owner, owner, why));
    };
    let Some(support) = claude_model(&model) else {
        return Ok(unknown(
            Some(model.clone()),
            model_owner,
            default_owner(&model),
            format!(
                "{model} is not in Claude Code's effort table, so its levels and default are unknown"
            ),
        ));
    };
    let Some(support) = support else {
        return Ok(EffortControl {
            lowered: Some(format!("{model} takes no effort level")),
            unresolved: None,
            ..unknown(
                Some(model.clone()),
                model_owner,
                default_owner(&model),
                String::new(),
            )
        });
    };
    // The level: the environment variable, then the highest file that sets one for this
    // model, then the model's default.
    let (level, owner) = match pinned {
        Some(value) => match value.as_str() {
            Some("auto" | "unset") => ("auto", pinned_owner),
            Some(level) if rank(level).is_some() => (level, pinned_owner),
            _ => {
                return Err(match pinned_owner.split_once(":env.") {
                    Some((file, _)) => invalid(format!("invalid {variable}"), file),
                    None => ScanError::new(format!("invalid {variable}")),
                });
            }
        },
        None => {
            let mut found = None;
            for scope in scopes.iter().rev() {
                if let Some((key, entry)) = model_entry(scope.config, &model)
                    && let Some(value) = entry.get("effortLevel")
                {
                    let level =
                        claude_level(value, "modelSettings effortLevel", scope.file, false)?;
                    found = Some((
                        level,
                        format!("{}:modelSettings.{key}.effortLevel", scope.file),
                    ));
                    break;
                }
                // The user's own top-level level is the older form, which newer models ignore.
                if let Some(value) = scope.config.get("effortLevel")
                    && (scope.file != user_file || support.legacy_user_level)
                {
                    let level = claude_level(value, "effortLevel", scope.file, false)?;
                    found = Some((level, format!("{}:effortLevel", scope.file)));
                    break;
                }
            }
            found.unwrap_or((support.default, default_owner(&model)))
        }
    };
    // The lowest cap any scope sets for this model; a model's own entry replaces its file's.
    let mut cap: Option<(&str, String)> = None;
    for scope in scopes {
        let entry = model_entry(scope.config, &model);
        let (value, key) =
            match entry.and_then(|(key, e)| e.get("maxEffortLevel").map(|v| (v, key))) {
                Some((value, key)) => (value, format!("modelSettings.{key}.maxEffortLevel")),
                None => match scope.config.get("maxEffortLevel") {
                    Some(value) => (value, "maxEffortLevel".to_string()),
                    None => continue,
                },
            };
        let value = claude_level(value, "maxEffortLevel", scope.file, true)?;
        if value != "max" && cap.as_ref().is_none_or(|(c, _)| rank(value) < rank(c)) {
            cap = Some((value, format!("{}:{key}", scope.file)));
        }
    }
    let supported = |level: &'static str| -> &'static str {
        if level == "xhigh" && !support.xhigh {
            "high"
        } else {
            level
        }
    };
    let as_static = |level: &str| LEVELS[rank(level).expect("validated level")];
    let (effective, lowered) = if level == "auto" {
        match &cap {
            // Below a cap, Claude Code still sends its own default, lowered to the cap.
            Some((c, owner)) => {
                let default = supported(support.default);
                let sent = if rank(c) < rank(default) {
                    as_static(c)
                } else {
                    default
                };
                (
                    sent,
                    Some(format!(
                        "{owner} applies, so Claude Code sends the model's default lowered to that cap"
                    )),
                )
            }
            None => ("auto", None),
        }
    } else {
        let mut effective = supported(as_static(level));
        let mut lowered = (effective != level)
            .then(|| format!("{model} has no {level}, so it runs at {effective}"));
        if let Some((c, owner)) = &cap
            && rank(c) < rank(effective)
        {
            effective = as_static(c);
            lowered = Some(format!("{owner} caps it at {c}"));
        }
        (effective, lowered)
    };
    Ok(EffortControl {
        model: Some(model),
        model_owner,
        level: Some(level.into()),
        owner,
        effective: Some(effective.into()),
        lowered,
        unresolved: None,
    })
}

/// One Codex model's reasoning levels from Codex's model catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexModel {
    pub default: String,
    pub supported: Vec<String>,
}

/// Codex's cached model catalog, `models_cache.json` in its home, by model slug; None when it
/// cannot be read.
pub fn codex_catalog(root: &str) -> Option<IndexMap<String, CodexModel>> {
    let path = std::path::Path::new(root).join("models_cache.json");
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let mut catalog = IndexMap::default();
    for m in value.get("models")?.as_array()? {
        let (Some(slug), Some(default)) = (
            m.get("slug").and_then(Value::as_str),
            m.get("default_reasoning_level").and_then(Value::as_str),
        ) else {
            continue;
        };
        let supported = m
            .get("supported_reasoning_levels")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|l| l.get("effort").and_then(Value::as_str).map(String::from))
            .collect();
        catalog.insert(
            slug.to_string(),
            CodexModel {
                default: default.into(),
                supported,
            },
        );
    }
    Some(catalog)
}

/// Resolve a Codex profile's default effort: `model_reasoning_effort` in the highest config
/// layer that sets it, else the model's default from Codex's model catalog. Scopes are the
/// layers Codex reads, lowest first.
pub fn codex_effort(
    scopes: &[SettingsScope],
    user_file: &str,
    catalog: Option<&IndexMap<String, CodexModel>>,
    recorded: Option<&str>,
) -> Result<EffortControl, ScanError> {
    let (model, model_owner) = match highest(scopes, "model") {
        Some((Value::String(m), file)) => (Some(m.clone()), format!("{file}:model")),
        Some((_, file)) => return Err(invalid("invalid model", file)),
        None => (recorded.map(String::from), RECORDED.to_string()),
    };
    let key = "model_reasoning_effort";
    let (level, owner) = match highest(scopes, key) {
        Some((Value::String(level), file)) => (Some(level.clone()), format!("{file}:{key}")),
        Some((_, file)) => return Err(invalid(format!("invalid {key}"), file)),
        None => {
            let default = model
                .as_deref()
                .and_then(|m| catalog.and_then(|c| c.get(m)))
                .map(|m| m.default.clone());
            (default, format!("{user_file}:{key} (the model's default)"))
        }
    };
    let unresolved = level.is_none().then(|| match (&model, catalog) {
        (None, _) => "no setting names the model and no main-session request recorded one".to_string(),
        (Some(_), None) => "model_reasoning_effort is unset and Codex's model catalog, models_cache.json, cannot be read for the model's default".to_string(),
        (Some(m), Some(_)) => format!("model_reasoning_effort is unset and Codex's model catalog does not list {m}"),
    });
    Ok(EffortControl {
        model,
        model_owner,
        effective: level.clone(),
        level,
        owner,
        lowered: None,
        unresolved,
    })
}

/// Inputs for the effort checks.
pub struct EffortInput<'a> {
    pub scan: &'a Scan,
    pub catalog: &'a Catalog,
    /// The explicit window start; None uses each profile's first request.
    pub since: Option<Millis>,
    /// The window end.
    pub now: Millis,
}

/// One line per profile with a resolved control.
pub fn effort_checks(input: &EffortInput<'_>) -> Vec<EffortCheck> {
    input
        .scan
        .profiles
        .iter()
        .filter_map(|p| Some(check(input, p, p.effort.as_ref()?)))
        .collect()
}

/// The level a `max` default should become: `xhigh`, or `high` on a model without it.
fn below_max(harness: Harness, control: &EffortControl) -> &'static str {
    let without = match harness {
        Harness::Claude => control
            .model
            .as_deref()
            .and_then(claude_model)
            .flatten()
            .is_some_and(|m| !m.xhigh),
        Harness::Codex => false,
    };
    if without { "high" } else { "xhigh" }
}

fn check(input: &EffortInput<'_>, profile: &Profile, control: &EffortControl) -> EffortCheck {
    let requests: Vec<&Request> = input
        .scan
        .requests
        .iter()
        .filter(|r| {
            r.agent == Agent::Main
                && r.harness == profile.harness
                && r.profiles.contains(&profile.id)
                && input.since.is_none_or(|s| r.time >= s)
        })
        .collect();
    // Requests and their cost by recorded model and effort.
    type Pair<'a> = (Option<&'a str>, Option<&'a str>);
    let mut groups: IndexMap<Pair<'_>, (u64, Sum)> = IndexMap::default();
    for r in &requests {
        let (n, sum) = groups
            .entry((r.model.as_deref(), r.effort.as_deref()))
            .or_insert_with(|| (0, Sum::new()));
        *n += 1;
        sum.add(&price_request(input.catalog, r));
    }
    let mut runs: Vec<EffortRun> = groups
        .into_iter()
        .map(|((model, effort), (requests, sum))| EffortRun {
            model: model.map(String::from),
            effort: effort.map(String::from),
            requests,
            cost: sum.cost(),
        })
        .collect();
    runs.sort_by_key(|r| Reverse(r.requests));
    let effective = control
        .unresolved
        .is_none()
        .then(|| control.effective.clone())
        .flatten();
    let (action, recommended) = match effective.as_deref() {
        None => (Action::Advisory, None),
        Some("max") => (
            Action::Change,
            Some(below_max(profile.harness, control).to_string()),
        ),
        Some("low") => (Action::Advisory, Some("medium".to_string())),
        Some(_) => (Action::Keep, None),
    };
    // Unknown stays advisory without a value; a known level other than max or low has no verdict.
    let action = if control.unresolved.is_some() {
        Action::Advisory
    } else {
        action
    };
    let mut uncertainty = vec![
        "a different effort changes the output tokens, so its cost is not repriced".to_string(),
        "a level picked in a session, such as with /effort or --effort, outranks the saved default and is not recorded; the recorded efforts show what ran".to_string(),
    ];
    if let Some(why) = &control.unresolved {
        uncertainty.push(why.clone());
    }
    let enterprise = profile.harness == Harness::Claude && profile.provider.is_none();
    if enterprise {
        uncertainty.push("a Claude Enterprise per-role effort cap, which no local file records, could lower the level further".into());
    }
    EffortCheck {
        base: CheckBase {
            profile: profile.id.clone(),
            history_profiles: vec![],
            evidence: Evidence::Guidance,
            action,
            current: effective,
            recommended,
            owner: control.owner.clone(),
            since: input.since.or(requests.iter().map(|r| r.time).min()),
            until: Some(input.now),
            window_basis: if input.since.is_none() {
                "available selected usage"
            } else {
                "explicit --since"
            }
            .into(),
            baseline: "the recorded main-session requests by model and effort".into(),
            requests: requests.len() as u64,
            coverage: None,
            savings_usd: None,
            monthly_savings_usd: None,
            break_even: None,
            assumptions: vec![
                "the current default is applied to the whole window; a level picked in a session is shown only by the recorded efforts".into(),
            ],
            uncertainty,
            sources: SOURCES.map(String::from).into(),
        },
        harness: profile.harness,
        control: control.clone(),
        enterprise,
        runs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    fn env(pairs: &[(&str, &str)]) -> Env {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    const USER: &str = "/home/u/.claude/settings.json";
    const PROJECT: &str = "/work/.claude/settings.json";
    const MANAGED: &str = "/etc/claude-code/managed-settings.json";

    fn resolve(
        user: Value,
        project: Value,
        managed: Value,
        vars: &[(&str, &str)],
        recorded: Option<&str>,
    ) -> Result<EffortControl, ScanError> {
        let (user, project, managed) = (config(user), config(project), config(managed));
        let scopes = [
            SettingsScope {
                config: &user,
                file: USER,
            },
            SettingsScope {
                config: &project,
                file: PROJECT,
            },
            SettingsScope {
                config: &managed,
                file: MANAGED,
            },
        ];
        claude_effort(&scopes, USER, &env(vars), recorded)
    }

    fn level(c: &EffortControl) -> (Option<&str>, Option<&str>, &str) {
        (c.level.as_deref(), c.effective.as_deref(), c.owner.as_str())
    }

    #[test]
    fn claude_models_resolve_through_ids_aliases_and_the_session() {
        for (id, canonical) in [
            ("claude-opus-5-5[1m]", "claude-opus-5-5"),
            ("claude-haiku-5-5-20260101", "claude-haiku-5-5"),
            (
                "us.anthropic.claude-opus-4-6-20260101-v1:0",
                "claude-opus-4-6",
            ),
            ("claude-sonnet-4-6@20260101", "claude-sonnet-4-6"),
        ] {
            assert_eq!(claude_canonical(id).as_deref(), Some(canonical), "{id}");
        }
        assert_eq!(claude_canonical("opus"), None);
        // An alias takes the latest session's model only when it is in the alias's family.
        let c = resolve(
            json!({"model": "opus"}),
            json!({}),
            json!({}),
            &[],
            Some("claude-opus-5-5"),
        )
        .unwrap();
        assert_eq!(c.model.as_deref(), Some("claude-opus-5-5"));
        let c = resolve(
            json!({"model": "opus"}),
            json!({}),
            json!({}),
            &[],
            Some("claude-sonnet-5-5"),
        )
        .unwrap();
        assert!(c.unresolved.is_some());
        let c = resolve(json!({}), json!({}), json!({}), &[], None).unwrap();
        assert!(c.unresolved.is_some());
        // ANTHROPIC_MODEL outranks the settings files.
        let c = resolve(
            json!({"model": "claude-opus-5-5"}),
            json!({}),
            json!({}),
            &[("ANTHROPIC_MODEL", "claude-opus-4-7")],
            None,
        )
        .unwrap();
        assert_eq!(
            (c.model.as_deref(), c.model_owner.as_str()),
            (Some("claude-opus-4-7"), "ANTHROPIC_MODEL")
        );
        assert_eq!(
            level(&c),
            (
                Some("xhigh"),
                Some("xhigh"),
                "/home/u/.claude/settings.json:modelSettings.claude-opus-4-7.effortLevel (the model's default)"
            )
        );
    }

    #[test]
    fn claude_levels_follow_the_documented_order() {
        let opus = Some("claude-opus-5-5");
        // The model's default.
        let c = resolve(json!({}), json!({}), json!({}), &[], opus).unwrap();
        assert_eq!(level(&c).1, Some("medium"));
        // The user's top-level level is ignored on Opus 5.5 but applies on Opus 5.
        let c = resolve(
            json!({"effortLevel": "high"}),
            json!({}),
            json!({}),
            &[],
            opus,
        )
        .unwrap();
        assert_eq!(level(&c).1, Some("medium"));
        let c = resolve(
            json!({"effortLevel": "low"}),
            json!({}),
            json!({}),
            &[],
            Some("claude-opus-5"),
        )
        .unwrap();
        assert_eq!(
            level(&c),
            (
                Some("low"),
                Some("low"),
                "/home/u/.claude/settings.json:effortLevel"
            )
        );
        // A project's top-level level applies to every model, and outranks a saved per-model level.
        let c = resolve(
            json!({"modelSettings": {"claude-opus-5-5": {"effortLevel": "high"}}}),
            json!({"effortLevel": "low"}),
            json!({}),
            &[],
            opus,
        )
        .unwrap();
        assert_eq!(
            level(&c),
            (
                Some("low"),
                Some("low"),
                "/work/.claude/settings.json:effortLevel"
            )
        );
        // Within one file, the model's own level wins.
        let c = resolve(
            json!({"effortLevel": "low", "modelSettings": {"claude-opus-5-5[1m]": {"effortLevel": "xhigh"}}}),
            json!({}),
            json!({}),
            &[],
            opus,
        )
        .unwrap();
        assert_eq!(
            level(&c),
            (
                Some("xhigh"),
                Some("xhigh"),
                "/home/u/.claude/settings.json:modelSettings.claude-opus-5-5[1m].effortLevel"
            )
        );
        // The environment variable outranks every file, and alone can name max.
        let c = resolve(
            json!({"env": {"CLAUDE_CODE_EFFORT_LEVEL": "max"}}),
            json!({}),
            json!({"effortLevel": "low"}),
            &[],
            opus,
        )
        .unwrap();
        assert_eq!(
            level(&c),
            (
                Some("max"),
                Some("max"),
                "/home/u/.claude/settings.json:env.CLAUDE_CODE_EFFORT_LEVEL"
            )
        );
        let c = resolve(
            json!({}),
            json!({}),
            json!({}),
            &[("CLAUDE_CODE_EFFORT_LEVEL", "max")],
            opus,
        )
        .unwrap();
        assert_eq!(
            level(&c),
            (Some("max"), Some("max"), "CLAUDE_CODE_EFFORT_LEVEL")
        );
        // Settings never accept max, and an unknown variable value fails loud.
        assert!(
            resolve(
                json!({"effortLevel": "max"}),
                json!({}),
                json!({}),
                &[],
                Some("claude-opus-5")
            )
            .is_err()
        );
        assert!(
            resolve(
                json!({}),
                json!({}),
                json!({}),
                &[("CLAUDE_CODE_EFFORT_LEVEL", "ultracode")],
                opus
            )
            .is_err()
        );
    }

    #[test]
    fn claude_levels_fall_back_and_caps_lower_them() {
        // xhigh runs as high on Opus 4.6.
        let c = resolve(
            json!({"effortLevel": "xhigh"}),
            json!({}),
            json!({}),
            &[],
            Some("claude-opus-4-6"),
        )
        .unwrap();
        assert_eq!(level(&c).1, Some("high"));
        assert!(c.lowered.unwrap().contains("has no xhigh"));
        // The lowest cap across files applies, and a model's own entry replaces its file's.
        let c = resolve(
            json!({"maxEffortLevel": "medium", "modelSettings": {"claude-opus-5-5": {"maxEffortLevel": "max"}}}),
            json!({"maxEffortLevel": "high"}),
            json!({}),
            &[("CLAUDE_CODE_EFFORT_LEVEL", "max")],
            Some("claude-opus-5-5"),
        )
        .unwrap();
        assert_eq!(
            level(&c),
            (Some("max"), Some("high"), "CLAUDE_CODE_EFFORT_LEVEL")
        );
        assert_eq!(
            c.lowered.as_deref(),
            Some("/work/.claude/settings.json:maxEffortLevel caps it at high")
        );
        // auto sends nothing, unless a cap below max makes Claude Code send its default lowered to it.
        let c = resolve(
            json!({}),
            json!({}),
            json!({}),
            &[("CLAUDE_CODE_EFFORT_LEVEL", "auto")],
            Some("claude-opus-4-7"),
        )
        .unwrap();
        assert_eq!(level(&c).1, Some("auto"));
        let c = resolve(
            json!({}),
            json!({}),
            json!({"maxEffortLevel": "high"}),
            &[("CLAUDE_CODE_EFFORT_LEVEL", "unset")],
            Some("claude-opus-4-7"),
        )
        .unwrap();
        assert_eq!(
            level(&c),
            (Some("auto"), Some("high"), "CLAUDE_CODE_EFFORT_LEVEL")
        );
        // A model without effort has no level, and that is not unknown.
        let c = resolve(
            json!({}),
            json!({}),
            json!({}),
            &[],
            Some("claude-haiku-4-5-20251001"),
        )
        .unwrap();
        assert_eq!((c.level.as_deref(), c.unresolved.as_deref()), (None, None));
    }

    #[test]
    fn the_session_model_is_the_one_the_latest_session_ran_most() {
        let request = |session: &str, time: Millis, model: &str| {
            let mut r = crate::test_support::make_request();
            let meta = std::sync::Arc::make_mut(&mut r.meta);
            meta.session = session.into();
            meta.model = Some(model.into());
            r.time = time;
            r
        };
        let requests = [
            request("old", 0, "claude-sonnet-5-5"),
            request("new", 1, "claude-opus-5-5"),
            request("new", 2, "claude-opus-5-5"),
            request("new", 3, "claude-haiku-5-5"),
        ];
        let refs: Vec<&Request> = requests.iter().collect();
        assert_eq!(latest_model(&refs), Some("claude-opus-5-5"));
        assert_eq!(latest_model(&refs[..1]), Some("claude-sonnet-5-5"));
        assert_eq!(latest_model(&[]), None);
    }

    #[test]
    fn codex_levels_come_from_config_or_the_catalog() {
        let user_file = "/home/u/.codex/config.toml";
        let user = config(json!({"model": "gpt-6.1-sol"}));
        let scopes = [SettingsScope {
            config: &user,
            file: user_file,
        }];
        let catalog: IndexMap<String, CodexModel> = [(
            "gpt-6.1-sol".to_string(),
            CodexModel {
                default: "low".into(),
                supported: vec!["low".into(), "max".into()],
            },
        )]
        .into_iter()
        .collect();
        let c = codex_effort(&scopes, user_file, Some(&catalog), None).unwrap();
        assert_eq!(
            level(&c),
            (
                Some("low"),
                Some("low"),
                "/home/u/.codex/config.toml:model_reasoning_effort (the model's default)"
            )
        );
        let c = codex_effort(&scopes, user_file, None, None).unwrap();
        assert!(c.unresolved.unwrap().contains("cannot be read"));
        let set = config(json!({"model": "gpt-6.1-sol", "model_reasoning_effort": "max"}));
        let scopes = [SettingsScope {
            config: &set,
            file: user_file,
        }];
        let c = codex_effort(&scopes, user_file, None, None).unwrap();
        assert_eq!(
            level(&c),
            (
                Some("max"),
                Some("max"),
                "/home/u/.codex/config.toml:model_reasoning_effort"
            )
        );
    }
}
