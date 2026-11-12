//! Resolve current settings files and environments into controls with explicit owners.

use crate::collections::{HashMap, IndexMap};

use serde_json::{Map, Value};

use crate::model::{
    BackgroundControl, BackgroundFeature, BashCapControl, CompactionControls, EventKind, Harness,
    IdleAttribution, IdleControl, Location, Millis, Scan, SpeedControl, TunedWindow, WindowControl,
};
use crate::safe::{ScanError, object};

/// A process environment, from variable name to value.
pub type Env = HashMap<String, String>;

/// One parsed settings file; scopes are listed lowest precedence first.
#[derive(Clone, Copy, Debug)]
pub struct SettingsScope<'a> {
    pub config: &'a Map<String, Value>,
    pub file: &'a str,
}

/// Settings environments merged in precedence order, with each effective key's owner.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScopedEnvironment {
    pub values: HashMap<String, Value>,
    pub owners: HashMap<String, String>,
}

impl ScopedEnvironment {
    /// Whether a variable is exactly this string; settings may hold other JSON types.
    fn is(&self, key: &str, text: &str) -> bool {
        matches!(self.values.get(key), Some(Value::String(s)) if s == text)
    }
}

const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// A settings error on the first line of the file that supplied the value.
fn invalid(message: impl Into<String>, file: &str) -> ScanError {
    ScanError::at(
        message,
        Location {
            file: file.into(),
            line: 1,
        },
    )
}

/// An object member, treating absence and non-objects as empty like `object(value)`.
pub(crate) fn members(value: Option<&Value>) -> &Map<String, Value> {
    object(value.unwrap_or(&Value::Null))
}

/// A number that JavaScript's `Number.isSafeInteger` accepts, including integral floats.
fn safe_integer(value: &Value) -> Option<i64> {
    let n = value.as_f64()?;
    (n.fract() == 0.0 && n.abs() <= MAX_SAFE_INTEGER).then_some(n as i64)
}

