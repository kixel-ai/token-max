//! Resolve current settings files and environments into controls with explicit owners.

use crate::collections::{HashMap, IndexMap};

use serde_json::{Map, Value};

use crate::model::{
    BackgroundControl, BackgroundFeature, BashCapControl, Change, CompactionControls, EventKind,
    Harness, IdleAttribution, IdleControl, Inbound, InboundControl, Location, MemoryControl,
    Millis, PercentOverride, Scan, Setting, Source, SpeedControl, TunedWindow, WindowControl,
};
use crate::safe::{MAX_SAFE_INTEGER, ScanError, object};

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

/// An integral number, float or not, of magnitude at most 2^53 - 1.
fn safe_integer(value: &Value) -> Option<i64> {
    let n = value.as_f64()?;
    (n.fract() == 0.0 && n.abs() <= MAX_SAFE_INTEGER as f64).then_some(n as i64)
}

/// A value as text: absent is "undefined", arrays join with commas and nulls in them as empty,
/// objects are "[object Object]".
pub(crate) fn js_string(value: Option<&Value>) -> String {
    match value {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => crate::exposure::number(n),
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
                .filter(|n| *n > 0.0 && *n <= MAX_SAFE_INTEGER as f64)
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
                scope: None,
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
        scope: None,
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
        scope: None,
        owner,
        off: format!("{key} = false"),
        off_file: Some(file.into()),
    }])
}

/// Claude auto memory, on by default: `CLAUDE_CODE_DISABLE_AUTO_MEMORY` in the
/// environment or settings env wins over `autoMemoryEnabled`.
/// <https://code.claude.com/docs/en/memory#auto-memory>
pub fn claude_memory(scopes: &[SettingsScope], env: &Env) -> Result<MemoryControl, ScanError> {
    let key = "autoMemoryEnabled";
    let variable = "CLAUDE_CODE_DISABLE_AUTO_MEMORY";
    let environment = scoped_environment(scopes, env);
    if let Some(disabled) = environment.values.get(variable).and_then(env_flag) {
        let owner = environment.owners[variable].clone();
        let scope = scopes
            .iter()
            .find(|s| owner == format!("{}:env.{variable}", s.file));
        return Ok(MemoryControl {
            value: !disabled,
            owner,
            off: match scope {
                Some(_) => format!("\"env\": {{\"{variable}\": \"1\"}}"),
                None => format!("{variable}=1"),
            },
            off_file: scope.map(|s| s.file.to_string()),
            directory: None,
            unresolved: None,
            overrides: vec![],
            stored: false,
        });
    }
    let first = scopes.first().map_or("settings.json", |s| s.file);
    let (value, owner, file) = match scoped_bool(scopes, key)? {
        Some((value, file)) => (value, format!("{file}:{key}"), file),
        None => (true, format!("{first}:{key} (default)"), first),
    };
    Ok(MemoryControl {
        value,
        owner,
        off: format!("\"{key}\": false"),
        off_file: Some(file.into()),
        directory: None,
        unresolved: None,
        overrides: vec![],
        stored: false,
    })
}

/// `crossSessionInbound` as Claude Code resolves it: managed, then user settings, the first
/// value found; then a project or local value from `repository`, only where it is stricter.
/// Neither file setting it leaves Claude Code to decide per message.
/// <https://code.claude.com/docs/en/settings-reference#crosssessioninbound>
pub fn claude_inbound(
    managed: SettingsScope,
    user: SettingsScope,
    repository: &[SettingsScope],
) -> Result<InboundControl, ScanError> {
    let key = "crossSessionInbound";
    let read = |scope: &SettingsScope| match scope.config.get(key) {
        None => Ok(None),
        Some(Value::String(s)) if s == "accept" => Ok(Some(Inbound::Accept)),
        Some(Value::String(s)) if s == "hold" => Ok(Some(Inbound::Hold)),
        Some(Value::String(s)) if s == "refuse" => Ok(Some(Inbound::Refuse)),
        Some(_) => Err(invalid(format!("invalid {key}"), scope.file)),
    };
    let mut resolved = None;
    for scope in [managed, user] {
        if let Some(value) = read(&scope)? {
            resolved = Some((value, scope.file));
            break;
        }
    }
    let policy = resolved.is_some_and(|(_, file)| file == managed.file);
    for scope in repository {
        let current = resolved.map(|(value, _)| value);
        let Some(value) = read(scope)? else { continue };
        // A repository value as strict as the user's also holds it there, so Claude Code
        // names the repository; managed settings keep their name.
        let tied = current == Some(value) && value != Inbound::Accept && !policy;
        if value > current.unwrap_or(Inbound::Accept) || tied {
            resolved = Some((value, scope.file));
        }
    }
    Ok(InboundControl {
        value: resolved.map(|(value, _)| value),
        file: resolved.map_or(user.file, |(_, file)| file).to_string(),
        directory: None,
        unresolved: None,
    })
}

/// Codex memories, off by default: `features.memories` turns them on.
/// `memories.use_memories = false` only keeps them out of the prompt and
/// `memories.generate_memories = false` only stops new ones, so both together are
/// off too. Scopes run from lowest to highest precedence; `user` is the file a
/// default is set in.
/// <https://learn.chatgpt.com/docs/customization/memories>
pub fn codex_memory(scopes: &[SettingsScope], user: &str) -> Result<MemoryControl, ScanError> {
    let key = "features.memories";
    let (enabled, file, owner) = match scoped_bool(scopes, key)? {
        Some((value, file)) => (value, file, format!("{file}:{key}")),
        None => (false, user, format!("{user}:{key} (default)")),
    };
    let used = scoped_bool(scopes, "memories.use_memories")?;
    let generated = scoped_bool(scopes, "memories.generate_memories")?;
    let (value, owner) = match (used, generated) {
        (Some((false, used)), Some((false, generated))) if enabled => (
            false,
            format!("{used}:memories.use_memories and {generated}:memories.generate_memories"),
        ),
        _ => (enabled, owner),
    };
    Ok(MemoryControl {
        value,
        owner,
        off: format!("{key} = false"),
        off_file: Some(file.into()),
        directory: None,
        unresolved: None,
        overrides: vec![],
        stored: false,
    })
}

