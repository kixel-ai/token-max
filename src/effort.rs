//! The main session's default reasoning effort: what decides it, the level the session runs
//! at, and what each model and effort pair cost, with no verdict: a different effort
//! changes the output tokens, so no cost is repriced and no saving can be measured.

use std::cmp::Reverse;

use serde_json::{Map, Value};

use crate::collections::IndexMap;
use crate::controls::{Env, ScopedEnvironment, SettingsScope, members, scoped_environment};
use crate::model::{
    Action, Agent, CheckBase, EffortCheck, EffortControl, EffortRun, Evidence, Harness, Location,
    Millis, Owned, Profile, Request, Scan,
};
use crate::prices::{Catalog, Sample, Sum, price_request};
use crate::safe::ScanError;

const SOURCES: [&str; 5] = [
    "https://code.claude.com/docs/en/model-config#adjust-effort-level (2026-11-14)",
    "https://code.claude.com/docs/en/settings-reference#modelsettings (2026-11-14)",
    "https://claude.dev/blog/which-claude-model-should-you-build-with/ (2026-11-14)",
    "https://developers.openai.com/api/docs/guides/reasoning (2026-11-14)",
    "https://learn.chatgpt.com/docs/config-file/config-reference (2026-11-14)",
];

/// A main-session model and effort.
pub type Pair = (&'static str, &'static str);

/// The maintainer's pick of a main-session default for each harness, then the other
/// defaults they accept as fine.
const PICKS: [(Harness, Pair, &[Pair]); 2] = [
    (
        Harness::Claude,
        ("claude-opus-5-5", "medium"),
        &[("claude-opus-5-5", "high")],
    ),
    (
        Harness::Codex,
        ("gpt-6.2-sol", "high"),
        &[("gpt-6.2-sol", "xhigh")],
    ),
];

/// The maintainer's pick of a main-session default for `harness`, and the other defaults
/// they accept.
pub fn pick(harness: Harness) -> (Pair, &'static [Pair]) {
    let (_, pick, accepted) = PICKS
        .iter()
        .find(|(h, _, _)| *h == harness)
        .expect("every harness has a pick");
    (*pick, accepted)
}

/// Whether a resolved default is the maintainer's pick for `harness` or one they accept.
pub fn accepted(control: &EffortControl, harness: Harness) -> bool {
    let (pick, others) = pick(harness);
    let current = (control.model.as_deref(), control.effective.as_deref());
    std::iter::once(&pick)
        .chain(others)
        .any(|(model, effort)| current == (Some(*model), Some(*effort)))
}