/// JavaScript's `String(value)`, where `None` is `undefined`.
pub(crate) fn js_string(value: Option<&Value>) -> String {
    match value {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => js_number(n.as_f64().unwrap_or(f64::NAN)),
        Some(Value::String(s)) => s.clone(),
        // Array holes and nulls join as empty strings.
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::Null => String::new(),
                other => js_string(Some(other)),
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

/// JavaScript number formatting: shortest round trip, exponents outside [1e-7, 1e21).
fn js_number(n: f64) -> String {
    if n == 0.0 {
        return "0".into();
    }
    if n.abs() >= 1e21 || n.abs() < 1e-6 {
        let text = format!("{n:e}");
        return match text.split_once('e') {
            Some((mantissa, exponent)) if !exponent.starts_with('-') => {
                format!("{mantissa}e+{exponent}")
            }
            _ => text,
        };
    }
    n.to_string()
}

/// Merge terminal environment with settings taking precedence, preserving each effective key's owner.
pub fn scoped_environment(scopes: &[SettingsScope], env: &Env) -> ScopedEnvironment {
    let mut result = ScopedEnvironment::default();
    for (key, value) in env {
        result
            .values
            .insert(key.clone(), Value::String(value.clone()));
        result.owners.insert(key.clone(), key.clone());
    }
    for scope in scopes {
        for (key, value) in members(scope.config.get("env")) {
            result.values.insert(key.clone(), value.clone());
            result
                .owners
                .insert(key.clone(), format!("{}:env.{key}", scope.file));
        }
    }
    result
}

/// Resolve native idle settings in precedence order, retaining invalid environment values as unknown.
pub fn idle_controls(scopes: &[SettingsScope], env: &Env) -> Result<IdleControl, ScanError> {
    let first = scopes.first().map_or("settings.json", |s| s.file);
    let mut value = true;
    let mut owner = format!("{first}:idleCompaction");
    let mut auto_compact = true;
    let mut threshold = Some(200_000);
    let mut threshold_owner = "CLAUDE_CODE_IDLE_COMPACT_MIN_TOKENS (default)".to_string();
    let environment = scoped_environment(scopes, env);
    for scope in scopes {
        for key in ["idleCompaction", "autoCompactEnabled"] {
            let Some(setting) = scope.config.get(key) else {
                continue;
            };
            let Value::Bool(setting) = *setting else {
                return Err(invalid(format!("invalid {key}"), scope.file));
            };
            if key == "idleCompaction" {
                value = setting;
                owner = format!("{}:{key}", scope.file);
            } else {
                auto_compact = setting;
            }
        }
    }
    if environment.is("DISABLE_AUTO_COMPACT", "1") {
        auto_compact = false;
    }
    let key = "CLAUDE_CODE_IDLE_COMPACT_MIN_TOKENS";
    if let Some(setting) = environment.values.get(key) {
        threshold_owner = environment.owners[key].clone();
        let text = js_string(Some(setting));
        threshold = if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) {
            text.parse::<f64>()
                .ok()
                .filter(|n| *n > 0.0 && *n <= MAX_SAFE_INTEGER)
                .map(|n| (n as i64).max(100_000))
        } else {
            None
        };
    }
    Ok(IdleControl {
        value: Some(value),
        owner,
        threshold,
        threshold_owner,
        auto_compact: Some(auto_compact),
        reason: if threshold.is_none() {
            "idle threshold environment value is invalid or unsupported"
        } else {
            "current native files and active-profile environment; unrecorded launch overrides remain outside observed ownership"
        }
        .into(),
        resolved: threshold.is_some(),
        ttl: None,
        compaction: None,
    })
}

/// A boolean environment value in the forms Claude Code accepts; None for anything else.
fn env_flag(value: &Value) -> Option<bool> {
    match js_string(Some(value)).trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// The last scope's boolean at a dotted key, with its file; an error for any other type.
fn scoped_bool<'a>(
    scopes: &[SettingsScope<'a>],
    key: &str,
) -> Result<Option<(bool, &'a str)>, ScanError> {
    let mut result = None;
    for scope in scopes {
        let mut parts = key.split('.');
        let mut row = parts.next().and_then(|part| scope.config.get(part));
        for part in parts {
            row = row.and_then(|r| r.as_object()?.get(part));
        }
        match row {
            None => {}
            Some(Value::Bool(b)) => result = Some((*b, scope.file)),
            Some(_) => return Err(invalid(format!("invalid {key}"), scope.file)),
        }
    }
    Ok(result)
}

/// Environment variables that can change background generation or its feature-flag gate.
pub(crate) const CLAUDE_BACKGROUND_ENV: [&str; 8] = [
    "CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION",
    "CLAUDE_CODE_ENABLE_AWAY_SUMMARY",
    "CLAUDE_CODE_GOAL_CHECKIN_MINUTES",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
    "DISABLE_TELEMETRY",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

/// Resolve Claude background controls in terminal precedence, including settings env over shell.
pub fn claude_background(
    scopes: &[SettingsScope],
    env: &Env,
) -> Result<Vec<BackgroundControl>, ScanError> {
    let first = scopes.first().map_or("settings.json", |s| s.file);
    let environment = scoped_environment(scopes, env);
    // These gates stop flag fetching, including nonempty "0" for the traffic switches.
    let flags = ![
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
        "DISABLE_TELEMETRY",
    ]
    .iter()
    .any(|key| {
        environment
            .values
            .get(*key)
            .is_some_and(|v| !js_string(Some(v)).is_empty())
    }) && ![
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
    ]
    .iter()
    .any(|key| environment.values.get(*key).and_then(env_flag) == Some(true));
    let environment_off = |variable: &str, off: &str| {
        let owner = environment.owners.get(variable);
        let file = scopes
            .iter()
            .find(|s| owner == Some(&format!("{}:env.{variable}", s.file)));
        match file {
            Some(scope) => (
                format!("\"env\": {{\"{variable}\": \"{off}\"}}"),
                Some(scope.file.to_string()),
            ),
            None => (format!("{variable}={off}"), None),
        }
    };
    let reason = "current terminal settings and active-profile environment applied to history; historical files, --settings, --setting-sources, /cd environment carry-over and hosted launch overrides are not recorded";
    let toggle = |feature, key: &str, variable: &str, off: &str| {
        let forced = environment.values.get(variable).and_then(env_flag);
        let (value, owner, file) = match forced {
            Some(value) => (value, environment.owners[variable].clone(), None),
            None => match scoped_bool(scopes, key)? {
                Some((value, file)) => (value, format!("{file}:{key}"), Some(file)),
                None => (true, format!("{first}:{key} (default)"), Some(first)),
            },
        };
        Ok::<_, ScanError>((
            BackgroundControl {
                feature,
                value: Some(value),
                reason: reason.into(),
                minutes: None,
                owner,
                off: file.map_or_else(
                    || environment_off(variable, off).0,
                    |_| format!("\"{key}\": false"),
                ),
                off_file: file
                    .map(String::from)
                    .or_else(|| environment_off(variable, off).1),
            },
            forced.is_some(),
        ))
    };
    let (mut suggestions, forced) = toggle(
        BackgroundFeature::PromptSuggestions,
        "promptSuggestionEnabled",
        "CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION",
        "false",
    )?;
    if !flags && !forced {
        suggestions.value = Some(false);
        suggestions.owner = format!("{} (off without feature flags)", suggestions.owner);
    }
    let (recaps, _) = toggle(
        BackgroundFeature::SessionRecaps,
        "awaySummaryEnabled",
        "CLAUDE_CODE_ENABLE_AWAY_SUMMARY",
        "0",
    )?;
    let variable = "CLAUDE_CODE_GOAL_CHECKIN_MINUTES";
    // Whole minutes up to a week in plain digits; Claude Code treats anything else as unset.
    let minutes = environment.values.get(variable).and_then(|v| {
        let text = js_string(Some(v));
        if text.is_empty() || text.len() > 5 || !text.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        text.parse::<i64>().ok().filter(|&n| n <= 10_080)
    });
    let goal = BackgroundControl {
        feature: BackgroundFeature::GoalCheckIns,
        value: Some(minutes != Some(0)),
        reason: reason.into(),
        minutes: Some(minutes.unwrap_or(30)),
        owner: match minutes {
            Some(_) => environment.owners[variable].clone(),
            None if environment.values.contains_key(variable) => {
                format!("{} (invalid; default)", environment.owners[variable])
            }
            None => format!("{variable} (default)"),
        },
        off: environment_off(variable, "0").0,
        off_file: environment_off(variable, "0").1,
    };
    Ok(vec![suggestions, recaps, goal])
}

/// Resolve Codex's automatic recaps, lowest-precedence scope first; on by default.
pub fn codex_background(scopes: &[SettingsScope]) -> Result<Vec<BackgroundControl>, ScanError> {
    let key = "tui.auto_recap";
    let user = scopes.last().map_or("config.toml", |s| s.file);
    let (value, file, owner) = match scoped_bool(scopes, key)? {
        Some((value, file)) => (value, file, format!("{file}:{key}")),
        None => (true, user, format!("{user}:{key} (default)")),
    };
    Ok(vec![BackgroundControl {
        feature: BackgroundFeature::AutoRecaps,
        value: Some(value),
        reason: "current config files applied to history; historical files and CLI/session overrides are not recorded".into(),
        minutes: None,
        owner,
        off: format!("{key} = false"),
        off_file: Some(file.into()),
    }])
}

/// A positive millisecond count as Claude Code parses it: a leading integer, or an integral exponent form.
fn timeout_millis(value: &Value) -> Option<Millis> {
    let text = js_string(Some(value));
    let text = text.trim();
    let parsed = if text.contains(['e', 'E']) {
        text.parse::<f64>()
            .ok()
            .filter(|n| n.fract() == 0.0 && n.abs() <= MAX_SAFE_INTEGER)
            .map(|n| n as i64)
    } else {
        let number = text.strip_prefix('+').unwrap_or(text);
        let end = number
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(number.len());
        number[..end].parse::<i64>().ok()
    };
    parsed.filter(|n| *n > 0)
}

/// Resolve Claude's foreground Bash ceiling, `max(BASH_MAX_TIMEOUT_MS, BASH_DEFAULT_TIMEOUT_MS)`.
///
/// Invalid or non-positive values fall back to the defaults, as Claude Code does.
pub fn bash_cap_control(scopes: &[SettingsScope], env: &Env) -> BashCapControl {
    let first = scopes.first().map_or("settings.json", |s| s.file);
    let environment = scoped_environment(scopes, env);
    let resolve = |key: &str, default: Millis| -> (Millis, String) {
        // An invalid value still owns the setting: it must be fixed where it is set.
        let owner = environment
            .owners
            .get(key)
            .cloned()
            .unwrap_or_else(|| format!("{first}:env.{key}"));
        let value = environment.values.get(key).and_then(timeout_millis);
        (value.unwrap_or(default), owner)
    };
    let (default_timeout, default_owner) = resolve("BASH_DEFAULT_TIMEOUT_MS", 120_000);
    let (maximum, owner) = resolve("BASH_MAX_TIMEOUT_MS", 600_000);
    BashCapControl {
        value: maximum.max(default_timeout),
        owner,
        default_timeout,
        default_owner,
    }
}

/// Documented minimum Claude settings and environment window.
pub const CLAUDE_WINDOW_MINIMUM: i64 = 100_000;
/// Documented maximum Claude settings and environment window.
pub const CLAUDE_WINDOW_MAXIMUM: i64 = 1_000_000;

/// The current files and environment that own a profile's compaction windows.
#[derive(Clone, Copy, Debug)]
pub struct CompactionSources<'a> {
    pub harness: Harness,
    pub user: &'a Map<String, Value>,
    pub managed: &'a Map<String, Value>,
    pub env: &'a Env,
    pub file: &'a str,
    pub managed_file: &'a str,
    /// Settings in precedence order; defaults to the user then managed file.
    pub scopes: Option<&'a [SettingsScope<'a>]>,
}

/// Parse `CLAUDE_CODE_AUTO_COMPACT_WINDOW`, such as `300k`, clamped to the documented bounds.
fn window_override(text: &str) -> Option<i64> {
    let (number, scale) = match text.as_bytes().last() {
        Some(b'k' | b'K') => (&text[..text.len() - 1], 1e3),
        Some(b'm' | b'M') => (&text[..text.len() - 1], 1e6),
        _ => (text, 1.0),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let valid = match number.split_once('.') {
        Some((whole, fraction)) => digits(whole) && digits(fraction),
        None => digits(number),
    };
    if !valid {
        return None;
    }
    let value = number.parse::<f64>().ok()? * scale;
    (value.is_finite() && value > 0.0)
        .then(|| (value.floor() as i64).clamp(CLAUDE_WINDOW_MINIMUM, CLAUDE_WINDOW_MAXIMUM))
}

/// Model keys that may name a per-model row, including bracketed `[1m]` variants.
fn model_key(key: &str) -> bool {
    (1..=160).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.:/@<>+[]-".contains(&b))
}

/// Resolve observed current files and environment, preserving each control's owner.
pub fn compaction_controls(sources: &CompactionSources) -> Result<CompactionControls, ScanError> {
    let CompactionSources {
        harness,
        user,
        managed,
        env,
        file,
        managed_file,
        scopes,
    } = *sources;
    let default_scopes = [
        SettingsScope { config: user, file },
        SettingsScope {
            config: managed,
            file: managed_file,
        },
    ];
    let scopes = scopes.unwrap_or(&default_scopes);
    let environment = scoped_environment(scopes, env);
    let mut models = IndexMap::default();
    if harness == Harness::Codex {
        let value = user.get("model_auto_compact_token_limit");
        if value.is_some_and(|v| safe_integer(v).is_none_or(|n| n <= 0)) {
            return Err(invalid("invalid compaction token limit", file));
        }
        let value = value.and_then(safe_integer);
        let supported = match user.get("model_auto_compact_token_limit_scope") {
            None => true,
            Some(scope) => scope.as_str() == Some("total"),
        };
        let ambiguous = !members(user.get("profiles")).is_empty();
        let reason = if !supported {
            "body-after-prefix or unknown threshold scope is not a total-context window"
        } else if ambiguous {
            "named Codex profiles exist; the active profile/CLI override is not established"
        } else if value.is_none() {
            "tuned model default is not established from current config"
        } else {
            "current explicit total-context limit; launch overrides are not recorded by config"
        };
        return Ok(CompactionControls {
            default: WindowControl {
                value,
                owner: format!("{file}:model_auto_compact_token_limit"),
                reason: reason.into(),
                applicable: value.is_some() && supported && !ambiguous,
                minimum_claude_patch: None,
                comparison_unknown: (!supported)
                    .then(|| "effective Codex threshold scope is unsupported".into()),
                unset: false,
                setting: None,
            },
            models,
            capped: false,
            declared: false,
            tuned: IndexMap::default(),
            contexts: IndexMap::default(),
        });
    }
    let env_key = "CLAUDE_CODE_AUTO_COMPACT_WINDOW";
    let raw = environment
        .values
        .get(env_key)
        .map(|value| js_string(Some(value)));
    let window = raw.as_deref().and_then(window_override);
    let suffix = window.is_some()
        && raw
            .as_deref()
            .is_some_and(|r| r.ends_with(['k', 'K', 'm', 'M']));
    let disabled = environment.is("DISABLE_AUTO_COMPACT", "1")
        || scopes
            .iter()
            .rev()
            .find_map(|s| s.config.get("autoCompactEnabled"))
            == Some(&Value::Bool(false));
    // The caller supplies the effective environment and its explicitly owned keys.
    let resolve = |model: Option<&str>| -> Result<WindowControl, ScanError> {
        let mut value: Option<&Value> = None;
        let mut owner = match model {
            Some(model) => format!("{file}:modelSettings.{model}.autoCompactWindow"),
            None => format!("{file}:autoCompactWindow"),
        };
        for scope in scopes {
            if let Some(setting) = scope.config.get("autoCompactWindow") {
                value = Some(setting);
                owner = format!("{}:autoCompactWindow", scope.file);
            }
            if let Some(model) = model {
                let row = members(members(scope.config.get("modelSettings")).get(model));
                if let Some(setting) = row.get("autoCompactWindow") {
                    value = Some(setting);
                    owner = format!("{}:modelSettings.{model}.autoCompactWindow", scope.file);
                }
            }
        }
        if let Some(setting) = value
            && setting.as_str() != Some("auto")
            && safe_integer(setting)
                .is_none_or(|n| !(CLAUDE_WINDOW_MINIMUM..=CLAUDE_WINDOW_MAXIMUM).contains(&n))
        {
            let source = if owner.starts_with(&format!("{managed_file}:")) {
                managed_file
            } else {
                file
            };
            return Err(invalid("invalid auto-compact window", source));
        }
        let mut value = value.and_then(safe_integer);
        if let Some(window) = window {
            value = Some(window);
            owner = environment
                .owners
                .get(env_key)
                .cloned()
                .unwrap_or_else(|| format!("{file}:env.{env_key}"));
        }
        let reason = if disabled {
            "automatic compaction is disabled; changing its window alone does not enable it"
        } else if window.is_some() {
            "current environment window override (documented clamp applied)"
        } else if value.is_some() {
            "current user/managed window; project, launch and per-agent overrides may take precedence"
        } else {
            "tuned model default is not established from current files"
        };
        Ok(WindowControl {
            value,
            owner,
            reason: reason.into(),
            applicable: value.is_some() && !disabled,
            minimum_claude_patch: Some(if suffix { 304 } else { 288 }),
            comparison_unknown: None,
            unset: value.is_none() && !disabled,
            setting: None,
        })
    };
    for scope in scopes {
        let settings = scope.config.get("modelSettings");
        if settings.is_some_and(|s| !s.is_object()) {
            return Err(invalid("invalid model settings", scope.file));
        }
        for (key, row) in members(settings) {
            if !model_key(key) || !row.is_object() {
                return Err(invalid("invalid per-model settings", scope.file));
            }
            models.insert(key.clone(), resolve(Some(key))?);
        }
    }
    Ok(CompactionControls {
        default: resolve(None)?,
        models,
        capped: environment.is("CLAUDE_CODE_DISABLE_1M_CONTEXT", "1"),
        // Only a positive token count declares a window; anything else leaves the cap in force.
        declared: environment
            .values
            .get("CLAUDE_CODE_MAX_CONTEXT_TOKENS")
            .map(|value| js_string(Some(value)))
            .is_some_and(|text| {
                !text.is_empty()
                    && text.bytes().all(|b| b.is_ascii_digit())
                    && text.parse::<u64>().is_ok_and(|n| n > 0)
            }),
        tuned: IndexMap::default(),
        contexts: IndexMap::default(),
    })
}

/// The family of a `claude-<family>-` model ID.
fn claude_family(model: &str) -> Option<&'static str> {
    let rest = model.strip_prefix("claude-")?;
    ["opus", "sonnet", "haiku", "fable", "mythos"]
        .into_iter()
        .find(|family| {
            rest.strip_prefix(family)
                .is_some_and(|tail| tail.starts_with('-'))
        })
}

/// Strip a trailing `-` and `digits` ASCII digits, such as a `-20261101` date.
fn strip_dated(text: &str, digits: usize) -> Option<&str> {
    let split = text.len().checked_sub(digits + 1)?;
    let (head, tail) = text.split_at_checked(split)?;
    let tail = tail.strip_prefix('-')?;
    tail.bytes().all(|b| b.is_ascii_digit()).then_some(head)
}

/// Why a capped window that applies to every model stays advisory.
pub const CAPPED_SHARED: &str =
    "the window applies to every model, so no setting changes this model's capped window alone";

/// Exact model settings win; known dated Claude IDs share their undated model row.
pub fn window_control(compaction: &CompactionControls, model: Option<&str>) -> WindowControl {
    let models = &compaction.models;
    let model = model.unwrap_or("");
    let canonical = strip_dated(model, 8).unwrap_or(model);
    let dated_family = claude_family(canonical).is_some_and(|family| {
        canonical["claude-".len() + family.len() + 1..]
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_digit())
    });
    let current = match models.get(model) {
        Some(row) if !model.is_empty() => row,
        _ => match models.get(canonical) {
            Some(row) if dated_family => row,
            _ => &compaction.default,
        },
    };
    let aliases = match claude_family(model) {
        Some(family) => vec![
            family.to_string(),
            format!("{family}[1m]"),
            format!("{canonical}[1m]"),
        ],
        None => vec![],
    };
    if aliases.iter().any(|alias| {
        models
            .get(alias)
            .is_some_and(|row| row.owner != current.owner)
    }) {
        return WindowControl {
            value: None,
            applicable: false,
            reason: "model alias or [1m] setting may own the current window; effective alias selection is unresolved".into(),
            ..current.clone()
        };
    }
    let mut control = current.clone();
    if control.unset
        && let Some(tuned) = compaction.tuned.get(canonical)
    {
        control.reason = tuned.reason.clone();
        if let Some(value) = tuned.value {
            control.value = Some(value);
            control.applicable = true;
            // Tuned windows differ by model, so only the model's own row changes one alone.
            if let Some(file) = control.owner.strip_suffix(":autoCompactWindow") {
                control.owner = format!("{file}:modelSettings.{canonical}.autoCompactWindow");
            }
        }
    } else if let (Some(setting), Some(&context)) =
        (control.value, compaction.contexts.get(canonical))
        && setting > context
    {
        control.setting = Some(setting);
        control.value = Some(context);
        let mut reason = format!(
            "Claude Code caps this {setting}-token window at the model's {context}-token context window"
        );
        // The cap differs by model, so only the model's own row changes its window alone; a
        // variable or T3 window above every row would change other models too.
        if let Some(file) = control.owner.strip_suffix(":autoCompactWindow") {
            control.owner = format!("{file}:modelSettings.{canonical}.autoCompactWindow");
        } else if !control.owner.contains(":modelSettings.") {
            control.owner = format!("{} (capped at {context})", control.owner);
            control.applicable = false;
            reason += &format!(" and {CAPPED_SHARED}");
        }
        control.reason = format!("{reason}; {}", control.reason);
    }
    control
}