/// A positive millisecond count as Claude Code parses it: a leading integer, or an integral exponent form.
fn timeout_millis(value: &Value) -> Option<Millis> {
    let text = js_string(Some(value));
    let text = text.trim();
    let parsed = if text.contains(['e', 'E']) {
        text.parse::<f64>()
            .ok()
            .filter(|n| n.fract() == 0.0 && n.abs() <= MAX_SAFE_INTEGER as f64)
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
        unresolved: None,
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
            declared: None,
            tuned: IndexMap::default(),
            contexts: IndexMap::default(),
            percent: None,
            layers: None,
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
    let mut controls = CompactionControls {
        default: resolve(None)?,
        models,
        capped: environment.is("CLAUDE_CODE_DISABLE_1M_CONTEXT", "1"),
        // Only a positive token count declares a window; anything else leaves the cap in force.
        declared: environment
            .values
            .get("CLAUDE_CODE_MAX_CONTEXT_TOKENS")
            .map(|value| js_string(Some(value)))
            .filter(|text| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|text| text.parse::<u64>().ok())
            .filter(|&n| n > 0),
        tuned: IndexMap::default(),
        contexts: IndexMap::default(),
        percent: percent_override(scopes, env, &environment),
        layers: Some(Box::new(WindowLayers {
            file: file.to_string(),
            managed_file: managed_file.to_string(),
            scopes: scopes
                .iter()
                .map(|scope| (scope.file.to_string(), window_settings(scope.config)))
                .collect(),
            shell: window_environment(env),
            instance: None,
        })),
    };
    date_percent(&mut controls);
    Ok(controls)
}

/// The variables a Claude window resolves from; no other value is kept.
const WINDOW_ENVIRONMENT: [&str; 5] = [
    WINDOW_KEY,
    PERCENT_KEY,
    "DISABLE_AUTO_COMPACT",
    "CLAUDE_CODE_DISABLE_1M_CONTEXT",
    "CLAUDE_CODE_MAX_CONTEXT_TOKENS",
];

fn window_environment(env: &Env) -> Env {
    env.iter()
        .filter(|(key, _)| WINDOW_ENVIRONMENT.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// The part of a settings file a Claude window resolves from.
fn window_settings(config: &Map<String, Value>) -> Map<String, Value> {
    let mut kept = Map::new();
    for key in ["autoCompactWindow", "autoCompactEnabled"] {
        if let Some(value) = config.get(key) {
            kept.insert(key.into(), value.clone());
        }
    }
    if let Some(env) = config.get("env") {
        kept.insert(
            "env".into(),
            match env {
                Value::Object(env) => Value::Object(
                    env.iter()
                        .filter(|(key, _)| WINDOW_ENVIRONMENT.contains(&key.as_str()))
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect(),
                ),
                other => other.clone(),
            },
        );
    }
    if let Some(rows) = config.get("modelSettings") {
        kept.insert(
            "modelSettings".into(),
            match rows {
                // A row's presence matters even when it names no window.
                Value::Object(rows) => Value::Object(
                    rows.iter()
                        .map(|(model, row)| {
                            let row = match row {
                                Value::Object(row) => Value::Object(
                                    row.iter()
                                        .filter(|(key, _)| *key == "autoCompactWindow")
                                        .map(|(key, value)| (key.clone(), value.clone()))
                                        .collect(),
                                ),
                                other => other.clone(),
                            };
                            (model.clone(), row)
                        })
                        .collect(),
                ),
                other => other.clone(),
            },
        );
    }
    kept
}

/// The raw settings a profile's or directory's Claude windows resolve from: each settings
/// file's window settings in precedence order, the shell's variables, and a T3 instance's.
#[derive(Clone, Debug, PartialEq)]
pub struct WindowLayers {
    file: String,
    managed_file: String,
    scopes: Vec<(String, Map<String, Value>)>,
    shell: Env,
    instance: Option<InstanceLayer>,
}

/// What a T3 instance sets above the settings files: its own variables and its own window.
#[derive(Clone, Debug, PartialEq)]
struct InstanceLayer {
    file: String,
    id: String,
    env: Env,
    window: Option<i64>,
}

/// The setting an owner names, or None when it names no window setting a change can make.
pub fn locate(owner: &str) -> Option<(Source, Setting)> {
    let owner = owner_setting(owner);
    if !owner.contains(':') {
        return [WINDOW_KEY, PERCENT_KEY]
            .contains(&owner)
            .then(|| (Source::Shell, Setting::Env(owner.to_string())));
    }
    let end = [".json:", ".toml:"]
        .iter()
        .filter_map(|ext| owner.find(ext).map(|i| i + ext.len() - 1))
        .min()?;
    let (file, rest) = (&owner[..end], &owner[end + 1..]);
    if rest == "autoCompactWindow" {
        return Some((Source::File(file.into()), Setting::Window));
    }
    if let Some(model) = rest
        .strip_prefix("modelSettings.")
        .and_then(|r| r.strip_suffix(".autoCompactWindow"))
    {
        return Some((Source::File(file.into()), Setting::Row(model.into())));
    }
    if let Some(key) = rest.strip_prefix("env.") {
        return Some((Source::File(file.into()), Setting::Env(key.into())));
    }
    let (id, tail) = rest.strip_prefix("providerInstances.")?.split_once('.')?;
    let source = Source::Instance {
        file: file.into(),
        id: id.into(),
    };
    match tail {
        "config.autoCompactWindow" => Some((source, Setting::Window)),
        _ => Some((
            source,
            Setting::Env(tail.strip_prefix("environment.")?.into()),
        )),
    }
}

/// The controls with `changes` applied to the settings they resolve from, or None when no
/// change touches them. Nothing recorded shows a trigger under the changed settings, so no
/// observation carries over; tuned defaults and context sizes do, since no change moves them.
pub fn reresolve(
    compaction: &CompactionControls,
    changes: &[Change],
) -> Option<Result<CompactionControls, ScanError>> {
    let mut layers = (**compaction.layers.as_ref()?).clone();
    let real = |file: &str| crate::discovery::realpath(file).unwrap_or_else(|_| file.to_string());
    let mut touched = false;
    for change in changes {
        match &change.source {
            Source::Shell => {
                if let Setting::Env(key) = &change.setting
                    && layers.shell.contains_key(key)
                {
                    touched = true;
                    match change.value {
                        Some(value) => layers.shell.insert(key.clone(), value.to_string()),
                        None => layers.shell.remove(key),
                    };
                }
            }
            Source::File(file) => {
                let file = real(file);
                for (scope, config) in &mut layers.scopes {
                    if real(scope) == file {
                        touched = true;
                        apply(config, &change.setting, change.value);
                    }
                }
            }
            Source::Instance { file, id } => {
                if let Some(instance) = &mut layers.instance
                    && &instance.id == id
                    && real(&instance.file) == real(file)
                {
                    touched = true;
                    match &change.setting {
                        Setting::Env(key) => match change.value {
                            Some(value) => instance.env.insert(key.clone(), value.to_string()),
                            None => instance.env.remove(key),
                        },
                        _ => {
                            instance.window = change.value;
                            None
                        }
                    };
                }
            }
        }
    }
    if !touched {
        return None;
    }
    let configs: Vec<(String, Map<String, Value>)> = layers.scopes.clone();
    let scopes: Vec<SettingsScope> = configs
        .iter()
        .map(|(file, config)| SettingsScope { config, file })
        .collect();
    let mut env = layers.shell.clone();
    if let Some(instance) = &layers.instance {
        env.extend(instance.env.clone());
    }
    let empty = Map::new();
    let result = compaction_controls(&CompactionSources {
        harness: Harness::Claude,
        user: scopes.first().map_or(&empty, |s| s.config),
        managed: &empty,
        env: &env,
        file: &layers.file,
        managed_file: &layers.managed_file,
        scopes: Some(&scopes),
    })
    .map(|mut changed| {
        if let Some(instance) = &layers.instance
            && let Some(window) = instance.window
        {
            changed.models.clear();
            changed.default = WindowControl {
                value: Some(window),
                owner: format!(
                    "{}:providerInstances.{}.config.autoCompactWindow",
                    instance.file, instance.id
                ),
                unset: false,
                setting: None,
                ..changed.default.clone()
            };
        }
        if let Some(percent) = &mut changed.percent {
            percent.observed.clear();
        }
        changed.tuned = compaction.tuned.clone();
        changed.contexts = compaction.contexts.clone();
        changed.layers = Some(Box::new(layers));
        changed
    });
    Some(result)
}

/// Set or remove one window setting in a settings file's window settings.
fn apply(config: &mut Map<String, Value>, setting: &Setting, value: Option<i64>) {
    match (setting, value) {
        (Setting::Window, Some(value)) => {
            config.insert("autoCompactWindow".into(), value.into());
        }
        (Setting::Window, None) => {
            config.remove("autoCompactWindow");
        }
        (Setting::Row(model), value) => {
            let rows = config
                .entry("modelSettings")
                .or_insert_with(|| Value::Object(Map::new()));
            if let Some(rows) = rows.as_object_mut() {
                let row = rows
                    .entry(model.to_string())
                    .or_insert_with(|| Value::Object(Map::new()));
                if let Some(row) = row.as_object_mut() {
                    match value {
                        Some(value) => row.insert("autoCompactWindow".into(), value.into()),
                        None => row.remove("autoCompactWindow"),
                    };
                }
            }
        }
        (Setting::Env(key), value) => {
            let env = config
                .entry("env")
                .or_insert_with(|| Value::Object(Map::new()));
            if let Some(env) = env.as_object_mut() {
                match value {
                    Some(value) => env.insert(key.clone(), value.to_string().into()),
                    None => env.remove(key),
                };
            }
        }
    }
}

/// The variable that lowers every model's trigger to a percentage of its window.
pub const PERCENT_KEY: &str = "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE";

/// The variable that sets every model's window, above each settings row.
pub const WINDOW_KEY: &str = "CLAUDE_CODE_AUTO_COMPACT_WINDOW";

/// Variables that decide a model's trigger, besides the window settings.
const TRIGGER_KEYS: [&str; 4] = [
    PERCENT_KEY,
    WINDOW_KEY,
    "CLAUDE_CODE_DISABLE_1M_CONTEXT",
    "CLAUDE_CODE_MAX_CONTEXT_TOKENS",
];

/// Resolve `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE`, or None when it is unset or cannot lower a
/// trigger: Claude Code ignores a percentage at or above its default, which is below 100.
fn percent_override(
    scopes: &[SettingsScope],
    env: &Env,
    environment: &ScopedEnvironment,
) -> Option<PercentOverride> {
    let raw = js_string(Some(environment.values.get(PERCENT_KEY)?));
    let text = raw.trim();
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let number = match text.split_once('.') {
        Some((whole, fraction)) => digits(whole) && digits(fraction),
        None => digits(text),
    };
    let value = number.then(|| text.parse::<f64>().ok()).flatten();
    if value.is_some_and(|v| v >= 100.0) {
        return None;
    }
    let mut owners = vec![];
    let mut windows = vec![];
    for (key, owners) in [(PERCENT_KEY, &mut owners), (WINDOW_KEY, &mut windows)] {
        if env.contains_key(key) {
            owners.push(key.to_string());
        }
    }
    let mut files: Vec<String> = vec![];
    for scope in scopes {
        let variables = members(scope.config.get("env"));
        for (key, owners) in [(PERCENT_KEY, &mut owners), (WINDOW_KEY, &mut windows)] {
            if variables.contains_key(key) {
                owners.push(format!("{}:env.{key}", scope.file));
            }
        }
        let windowed = scope.config.contains_key("autoCompactWindow")
            || members(scope.config.get("modelSettings"))
                .values()
                .any(|row| members(Some(row)).contains_key("autoCompactWindow"));
        if (windowed || TRIGGER_KEYS.iter().any(|k| variables.contains_key(*k)))
            && !files.iter().any(|f| f == scope.file)
        {
            files.push(scope.file.to_string());
        }
    }
    Some(PercentOverride {
        // Claude Code documents 1 to 100; below 1 is no percentage it defines.
        value: value.filter(|v| *v >= 1.0),
        owners,
        windows,
        files,
        timeless: TRIGGER_KEYS
            .iter()
            .filter(|k| env.contains_key(**k))
            .map(|k| k.to_string())
            .collect(),
        since: None,
        observed: IndexMap::default(),
    })
}

/// Credit the variables a T3 instance's own environment sets, and its own window, to T3's
/// settings file, which dates them, rather than to the shell, with or without the override.
pub fn t3_percent(
    compaction: &mut CompactionControls,
    environment: &[String],
    file: &str,
    instance: &str,
    window: bool,
    shell: &Env,
) {
    // Split the merged environment back into the shell's and the instance's own.
    let own_window = window.then_some(compaction.default.value).flatten();
    if let Some(layers) = &mut compaction.layers {
        let mut own = Env::default();
        for key in environment {
            // A name listed twice is the instance's once.
            if own.contains_key(key) {
                continue;
            }
            if let Some(value) = layers.shell.remove(key) {
                own.insert(key.clone(), value);
            }
            if let Some(value) = shell
                .get(key)
                .filter(|_| WINDOW_ENVIRONMENT.contains(&key.as_str()))
            {
                layers.shell.insert(key.clone(), value.clone());
            }
        }
        layers.instance = Some(InstanceLayer {
            file: file.to_string(),
            id: instance.to_string(),
            env: own,
            window: own_window,
        });
    }
    // The window variable the instance sets is the effective one, so its own entry owns it.
    if environment.iter().any(|k| k == WINDOW_KEY) {
        let own = format!("{file}:providerInstances.{instance}.environment.{WINDOW_KEY}");
        for control in
            std::iter::once(&mut compaction.default).chain(compaction.models.values_mut())
        {
            if control.owner == WINDOW_KEY {
                control.owner = own.clone();
            }
        }
    }
    let Some(percent) = &mut compaction.percent else {
        return;
    };
    for owners in [&mut percent.owners, &mut percent.windows] {
        // The instance's copy outranks the shell's, which stays listed so a plan lifts both.
        *owners = std::mem::take(owners)
            .into_iter()
            .flat_map(|owner| {
                if !environment.contains(&owner) {
                    return vec![owner];
                }
                let own = format!("{file}:providerInstances.{instance}.environment.{owner}");
                if shell.contains_key(&owner) {
                    vec![owner, own]
                } else {
                    vec![own]
                }
            })
            .collect();
    }
    percent.timeless.retain(|key| !environment.contains(key));
    let decides = window
        || environment
            .iter()
            .any(|k| TRIGGER_KEYS.contains(&k.as_str()));
    if decides && !percent.files.iter().any(|f| f == file) {
        percent.files.push(file.to_string());
    }
    date_percent(compaction);
}

/// A window owner without its trailing note, such as ` (capped at 200000)`; a settings path
/// may itself hold ` (`, so the note is looked for only after it.
pub fn owner_setting(owner: &str) -> &str {
    // The key follows the file's path, which may itself hold a key's name.
    let start = [".json:", ".toml:"]
        .iter()
        .filter_map(|ext| owner.find(ext).map(|i| i + ext.len()))
        .min()
        .unwrap_or(0);
    // Only the note goes: a model's row key may itself hold `.autoCompactWindow`.
    let end = owner[start..]
        .find(" (")
        .map_or(owner.len(), |at| start + at);
    &owner[..end]
}

/// Date a resolved override by when the last file that decides a trigger changed.
pub fn date_percent(compaction: &mut CompactionControls) {
    let Some(percent) = &mut compaction.percent else {
        return;
    };
    percent.since = None;
    if !percent.timeless.is_empty() {
        return;
    }
    let mut since = Millis::MIN;
    for file in &percent.files {
        // A time before 1970 is still a time.
        let modified = std::fs::metadata(file)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| match t.duration_since(std::time::UNIX_EPOCH) {
                Ok(after) => Millis::try_from(after.as_millis()).ok(),
                Err(before) => Millis::try_from(before.duration().as_millis())
                    .ok()
                    .map(|m| -m),
            });
        let Some(modified) = modified else {
            return;
        };
        since = since.max(modified);
    }
    percent.since = Some(since);
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
pub(crate) fn strip_dated(text: &str, digits: usize) -> Option<&str> {
    let split = text.len().checked_sub(digits + 1)?;
    let (head, tail) = text.split_at_checked(split)?;
    let tail = tail.strip_prefix('-')?;
    tail.bytes().all(|b| b.is_ascii_digit()).then_some(head)
}

/// Why a capped window that applies to every model stays advisory.
pub const CAPPED_SHARED: &str =
    "the window applies to every model, so no setting changes this model's capped window alone";

/// Exact model settings win; known dated Claude IDs share their undated model row. The value
/// is the trigger, lowered by `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` where it applies.
pub fn window_control(compaction: &CompactionControls, model: Option<&str>) -> WindowControl {
    let control = window_setting(compaction, model);
    match &compaction.percent {
        Some(percent) => lowered(compaction, percent, model.unwrap_or(""), control),
        None => control,
    }
}

/// The trigger under `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE`. An explicit window below the model's
/// context compacts at the percentage of it. Elsewhere the trigger rests on Claude Code's
/// undocumented default percentage, or on whether the override applies at all where a session
/// compacts at the context limit, so only a recorded trigger under the current settings shows it.
fn lowered(
    compaction: &CompactionControls,
    percent: &PercentOverride,
    model: &str,
    mut control: WindowControl,
) -> WindowControl {
    let owner = percent.owner();
    let Some(value) = percent.value else {
        control.value = None;
        control.applicable = false;
        control.reason = format!(
            "{owner} is not a percentage Claude Code documents, so the trigger is unresolved; {}",
            control.reason
        );
        return control;
    };
    let canonical = strip_dated(model, 8).unwrap_or(model);
    // The context is known when Claude Code holds the model below 1M, or documents it at 1M.
    let context =
        compaction
            .contexts
            .get(canonical)
            .copied()
            .or((Sizing::of(compaction, canonical) == Sizing::Native
                && native_million(canonical) == Some(true))
            .then_some(CLAUDE_WINDOW_MAXIMUM));
    let percentage = format!("{PERCENT_KEY}={value}");
    // An unresolved alias selection leaves no window to lower, and a recorded trigger cannot
    // show which setting owned it.
    if control.value.is_none() && !control.unset {
        return control;
    }
    if let Some(window) = control.value
        && !control.unset
        && control.setting.is_none()
        && context.is_some_and(|context| window < context)
    {
        let trigger = (window as f64 * value / 100.0).floor() as i64;
        control.value = Some(trigger);
        control.reason = format!(
            "{owner} sets {percentage}, so this {window}-token window compacts at {trigger} tokens; {}",
            control.reason
        );
        return control;
    }
    // A dated ID borrows its undated ID's recorded trigger only under the same settings.
    let borrowed = || {
        (canonical != model
            && percent_key(compaction, percent, model)
                == percent_key(compaction, percent, canonical))
        .then(|| percent.observed.get(canonical))
        .flatten()
    };
    control.value = percent.observed.get(model).or_else(borrowed).copied();
    let evidence = match (control.value, percent.since) {
        (Some(trigger), _) => format!(
            "the latest automatic compaction on this model since the settings that decide it last changed triggered at {trigger} tokens"
        ),
        (None, Some(_)) => "no automatic compaction on this model was recorded since the settings that decide it last changed".into(),
        (None, None) => format!(
            "{} keeps no modification time, so no recorded compaction shows it",
            percent
                .timeless
                .first()
                .or(percent.files.first())
                .map_or("a setting", String::as_str)
        ),
    };
    control.reason = format!(
        "{owner} sets {percentage}, and where this model compacts under it rests on Claude Code's undocumented default percentage; {evidence}; {}",
        control.reason
    );
    match control.value {
        // Recorded triggers differ by model, so only the model's own row holds one.
        Some(_) => {
            // An unset window lacked only its trigger, which the recorded one now gives.
            if control.unset {
                control.applicable = true;
            }
            if let Some(file) = control.owner.strip_suffix(":autoCompactWindow") {
                control.owner = format!("{file}:modelSettings.{canonical}.autoCompactWindow");
            } else if !control.owner.contains(":modelSettings.") {
                // A variable or T3 window has no row per model, so the owner keys the model too.
                control.owner += &format!(" (recorded on {canonical})");
            }
        }
        None => control.applicable = false,
    }
    control
}

/// The window a setting or Claude Code's tuned default gives the model, before any percentage.
fn window_setting(compaction: &CompactionControls, model: Option<&str>) -> WindowControl {
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
            // The alias's row may name the window, so its triggers do not show the default.
            unset: false,
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
            // Tuned windows differ by model, so only the model's own row changes one alone; they
            // also differ by what sizes the context, so scopes sized apart never share an owner,
            // even where the model's own row is already the owner, as with an explicit "auto".
            if let Some(file) = control.owner.strip_suffix(":autoCompactWindow") {
                control.owner = format!("{file}:modelSettings.{canonical}.autoCompactWindow");
            }
            if let Some(sized) = Sizing::of(compaction, canonical).suffix() {
                control.owner += &sized;
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
            control.applicable = false;
            reason += &format!(" and {CAPPED_SHARED}");
        }
        // The owner keys one replayed window, so it names what sets the cap: scopes sized apart
        // never share one, nor do a capped model and an uncapped one under a shared window.
        control.owner += &Sizing::of(compaction, canonical)
            .suffix()
            .unwrap_or_else(|| format!(" (capped at {context})"));
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

/// What sets a model's context window, and so its tuned window, under one scope's environment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Sizing {
    /// The model's own context window.
    Native,
    /// `CLAUDE_CODE_DISABLE_1M_CONTEXT=1` holds the model at 200k.
    Capped,
    /// `CLAUDE_CODE_MAX_CONTEXT_TOKENS` may size an ID outside the documented table.
    Declared(u64),
}

impl Sizing {
    fn of(compaction: &CompactionControls, model: &str) -> Self {
        // The cap holds even an explicit window; only a declared window can size an ID Claude
        // Code does not recognize, and it never sizes a documented model.
        match compaction.declared {
            Some(tokens) if native_million(model).is_none() => Self::Declared(tokens),
            _ if compaction.capped => Self::Capped,
            _ => Self::Native,
        }
    }

    /// The owner suffix that keeps windows sized this way apart from the model's native ones.
    fn suffix(self) -> Option<String> {
        match self {
            Self::Native => None,
            Self::Capped => Some(" (under CLAUDE_CODE_DISABLE_1M_CONTEXT=1)".into()),
            Self::Declared(tokens) => {
                Some(format!(" (under CLAUDE_CODE_MAX_CONTEXT_TOKENS={tokens})"))
            }
        }
    }

    /// The context window that caps any window, when Claude Code holds the model below the 1M
    /// maximum. A model documented at 200k runs larger only through its `[1m]` variant, and an
    /// unrecognized `[1m]` spelling ignores a declared window unless the 1M opt-out is set, so a
    /// recorded request above either size shows that variant ran. Without a declared window, an
    /// ID outside the table may be one Claude Code recognizes, so its context stays unknown.
    fn context(self, compaction: &CompactionControls, model: &str, largest: i64) -> Option<i64> {
        match self {
            Self::Capped => Some(TUNED_STANDARD),
            Self::Declared(tokens) => {
                let tokens = i64::try_from(tokens).unwrap_or(i64::MAX);
                (compaction.capped || largest <= tokens).then_some(tokens)
            }
            Self::Native => (native_million(model) == Some(false) && largest <= TUNED_STANDARD)
                .then_some(TUNED_STANDARD),
        }
    }
}

/// What one line's requests show about its compaction windows.
struct Observation {
    /// The largest input of each model the line used.
    largest: IndexMap<String, i64>,
    /// The latest automatic compaction trigger on each model and sizing that ran under an unset
    /// window.
    observed: IndexMap<(String, Sizing), (Millis, i64)>,
    /// The latest automatic compaction trigger under each percent override.
    percented: IndexMap<PercentKey, (Millis, i64)>,
}

/// What a line's requests show about its compaction windows.
fn observe(
    profile: &crate::model::Profile,
    line: &[String],
    by_id: &HashMap<&str, &crate::model::Profile>,
    requests: &[crate::model::Request],
    events: &[crate::model::StructuralEvent],
    cache_context: &crate::model::CacheContext,
    undated: &dyn Fn(&str) -> String,
) -> Observation {
    let mine = |profiles: &[String]| profiles.iter().any(|p| line.contains(p));
    let mut largest: IndexMap<String, i64> = IndexMap::default();
    for r in cache_context.requests.iter().chain(requests.iter()) {
        if !mine(&r.profiles) {
            continue;
        }
        for p in &r.parts {
            if let Some(model) = &p.model {
                let size = largest.entry(undated(model)).or_default();
                *size = (*size).max(p.usage.input.unwrap_or(0));
            }
        }
    }
    // Idle and keepalive read a recorded directory's own controls, else the profile's;
    // an event's are those of the line's profile that recorded it, which holds its cwd.
    let scope = |profiles: &[String], cwd: Option<&str>| {
        let origin = profiles
            .iter()
            .filter(|p| line.contains(p))
            .find_map(|p| by_id.get(p.as_str()).copied())
            .unwrap_or(profile);
        cwd.filter(|c| !c.is_empty())
            .and_then(|c| {
                origin
                    .idle_directories
                    .as_ref()?
                    .get(c)?
                    .compaction
                    .as_ref()
            })
            .unwrap_or(&origin.compaction)
    };
    let mut observed: IndexMap<(String, Sizing), (Millis, i64)> = IndexMap::default();
    // A session reads its settings when it starts, so its first request dates them.
    let mut started: HashMap<&str, Millis> = HashMap::default();
    for r in cache_context.requests.iter().chain(requests.iter()) {
        let start = started.entry(&r.session).or_insert(r.time);
        *start = (*start).min(r.time);
    }
    let mut percented: IndexMap<PercentKey, (Millis, i64)> = IndexMap::default();
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
        ) && mine(&e.profiles)
            && tokens > 0
        {
            let at = scope(&e.profiles, e.cwd.as_deref());
            // A percentage can lower a trigger below the smallest window a setting may name.
            if let Some(percent) = &at.percent
                && let Some(since) = percent.since
                && started.get(&*e.session).is_some_and(|&start| start > since)
            {
                // The recorded ID, since an exact dated row may set its window.
                let key = percent_key(at, percent, model);
                let latest = percented
                    .entry((model.to_string(), key))
                    .or_insert((e.time, tokens));
                if e.time >= latest.0 {
                    *latest = (e.time, tokens);
                }
            }
            // A trigger under an explicit window, or one an override may have lowered, does
            // not show the default.
            if at.percent.is_some()
                || !(CLAUDE_WINDOW_MINIMUM..=CLAUDE_WINDOW_MAXIMUM).contains(&tokens)
                || !window_setting(at, Some(model)).unset
            {
                continue;
            }
            let model = undated(model);
            let sizing = Sizing::of(at, &model);
            let latest = observed.entry((model, sizing)).or_insert((e.time, tokens));
            if e.time >= latest.0 {
                *latest = (e.time, tokens);
            }
        }
    }
    Observation {
        largest,
        observed,
        percented,
    }
}

/// A model, and what decides its trigger under an override, which a recorded trigger must
/// share to show the trigger under another directory's controls.
type PercentKey = (String, PercentScope);

/// The override's value, when its settings last changed, the window a setting names (or none)
/// and its owner, and what sizes the model's context.
type PercentScope = (Option<u64>, Option<Millis>, Option<(i64, String)>, Sizing);

fn percent_key(
    compaction: &CompactionControls,
    percent: &PercentOverride,
    model: &str,
) -> PercentScope {
    let control = window_setting(compaction, Some(model));
    // A capped window keeps the setting it caps, and owners carry notes after their key.
    let window = (!control.unset).then(|| {
        let owner = owner_setting(&control.owner);
        (
            control.setting.or(control.value).unwrap_or(0),
            owner.to_string(),
        )
    });
    (
        percent.value.map(f64::to_bits),
        percent.since,
        window,
        // Sizing reads the documented table, which knows only undated IDs.
        Sizing::of(compaction, strip_dated(model, 8).unwrap_or(model)),
    )
}

/// Fill the tuned window of every model a Claude profile used, in the profile's controls and in
/// each recorded directory's, which idle and keepalive read: the trigger of the latest automatic
/// compaction on that model under the same sizing, else Claude Code's documented default. Also
/// fill each model's context window there, which caps an explicit window.
pub fn tune_windows(scan: &mut Scan) {
    let Scan {
        profiles,
        requests,
        events,
        cache_context,
        ..
    } = scan;
    let undated = |model: &str| strip_dated(model, 8).unwrap_or(model).to_string();
    // Profiles whose windows one file sets share a line, so each reads all of their history.
    let lines: Vec<Vec<String>> = {
        let groups = crate::rows::groups(profiles.iter(), crate::rows::compaction_owner);
        profiles
            .iter()
            .map(|p| {
                let group = groups.iter().find(|g| g.ids.contains(&p.id));
                group.expect("every profile has a line").ids.clone()
            })
            .collect()
    };
    // Every observation is read before any profile's windows are filled.
    let by_id: HashMap<&str, &crate::model::Profile> =
        profiles.iter().map(|p| (p.id.as_str(), p)).collect();
    let observations: Vec<_> = profiles
        .iter()
        .zip(&lines)
        .map(|(profile, line)| {
            (profile.harness == Harness::Claude).then(|| {
                observe(
                    profile,
                    line,
                    &by_id,
                    requests,
                    events,
                    cache_context,
                    &undated,
                )
            })
        })
        .collect();
    for (profile, observation) in profiles.iter_mut().zip(observations) {
        let Some(Observation {
            largest,
            observed,
            percented,
        }) = observation
        else {
            continue;
        };
        let directories = profile.idle_directories.iter_mut().flatten();
        for compaction in std::iter::once(&mut profile.compaction)
            .chain(directories.filter_map(|(_, idle)| idle.compaction.as_mut()))
        {
            if let Some(percent) = &compaction.percent {
                let observed = percented
                    .iter()
                    .filter(|((model, key), _)| *key == percent_key(compaction, percent, model))
                    .map(|((model, _), &(_, tokens))| (model.clone(), tokens))
                    .collect();
                compaction.percent.as_mut().expect("checked above").observed = observed;
            }
            // Directories share the profile's history, so its largest requests stand for each.
            compaction.contexts = largest
                .iter()
                .filter_map(|(model, &size)| {
                    let context = Sizing::of(compaction, model).context(compaction, model, size);
                    Some((model.clone(), context?))
                })
                .collect();
            for (model, &size) in &largest {
                let sizing = Sizing::of(compaction, model);
                let trigger = observed.get(&(model.clone(), sizing));
                let (value, reason) = match (sizing, trigger, native_million(model)) {
                    // A trigger from before the cap cannot lift it.
                    (Sizing::Capped, ..) => (
                        Some(TUNED_STANDARD),
                        "no setting names a window; CLAUDE_CODE_DISABLE_1M_CONTEXT=1 holds this model at the documented 200k".into(),
                    ),
                    (_, Some(&(_, tokens)), _) => (
                        Some(tokens),
                        format!("no setting names a window; the latest automatic compaction on this model triggered at {tokens} tokens"),
                    ),
                    (_, None, Some(true)) => (
                        Some(TUNED_MILLION),
                        "no setting names a window and no automatic compaction on this model was recorded; assumed Claude Code's documented default of about 967k tokens for native-1M models, which the server can retune".into(),
                    ),
                    (_, None, Some(false)) if size <= TUNED_STANDARD => (
                        Some(TUNED_STANDARD),
                        "no setting names a window and no automatic compaction on this model was recorded; assumed Claude Code's documented 200k default for this model".into(),
                    ),
                    (_, None, Some(false)) => (
                        None,
                        "no setting names a window and a recorded request exceeded this model's documented 200k default, so a [1m] variant may set it; its tuned default is not established".into(),
                    ),
                    // Claude Code compacts a provider or custom ID it cannot resolve at the
                    // declared window, but not a bare claude- name, nor an unrecognized [1m]
                    // spelling, which a recorded request above the declared window shows ran.
                    (Sizing::Declared(tokens), None, None)
                        if !model.starts_with("claude-")
                            && compaction.contexts.contains_key(model) =>
                    (
                        compaction.contexts.get(model).copied(),
                        format!("no setting names a window and no automatic compaction on this model was recorded; assumed Claude Code compacts this unrecognized ID at CLAUDE_CODE_MAX_CONTEXT_TOKENS={tokens}"),
                    ),
                    (_, None, None) => (
                        None,
                        "no setting names a window and Claude Code documents no default for this model ID; its tuned default is not established".into(),
                    ),
                };
                compaction
                    .tuned
                    .insert(model.clone(), TunedWindow { value, reason });
            }
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

    #[test]
    fn a_settings_file_changed_before_1970_still_dates_the_override() {
        let w = crate::test_support::Workspace::new();
        let file = w.home.join("settings.json");
        std::fs::write(&file, "{}").unwrap();
        let before = std::time::UNIX_EPOCH - std::time::Duration::from_secs(86_400);
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(before)
            .unwrap();
        let mut controls = CompactionControls {
            percent: Some(PercentOverride {
                value: Some(50.0),
                files: vec![file.display().to_string()],
                ..Default::default()
            }),
            ..compaction_controls(&claude_sources(&Map::new(), &Map::new(), &Env::default()))
                .unwrap()
        };
        date_percent(&mut controls);
        assert_eq!(controls.percent.unwrap().since, Some(-86_400_000));
    }

    #[test]
    fn a_t3_instance_owns_and_dates_the_override_its_environment_sets() {
        let w = crate::test_support::Workspace::new();
        let state = w.home.join("settings.json");
        std::fs::write(&state, "{}").unwrap();
        let state = state.display().to_string();
        let empty = Map::new();
        let mut env = Env::default();
        env.insert(PERCENT_KEY.into(), "60".into());
        env.insert("CLAUDE_CODE_DISABLE_1M_CONTEXT".into(), "1".into());
        let mut controls = compaction_controls(&claude_sources(&empty, &empty, &env)).unwrap();
        let shell = controls.percent.clone().unwrap();
        assert_eq!(shell.owners, [PERCENT_KEY]);
        assert_eq!(shell.since, None);
        // The instance sets the override, but the 1M opt-out still comes from the shell.
        t3_percent(
            &mut controls,
            &[PERCENT_KEY.into()],
            &state,
            "claudeAgent",
            false,
            &Env::default(),
        );
        let percent = controls.percent.clone().unwrap();
        assert_eq!(
            percent.owners,
            [format!(
                "{state}:providerInstances.claudeAgent.environment.{PERCENT_KEY}"
            )]
        );
        assert_eq!(percent.files, std::slice::from_ref(&state));
        assert_eq!(percent.timeless, ["CLAUDE_CODE_DISABLE_1M_CONTEXT"]);
        assert_eq!(percent.since, None);
        let environment = [PERCENT_KEY.into(), "CLAUDE_CODE_DISABLE_1M_CONTEXT".into()];
        t3_percent(
            &mut controls,
            &environment,
            &state,
            "claudeAgent",
            false,
            &Env::default(),
        );
        let percent = controls.percent.unwrap();
        assert!(percent.timeless.is_empty());
        assert!(percent.since.is_some_and(|t| t > 0));
    }

    #[test]
    fn a_t3_instance_copy_keeps_the_shell_copy_it_outranks() {
        let empty = Map::new();
        let mut env = Env::default();
        env.insert(PERCENT_KEY.into(), "60".into());
        env.insert(WINDOW_KEY.into(), "150000".into());
        let mut controls = compaction_controls(&claude_sources(&empty, &empty, &env)).unwrap();
        // The shell exports both; the instance sets both again.
        let shell = env.clone();
        let environment = [PERCENT_KEY.into(), WINDOW_KEY.into()];
        t3_percent(
            &mut controls,
            &environment,
            "t3.json",
            "claudeAgent",
            true,
            &shell,
        );
        let percent = controls.percent.unwrap();
        let own = |key: &str| format!("t3.json:providerInstances.claudeAgent.environment.{key}");
        assert_eq!(percent.owners, [PERCENT_KEY.to_string(), own(PERCENT_KEY)]);
        assert_eq!(percent.windows, [WINDOW_KEY.to_string(), own(WINDOW_KEY)]);
        assert_eq!(percent.owners.last().unwrap(), &own(PERCENT_KEY));
    }

    #[test]
    fn an_override_neither_lowers_an_unresolved_alias_nor_accepts_a_fraction_of_a_percent() {
        let user = map(json!({
            "env": {PERCENT_KEY: "50"},
            "modelSettings": {
                "opus[1m]": { "autoCompactWindow": 400000 },
                "claude-opus-4-6[1m]": { "autoCompactWindow": 300000 },
            },
        }));
        let empty = Map::new();
        let none = Env::default();
        let mut p = compaction_controls(&claude_sources(&user, &empty, &none)).unwrap();
        // A recorded trigger cannot say which alias owned the window.
        let percent = p.percent.as_mut().unwrap();
        percent.since = Some(0);
        percent.observed.insert("claude-opus-4-6".into(), 150_000);
        let aliased = window_control(&p, Some("claude-opus-4-6"));
        assert_eq!(aliased.value, None);
        assert!(aliased.reason.contains("alias"));

        // Claude Code documents 1 to 100.
        let fraction = map(json!({"env": {PERCENT_KEY: "0.5"}, "autoCompactWindow": 400000}));
        let p = compaction_controls(&claude_sources(&fraction, &empty, &none)).unwrap();
        assert_eq!(p.percent.as_ref().unwrap().value, None);
        assert_eq!(window_control(&p, Some("claude-opus-4-6")).value, None);
    }

    #[test]
    fn a_recorded_trigger_makes_an_unset_custom_window_applicable() {
        let user = map(json!({"env": {PERCENT_KEY: "50"}}));
        let empty = Map::new();
        let mut p = compaction_controls(&claude_sources(&user, &empty, &Env::default())).unwrap();
        // No documented default covers the ID, so only a recorded trigger resolves it.
        assert!(!window_control(&p, Some("gateway-model")).applicable);
        let percent = p.percent.as_mut().unwrap();
        percent.since = Some(0);
        percent.observed.insert("gateway-model".into(), 120_000);
        let control = window_control(&p, Some("gateway-model"));
        assert_eq!(control.value, Some(120_000));
        assert!(control.applicable);
    }

    #[test]
    fn a_t3_instance_that_lists_a_variable_twice_keeps_its_own_value() {
        let empty = Map::new();
        let mut merged = Env::default();
        merged.insert(PERCENT_KEY.into(), "60".into());
        let mut controls = compaction_controls(&claude_sources(&empty, &empty, &merged)).unwrap();
        let mut shell = Env::default();
        shell.insert(PERCENT_KEY.into(), "80".into());
        let twice = [PERCENT_KEY.to_string(), PERCENT_KEY.to_string()];
        t3_percent(
            &mut controls,
            &twice,
            "t3.json",
            "claudeAgent",
            false,
            &shell,
        );
        let layers = controls.layers.unwrap();
        assert_eq!(layers.instance.unwrap().env[PERCENT_KEY], "60");
        assert_eq!(layers.shell[PERCENT_KEY], "80");
    }

    #[test]
    fn a_t3_instance_owns_the_window_variable_it_sets_without_the_override() {
        let empty = Map::new();
        let mut env = Env::default();
        env.insert(WINDOW_KEY.into(), "300000".into());
        let mut controls = compaction_controls(&claude_sources(&empty, &empty, &env)).unwrap();
        assert!(controls.percent.is_none());
        assert_eq!(controls.default.owner, WINDOW_KEY);
        t3_percent(
            &mut controls,
            &[WINDOW_KEY.into()],
            "t3.json",
            "claudeAgent",
            false,
            &Env::default(),
        );
        assert_eq!(
            controls.default.owner,
            format!("t3.json:providerInstances.claudeAgent.environment.{WINDOW_KEY}")
        );
        // A settings file's own variable outranks the instance's and keeps its owner.
        let file = map(json!({"env": {WINDOW_KEY: "300000"}}));
        let mut controls = compaction_controls(&claude_sources(&file, &empty, &env)).unwrap();
        let owner = controls.default.owner.clone();
        t3_percent(
            &mut controls,
            &[WINDOW_KEY.into()],
            "t3.json",
            "claudeAgent",
            false,
            &Env::default(),
        );
        assert_eq!(controls.default.owner, owner);
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
        assert_eq!(c[2].value, Some(false));
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
        assert_eq!(c[2].value, Some(false));
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

    #[test]
    fn memory_controls_follow_the_environment_then_settings_with_harness_defaults() {
        let user = map(json!({ "autoMemoryEnabled": false }));
        let managed = map(json!({ "env": { "CLAUDE_CODE_DISABLE_AUTO_MEMORY": "0" } }));
        let scope = |config, file| SettingsScope { config, file };
        let empty = Map::new();
        let default = claude_memory(&[scope(&empty, "user.json")], &Env::default()).unwrap();
        assert_eq!(
            (default.value, default.owner.as_str(), default.off.as_str()),
            (
                true,
                "user.json:autoMemoryEnabled (default)",
                r#""autoMemoryEnabled": false"#
            )
        );
        let set = claude_memory(&[scope(&user, "user.json")], &Env::default()).unwrap();
        assert!(!set.value);
        // The environment wins over the setting, whether from the shell or settings env.
        let shell = env(&[("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1")]);
        let forced = claude_memory(&[scope(&empty, "user.json")], &shell).unwrap();
        assert_eq!(
            (forced.value, forced.off.as_str(), forced.off_file),
            (false, "CLAUDE_CODE_DISABLE_AUTO_MEMORY=1", None)
        );
        let scoped = claude_memory(
            &[scope(&user, "user.json"), scope(&managed, "managed.json")],
            &Env::default(),
        )
        .unwrap();
        assert_eq!(
            (scoped.value, scoped.off_file.as_deref()),
            (true, Some("managed.json"))
        );

        let codex = |config: Value| {
            let config = map(config);
            codex_memory(
                &[SettingsScope {
                    config: &config,
                    file: "config.toml",
                }],
                "config.toml",
            )
            .unwrap()
        };
        assert!(!codex(json!({})).value);
        let on = codex(json!({ "features": { "memories": true } }));
        assert_eq!(
            (on.value, on.off.as_str()),
            (true, "features.memories = false")
        );
        // Keeping them out of the prompt alone still generates new ones.
        let unused = codex(json!({
            "features": { "memories": true },
            "memories": { "use_memories": false },
        }));
        assert!(unused.value);
        let inert = codex(json!({
            "features": { "memories": true },
            "memories": { "use_memories": false, "generate_memories": false },
        }));
        assert_eq!(
            (inert.value, inert.owner.as_str()),
            (
                false,
                "config.toml:memories.use_memories and config.toml:memories.generate_memories"
            )
        );
    }

    // Effective per-model controls honor managed scope, environment ownership and dated IDs.
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

    // Bracketed Claude model controls scan safely and unresolved aliases stay advisory.
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

    // Claude disabling controls and unsupported models suppress saved fast preferences, with exact owners.
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

    // Codex gates are independent, support ultrafast and keep unsupported tiers unknown.
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

    // Invalid speed controls name the effective source file including Windows drive letters.
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

    #[test]
    fn inbound_takes_the_first_trusted_value_and_only_stricter_repository_values() {
        let config = |value: Option<&str>| match value {
            Some(v) => map(json!({ "crossSessionInbound": v })),
            None => Map::new(),
        };
        // Managed, user, then each repository file; the value and the file that decides it.
        let resolve = |managed: Option<&str>, user: Option<&str>, repository: &[Option<&str>]| {
            let configs: Vec<Map<String, Value>> =
                repository.iter().map(|value| config(*value)).collect();
            let names: Vec<String> = (0..configs.len()).map(|i| format!("repo{i}")).collect();
            let scopes: Vec<SettingsScope> = configs
                .iter()
                .zip(&names)
                .map(|(config, file)| SettingsScope { config, file })
                .collect();
            let (managed, user) = (config(managed), config(user));
            let c = claude_inbound(
                SettingsScope {
                    config: &managed,
                    file: "managed",
                },
                SettingsScope {
                    config: &user,
                    file: "user",
                },
                &scopes,
            )
            .unwrap();
            (c.value.map(Inbound::as_str), c.file)
        };
        let at = |value: Option<&'static str>, file: &str| (value, file.to_string());
        // Unset everywhere leaves the per-message default, set in the user's file.
        assert_eq!(resolve(None, None, &[]), at(None, "user"));
        // Managed outranks the user, whatever either says.
        assert_eq!(
            resolve(Some("accept"), Some("refuse"), &[]),
            at(Some("accept"), "managed")
        );
        assert_eq!(resolve(None, Some("hold"), &[]), at(Some("hold"), "user"));
        // A repository value applies only where stricter, even over managed settings.
        assert_eq!(
            resolve(None, Some("hold"), &[Some("accept")]),
            at(Some("hold"), "user")
        );
        assert_eq!(
            resolve(Some("accept"), None, &[Some("hold"), Some("refuse")]),
            at(Some("refuse"), "repo1")
        );
        // With nothing trusted set, a repository accept stays the per-message default.
        assert_eq!(resolve(None, None, &[Some("accept")]), at(None, "user"));
        assert_eq!(
            resolve(None, None, &[Some("hold")]),
            at(Some("hold"), "repo0")
        );
        // A tie holds it in the repository too, unless managed settings decide it.
        assert_eq!(
            resolve(None, Some("hold"), &[Some("hold")]),
            at(Some("hold"), "repo0")
        );
        assert_eq!(
            resolve(Some("hold"), None, &[Some("hold")]),
            at(Some("hold"), "managed")
        );
        // A value Claude Code does not recognize fails where it is set.
        let bad = map(json!({ "crossSessionInbound": "deny" }));
        let none = Map::new();
        let error = claude_inbound(
            SettingsScope {
                config: &none,
                file: "managed",
            },
            SettingsScope {
                config: &bad,
                file: "/synthetic/settings.json",
            },
            &[],
        )
        .unwrap_err();
        assert!(error.message.contains("invalid crossSessionInbound"));
    }
}