/// Whether Claude level `level` is below `than`; false when either is not a Claude level.
pub(crate) fn below(level: &str, than: &str) -> bool {
    rank(level).zip(rank(than)).is_some_and(|(l, t)| l < t)
}

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
    if let Some(head) = crate::controls::strip_dated(&id, 8) {
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

/// What a level owner ends with when no setting names a level.
pub(crate) const MODEL_DEFAULT: &str = " (the model's default)";

/// The model owner when no setting names the model.
pub(crate) const RECORDED: &str = "the latest recorded main-session model";

/// Why the main-session default is unknown when nothing names or recorded a model.
pub(crate) const NO_MODEL: &str =
    "no setting names the model and no main-session request recorded one";

/// Why it is unknown when only requests whose owner T3 could not settle recorded one.
pub(crate) const UNSETTLED_MODEL: &str = "no setting names the model, and the main-session requests that recorded one may be a T3 thread's, since T3 could not settle their owner";

/// The setting's value at `key` in the highest scope that sets it, with that scope.
pub(crate) fn highest<'a>(scopes: &[SettingsScope<'a>], key: &str) -> Option<(&'a Value, &'a str)> {
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
    // The model: ANTHROPIC_MODEL, then the highest settings file's `model`, then the session.
    let (named, model_owner) = match environment.values.get("ANTHROPIC_MODEL") {
        Some(Value::String(m)) => (
            Some(m.as_str()),
            environment.owners["ANTHROPIC_MODEL"].clone(),
        ),
        _ => match highest(scopes, "model") {
            Some((Value::String(m), file)) => (Some(m.as_str()), format!("{file}:model")),
            // A null outranks the files below it, so it owns the model the session falls back to.
            Some((Value::Null, file)) => (recorded, format!("{file}:model")),
            None => (recorded, RECORDED.to_string()),
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
    let mut control = claude_on(
        scopes,
        user_file,
        &environment,
        model,
        named,
        model_owner.clone(),
    )?;
    let (pick, _) = pick(Harness::Claude);
    if control.unresolved.is_none() && control.model.as_deref() != Some(pick.0) {
        // A setting only the pick reads, such as an invalid level in its own `modelSettings`
        // row, leaves the pick unresolved: the current default reads none of it.
        let mut picked = match claude_on(
            scopes,
            user_file,
            &environment,
            Some(pick.0.into()),
            Some(pick.0),
            model_owner.clone(),
        ) {
            Ok(picked) => picked,
            Err(error) => {
                let file = error
                    .location
                    .map_or_else(|| user_file.to_string(), |l| l.file.to_string());
                EffortControl {
                    model: Some(pick.0.into()),
                    model_owner,
                    level: None,
                    unresolved: Some(format!(
                        "{} in {file}, which only {} reads",
                        error.message, pick.0
                    )),
                    owner: file,
                    effective: None,
                    lowered: None,
                    caps: vec![],
                    default: None,
                    picked: None,
                    provider: None,
                }
            }
        };
        // A `[1m]` context window carries over to the pick, as the edit names it.
        if named.is_some_and(|m| m.trim().to_ascii_lowercase().ends_with("[1m]")) {
            picked.model = Some(format!("{}[1m]", pick.0));
        }
        control.picked = Some(Box::new(picked));
    }
    control.provider = claude_provider(&environment);
    Ok(control)
}

/// The cloud provider a Claude Code environment sends requests to; None for Anthropic's
/// API, which a custom ANTHROPIC_BASE_URL (a gateway or proxy) still speaks.
pub(crate) fn claude_provider(environment: &ScopedEnvironment) -> Option<String> {
    let enabled =
        |key: &str| matches!(environment.values.get(key), Some(Value::String(s)) if s == "1");
    [
        ("CLAUDE_CODE_USE_BEDROCK", "amazon-bedrock"),
        ("CLAUDE_CODE_USE_VERTEX", "google-vertex-anthropic"),
        ("CLAUDE_CODE_USE_FOUNDRY", "azure"),
    ]
    .into_iter()
    .find(|(key, _)| enabled(key))
    .map(|(_, provider)| provider.to_string())
}

/// A Claude profile's default effort on `model`, which `named` names in the settings or
/// `model_owner` records; `unresolved` says why when it cannot be resolved.
fn claude_on(
    scopes: &[SettingsScope],
    user_file: &str,
    environment: &ScopedEnvironment,
    model: Option<String>,
    named: Option<&str>,
    model_owner: String,
) -> Result<EffortControl, ScanError> {
    let unknown =
        |model: Option<String>, model_owner: String, owner: String, why: String| EffortControl {
            model,
            model_owner,
            level: None,
            owner,
            effective: None,
            lowered: None,
            caps: vec![],
            default: None,
            unresolved: Some(why),
            picked: None,
            provider: None,
        };
    let variable = "CLAUDE_CODE_EFFORT_LEVEL";
    let pinned = environment.values.get(variable);
    let pinned_owner = environment
        .owners
        .get(variable)
        .cloned()
        .unwrap_or_default();
    let default_owner =
        |model: &str| format!("{user_file}:modelSettings.{model}.effortLevel{MODEL_DEFAULT}");
    let Some(model) = model else {
        let why = match named {
            None => NO_MODEL.to_string(),
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
    // Every cap a scope sets for this model, where a model's own entry replaces its file's;
    // the lowest applies.
    let mut caps: Vec<Owned> = vec![];
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
        if value == "max" {
            continue;
        }
        let owner = format!("{}:{key}", scope.file);
        if cap.as_ref().is_none_or(|(c, _)| rank(value) < rank(c)) {
            cap = Some((value, owner.clone()));
        }
        caps.push(Owned {
            value: value.into(),
            owner,
        });
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
                (
                    if rank(c) < rank(default) {
                        as_static(c)
                    } else {
                        default
                    },
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
        caps,
        default: Some(support.default.into()),
        unresolved: None,
        picked: None,
        provider: None,
    })
}

/// One Codex model's reasoning levels from Codex's model catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexModel {
    pub default: String,
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
        catalog.insert(
            slug.to_string(),
            CodexModel {
                default: default.into(),
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
    let mut control = codex_on(scopes, user_file, catalog, model, model_owner.clone())?;
    let (pick, _) = pick(Harness::Codex);
    if control.unresolved.is_none() && control.model.as_deref() != Some(pick.0) {
        let picked = codex_on(scopes, user_file, catalog, Some(pick.0.into()), model_owner)?;
        control.picked = Some(Box::new(picked));
    }
    // Codex's built-in OpenAI provider takes OpenAI's model IDs.
    control.provider = highest(scopes, "model_provider")
        .and_then(|(value, _)| crate::safe::label(value))
        .filter(|p| p != "openai");
    Ok(control)
}

/// A Codex profile's default effort on `model`, which `model_owner` names.
fn codex_on(
    scopes: &[SettingsScope],
    user_file: &str,
    catalog: Option<&IndexMap<String, CodexModel>>,
    model: Option<String>,
    model_owner: String,
) -> Result<EffortControl, ScanError> {
    let key = "model_reasoning_effort";
    let default = model
        .as_deref()
        .and_then(|m| catalog.and_then(|c| c.get(m)))
        .map(|m| m.default.clone());
    let (level, owner) = match highest(scopes, key) {
        Some((Value::String(level), file)) => (Some(level.clone()), format!("{file}:{key}")),
        Some((_, file)) => return Err(invalid(format!("invalid {key}"), file)),
        None => (default.clone(), format!("{user_file}:{key}{MODEL_DEFAULT}")),
    };
    let unresolved = level.is_none().then(|| match (&model, catalog) {
        (None, _) => NO_MODEL.to_string(),
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
        caps: vec![],
        default,
        unresolved,
        picked: None,
        provider: None,
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
    let owner = |p: &Profile| {
        let e = p.effort.as_ref().expect("filtered on an effort control");
        // One owner can resolve to different levels on different recorded models.
        // The pick's own level can come from a file one profile reads and another does not.
        let picked = e
            .picked
            .as_ref()
            .map(|p| (&p.model, &p.owner, &p.level, &p.effective, &p.caps));
        // Caps and the provider each change the edit the line's card offers.
        format!(
            "{}\n{}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{picked:?}",
            e.owner, e.model_owner, e.model, e.level, e.effective, e.unresolved, e.caps, e.provider
        )
    };
    let controlled = input.scan.profiles.iter().filter(|p| p.effort.is_some());
    crate::rows::groups(controlled, owner)
        .iter()
        .map(|line| check(input, line, line.lead.effort.as_ref().expect("a control")))
        .collect()
}

fn check(
    input: &EffortInput<'_>,
    line: &crate::rows::Group<'_>,
    control: &EffortControl,
) -> EffortCheck {
    let profile = line.lead;
    let requests: Vec<&Request> = input
        .scan
        .requests
        .iter()
        .filter(|r| {
            r.agent == Agent::Main
                && r.harness == profile.harness
                && line.covers(&r.profiles)
                && input.since.is_none_or(|s| r.time >= s)
        })
        .collect();
    // Requests and their cost by recorded model and effort.
    type Pair<'a> = (Option<&'a str>, Option<&'a str>);
    let mut groups: IndexMap<Pair<'_>, (u64, Sum, Sample)> = IndexMap::default();
    let mut all = Sample::default();
    for r in &requests {
        let (n, sum, sample) = groups
            .entry((r.model.as_deref(), r.effort.as_deref()))
            .or_insert_with(|| (0, Sum::new(), Sample::default()));
        let cost = price_request(input.catalog, r);
        *n += 1;
        sample.add_cost(&cost);
        all.add_cost(&cost);
        sum.add(&cost);
    }
    let mut runs: Vec<EffortRun> = groups
        .into_iter()
        .map(|((model, effort), (requests, sum, sample))| EffortRun {
            model: model.map(String::from),
            effort: effort.map(String::from),
            requests,
            cost: sum.cost(),
            estimate: sample.estimate(requests),
        })
        .collect();
    let estimate = all.estimate(requests.len() as u64);
    runs.sort_by_key(|r| Reverse(r.requests));
    let effective = control
        .unresolved
        .is_none()
        .then(|| control.effective.clone())
        .flatten();
    // No level can be weighed against another, so a known one has no verdict.
    let action = if control.unresolved.is_some() {
        Action::Advisory
    } else {
        Action::Keep
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
            profiles: line.ids.clone(),
            history_profiles: vec![],
            evidence: Evidence::Estimated,
            action,
            current: effective,
            recommended: None,
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
            margin: None,
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
        estimate,
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
        assert!(c.caps.is_empty());
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
        // Every cap below max is kept, so a higher target can name each one in its way.
        let caps: Vec<(&str, &str)> = c
            .caps
            .iter()
            .map(|c| (c.value.as_str(), c.owner.as_str()))
            .collect();
        assert_eq!(
            caps,
            [("high", "/work/.claude/settings.json:maxEffortLevel")]
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

    #[test]
    fn the_pick_takes_its_own_models_level_from_whichever_setting_decides_it() {
        // On the pick's model already, nothing more is resolved.
        let opus = Some("claude-opus-5-5");
        let c = resolve(json!({}), json!({}), json!({}), &[], opus).unwrap();
        assert_eq!(c.picked, None);
        // Off it, the pick's model takes its own entry's level, from another file, under its cap.
        let user = json!({
            "model": "claude-fable-5-2",
            "modelSettings": {"claude-fable-5-2": {"effortLevel": "xhigh"}},
        });
        let project = json!({
            "modelSettings": {"claude-opus-5-5": {"effortLevel": "high", "maxEffortLevel": "low"}},
        });
        let c = resolve(user.clone(), project, json!({}), &[], None).unwrap();
        assert_eq!(
            level(&c).2,
            format!("{USER}:modelSettings.claude-fable-5-2.effortLevel")
        );
        let picked = c.picked.expect("the pick is resolved off its model");
        assert_eq!(
            (picked.model.as_deref(), picked.model_owner.as_str()),
            (opus, format!("{USER}:model").as_str())
        );
        let owner = format!("{PROJECT}:modelSettings.claude-opus-5-5.effortLevel");
        assert_eq!(level(&picked), (Some("high"), Some("low"), owner.as_str()));
        assert_eq!(picked.caps.len(), 1);
        // With no setting for it, it takes its own default, which differs from Fable's level.
        let c = resolve(user, json!({}), json!({}), &[], None).unwrap();
        let default =
            format!("{USER}:modelSettings.claude-opus-5-5.effortLevel (the model's default)");
        assert_eq!(
            level(&c.picked.unwrap()),
            (Some("medium"), Some("medium"), default.as_str())
        );
        // The variable decides every model's level.
        let c = resolve(
            json!({}),
            json!({}),
            json!({}),
            &[("CLAUDE_CODE_EFFORT_LEVEL", "max")],
            Some("claude-sonnet-5-5"),
        )
        .unwrap();
        assert_eq!(
            level(&c.picked.unwrap()),
            (Some("max"), Some("max"), "CLAUDE_CODE_EFFORT_LEVEL")
        );
        // An unresolved default has no pick to resolve.
        let c = resolve(json!({}), json!({}), json!({}), &[], None).unwrap();
        assert!(c.unresolved.is_some() && c.picked.is_none());
        // An invalid level only the pick's own row holds leaves the pick unresolved, not the scan;
        // the same level on the current model still fails it.
        let dormant = json!({
            "model": "claude-fable-5-2",
            "modelSettings": {"claude-opus-5-5": {"effortLevel": "max"}},
        });
        let c = resolve(dormant, json!({}), json!({}), &[], None).unwrap();
        assert_eq!(c.unresolved, None);
        let picked = c.picked.expect("the pick is kept, unresolved");
        assert_eq!(
            (picked.level, picked.unresolved.as_deref()),
            (
                None,
                Some(
                    format!("invalid modelSettings effortLevel in {USER}, which only claude-opus-5-5 reads")
                        .as_str()
                )
            )
        );
        let live = json!({
            "model": "claude-opus-5-5",
            "modelSettings": {"claude-opus-5-5": {"effortLevel": "max"}},
        });
        assert!(resolve(live, json!({}), json!({}), &[], None).is_err());

        // Codex's level is the model's only when no layer sets one.
        let user_file = "/home/u/.codex/config.toml";
        let catalog: IndexMap<String, CodexModel> =
            [("gpt-6.1-sol", "low"), ("gpt-6.2-sol", "medium")]
                .into_iter()
                .map(|(m, d)| (m.to_string(), CodexModel { default: d.into() }))
                .collect();
        let unset = config(json!({"model": "gpt-6.1-sol"}));
        let set = config(json!({"model": "gpt-6.1-sol", "model_reasoning_effort": "xhigh"}));
        let picked = |user: &Map<String, Value>| {
            let scopes = [SettingsScope {
                config: user,
                file: user_file,
            }];
            let c = codex_effort(&scopes, user_file, Some(&catalog), None).unwrap();
            assert_eq!(c.model_owner, format!("{user_file}:model"));
            let picked = *c.picked.expect("the pick is resolved off its model");
            assert_eq!(picked.model.as_deref(), Some("gpt-6.2-sol"));
            assert_eq!(picked.model_owner, c.model_owner);
            (picked.level, picked.owner)
        };
        assert_eq!(
            picked(&unset),
            (
                Some("medium".into()),
                format!("{user_file}:model_reasoning_effort (the model's default)")
            )
        );
        assert_eq!(
            picked(&set),
            (
                Some("xhigh".into()),
                format!("{user_file}:model_reasoning_effort")
            )
        );
    }
}