/// Claude Code's documented window when none is set, checked 2026-11-11 at
/// <https://code.claude.com/docs/en/model-config#default-auto-compact-thresholds>.
const TUNED_MILLION: i64 = 967_000;
const TUNED_STANDARD: i64 = 200_000;

/// Whether Claude Code documents an undated model as running a native 1M window, or a 200k one;
/// None for an ID the documented table does not cover.
fn native_million(model: &str) -> Option<bool> {
    let family = claude_family(model)?;
    let mut parts = model["claude-".len() + family.len() + 1..].split('-');
    let number = |part: &str| {
        (!part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
            .then(|| part.parse::<u32>().ok())
            .flatten()
    };
    let major = number(parts.next()?)?;
    let minor = parts.next().map_or(Some(0), number)?;
    if parts.next().is_some() {
        return None;
    }
    let version = (major, minor);
    match family {
        "fable" => Some(true),
        "sonnet" => Some(version >= (5, 0)),
        "opus" => Some(version >= (4, 7)),
        "haiku" if version == (5, 5) => Some(true),
        "haiku" if version < (5, 5) => Some(false),
        _ => None,
    }
}

/// The model's context window when Claude Code holds it below the 1M maximum: 200k under the 1M
/// opt-out, unless only a declared window sizes the ID, and on a model documented at 200k
/// unless a recorded request larger than that shows its `[1m]` variant ran.
fn context_window(compaction: &CompactionControls, model: &str, largest: i64) -> Option<i64> {
    let documented = native_million(model);
    let held = if compaction.capped {
        documented.is_some() || !compaction.declared
    } else {
        documented == Some(false) && largest <= TUNED_STANDARD
    };
    held.then_some(TUNED_STANDARD)
}

/// Fill each Claude profile's tuned window for every model it used: the trigger of its latest
/// automatic compaction on that model, else Claude Code's documented default. Also fill each
/// model's context window, which caps an explicit window, in the profile and every directory.
pub fn tune_windows(scan: &mut Scan) {
    let Scan {
        profiles,
        requests,
        events,
        cache_context,
        ..
    } = scan;
    let undated = |model: &str| strip_dated(model, 8).unwrap_or(model).to_string();
    for profile in profiles.iter_mut().filter(|p| p.harness == Harness::Claude) {
        let mut largest: IndexMap<String, i64> = IndexMap::default();
        for r in cache_context.requests.iter().chain(requests.iter()) {
            if !r.profiles.contains(&profile.id) {
                continue;
            }
            for p in &r.parts {
                if let Some(model) = &p.model {
                    let size = largest.entry(undated(model)).or_default();
                    *size = (*size).max(p.usage.input.unwrap_or(0));
                }
            }
        }
        let mut observed: IndexMap<String, (Millis, i64)> = IndexMap::default();
        for e in cache_context.boundaries.iter().chain(events.iter()) {
            if let (
                EventKind::Compaction,
                Some("auto"),
                IdleAttribution::Unknown,
                Some(model),
                Some(tokens),
            ) = (
                e.kind,
                e.trigger.as_deref(),
                e.idle_attribution,
                &e.model,
                e.pre_tokens,
            ) && e.profiles.contains(&profile.id)
                && (CLAUDE_WINDOW_MINIMUM..=CLAUDE_WINDOW_MAXIMUM).contains(&tokens)
            {
                let latest = observed.entry(undated(model)).or_insert((e.time, tokens));
                if e.time >= latest.0 {
                    *latest = (e.time, tokens);
                }
            }
        }
        // Directories share the profile's history, so its largest requests stand for each.
        let directories = profile
            .idle_directories
            .iter_mut()
            .flat_map(|d| d.values_mut());
        for compaction in profile
            .idle
            .iter_mut()
            .chain(directories)
            .filter_map(|c| c.compaction.as_mut())
            .chain([&mut profile.compaction])
        {
            compaction.contexts = largest
                .iter()
                .filter_map(|(model, &size)| {
                    Some((model.clone(), context_window(compaction, model, size)?))
                })
                .collect();
        }
        let capped = profile.compaction.capped;
        for (model, size) in largest {
            let documented = native_million(&model);
            let context = profile.compaction.contexts.get(&model);
            let (value, reason) = match (observed.get(&model), documented) {
                // The cap holds even an explicit window, so a trigger from before it cannot lift
                // it; only a declared window can size an ID Claude Code does not recognize.
                _ if capped && context.is_some() => (
                    Some(TUNED_STANDARD),
                    "no setting names a window; CLAUDE_CODE_DISABLE_1M_CONTEXT=1 holds this model at the documented 200k".into(),
                ),
                (Some(&(_, tokens)), _) => (
                    Some(tokens),
                    format!("no setting names a window; the latest automatic compaction on this model triggered at {tokens} tokens"),
                ),
                (None, Some(true)) => (
                    Some(TUNED_MILLION),
                    "no setting names a window and no automatic compaction on this model was recorded; assumed Claude Code's documented default of about 967k tokens for native-1M models, which the server can retune".into(),
                ),
                (None, Some(false)) if size <= TUNED_STANDARD => (
                    Some(TUNED_STANDARD),
                    "no setting names a window and no automatic compaction on this model was recorded; assumed Claude Code's documented 200k default for this model".into(),
                ),
                (None, Some(false)) => (
                    None,
                    "no setting names a window and a recorded request exceeded this model's documented 200k default, so a [1m] variant may set it; its tuned default is not established".into(),
                ),
                (None, None) => (
                    None,
                    "no setting names a window and Claude Code documents no default for this model ID; its tuned default is not established".into(),
                ),
            };
            profile
                .compaction
                .tuned
                .insert(model, TunedWindow { value, reason });
        }
    }
}

/// Whether a current documented Claude model supports fast speed, without resolving custom aliases.
pub fn claude_fast_support(model: Option<&str>) -> Option<bool> {
    let model = model?;
    let base = model.strip_suffix("[1m]").unwrap_or(model);
    let fast = |rest: &str| ["5-5", "5", "4-8"].contains(&rest);
    if base == "opus"
        || base
            .strip_prefix("claude-opus-")
            .is_some_and(|rest| fast(rest) || strip_dated(rest, 8).is_some_and(fast))
    {
        return Some(true);
    }
    let versioned = |rest: &str| {
        rest.split('-')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
    };
    let standard = ["sonnet", "haiku"].contains(&base)
        || ["opus", "sonnet", "haiku"].into_iter().any(|family| {
            base.strip_prefix("claude-")
                .and_then(|rest| rest.strip_prefix(family))
                .and_then(|rest| rest.strip_prefix('-'))
                .is_some_and(versioned)
        });
    standard.then_some(false)
}

/// Whether a Codex model is documented for a premium tier, optionally with a `-YYYY-MM-DD` snapshot.
fn codex_premium_model(model: &str, ultrafast: bool) -> bool {
    let Some(rest) = model.strip_prefix("gpt-") else {
        return false;
    };
    let known: &[&str] = if ultrafast {
        &["6-astra", "6.1-sol"]
    } else {
        &[
            "6-sol",
            "6-astra",
            "6-luna",
            "6.1-sol",
            "6.1-terra",
            "5.6",
            "5.6-sol",
            "5.6-terra",
            "5.6-luna",
            "5.4",
            "5.5",
            "5.4-mini",
        ]
    };
    let snapshot = strip_dated(rest, 2)
        .and_then(|day| strip_dated(day, 2))
        .and_then(|month| strip_dated(month, 4));
    known.contains(&rest) || snapshot.is_some_and(|base| known.contains(&base))
}

/// The effective value of a dotted settings key and the scope that supplied it.
struct Effective<'a> {
    value: Option<&'a Value>,
    owner: String,
    file: &'a str,
}

/// Resolve current speed controls using documented defaults and explicit disabling gates.
pub fn speed_controls(
    harness: Harness,
    scopes: &[SettingsScope],
    env: &Env,
) -> Result<SpeedControl, ScanError> {
    let first = scopes.first().map_or("settings", |s| s.file);
    let effective = |key: &str| -> Effective {
        let mut result = Effective {
            value: None,
            owner: format!("{first}:{key} (documented default)"),
            file: first,
        };
        for scope in scopes {
            let mut parts = key.split('.');
            let mut row = parts.next().and_then(|part| scope.config.get(part));
            for part in parts {
                row = row.and_then(|r| object(r).get(part));
            }
            if let Some(row) = row {
                result = Effective {
                    value: Some(row),
                    owner: format!("{}:{key}", scope.file),
                    file: scope.file,
                };
            }
        }
        result
    };
    let boolean = |key: &str| -> Result<Effective, ScanError> {
        let row = effective(key);
        if row.value.is_some_and(|v| !v.is_boolean()) {
            return Err(invalid(format!("invalid {key}"), row.file));
        }
        Ok(row)
    };
    let result = |value: Option<&str>, owner: &str, reason: &str| SpeedControl {
        value: value.map(Into::into),
        owner: owner.into(),
        replacement: match harness {
            Harness::Claude => "fastMode: false; /fast off for an active session",
            Harness::Codex => "service_tier = \"default\"; /fast off for an active session",
        }
        .into(),
        resolved: value.is_some(),
        reason: reason.into(),
    };
    if harness == Harness::Codex {
        for scope in scopes {
            if scope.config.get("features").is_some_and(|f| !f.is_object()) {
                return Err(invalid("invalid speed features", scope.file));
            }
        }
    }
    let model = effective("model");
    if harness == Harness::Claude {
        let fast = boolean("fastMode")?;
        let opt = boolean("fastModePerSessionOptIn")?;
        let env = scoped_environment(scopes, env);
        if env.is("CLAUDE_CODE_DISABLE_FAST_MODE", "1") {
            return Ok(result(
                Some("standard"),
                &env.owners["CLAUDE_CODE_DISABLE_FAST_MODE"],
                "fast mode disabled by environment",
            ));
        }
        // The managed layer is last, including an enforced false value.
        if fast.value != Some(&Value::Bool(true)) {
            return Ok(result(
                Some("standard"),
                &fast.owner,
                "fast mode off (unset defaults off)",
            ));
        }
        if opt.value == Some(&Value::Bool(true)) {
            return Ok(result(
                Some("standard"),
                &opt.owner,
                "per-session opt-in disables the saved preference at startup; no current session opt-in observed",
            ));
        }
        let gateway = env
            .values
            .get("ANTHROPIC_BASE_URL")
            .is_some_and(|url| url.as_str() != Some("https://api.anthropic.com"));
        if gateway
            || [
                "CLAUDE_CODE_USE_BEDROCK",
                "CLAUDE_CODE_USE_VERTEX",
                "CLAUDE_CODE_USE_FOUNDRY",
            ]
            .iter()
            .any(|key| env.is(key, "1"))
        {
            return Ok(result(
                None,
                &fast.owner,
                "premium availability/effective ownership through this provider or gateway is unresolved",
            ));
        }
        let environment_model = env.values.get("ANTHROPIC_MODEL");
        // `??` also skips a null settings value.
        let selected = environment_model
            .filter(|v| !v.is_null())
            .or(model.value)
            .and_then(Value::as_str);
        return Ok(match claude_fast_support(selected) {
            Some(true) => result(
                Some("fast"),
                &fast.owner,
                "current supported Opus fast preference; account availability and unrecorded launch/session overrides are unknown",
            ),
            Some(false) => result(
                Some("standard"),
                if environment_model.is_some() {
                    &env.owners["ANTHROPIC_MODEL"]
                } else {
                    &model.owner
                },
                "current model does not support fast mode; saved preference is inactive",
            ),
            None => result(
                None,
                &fast.owner,
                "current model alias or fast-mode support is unresolved",
            ),
        });
    }
    let tier = effective("service_tier");
    if tier.value.is_some_and(|v| !v.is_string()) {
        return Err(invalid("invalid service_tier", tier.file));
    }
    let fast = boolean("features.fast_mode")?;
    let ultra = boolean("features.ultrafast_mode")?;
    let value = tier.value.and_then(Value::as_str);
    match value {
        None | Some("default" | "standard") => Ok(result(
            Some("standard"),
            &tier.owner,
            "no premium speed preference; unrecorded API project/session overrides remain unknown",
        )),
        Some(premium @ ("fast" | "priority" | "ultrafast")) => {
            let gate = if premium == "ultrafast" {
                &ultra
            } else {
                &fast
            };
            if gate.value == Some(&Value::Bool(false)) {
                return Ok(result(
                    Some("standard"),
                    &gate.owner,
                    "saved premium tier is disabled by its feature gate",
                ));
            }
            let provider = effective("model_provider");
            if provider.value.is_some_and(|p| p.as_str() != Some("openai")) {
                return Ok(result(
                    None,
                    &tier.owner,
                    "custom provider speed support and ownership are unresolved",
                ));
            }
            let supported = model
                .value
                .and_then(Value::as_str)
                .is_some_and(|m| codex_premium_model(m, premium == "ultrafast"));
            Ok(result(
                supported.then_some(premium),
                &tier.owner,
                if supported {
                    "current premium preference; independent feature gates default on in current Codex; account access and unrecorded session overrides are unknown"
                } else {
                    "saved premium preference has unresolved current model support"
                },
            ))
        }
        Some(_) => Ok(result(
            None,
            &tier.owner,
            "auto, flex, scale or unrecognized tier is not an established standard/premium speed control",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            _ => panic!("not an object"),
        }
    }

    fn env(pairs: &[(&str, &str)]) -> Env {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn claude_sources<'a>(
        user: &'a Map<String, Value>,
        managed: &'a Map<String, Value>,
        env: &'a Env,
    ) -> CompactionSources<'a> {
        CompactionSources {
            harness: Harness::Claude,
            user,
            managed,
            env,
            file: "user.json",
            managed_file: "managed.json",
            scopes: None,
        }
    }

    fn speed(
        harness: Harness,
        config: Value,
        extra: Value,
        environment: &[(&str, &str)],
    ) -> Result<SpeedControl, ScanError> {
        let (config, extra) = (map(config), map(extra));
        speed_controls(
            harness,
            &[
                SettingsScope {
                    config: &config,
                    file: "user",
                },
                SettingsScope {
                    config: &extra,
                    file: "managed",
                },
            ],
            &env(environment),
        )
    }

    #[test]
    fn javascript_strings_match_number_formatting() {
        assert_eq!(js_string(Some(&json!(150000.0))), "150000");
        assert_eq!(js_string(Some(&json!(1e21))), "1e+21");
        assert_eq!(js_string(Some(&json!(1.5e-7))), "1.5e-7");
        assert_eq!(js_string(Some(&json!(2.5))), "2.5");
        assert_eq!(js_string(Some(&json!(["fast"]))), "fast");
        assert_eq!(js_string(Some(&json!([1, null, "a"]))), "1,,a");
        assert_eq!(js_string(Some(&json!({}))), "[object Object]");
        assert_eq!(js_string(None), "undefined");
    }

    #[test]
    fn idle_thresholds_clamp_and_keep_invalid_values_unknown() {
        let user = map(json!({ "env": { "CLAUDE_CODE_IDLE_COMPACT_MIN_TOKENS": 5000 } }));
        let scopes = [SettingsScope {
            config: &user,
            file: "user.json",
        }];
        let c = idle_controls(&scopes, &Env::default()).unwrap();
        assert_eq!(c.threshold, Some(100_000));
        assert_eq!(
            c.threshold_owner,
            "user.json:env.CLAUDE_CODE_IDLE_COMPACT_MIN_TOKENS"
        );
        assert_eq!(c.owner, "user.json:idleCompaction");
        let c = idle_controls(
            &scopes,
            &env(&[("CLAUDE_CODE_IDLE_COMPACT_MIN_TOKENS", "2e5")]),
        )
        .unwrap();
        assert_eq!(c.threshold, Some(100_000));
        assert_eq!(
            c.threshold_owner,
            "user.json:env.CLAUDE_CODE_IDLE_COMPACT_MIN_TOKENS"
        );
        let empty = Map::new();
        let c = idle_controls(
            &[SettingsScope {
                config: &empty,
                file: "user.json",
            }],
            &env(&[("CLAUDE_CODE_IDLE_COMPACT_MIN_TOKENS", "2e5")]),
        )
        .unwrap();
        assert_eq!(c.threshold, None);
        assert!(!c.resolved);
        assert_eq!(c.threshold_owner, "CLAUDE_CODE_IDLE_COMPACT_MIN_TOKENS");
        let bad = map(json!({ "idleCompaction": "yes" }));
        let error = idle_controls(
            &[SettingsScope {
                config: &bad,
                file: "bad.json",
            }],
            &Env::default(),
        )
        .unwrap_err();
        assert_eq!(error.message, "invalid idleCompaction");
        assert_eq!(&*error.location.unwrap().file, "bad.json");
    }

    #[test]
    fn background_controls_follow_settings_env_then_inherited_env_then_settings_keys() {
        let user = map(json!({ "promptSuggestionEnabled": false, "awaySummaryEnabled": false }));
        let managed = map(json!({
            "awaySummaryEnabled": true,
            "env": { "CLAUDE_CODE_GOAL_CHECKIN_MINUTES": "0" }
        }));
        let scopes = [
            SettingsScope {
                config: &user,
                file: "user.json",
            },
            SettingsScope {
                config: &managed,
                file: "managed.json",
            },
        ];
        let c = claude_background(&scopes, &Env::default()).unwrap();
        assert_eq!(
            (c[0].value, c[0].owner.as_str(), c[0].off.as_str()),
            (
                Some(false),
                "user.json:promptSuggestionEnabled",
                "\"promptSuggestionEnabled\": false"
            )
        );
        assert_eq!(
            (c[1].value, c[1].owner.as_str()),
            (Some(true), "managed.json:awaySummaryEnabled")
        );
        // Turning it off has to happen where the effective value is set.
        assert_eq!(c[1].off_file.as_deref(), Some("managed.json"));
        assert_eq!((c[2].value, c[2].minutes), (Some(false), Some(0)));
        // A shell variable overrides its paired setting, while managed env replaces the shell.
        let shell = env(&[
            ("CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION", "true"),
            ("CLAUDE_CODE_ENABLE_AWAY_SUMMARY", "maybe"),
            ("CLAUDE_CODE_GOAL_CHECKIN_MINUTES", "1.5"),
        ]);
        let c = claude_background(&scopes, &shell).unwrap();
        assert_eq!(
            (c[0].value, c[0].off.as_str()),
            (Some(true), "CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION=false")
        );
        assert_eq!(c[0].off_file, None);
        assert_eq!(c[1].owner, "managed.json:awaySummaryEnabled");
        assert_eq!((c[2].value, c[2].minutes), (Some(false), Some(0)));
    }

    #[test]
    fn suggestions_stay_off_without_feature_flags_unless_forced() {
        let empty = Map::new();
        let scopes = [SettingsScope {
            config: &empty,
            file: "user.json",
        }];
        let on = |shell: &[(&str, &str)], flags: bool| {
            let mut environment = env(shell);
            if !flags {
                environment.insert("CLAUDE_CODE_USE_VERTEX".into(), "1".into());
            }
            claude_background(&scopes, &environment).unwrap()[0].value
        };
        assert_eq!(on(&[], true), Some(true));
        assert_eq!(on(&[], false), Some(false));
        assert_eq!(
            on(&[("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "0")], true),
            Some(false)
        );
        assert_eq!(
            on(&[("CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION", "1")], false),
            Some(true)
        );
        assert_eq!(
            on(&[("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "")], true),
            Some(true)
        );
        assert_eq!(on(&[("DISABLE_TELEMETRY", "1")], true), Some(false));
        let project = map(json!({ "env": { "CLAUDE_CODE_USE_VERTEX": "1" } }));
        let scoped = [SettingsScope {
            config: &project,
            file: "project.json",
        }];
        assert_eq!(
            claude_background(&scoped, &Env::default()).unwrap()[0].value,
            Some(false)
        );
        let codex = map(json!({ "tui": { "auto_recap": false } }));
        let c = codex_background(&[SettingsScope {
            config: &codex,
            file: "config.toml",
        }])
        .unwrap();
        assert_eq!(
            (c[0].value, c[0].owner.as_str()),
            (Some(false), "config.toml:tui.auto_recap")
        );
        let bad = map(json!({ "tui": { "auto_recap": "no" } }));
        let error = codex_background(&[SettingsScope {
            config: &bad,
            file: "config.toml",
        }])
        .unwrap_err();
        assert_eq!(error.message, "invalid tui.auto_recap");
    }

    // Ported from compaction.test.ts: "effective per-model controls honor managed scope, environment ownership and dated IDs".
    #[test]
    fn per_model_controls_honor_managed_scope_environment_and_dated_ids() {
        let user = map(json!({
            "autoCompactWindow": 600000,
            "modelSettings": { "claude-opus-5-5": { "autoCompactWindow": 400000 } },
            "env": { "CLAUDE_CODE_AUTO_COMPACT_WINDOW": "300k" },
        }));
        let empty = Map::new();
        let none = Env::default();
        let p = compaction_controls(&claude_sources(&user, &empty, &none)).unwrap();
        assert_eq!(
            window_control(&p, Some("claude-opus-5-5-20261101")).value,
            Some(300_000)
        );
        let current = window_control(&p, Some("claude-opus-5-5"));
        assert_eq!(
            current.owner,
            "user.json:env.CLAUDE_CODE_AUTO_COMPACT_WINDOW"
        );
        assert_eq!(current.minimum_claude_patch, Some(304));

        let mut quiet = user.clone();
        quiet.insert("env".into(), json!({}));
        let managed = map(json!({ "autoCompactWindow": 500000 }));
        let p = compaction_controls(&claude_sources(&quiet, &managed, &none)).unwrap();
        let current = window_control(&p, Some("claude-opus-5-5"));
        assert_eq!(current.value, Some(500_000));
        assert_eq!(current.owner, "managed.json:autoCompactWindow");

        let shell = env(&[("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "200000")]);
        let p = compaction_controls(&claude_sources(&user, &empty, &shell)).unwrap();
        let current = window_control(&p, Some("claude-opus-5-5"));
        assert_eq!(
            current.owner,
            "user.json:env.CLAUDE_CODE_AUTO_COMPACT_WINDOW"
        );
        assert_eq!(current.minimum_claude_patch, Some(304));

        let disabled = map(json!({ "autoCompactEnabled": false }));
        let p = compaction_controls(&claude_sources(&user, &disabled, &none)).unwrap();
        assert!(!window_control(&p, Some("claude-opus-5-5")).applicable);

        let negative = map(json!({ "autoCompactWindow": -1 }));
        let error = compaction_controls(&claude_sources(&negative, &empty, &none)).unwrap_err();
        assert_eq!(error.message, "invalid auto-compact window");
        let error = compaction_controls(&claude_sources(&empty, &negative, &none)).unwrap_err();
        assert_eq!(&*error.location.unwrap().file, "managed.json");
    }

    // Ported from compaction.test.ts: "bracketed Claude model controls scan safely and unresolved aliases stay advisory".
    #[test]
    fn bracketed_model_controls_and_unresolved_aliases() {
        let user = map(json!({
            "autoCompactWindow": 600000,
            "modelSettings": {
                "opus[1m]": { "autoCompactWindow": 400000 },
                "claude-opus-4-6[1m]": { "autoCompactWindow": 300000 },
            },
        }));
        let empty = Map::new();
        let none = Env::default();
        let p = compaction_controls(&claude_sources(&user, &empty, &none)).unwrap();
        assert_eq!(window_control(&p, Some("opus[1m]")).value, Some(400_000));
        let aliased = window_control(&p, Some("claude-opus-4-6"));
        assert_eq!(aliased.value, None);
        assert!(!aliased.applicable);
        assert!(aliased.reason.contains("alias"));
        assert_eq!(
            p.models.keys().collect::<Vec<_>>(),
            ["opus[1m]", "claude-opus-4-6[1m]"]
        );

        let managed = map(json!({ "autoCompactWindow": 500000 }));
        let p = compaction_controls(&claude_sources(&user, &managed, &none)).unwrap();
        let current = window_control(&p, Some("claude-opus-4-6"));
        assert_eq!(current.value, Some(500_000));
        assert_eq!(current.owner, "managed.json:autoCompactWindow");

        let null_row = map(json!({ "modelSettings": { "opus[1m]": null } }));
        let error = compaction_controls(&claude_sources(&null_row, &empty, &none)).unwrap_err();
        assert_eq!(error.message, "invalid per-model settings");
        let bad_key = map(json!({ "modelSettings": { "has space": {} } }));
        assert!(compaction_controls(&claude_sources(&bad_key, &empty, &none)).is_err());
        let not_object = map(json!({ "modelSettings": [] }));
        let error = compaction_controls(&claude_sources(&not_object, &empty, &none)).unwrap_err();
        assert_eq!(error.message, "invalid model settings");
    }

    #[test]
    fn codex_limits_require_total_scope_and_a_known_profile() {
        let empty = Map::new();
        let none = Env::default();
        let codex = |user: &Map<String, Value>| {
            compaction_controls(&CompactionSources {
                harness: Harness::Codex,
                user,
                managed: &empty,
                env: &none,
                file: "config.toml",
                managed_file: "managed.json",
                scopes: None,
            })
        };
        let limit = map(json!({ "model_auto_compact_token_limit": 240000.0 }));
        let c = codex(&limit).unwrap().default;
        assert_eq!(c.value, Some(240_000));
        assert!(c.applicable);
        assert_eq!(c.minimum_claude_patch, None);
        let scoped = map(json!({
            "model_auto_compact_token_limit": 240000,
            "model_auto_compact_token_limit_scope": "body_after_prefix",
        }));
        let c = codex(&scoped).unwrap().default;
        assert!(!c.applicable);
        assert_eq!(
            c.comparison_unknown.as_deref(),
            Some("effective Codex threshold scope is unsupported")
        );
        let named = map(json!({
            "model_auto_compact_token_limit": 240000,
            "profiles": { "work": {} },
        }));
        assert!(!codex(&named).unwrap().default.applicable);
        let bad = map(json!({ "model_auto_compact_token_limit": "bad" }));
        assert_eq!(
            codex(&bad).unwrap_err().message,
            "invalid compaction token limit"
        );
    }

    #[test]
    fn environment_windows_parse_suffixes_and_clamp() {
        assert_eq!(window_override("300k"), Some(300_000));
        assert_eq!(window_override("1.5M"), Some(1_000_000));
        assert_eq!(window_override("0.2m"), Some(200_000));
        assert_eq!(window_override("5"), Some(100_000));
        assert_eq!(window_override("0"), None);
        assert_eq!(window_override("1e5"), None);
        assert_eq!(window_override(".5k"), None);
        assert_eq!(window_override("k"), None);
    }

    #[test]
    fn bash_cap_follows_claude_code_parsing_and_precedence() {
        let resolve = |config: Value, pairs: &[(&str, &str)]| {
            let config = map(config);
            let scopes = [SettingsScope {
                config: &config,
                file: "user.json",
            }];
            bash_cap_control(&scopes, &env(pairs))
        };
        let defaults = resolve(json!({}), &[]);
        assert_eq!(defaults.value, 600_000);
        assert_eq!(defaults.owner, "user.json:env.BASH_MAX_TIMEOUT_MS");
        assert_eq!(defaults.default_timeout, 120_000);
        let settings = json!({ "env": { "BASH_MAX_TIMEOUT_MS": 3_300_000 } });
        assert_eq!(resolve(settings.clone(), &[]).value, 3_300_000);
        // Terminal settings env wins over the inherited shell.
        let shell = resolve(settings, &[("BASH_MAX_TIMEOUT_MS", "200000")]);
        assert_eq!(
            (shell.value, shell.owner.as_str()),
            (3_300_000, "user.json:env.BASH_MAX_TIMEOUT_MS")
        );
        for (text, value) in [
            ("3300000ms", 3_300_000),
            (" 3.3e6 ", 3_300_000),
            ("0", 600_000),
            ("-5", 600_000),
            ("soon", 600_000),
        ] {
            let control = resolve(json!({}), &[("BASH_MAX_TIMEOUT_MS", text)]);
            assert_eq!(control.value, value, "{text}");
            assert_eq!(control.owner, "BASH_MAX_TIMEOUT_MS", "{text}");
        }
        // A larger default timeout raises the ceiling past the maximum.
        let control = resolve(
            json!({}),
            &[
                ("BASH_MAX_TIMEOUT_MS", "200000"),
                ("BASH_DEFAULT_TIMEOUT_MS", "900000"),
            ],
        );
        assert_eq!(control.value, 900_000);
    }

    #[test]
    fn fast_support_follows_documented_models() {
        for model in [
            "opus",
            "opus[1m]",
            "claude-opus-5-5",
            "claude-opus-4-8-20261101[1m]",
        ] {
            assert_eq!(claude_fast_support(Some(model)), Some(true), "{model}");
        }
        for model in ["sonnet", "haiku[1m]", "claude-opus-4-6", "claude-sonnet-5"] {
            assert_eq!(claude_fast_support(Some(model)), Some(false), "{model}");
        }
        for model in ["custom", "claude-opus-", "claude-opus-4--6", "opus[1m][1m]"] {
            assert_eq!(claude_fast_support(Some(model)), None, "{model}");
        }
        assert_eq!(claude_fast_support(None), None);
        assert!(codex_premium_model("gpt-6.1-sol-2026-01-02", false));
        assert!(codex_premium_model("gpt-5.4-mini", false));
        assert!(!codex_premium_model("gpt-6-luna", true));
        assert!(!codex_premium_model("gpt-6.1-sol-2026-1-02", false));
    }

    // Ported from fast.test.ts: "Claude disabling controls and unsupported models suppress saved fast preferences, with exact owners".
    #[test]
    fn claude_disabling_controls_suppress_saved_fast_preferences() {
        let c = json!({ "model": "claude-opus-5-5", "fastMode": true });
        let claude = Harness::Claude;
        assert_eq!(
            speed(claude, c.clone(), json!({}), &[])
                .unwrap()
                .value
                .as_deref(),
            Some("fast")
        );
        let cases = [
            (json!({ "fastMode": false }), vec![], "managed:fastMode"),
            (
                json!({ "fastModePerSessionOptIn": true }),
                vec![],
                "managed:fastModePerSessionOptIn",
            ),
            (
                json!({}),
                vec![("CLAUDE_CODE_DISABLE_FAST_MODE", "1")],
                "CLAUDE_CODE_DISABLE_FAST_MODE",
            ),
            (
                json!({ "env": { "CLAUDE_CODE_DISABLE_FAST_MODE": "1" } }),
                vec![],
                "managed:env.CLAUDE_CODE_DISABLE_FAST_MODE",
            ),
            (json!({ "model": "sonnet" }), vec![], "managed:model"),
        ];
        for (extra, environment, owner) in cases {
            let result = speed(claude, c.clone(), extra, &environment).unwrap();
            assert_eq!(result.value.as_deref(), Some("standard"));
            assert_eq!(result.owner, owner);
        }
        let opt_in = json!({ "model": "claude-opus-5-5", "fastMode": true, "fastModePerSessionOptIn": true });
        assert_eq!(
            speed(claude, opt_in, json!({}), &[])
                .unwrap()
                .value
                .as_deref(),
            Some("standard")
        );
        let haiku = speed(
            claude,
            c.clone(),
            json!({}),
            &[("ANTHROPIC_MODEL", "haiku")],
        )
        .unwrap();
        assert_eq!(haiku.value.as_deref(), Some("standard"));
        assert_eq!(haiku.owner, "ANTHROPIC_MODEL");
        assert_eq!(
            speed(claude, json!({}), json!({}), &[])
                .unwrap()
                .value
                .as_deref(),
            Some("standard")
        );
        assert_eq!(
            speed(claude, json!({ "fastMode": true }), json!({}), &[])
                .unwrap()
                .value,
            None
        );
        let gateway = speed(
            claude,
            c,
            json!({}),
            &[("ANTHROPIC_BASE_URL", "https://proxy.invalid")],
        )
        .unwrap();
        assert_eq!(gateway.value, None);
        assert!(!gateway.resolved);
        assert_eq!(
            speed(claude, json!({ "fastMode": "true" }), json!({}), &[])
                .unwrap_err()
                .message,
            "invalid fastMode"
        );
    }

    // Ported from fast.test.ts: "Codex gates are independent, support ultrafast and keep unsupported tiers unknown".
    #[test]
    fn codex_gates_are_independent_and_unsupported_tiers_stay_unknown() {
        let codex = |config: Value| speed(Harness::Codex, config, json!({}), &[]);
        let value = |config: Value| codex(config).unwrap().value;
        let base = json!({ "model": "gpt-6.1-sol", "service_tier": "fast" });
        let with = |extra: Value| {
            let mut config = base.clone();
            for (k, v) in map(extra) {
                config[k] = v;
            }
            config
        };
        assert_eq!(value(base.clone()).as_deref(), Some("fast"));
        assert_eq!(
            value(with(json!({ "features": { "fast_mode": false } }))).as_deref(),
            Some("standard")
        );
        assert_eq!(
            value(with(
                json!({ "service_tier": "ultrafast", "features": { "fast_mode": false } })
            ))
            .as_deref(),
            Some("ultrafast")
        );
        assert_eq!(
            codex(with(
                json!({ "service_tier": "ultrafast", "features": { "ultrafast_mode": false } })
            ))
            .unwrap()
            .owner,
            "user:features.ultrafast_mode"
        );
        assert_eq!(
            value(with(json!({ "features": { "ultrafast_mode": false } }))).as_deref(),
            Some("fast")
        );
        assert_eq!(value(with(json!({ "model": "custom-model" }))), None);
        assert_eq!(
            value(with(
                json!({ "service_tier": "ultrafast", "model": "gpt-6-luna" })
            )),
            None
        );
        assert_eq!(value(json!({ "service_tier": "auto" })), None);
        assert_eq!(value(json!({})).as_deref(), Some("standard"));
        assert_eq!(value(with(json!({ "model_provider": "azure" }))), None);
        assert!(codex(with(json!({ "features": { "fast_mode": "false" } }))).is_err());
        assert_eq!(
            codex(with(json!({ "features": [] }))).unwrap_err().message,
            "invalid speed features"
        );
    }

    // Ported from fast.test.ts: "invalid speed controls name the effective source file including Windows drive letters".
    #[test]
    fn invalid_speed_controls_name_the_effective_source_file() {
        let system = Map::new();
        for (file, key, config) in [
            (
                "user/config.toml",
                "service_tier",
                json!({ "service_tier": false }),
            ),
            (
                "C:\\user\\config.toml",
                "features.fast_mode",
                json!({ "features": { "fast_mode": "false" } }),
            ),
        ] {
            let config = map(config);
            let error = speed_controls(
                Harness::Codex,
                &[
                    SettingsScope {
                        config: &system,
                        file: "/etc/codex/config.toml",
                    },
                    SettingsScope {
                        config: &config,
                        file,
                    },
                ],
                &Env::default(),
            )
            .unwrap_err();
            assert!(error.message.contains(key));
            assert_eq!(&*error.location.unwrap().file, file);
        }
    }

    #[test]
    fn documented_windows_follow_the_model_generation() {
        for (model, million) in [
            ("claude-fable-5", Some(true)),
            ("claude-sonnet-5", Some(true)),
            ("claude-sonnet-4-6", Some(false)),
            ("claude-opus-4-7", Some(true)),
            ("claude-opus-4-6", Some(false)),
            ("claude-opus-4", Some(false)),
            ("claude-haiku-5-5", Some(true)),
            ("claude-haiku-4-5", Some(false)),
            ("claude-haiku-6", None),
            ("claude-mythos-5", None),
            ("claude-3-5-sonnet", None),
            ("claude-opus-4-+6", None),
            ("claude-opus-4-6-v1", None),
        ] {
            assert_eq!(native_million(model), million, "{model}");
        }
    }
}
