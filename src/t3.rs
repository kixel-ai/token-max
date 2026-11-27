//! T3 discovery and allowlisted metadata, never a second usage or billing ledger.

use crate::collections::{HashMap, HashSet, IndexMap};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, Transaction};
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::{Map, Value, value::RawValue};

use crate::controls::Env;
use crate::discovery::DiscoveryOptions;
use crate::model::*;
use crate::safe::{ScanError, digest, identity, iso, parse_instant};

mod guards;

/// The upstream implementation whose settings and projections this adapter supports.
pub const SOURCE: &str = "https://github.com/pingdotgg/t3code/tree/c4ce7b0fd74f6beb37ed6034f5c8416e5aa7e165 (2026-11-08)";
/// T3's default text-generation model on its default Codex instance.
const DEFAULT_TEXT_MODEL: &str = "gpt-6-luna";
/// Default instances T3 tries, in order, when the text-generation selection is disabled.
const TEXT_FALLBACK: &[&str] = &[
    "codex",
    "claudeAgent",
    "cursor",
    "grok",
    "muse",
    "pi",
    "opencode",
    "antigravity",
];
/// Drivers whose instances are disabled unless enabled explicitly.
const DEFAULT_DISABLED_DRIVERS: &[&str] = &[
    "cursor",
    "grok",
    "fx",
    "kiro",
    "muse",
    "pi",
    "opencode",
    "antigravity",
];
const ENVIRONMENT: &[&str] = &[
    "CLAUDE_CONFIG_DIR",
    "CODEX_HOME",
    "DISABLE_PROMPT_CACHING",
    "FORCE_PROMPT_CACHING_5M",
    "ENABLE_PROMPT_CACHING_1H",
    "CLAUDE_CODE_PROMPT_CACHE_TTL",
    "CLAUDE_CODE_SUBAGENT_PROMPT_CACHE_TTL",
    "CLAUDE_CODE_SUBAGENT_MODEL",
    "CLAUDE_CODE_SUBAGENT_EFFORT",
    "CLAUDE_CODE_SUBAGENT_MODEL_FORCE",
    "CLAUDE_CODE_DISABLE_EXPLORE_INHERIT_CAP",
    "CLAUDE_CODE_DISABLE_FAST_MODE",
    "CLAUDE_CODE_DISABLE_AUTO_MEMORY",
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
    "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE",
    "BASH_MAX_OUTPUT_LENGTH",
    "MAX_MCP_OUTPUT_TOKENS",
    "BASH_MAX_TIMEOUT_MS",
    "BASH_DEFAULT_TIMEOUT_MS",
];

/// Whether an instance environment variable is one discovery reads: one in `ENVIRONMENT` or a
/// Claude provider's.
fn readable_variable(name: &str) -> bool {
    ENVIRONMENT.contains(&name)
        || crate::controls::CLAUDE_PROVIDERS
            .iter()
            .any(|p| p.variable == name)
}

/// The variables only subagent discovery reads, whose secret value leaves just the subagent
/// controls unresolved.
pub(crate) const SUBAGENT_ENVIRONMENT: &[&str] = &[
    "CLAUDE_CODE_SUBAGENT_MODEL",
    "CLAUDE_CODE_SUBAGENT_EFFORT",
    "CLAUDE_CODE_SUBAGENT_MODEL_FORCE",
    "CLAUDE_CODE_DISABLE_EXPLORE_INHERIT_CAP",
];

/// The profile id of a T3 instance, which a removed instance keeps for its retained history.
pub(crate) fn profile_key(state: &str, instance: &str) -> String {
    format!("t3:{state}:{instance}")
}

/// Whether a line's T3 instances can take guidance: each is enabled with resolved settings.
/// A request T3 could not settle opens the control it reads instead, on the control itself
/// (see [`Control`]), so each check decides it by the control it applies.
pub(crate) fn settled(profiles: &[Profile], line: &[String]) -> bool {
    profiles
        .iter()
        .filter(|p| line.contains(&p.id))
        .filter_map(|p| p.t3.as_ref())
        .all(|t| t.enabled && !t.settings_unresolved)
}

/// Retained history is still priced when a current instance cannot receive guidance.
pub(crate) fn constrain<T>(base: &mut CheckBase<T>, profiles: &[Profile]) {
    if !settled(profiles, &base.profiles) {
        base.action = Action::Advisory;
        base.recommended = None;
        base.savings_usd = None;
        base.monthly_savings_usd = None;
        // Upgrading would not give the instance an active setting either.
        base.upgrade = None;
        base.uncertainty.push(UNOWNED.into());
    }
}

/// How the issue on a request whose owner T3 could not settle begins.
pub(crate) const UNSETTLED: &str = "T3/shared native history ownership is unresolved";

/// What that issue adds when T3 names a possible owner outside the request's profiles.
pub(crate) const OUTSIDE: &str = ", and T3 names an owner outside this home";

/// Why a check of a T3 instance gives no verdict when the instance's settings owner is unknown.
pub(crate) const UNOWNED: &str = "T3 instance is disabled or its effective control ownership is unresolved; retained historical costs do not establish an active setting to change";

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Settings {
    provider_instances: IndexMap<String, Instance>,
    providers: IndexMap<String, Box<RawValue>>,
    idle_compaction: Option<T3IdleMode>,
    compact_idle_threads: Option<LegacyIdle>,
    default_model_selection: Option<Selection>,
    text_generation_model_selection: Option<Selection>,
    source_control_writer_model_selection: Option<Selection>,
    review_guide_model_selection: Option<Selection>,
    speech_post_processing_model_selection: Option<Selection>,
    text_generators: IndexMap<String, TextGenerator>,
    project_settings_overrides: IndexMap<String, IndexMap<String, IgnoredAny>>,
}

/// A model API key that generates text directly; only whether it is enabled is read.
#[derive(Deserialize)]
struct TextGenerator {
    #[serde(default = "enabled")]
    enabled: bool,
}
fn enabled() -> bool {
    true
}

/// T3's `ModelSelection`, including the legacy `provider` key; options are not read.
#[derive(Clone, Deserialize)]
#[serde(try_from = "SelectionWire")]
struct Selection(T3Selection);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SelectionWire {
    instance_id: Option<String>,
    provider: Option<Value>,
    model: String,
}

impl TryFrom<SelectionWire> for Selection {
    type Error = &'static str;
    fn try_from(wire: SelectionWire) -> Result<Self, Self::Error> {
        let instance = wire
            .instance_id
            .or(match wire.provider {
                Some(Value::String(provider)) => Some(provider),
                _ => None,
            })
            .ok_or("model selection names no instance")?;
        let model = wire.model.trim();
        if !slug(&instance)
            || model.is_empty()
            || model.len() > 256
            || model.chars().any(char::is_control)
        {
            return Err("invalid model selection");
        }
        Ok(Self(T3Selection {
            instance,
            model: model.into(),
        }))
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum LegacyIdle {
    Bool(bool),
    Mode(T3IdleMode),
}

#[derive(Default)]
struct Instance {
    driver: String,
    enabled: Option<bool>,
    /// `config.enabled` when it is a boolean, for every driver.
    config_enabled: Option<bool>,
    /// `config.customModels` slugs, which let a fallback keep the thread default model.
    custom_models: Vec<String>,
    config: Option<Box<RawValue>>,
    environment: Vec<Environment>,
}

impl Instance {
    /// T3's `resolveProviderInstanceEnabled`: either flag disables, else the driver's default.
    fn resolved_enabled(&self) -> bool {
        if self.enabled == Some(false) || self.config_enabled == Some(false) {
            return false;
        }
        self.enabled
            .or(self.config_enabled)
            .unwrap_or(!DEFAULT_DISABLED_DRIVERS.contains(&self.driver.as_str()))
    }
}

/// `config.enabled` and `config.customModels`, read leniently for every driver because
/// text generation can fall back to any of them.
fn common_config(config: Option<&RawValue>) -> (Option<bool>, Vec<String>) {
    #[derive(Default, Deserialize)]
    #[serde(default, rename_all = "camelCase")]
    struct Common {
        enabled: Value,
        custom_models: Value,
    }
    let common = config
        .and_then(|v| serde_json::from_str::<Common>(v.get()).ok())
        .unwrap_or_default();
    let custom_models = match common.custom_models {
        Value::Array(models) => models
            .iter()
            .filter_map(|m| m.as_str().or_else(|| m.get("slug")?.as_str()))
            .map(|slug| slug.trim().to_string())
            .collect(),
        _ => vec![],
    };
    (common.enabled.as_bool(), custom_models)
}

impl<'de> Deserialize<'de> for Instance {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Default, Deserialize)]
        #[serde(default)]
        struct Fields {
            driver: Option<Box<RawValue>>,
            enabled: Option<Box<RawValue>>,
            config: Option<Box<RawValue>>,
            environment: Option<Box<RawValue>>,
        }
        let raw = Fields::deserialize(d)?;
        let driver = raw
            .driver
            .and_then(|v| serde_json::from_str::<String>(v.get()).ok())
            .unwrap_or_default();
        let (config_enabled, custom_models) = common_config(raw.config.as_deref());
        if !matches!(driver.as_str(), "claudeAgent" | "codex" | "opencode") {
            return Ok(Self {
                enabled: raw.enabled.and_then(|v| serde_json::from_str(v.get()).ok()),
                driver,
                config_enabled,
                custom_models,
                ..Self::default()
            });
        }
        let enabled = raw
            .enabled
            .map(|v| serde_json::from_str(v.get()))
            .transpose()
            .map_err(serde::de::Error::custom)?;
        let environment = raw
            .environment
            .map(|v| serde_json::from_str(v.get()))
            .transpose()
            .map_err(serde::de::Error::custom)?
            .unwrap_or_default();
        Ok(Self {
            driver,
            enabled,
            config_enabled,
            custom_models,
            environment,
            config: raw.config,
        })
    }
}

/// The exclusion a T3 state carries when unread machine policy may move its history homes.
pub(crate) const POLICY_GAP: &str = "T3 machine managed policy is present, inaccessible or unsupported on this platform; it may override homes and settings, so discovery coverage and current control ownership are unresolved";

/// Platform policy can replace homes and controls. Do not claim user-file ownership
/// when that uninspected layer exists (or cannot be checked).
fn managed_policy_unresolved() -> bool {
    let paths: &[&str] = if cfg!(target_os = "macos") {
        &[
            "/Library/Application Support/T3Code/policy.json",
            "/Library/Managed Preferences/com.t3tools.t3code.plist",
        ]
    } else if cfg!(windows) {
        // Windows also has a machine registry policy, which this adapter does not read.
        return true;
    } else {
        &["/etc/t3code/policy.json"]
    };
    paths
        .iter()
        .any(|path| !matches!(fs::metadata(path), Err(e) if e.kind()==ErrorKind::NotFound))
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Config {
    home_path: String,
    shadow_home_path: String,
    setup_mode: Option<String>,
    auto_compact_window: String,
    idle_compaction: Option<String>,
    launch_args: String,
}

/// The flags in T3's launch arguments, tokenized and parsed as T3's `parseCliArgs` does:
/// arguments after `--` and single-dash options are operands, not flags.
fn launch_flags(args: &str) -> HashSet<String> {
    // JavaScript's `\s` matches the byte order mark but not NEL.
    let space = |c: char| (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}';
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut quoted = false;
    let mut chars = args.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else if c == '\\'
                && q == '"'
                && let Some(&next) = chars.peek().filter(|n| "\"\\$`".contains(**n))
            {
                current.push(next);
                chars.next();
            } else {
                current.push(c);
            }
        } else if c == '\'' || c == '"' {
            quote = Some(c);
            quoted = true;
        } else if space(c) {
            if !current.is_empty() || quoted {
                tokens.push(std::mem::take(&mut current));
                quoted = false;
            }
        } else if c == '\\'
            && let Some(&next) = chars.peek().filter(|n| space(**n) || "'\"".contains(**n))
        {
            current.push(next);
            chars.next();
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() || quoted {
        tokens.push(current);
    }
    let mut flags = HashSet::default();
    let mut tokens = tokens.into_iter().peekable();
    while let Some(token) = tokens.next() {
        if token == "--" {
            break;
        }
        let Some(rest) = token.strip_prefix("--") else {
            continue;
        };
        match rest.split_once('=') {
            Some((name, _)) => {
                flags.insert(name.to_string());
            }
            None => {
                tokens.next_if(|next| !next.starts_with("--"));
                flags.insert(rest.to_string());
            }
        }
    }
    flags
}

/// Unknown environment values and secret references are skipped, not deserialized as strings.
struct Environment {
    name: String,
    value: Option<String>,
}
impl<'de> Deserialize<'de> for Environment {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Entry<'a> {
            name: String,
            #[serde(borrow)]
            value: Option<&'a RawValue>,
            #[serde(default)]
            sensitive: bool,
            #[serde(borrow)]
            secret: Option<&'a RawValue>,
        }
        let entry = Entry::deserialize(d)?;
        let readable = !entry.sensitive && entry.secret.is_none();
        // Only the code intelligence card reads it, so a value that isn't a string leaves it
        // alone unread rather than failing the scan.
        if entry.name == crate::lsp::CACHE_DIR {
            let value = entry
                .value
                .filter(|_| readable)
                .and_then(|v| serde_json::from_str(v.get()).ok());
            return Ok(Self {
                name: entry.name,
                value,
            });
        }
        let allowed = readable_variable(&entry.name) && readable;
        let value = if allowed {
            entry
                .value
                .map(|v| serde_json::from_str(v.get()))
                .transpose()
                .map_err(serde::de::Error::custom)?
        } else {
            None
        };
        Ok(Self {
            name: entry.name,
            value,
        })
    }
}

fn fail(message: &str, path: &Path) -> ScanError {
    ScanError::at(
        message,
        Location {
            file: path.to_string_lossy().as_ref().into(),
            line: 0,
        },
    )
}
fn expand(home: &str, path: &str) -> PathBuf {
    if path == "~" {
        PathBuf::from(home)
    } else if let Some(suffix) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        Path::new(home).join(suffix)
    } else {
        PathBuf::from(path)
    }
}
fn absolute(path: PathBuf) -> Result<PathBuf, ScanError> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()
            .map_err(|_| ScanError::new("cannot resolve T3 path"))?
            .join(path))
    }
}
fn slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.as_bytes()[0].is_ascii_alphabetic()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}

/// Resolve the model T3 generates titles, branch names, PR text and other text with,
/// as its server does with no project selected and provider snapshots unavailable.
fn text_generation(
    settings: &Settings,
    instances: &IndexMap<String, Instance>,
    file: &Path,
) -> T3TextGeneration {
    let generator = |id: &str| settings.text_generators.get(id).map(|g| g.enabled);
    let instance = |id: &str| instances.get(id).is_some_and(Instance::resolved_enabled);
    // A text generator shares the instance id namespace and takes precedence.
    let enabled = |id: &str| generator(id).unwrap_or_else(|| instance(id));
    let (selection, origin) = match &settings.text_generation_model_selection {
        Some(Selection(s)) => (s.clone(), "set"),
        None => (
            T3Selection {
                instance: "codex".into(),
                model: DEFAULT_TEXT_MODEL.into(),
            },
            "T3 default",
        ),
    };
    let usable = enabled(&selection.instance)
        && instances
            .get(&selection.instance)
            .is_none_or(|i| i.driver != "acpRegistry");
    let (selection, resolution) = if usable {
        (selection, origin.to_string())
    } else if let Some(&id) = TEXT_FALLBACK.iter().find(|&&id| instance(id)) {
        let model = match &settings.default_model_selection {
            Some(Selection(d))
                if d.instance == id && instances[id].custom_models.contains(&d.model) =>
            {
                d.model.clone()
            }
            _ => match id {
                "claudeAgent" => "claude-haiku-5-5",
                "cursor" => "composer-2",
                "grok" => "grok-build",
                "muse" | "pi" => "default",
                "opencode" => "opencode/big-pickle",
                "antigravity" => "antigravity-default",
                _ => DEFAULT_TEXT_MODEL,
            }
            .into(),
        };
        (
            T3Selection {
                instance: id.into(),
                model,
            },
            format!(
                "{origin} {} cannot generate text, so T3 falls back to instance {id}",
                selection.instance
            ),
        )
    } else {
        (
            selection,
            format!("{origin}, but no enabled instance can generate text"),
        )
    };
    let mut overrides = IndexMap::default();
    for (key, value, usable) in [
        (
            "sourceControlWriterModelSelection",
            &settings.source_control_writer_model_selection,
            &enabled as &dyn Fn(&str) -> bool,
        ),
        (
            // Text generators cannot investigate a checkout, so they never write review guides.
            "reviewGuideModelSelection",
            &settings.review_guide_model_selection,
            &|id: &str| generator(id).is_none() && instance(id),
        ),
        (
            "speechPostProcessingModelSelection",
            &settings.speech_post_processing_model_selection,
            &enabled,
        ),
    ] {
        if let Some(Selection(s)) = value
            && usable(&s.instance)
        {
            overrides.insert(key.to_string(), s.clone());
        }
    }
    const SCOPED: [&str; 3] = [
        "textGenerationModelSelection",
        "sourceControlWriterModelSelection",
        "reviewGuideModelSelection",
    ];
    T3TextGeneration {
        selection,
        owner: format!("{}:textGenerationModelSelection", file.display()),
        resolution,
        overrides,
        project_overrides: settings
            .project_settings_overrides
            .values()
            .filter(|o| SCOPED.iter().any(|k| o.contains_key(*k)))
            .count() as u64,
    }
}

/// Add distinct T3 control profiles while sharing the native history snapshot walk.
pub(crate) fn discover(
    options: &DiscoveryOptions,
    profiles: &mut Vec<Profile>,
) -> Result<Vec<T3State>, ScanError> {
    let mut paths = Vec::new();
    if !options.explicit() {
        paths.push((Path::new(&options.home).join(".t3/userdata"), false));
        let dev = Path::new(&options.home).join(".t3/dev");
        if dev.exists() {
            paths.push((dev, false));
        }
        if let Some(root) = options
            .env
            .get("T3CODE_HOME")
            .filter(|s| !s.trim().is_empty())
        {
            paths.push((expand(&options.home, root.trim()).join("userdata"), true));
        }
    }
    paths.extend(
        options
            .t3_dirs
            .iter()
            .map(|s| (expand(&options.home, s), true)),
    );
    let mut states = Vec::new();
    let mut seen = HashSet::default();
    for (path, required) in paths {
        let path = absolute(path)?;
        let path = match crate::discovery::realpath(&path.to_string_lossy()) {
            Ok(path) => PathBuf::from(path),
            Err(e) if e.kind() == ErrorKind::NotFound && !required => {
                states.push(T3State {
                    path: path.to_string_lossy().into_owned(),
                    status: "missing".into(),
                    exclusions: vec![],
                    metadata: "not selected".into(),
                    snapshot_started_at: None,
                    snapshot_finished_at: None,
                    ambiguous_requests: 0,
                    unresolved: IndexMap::default(),
                    removed: IndexMap::default(),
                    unsupported: 0,
                    missing: 0,
                    instances: HashSet::default(),
                    text_generation: None,
                });
                continue;
            }
            Err(_) => return Err(fail("selected T3 state is missing or inaccessible", &path)),
        };
        if !seen.insert(path.clone()) {
            continue;
        }
        let file = path.join("settings.json");
        let mut settings = match fs::read(&file) {
            Ok(bytes) => serde_json::from_slice::<Settings>(&bytes)
                .map_err(|_| fail("invalid T3 settings", &file))?,
            Err(e) if e.kind() == ErrorKind::NotFound => Settings::default(),
            Err(_) => return Err(fail("cannot read T3 settings", &file)),
        };
        let mut instances = std::mem::take(&mut settings.provider_instances);
        for (driver, config) in std::mem::take(&mut settings.providers) {
            let (config_enabled, custom_models) = common_config(Some(&config));
            instances.entry(driver.clone()).or_insert(Instance {
                driver,
                config_enabled,
                custom_models,
                config: Some(config),
                ..Default::default()
            });
        }
        if options.defaults {
            // Each instance keeps its driver and the home it reads; every setting is T3's default.
            settings = Settings::default();
            for instance in instances.values_mut() {
                instance.enabled = None;
                instance.config_enabled = None;
                instance.custom_models.clear();
                // Only the variables that choose its home or its provider stay.
                instance.environment.retain(|e| {
                    matches!(e.name.as_str(), "CLAUDE_CONFIG_DIR" | "CODEX_HOME")
                        || crate::controls::CLAUDE_PROVIDERS
                            .iter()
                            .any(|p| p.variable == e.name)
                });
            }
        }
        let global = settings
            .idle_compaction
            .unwrap_or(match settings.compact_idle_threads {
                Some(LegacyIdle::Mode(mode)) => mode,
                Some(LegacyIdle::Bool(true)) => T3IdleMode::BeforeCacheExpires,
                _ => T3IdleMode::OnReturn,
            });
        // Only an instance the settings name counts as a missing history home: T3 adds the
        // built-ins below even for a user who never set up that provider.
        let named: HashSet<String> = instances.keys().cloned().collect();
        // T3 restores these built-in instances even before settings have been saved.
        for driver in ["claudeAgent", "codex"] {
            instances.entry(driver.into()).or_insert(Instance {
                driver: driver.into(),
                ..Default::default()
            });
        }
        let mut state = T3State {
            path: path.to_string_lossy().into_owned(),
            status: "selected".into(),
            exclusions: vec!["Native terminal suggestions, away summaries, goal check-ins, Codex TUI recaps and terminal keepalive guidance do not apply to T3; their native toggles do not control T3 helpers".into()],
            metadata: "not read".into(),
            snapshot_started_at: None,
            snapshot_finished_at: None,
            ambiguous_requests: 0,
            unresolved: IndexMap::default(),
            removed: IndexMap::default(),
            unsupported: 0,
            missing: 0,
            instances: instances.keys().cloned().collect(),
            text_generation: Some(text_generation(&settings, &instances, &file)),
        };
        // Recorded before any home is probed: policy may have moved a missing home elsewhere.
        if managed_policy_unresolved() {
            state.exclusions.push(POLICY_GAP.into());
        }
        for (id, instance) in instances {
            let enabled = instance.resolved_enabled();
            let harness = match instance.driver.as_str() {
                "claudeAgent" => Harness::Claude,
                "codex" => Harness::Codex,
                "opencode" => {
                    if !slug(&id) {
                        return Err(fail("invalid supported T3 provider identity", &file));
                    }
                    let found = opencode_instance(options, &state, &id, instance, &file)?;
                    match found {
                        Ok(profile) => profiles.push(profile),
                        Err(exclusion) => {
                            if exclusion.missing && named.contains(&id) {
                                state.missing += 1;
                            }
                            if exclusion.unsupported {
                                state.unsupported += 1;
                            }
                            state.exclusions.push(exclusion.reason);
                        }
                    }
                    continue;
                }
                _ => {
                    state.unsupported += 1;
                    state.exclusions.push(if slug(&id) && slug(&instance.driver) {format!("T3 instance {id} ({}) is unsupported; its spend is excluded",instance.driver)} else {"T3 provider instance has an unsupported identity/driver; its spend is excluded".into()});
                    continue;
                }
            };
            if !slug(&id) {
                return Err(fail("invalid supported T3 provider identity", &file));
            }
            let mut env: Env = options
                .env
                .iter()
                // Where plugins are, which only the code intelligence card reads.
                .filter(|(k, _)| readable_variable(k) || k.as_str() == crate::lsp::CACHE_DIR)
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let mut unresolved_env = false;
            // A plugins directory token-max can't read leaves only the card's plugins unread,
            // not the settings every other line reads.
            let mut secret_plugins = false;
            let mut own_environment = vec![];
            for entry in instance.environment {
                if entry.name == crate::lsp::CACHE_DIR {
                    match entry.value {
                        Some(value) => {
                            env.insert(entry.name, value);
                        }
                        None => {
                            secret_plugins = true;
                            env.remove(&entry.name);
                        }
                    }
                } else if readable_variable(&entry.name) {
                    match entry.value {
                        Some(value) => {
                            if !own_environment.contains(&entry.name) {
                                own_environment.push(entry.name.clone());
                            }
                            env.insert(entry.name, value);
                        }
                        // A secret subagent variable leaves only the subagent controls
                        // unresolved, which their discovery reads from the owned name.
                        None if SUBAGENT_ENVIRONMENT.contains(&entry.name.as_str()) => {
                            env.remove(&entry.name);
                            if !own_environment.contains(&entry.name) {
                                own_environment.push(entry.name.clone());
                            }
                        }
                        None => {
                            env.remove(&entry.name);
                            unresolved_env = true;
                        }
                    }
                }
            }
            let mut config = instance
                .config
                .map(|raw| serde_json::from_str::<Config>(raw.get()))
                .transpose()
                .map_err(|_| fail("invalid supported T3 provider controls", &file))?
                .unwrap_or_default();
            if options.defaults {
                config = Config {
                    home_path: config.home_path,
                    shadow_home_path: config.shadow_home_path,
                    setup_mode: config.setup_mode,
                    ..Config::default()
                };
            }
            let policy_unresolved = managed_policy_unresolved();
            let variable = if harness == Harness::Claude {
                "CLAUDE_CONFIG_DIR"
            } else {
                "CODEX_HOME"
            };
            let default = if harness == Harness::Claude {
                ".claude"
            } else {
                ".codex"
            };
            let configured = config.home_path.trim();
            // A configured shadow home suppresses the environment-home fallback in T3.
            let native_root = if !configured.is_empty() {
                expand(&options.home, configured)
            } else if (harness == Harness::Claude
                || (config.shadow_home_path.trim().is_empty()
                    && config.setup_mode.as_deref() != Some("managed")))
                && let Some(root) = env.get(variable).filter(|s| !s.trim().is_empty())
            {
                PathBuf::from(root.trim())
            } else {
                Path::new(&options.home).join(default)
            };
            let root = match fs::canonicalize(&native_root) {
                Ok(root) => root,
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    if named.contains(&id) {
                        state.missing += 1;
                    }
                    state.exclusions.push(format!(
                        "T3 instance {id}: native history home {} is missing",
                        native_root.display()
                    ));
                    continue;
                }
                Err(_) => return Err(fail("cannot inspect T3 provider home", &native_root)),
            };
            let root = crate::discovery::realpath(&root.to_string_lossy())
                .map_err(|_| fail("cannot resolve T3 provider home", &root))?;
            env.insert(variable.into(), root.clone());
            let local = DiscoveryOptions {
                env,
                ..options.clone()
            };
            let key = profile_key(&state.path, &id);
            let mut profile = crate::discovery::profile(&local, harness, root.clone(), root, &key)?;
            if let Some(plugins) = profile.plugins.as_mut().filter(|_| secret_plugins) {
                plugins.dir = None;
                plugins.cache_dir = None;
            }
            let owner = format!("{}:providerInstances.{id}", file.display());
            let (idle_mode, idle_owner) = match config.idle_compaction.as_deref() {
                None | Some("default") => (
                    global,
                    format!("{}:idleCompaction (instance {id}: default)", file.display()),
                ),
                Some(value) => (
                    match value {
                        "onReturn" => T3IdleMode::OnReturn,
                        "beforeCacheExpires" => T3IdleMode::BeforeCacheExpires,
                        "off" => T3IdleMode::Off,
                        _ => return Err(fail("invalid T3 idle policy", &file)),
                    },
                    format!("{owner}.config.idleCompaction"),
                ),
            };
            let mut limitations = vec!["T3's current thread selection and new-thread browser default are unavailable with the server closed; historical requested speed does not establish current selection or billed speed".into()];
            // Only these Claude flags change the settings Claude Code reads; Codex launch
            // arguments can override any config key and are not inspected.
            let launch_unresolved = if harness == Harness::Claude {
                let flags = launch_flags(&config.launch_args);
                flags.contains("settings") || flags.contains("setting-sources")
            } else {
                !config.launch_args.trim().is_empty()
            };
            if launch_unresolved {
                limitations.push(if harness == Harness::Claude {
                    "T3 launchArgs pass --settings or --setting-sources, which can replace native settings; affected native fallback controls remain advisory"
                } else {
                    "T3 launchArgs are present; arbitrary CLI overrides are not inspected, so affected native fallback controls remain advisory"
                }.into());
            }
            if unresolved_env {
                limitations.push("a relevant T3 environment override uses a secret or redacted value; it was not read and effective ownership is unresolved".into());
            }
            if policy_unresolved {
                limitations.push(POLICY_GAP.into());
                profile.ttl.main.value = None;
                profile.ttl.subagent.value = None;
            }
            if harness == Harness::Claude && !config.auto_compact_window.is_empty() {
                let value = config
                    .auto_compact_window
                    .trim()
                    .parse::<i64>()
                    .ok()
                    .filter(|n| (100_000..=1_000_000).contains(n))
                    .ok_or_else(|| fail("invalid T3 compaction window", &file))?;
                profile.compaction.models.clear();
                profile.compaction.default = WindowControl {
                    value: Some(value),
                    owner: format!("{owner}.config.autoCompactWindow"),
                    reason:
                        "T3 supplies this SDK setting above native user/project window preferences"
                            .into(),
                    applicable: enabled && !unresolved_env,
                    minimum_claude_patch: None,
                    comparison_unknown: None,
                    unset: false,
                    setting: None,
                };
            }
            if harness == Harness::Claude {
                crate::controls::t3_percent(
                    &mut profile.compaction,
                    &own_environment,
                    &file.display().to_string(),
                    &id,
                    !config.auto_compact_window.is_empty(),
                    &options.env,
                );
            }
            let resolved = enabled && !unresolved_env && !launch_unresolved && !policy_unresolved;
            if !resolved {
                profile.compaction.default.comparison_unknown = Some("T3 instance is disabled or effective environment/launch overrides are unresolved".into());
            }
            if let Some(reason) = &profile.compaction.default.comparison_unknown {
                for control in profile.compaction.models.values_mut() {
                    control.comparison_unknown = Some(reason.clone());
                }
            }
            let speed = profile
                .speed
                .as_mut()
                .expect("native profiles have speed controls");
            if options.defaults {
                // A thread's selection defaults to no fastMode or serviceTier, so a Claude
                // thread runs at standard speed and a Codex one at the native default, unless
                // machine policy could change either.
                speed.owner = format!("T3 instance {id}: thread model selection");
                if harness == Harness::Claude {
                    speed.value = Some("standard".into());
                    speed.reason = "T3 defaults Claude fastMode to false".into();
                }
                speed.resolved = speed.resolved && !policy_unresolved;
            } else if harness == Harness::Claude {
                speed.value = None;
                speed.owner = format!("T3 instance {id}: thread modelSelection.options.fastMode");
                speed.replacement = "fastMode=false in the T3 thread model selection".into();
                speed.reason = "T3 explicitly defaults supported Claude fastMode to false and overrides native fastMode; current browser/thread selection is unavailable".into();
            } else {
                speed.owner = format!(
                    "T3 instance {id}: thread modelSelection.options.serviceTier; native fallback {}",
                    speed.owner
                );
                speed.value = None;
                speed.replacement = "serviceTier=default in the T3 thread model selection".into();
                speed.reason = "T3 passes a selected serviceTier (legacy fastMode=true maps to fast); omission can inherit native service_tier; current selection is unavailable".into();
            }
            if !options.defaults {
                speed.resolved = false;
            }
            let owner = format!("T3 instance {id}: thread model selection");
            profile.effort = Some(crate::model::EffortControl {
                model: None,
                model_owner: owner.clone(),
                level: None,
                owner,
                effective: None,
                lowered: None,
                caps: vec![],
                default: None,
                unresolved: Some(
                    "T3 picks the model and effort for each thread, and its current selection is unavailable"
                        .into(),
                ),
                picked: None,
                provider: None,
            });
            // These terminal-client features do not run in T3's SDK/app-server client.
            profile.background.clear();
            profile.idle = None;
            profile.t3 = Some(T3Instance {
                state: state.path.clone(),
                instance: id,
                enabled,
                idle_mode,
                idle_owner,
                settings_unresolved: unresolved_env || launch_unresolved || policy_unresolved,
                limitations,
                env: (resolved && harness == Harness::Claude).then(|| local.env.clone()),
                // Without its own window, T3 passes no window, so Claude Code reads the
                // same user, project and local settings as a native session.
                inherited_window: config.auto_compact_window.is_empty(),
                environment: own_environment,
                retained_since: None,
            });
            if harness == Harness::Codex {
                // Project layers are resolved only for native Codex homes.
                profile.instruction_budget_unresolved = Some(
                    "T3 Codex instances can take project_doc_max_bytes from project or launch configuration, which is not resolved for them".into(),
                );
            }
            profiles.push(profile);
        }
        states.push(state);
    }
    Ok(states)
}

/// Why a T3 OpenCode instance has no profile.
struct Excluded {
    reason: String,
    /// Its data directory does not exist.
    missing: bool,
    /// Its history is somewhere token-max does not read.
    unsupported: bool,
}

/// A T3 OpenCode instance's profile: the data directory and config the server T3 spawns
/// reads, under T3's environment and the instance's own. An instance that connects to a
/// server at `serverUrl` keeps its history wherever that server runs, so it is excluded.
fn opencode_instance(
    options: &DiscoveryOptions,
    state: &T3State,
    id: &str,
    instance: Instance,
    file: &Path,
) -> Result<Result<Profile, Excluded>, ScanError> {
    #[derive(Default, Deserialize)]
    #[serde(default, rename_all = "camelCase")]
    struct OpenCodeConfig {
        server_url: String,
    }
    let enabled = instance.resolved_enabled();
    let config = instance
        .config
        .map(|raw| serde_json::from_str::<OpenCodeConfig>(raw.get()))
        .transpose()
        .map_err(|_| fail("invalid supported T3 provider controls", file))?
        .unwrap_or_default();
    if !config.server_url.trim().is_empty() {
        return Ok(Err(Excluded {
            reason: format!(
                "T3 instance {id} connects to an OpenCode server at its serverUrl, whose history token-max does not read; its spend is excluded"
            ),
            missing: false,
            unsupported: true,
        }));
    }
    // The variables that move OpenCode's data directory or config.
    const LOCATIONS: [&str; 4] = [
        "XDG_DATA_HOME",
        "XDG_CONFIG_HOME",
        "OPENCODE_CONFIG",
        "OPENCODE_DB",
    ];
    let mut env = options.env.clone();
    let mut own_environment = vec![];
    for entry in instance.environment {
        if !LOCATIONS.contains(&entry.name.as_str()) {
            continue;
        }
        let Some(value) = entry.value else {
            return Ok(Err(Excluded {
                reason: format!(
                    "T3 instance {id} sets {} to a secret value, so its OpenCode history is not located; its spend is excluded",
                    entry.name
                ),
                missing: false,
                unsupported: true,
            }));
        };
        own_environment.push(entry.name.clone());
        env.insert(entry.name, value);
    }
    let native_root = crate::opencode::data_dir(&options.home, &env);
    let root = match crate::discovery::realpath(&native_root) {
        Ok(root) => root,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return Ok(Err(Excluded {
                reason: format!("T3 instance {id}: native history home {native_root} is missing"),
                missing: true,
                unsupported: false,
            }));
        }
        Err(_) => {
            return Err(fail(
                "cannot inspect T3 provider home",
                Path::new(&native_root),
            ));
        }
    };
    let local = DiscoveryOptions {
        env,
        ..options.clone()
    };
    let key = profile_key(&state.path, id);
    let mut profile = crate::opencode::profile(&local, root.clone(), root, &key)?;
    let policy_unresolved = managed_policy_unresolved();
    profile.t3 = Some(T3Instance {
        state: state.path.clone(),
        instance: id.into(),
        enabled,
        // T3's idle compaction drives Claude sessions only.
        idle_mode: T3IdleMode::Off,
        idle_owner: format!("{}:providerInstances.{id}", file.display()),
        settings_unresolved: policy_unresolved,
        limitations: if policy_unresolved {
            vec![POLICY_GAP.into()]
        } else {
            vec![]
        },
        env: None,
        inherited_window: true,
        environment: own_environment,
        retained_since: None,
    });
    Ok(Ok(profile))
}

/// Link explicit T3 idle run identity through the shared native summary/boundary owner.
pub(crate) fn attribute_compactions(scan: &mut Scan, charges: &[CompactionCharge]) {
    for charge in charges {
        let Some(evidence) = charge
            .request
            .as_ref()
            .and_then(|id| {
                scan.requests
                    .iter()
                    .chain(&scan.cache_context.requests)
                    .find(|r| *r.id == **id)
            })
            .and_then(|r| r.t3.as_ref())
            .filter(|t| t.idle_mode.is_some())
        else {
            continue;
        };
        if let Some(event) = scan
            .events
            .iter_mut()
            .chain(&mut scan.cache_context.boundaries)
            .find(|e| e.id == charge.event)
        {
            event.t3 = Some(evidence.clone());
            event.idle_attribution = IdleAttribution::T3;
        }
    }
}

/// Private native keys exist only until metadata has been joined; all exported identities stay hashed.
#[derive(Default)]
pub(crate) struct NativeIds {
    ids: HashMap<String, String>,
    /// A subagent session's key to its parent's, whose T3 owner it shares.
    parents: HashMap<String, String>,
    items: HashMap<String, Vec<String>>,
    events: HashMap<Id, String>,
    last_session: (String, String),
    last_turn: String,
    last_message: (String, String),
}
impl NativeIds {
    /// Record one OpenCode session's native id, and its parent's for a subagent; `session`
    /// is the key the reader gave it.
    pub(crate) fn opencode(&mut self, session: &str, native: &str, parent: Option<&str>) {
        self.ids
            .entry(identity(native))
            .or_insert_with(|| native.into());
        if let Some(parent) = parent {
            let key = identity(parent);
            self.ids.entry(key.clone()).or_insert_with(|| parent.into());
            self.parents.insert(session.into(), key);
        }
    }

    /// Record one transcript record's native keys; `session` is the key the reader gave it.
    pub(crate) fn observe(&mut self, raw: &Map<String, Value>, harness: Harness, session: &str) {
        let payload = raw.get("payload").and_then(Value::as_object);
        // A Claude subagent file records its parent's session id under its own session key.
        let (native, parent) = if harness == Harness::Claude {
            (raw.get("sessionId").and_then(Value::as_str), None)
        } else if raw.get("type").and_then(Value::as_str) == Some("session_meta")
            && let Some(p) = payload
        {
            let thread = crate::reader::CodexThread::parse(p);
            (thread.id, thread.parent)
        } else {
            (None, None)
        };
        if let Some(native) = native
            && (native != self.last_session.0 || session != self.last_session.1)
        {
            let key = identity(native);
            self.ids.entry(key.clone()).or_insert_with(|| native.into());
            let parent = match parent {
                Some(parent) => {
                    let parent_key = identity(parent);
                    self.ids
                        .entry(parent_key.clone())
                        .or_insert_with(|| parent.into());
                    parent_key
                }
                None => key,
            };
            if parent != session {
                self.parents.insert(session.into(), parent);
            }
            self.last_session = (native.into(), session.into());
        }
        if harness == Harness::Codex
            && let Some(turn) = payload
                .and_then(|p| p.get("turn_id"))
                .and_then(Value::as_str)
            && turn != self.last_turn
        {
            self.ids
                .entry(identity(turn))
                .or_insert_with(|| turn.into());
            self.last_turn = turn.into();
        }
        if harness != Harness::Claude {
            return;
        }
        if raw.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
            && let Some(uuid) = raw.get("uuid").and_then(Value::as_str)
        {
            self.events
                .insert(digest(&format!("claude:compaction:{uuid}")), uuid.into());
        }
        if raw.get("type").and_then(Value::as_str) != Some("assistant") {
            return;
        }
        let message = raw.get("message").and_then(Value::as_object);
        let message_id = message.and_then(|m| m.get("id")).filter(|v| !v.is_null());
        let name = message_id
            .or_else(|| raw.get("requestId").filter(|v| !v.is_null()))
            .or_else(|| raw.get("requestRef"))
            .and_then(Value::as_str);
        let (Some(name), Some(uuid)) = (name, raw.get("uuid").and_then(Value::as_str)) else {
            return;
        };
        if self.last_message.0 != name {
            self.last_message = (name.into(), format!("claude:{}", identity(name)));
        }
        let items = self.items.entry(self.last_message.1.clone()).or_default();
        let mut add = |item: &str| {
            if !items.iter().any(|v| v == item) {
                items.push(item.into());
            }
        };
        // T3 keys text by record uuid, tools by tool-use id, and thinking by
        // `<message id>:thinking:<n>`, which `item_key` reduces to `<message id>:thinking`.
        add(uuid);
        for block in message
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            match block.get("type").and_then(Value::as_str) {
                Some("tool_use" | "server_tool_use") => {
                    if let Some(id) = block.get("id").and_then(Value::as_str) {
                        add(id);
                    }
                }
                Some("thinking") => {
                    if let Some(id) = message_id.and_then(Value::as_str) {
                        add(&format!("{id}:thinking"));
                    }
                }
                _ => {}
            }
        }
    }

    /// The session whose T3 owner a request's session shares: itself unless it is a subagent's.
    fn root<'a>(&'a self, session: &'a str) -> &'a str {
        let mut root = session;
        // Bounded so a cycle in recorded parent ids cannot loop.
        for _ in 0..self.parents.len() {
            match self.parents.get(root) {
                Some(parent) => root = parent,
                None => break,
            }
        }
        root
    }
}

const ITEM_PREFIX: &str = "turn-item:provider:claudeAgent:native-item:";

/// The native item id inside a T3 Claude turn item id, with a thinking block's index dropped.
/// T3 derives the id as `turn-item:provider:claudeAgent:native-item:` plus the
/// URI-component-encoded native id, so no item payload (tool output) needs reading.
fn item_key(turn_item: &str) -> Option<String> {
    let encoded = turn_item.strip_prefix(ITEM_PREFIX)?;
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            let digit = |b: u8| (b as char).to_digit(16);
            decoded.push((digit(hex[0])? * 16 + digit(hex[1])?) as u8);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    let native = String::from_utf8(decoded).ok()?;
    Some(match native.rsplit_once(":thinking:") {
        Some((message, index))
            if !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit()) =>
        {
            format!("{message}:thinking")
        }
        _ => native,
    })
}

/// The primary-key range of the T3 Claude turn items for one native key that `item_key`
/// returns: that id exactly, or each `:thinking:<n>` block of a message.
fn item_range(native: &str) -> (String, String) {
    let encode = |s: &str| {
        // JavaScript's encodeURIComponent, which T3 applies to the native id.
        let mut out = String::from(ITEM_PREFIX);
        for b in s.bytes() {
            if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
                out.push(b as char);
            } else {
                out.push_str(&format!("%{b:02X}"));
            }
        }
        out
    };
    match native.strip_suffix(":thinking") {
        Some(message) => {
            let key = encode(&format!("{message}:thinking:"));
            // `%3A` is the encoded colon; `%3B` sorts just after every `%3A` suffix.
            let end = format!("{}B", &key[..key.len() - 1]);
            (key, end)
        }
        // Only the id itself sorts at or above it and below it followed by NUL.
        None => {
            let key = encode(native);
            let end = format!("{key}\0");
            (key, end)
        }
    }
}

/// Borrow only join keys; native billing parts and locations never need copying.
struct NativeRequest<'a> {
    id: &'a str,
    /// The key of the session that owns the request: its root for a subagent's.
    root: &'a str,
    native_session: &'a str,
    native_turns: Vec<&'a str>,
    harness: Harness,
    time: Millis,
    model: Option<&'a str>,
    profiles: &'a [String],
}
fn native_request<'a>(r: &'a Request, ids: &'a NativeIds) -> Option<NativeRequest<'a>> {
    let root = ids.root(&r.session);
    Some(NativeRequest {
        id: &r.id,
        root,
        native_session: ids.ids.get(root)?,
        // T3 records turns for the main conversation only; a subagent shares its root's owner.
        native_turns: if r.agent == Agent::Subagent || r.harness == Harness::OpenCode {
            // T3 records no OpenCode turn token-max joins; the session's owner decides.
            vec![]
        } else if r.harness == Harness::Codex {
            r.turn
                .as_ref()
                .and_then(|t| ids.ids.get(&**t))
                .map(String::as_str)
                .into_iter()
                .collect()
        } else {
            ids.items
                .get(&*r.id)
                .into_iter()
                .flatten()
                .map(String::as_str)
                .collect()
        },
        harness: r.harness,
        time: r.time,
        model: r.model.as_deref(),
        profiles: &r.profiles,
    })
}

// T3 indexes no Claude native identity (its one native index covers Codex turns), so one
// pass over provider threads maps the selected native sessions to T3 threads. Every
// later lookup follows T3's indexes from those threads, so cost tracks the selected
// history, not the database. No Usage totals, messages, prompts, credentials, or
// arbitrary JSON objects leave SQLite.
const PROVIDER_THREADS: &str = "SELECT provider_thread_id, driver,
 json_extract(payload_json,'$.nativeThreadRef.nativeId'), provider_instance_id, updated_at<=?1
 FROM orchestration_v2_projection_provider_threads WHERE driver IN ('claudeAgent','codex','opencode')";
// A provider thread updated after the native bound owns its session only through an
// earlier event that already linked it.
const OWNER_EVENT: &str = "SELECT EXISTS (SELECT 1 FROM orchestration_v2_projection_provider_threads pt
 JOIN orchestration_events e ON e.stream_id=pt.thread_id
 WHERE pt.provider_thread_id=?1 AND e.application_event_version=2 AND e.aggregate_kind='thread'
   AND e.event_type='provider-thread.updated' AND e.occurred_at<=?2
   AND json_extract(e.payload_json,'$.id')=pt.provider_thread_id
   AND json_extract(e.payload_json,'$.providerInstanceId')=pt.provider_instance_id
   AND json_extract(e.payload_json,'$.nativeThreadRef.driver')=pt.driver
   AND json_extract(e.payload_json,'$.nativeThreadRef.nativeId')=json_extract(pt.payload_json,'$.nativeThreadRef.nativeId'))";
// A provider turn's native turn reference: its driver, and its id when that is a string.
macro_rules! native_turn_ref {
    () => {
        "json_extract(payload_json,'$.nativeTurnRef.driver'),
 CASE WHEN json_type(payload_json,'$.nativeTurnRef.nativeId')='text'
 THEN json_extract(payload_json,'$.nativeTurnRef.nativeId') END"
    };
}
// One Codex provider thread's turns, which join by native turn id alone.
const PROVIDER_TURNS: &str = concat!(
    "SELECT provider_turn_id, ",
    native_turn_ref!(),
    " FROM orchestration_v2_projection_provider_turns WHERE provider_thread_id=?1"
);
// The T3 threads holding one Claude provider thread's turns, whose items the join reads.
const TURN_THREADS: &str = "SELECT DISTINCT thread_id
 FROM orchestration_v2_projection_provider_turns WHERE provider_thread_id=?1";
// One T3 thread's provider turns, of every provider thread. No payload is read, so an
// unselected provider's turn past the value bound cannot fail the join.
const THREAD_TURNS: &str = "SELECT provider_turn_id, provider_thread_id
 FROM orchestration_v2_projection_provider_turns WHERE thread_id=?1";
// A selected provider turn's native turn reference.
const TURN_REF: &str = concat!(
    "SELECT ",
    native_turn_ref!(),
    " FROM orchestration_v2_projection_provider_turns WHERE provider_turn_id=?1"
);
// A Claude turn's items join it by turn item id, which `item_key` decodes. Item rows hold
// tool outputs, so the join reads two of T3's indexes instead: a provider turn's item rows,
// and each item's id by row within its thread.
const TURN_ITEM_ROWS: &str =
    "SELECT rowid FROM orchestration_v2_projection_turn_items WHERE provider_turn_id=?1";
const THREAD_ITEM_IDS: &str =
    "SELECT rowid, turn_item_id FROM orchestration_v2_projection_turn_items WHERE thread_id=?1";
// The provider thread holding a Claude turn item, by primary-key range from `item_range`.
// After `/clear` the native session gets a new id while T3's provider thread keeps the old
// one, yet T3 still records the new transcript's items under that thread.
const ITEM_THREAD: &str = "SELECT t.provider_thread_id, pt.driver, pt.provider_instance_id, pt.updated_at<=?3
 FROM orchestration_v2_projection_turn_items i
 LEFT JOIN orchestration_v2_projection_provider_turns t ON t.provider_turn_id=i.provider_turn_id AND t.thread_id=i.thread_id
 LEFT JOIN orchestration_v2_projection_provider_threads pt ON pt.provider_thread_id=t.provider_thread_id
 WHERE i.turn_item_id>=?1 AND i.turn_item_id<?2";
// When T3 began recording provider turn items: the first one inserted, of any driver, so a
// T3 that ran only Codex still dates it. T3 never deletes turn items, and V1 imports carry
// no native ids, so every later Claude turn has its items. The update time can only be
// later than the insertion, which errs toward unresolved. The subquery reads only the
// primary-key index's provider range.
const FIRST_ITEM: &str = "SELECT updated_at FROM orchestration_v2_projection_turn_items
 WHERE rowid=(SELECT min(rowid) FROM orchestration_v2_projection_turn_items
  WHERE turn_item_id>=?1 AND turn_item_id<?2)";
// A provider turn and its run attempt, which `ATTEMPT` and `TURN` both join.
macro_rules! turn_attempt {
    () => {
        " FROM orchestration_v2_projection_provider_turns t
 LEFT JOIN orchestration_v2_projection_run_attempts a ON a.attempt_id=t.run_attempt_id
   AND a.thread_id=t.thread_id AND a.provider_thread_id=t.provider_thread_id
 WHERE t.provider_turn_id=?1"
    };
}
// A provider turn's attempt, whose earliest running selection `Selections` finds.
const ATTEMPT: &str = concat!(
    "SELECT a.thread_id, a.run_id, a.attempt_id",
    turn_attempt!()
);
// Each run selection one thread recorded, in order. Ids that are not JSON strings never
// match a run or attempt, so no nested value leaves SQLite.
const SELECTIONS: &str = "SELECT sequence,
 CASE WHEN json_type(payload_json,'$.id')='text' THEN json_extract(payload_json,'$.id') END,
 CASE WHEN json_type(payload_json,'$.activeAttemptId')='text'
   THEN json_extract(payload_json,'$.activeAttemptId') END,
 json_extract(payload_json,'$.status')='running'
 FROM orchestration_events
 WHERE aggregate_kind='thread' AND application_event_version=2 AND stream_id=?1
   AND event_type='run.updated'
 ORDER BY sequence";
// Native identities and the earliest running selection, `?2`, are the join.
const TURN: &str = concat!(
    "WITH matched AS (
 SELECT t.*, a.attempt_id, a.run_id, a.provider_instance_id,
 json_extract(a.payload_json,'$.nativeThreadId') AS native_session",
    turn_attempt!(),
    ")
 SELECT m.native_session, m.provider_instance_id, m.thread_id, m.run_id, m.attempt_id,
 m.provider_turn_id, m.status, m.started_at, m.completed_at,
 json_extract(m.payload_json,'$.tokenUsage.updatedAt'),
 json_extract(m.payload_json,'$.tokenUsage.usedTokens'),
 json_extract(m.payload_json,'$.tokenUsage.promptCacheExpiresAt'),
 json_extract(e.payload_json,'$.modelSelection.model'),
 (SELECT json_extract(o.value,'$.value') FROM json_each(e.payload_json,'$.modelSelection.options') o
   WHERE json_extract(o.value,'$.id')='serviceTier' LIMIT 1),
 (SELECT json_extract(o.value,'$.value') FROM json_each(e.payload_json,'$.modelSelection.options') o
   WHERE json_extract(o.value,'$.id')='fastMode' LIMIT 1),
 (SELECT count(*) FROM json_each(e.payload_json,'$.modelSelection.options') o
   WHERE json_extract(o.value,'$.id')='serviceTier'),
 (SELECT count(*) FROM json_each(e.payload_json,'$.modelSelection.options') o
   WHERE json_extract(o.value,'$.id')='fastMode'),
 e.occurred_at,
 CASE WHEN lower(trim(json_extract(msg.payload_json,'$.text')))='/compact'
   THEN COALESCE(json_extract(msg.payload_json,'$.idleCompaction'),
     CASE WHEN json_extract(msg.payload_json,'$.createdBy')='agent'
       AND json_extract(msg.payload_json,'$.creationSource')='server' THEN 'beforeCacheExpires' END) END,
 msg.message_id IS NOT NULL AND COALESCE(lower(trim(json_extract(msg.payload_json,'$.text'))),'') != '/compact'
 FROM matched m
 LEFT JOIN orchestration_events e ON e.sequence=?2
 LEFT JOIN orchestration_v2_projection_runs r ON r.run_id=m.run_id AND r.thread_id=m.thread_id
 LEFT JOIN orchestration_v2_projection_messages msg ON msg.message_id=json_extract(r.payload_json,'$.userMessageId') AND msg.thread_id=m.thread_id",
);

/// The longest SQLite value the adapter reads, bounding what a T3 database can make it hold.
const VALUE_BOUND: i32 = 1_048_576;

/// Why optional T3 metadata could not be read.
#[derive(Clone, Debug)]
enum Unavailable {
    Schema(String),
    Budget(String),
    Access(String),
}
impl From<rusqlite::Error> for Unavailable {
    fn from(e: rusqlite::Error) -> Self {
        use rusqlite::ErrorCode::*;
        // SQLite messages name codes, tables and columns, never row values.
        let detail = e.to_string();
        match e.sqlite_error_code() {
            Some(TooBig | OutOfMemory) => Self::Budget(detail),
            Some(
                PermissionDenied
                | DatabaseBusy
                | DatabaseLocked
                | ReadOnly
                | SystemIoFailure
                | CannotOpen
                | NotFound
                | FileLockingProtocolFailed
                | NoLargeFileSupport,
            ) => Self::Access(detail),
            _ => Self::Schema(detail),
        }
    }
}
impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Schema(d) => write!(f, "incompatible or malformed schema ({d})"),
            Self::Budget(d) => write!(f, "a value exceeds the 1 MiB resource bound ({d})"),
            Self::Access(d) => write!(f, "read-only database/WAL access failed ({d})"),
        }
    }
}

#[derive(Clone)]
struct Joined {
    instance: Text,
    /// Shared by every request this turn joins.
    evidence: std::sync::Arc<T3Request>,
    requested: Option<RequestedSpeed>,
}
#[derive(Default)]
struct Projection {
    /// Instances whose provider threads own each root session, keyed by its session key.
    owners: HashMap<(Harness, String), Vec<String>>,
    /// Root sessions with a provider thread linked only after the native bound.
    unlinked: HashSet<(Harness, String)>,
    /// Root sessions a turn item records without a provider thread to own it.
    orphaned: HashSet<(Harness, String)>,
    /// When T3 began recording provider turn items, or None if it has none.
    items_since: Option<Millis>,
    /// When T3's pruned event log becomes complete, or None if it never pruned.
    retained_since: Option<Millis>,
    turns: HashMap<String, TurnMatch>,
}

#[derive(Default)]
struct TurnMatch {
    count: usize,
    joined: Option<Joined>,
    /// The provider turn it joined.
    turn: Option<String>,
}

/// One provider turn's allowlisted columns, read once and judged against each native request.
struct TurnRow {
    native_session: Option<String>,
    instance: Option<Text>,
    started_at: Option<Millis>,
    completed_at: Option<Millis>,
    usage_updated_at: Option<Millis>,
    model: Option<String>,
    tier: Option<String>,
    fast: Option<bool>,
    tier_count: Option<i64>,
    fast_count: Option<i64>,
    event_time: Option<Millis>,
    /// The raw thread and run ids, which the guard rebuild reads T3's event log by.
    thread: Option<String>,
    run: Option<String>,
    /// Whether the run's user message exists and is not `/compact`, so it can arm the timer.
    compactable: bool,
    /// What the turn records for every request it joins, read once; None when incomplete.
    evidence: Option<std::sync::Arc<T3Request>>,
}
impl TurnRow {
    fn read(row: &rusqlite::Row<'_>, bound_ms: Millis) -> Self {
        let completed_at = instant(text_column(row, 8));
        let usage_updated_at = instant(text_column(row, 9));
        let evidence = (|| {
            // A row missing its thread, attempt or provider turn is incomplete evidence,
            // though only the run is kept.
            for index in [2, 4, 5] {
                text_column(row, index)?;
            }
            Some(T3Request {
                run: Id::from(identity(&text_column(row, 3)?)),
                status: match text_column(row, 6)?.as_str() {
                    "completed" => "completed",
                    "running" => "running",
                    "failed" => "failed",
                    "interrupted" => "interrupted",
                    "cancelled" => "cancelled",
                    _ => "unknown",
                },
                completed_at,
                usage_updated_at,
                used_tokens: integer_column(row, 10).filter(|n| {
                    (0..=crate::safe::MAX_SAFE_INTEGER).contains(n)
                        && usage_updated_at.is_some_and(|t| t <= bound_ms)
                }),
                cache_expires_at: usage_updated_at
                    .filter(|t| *t <= bound_ms)
                    .and(instant(text_column(row, 11))),
                idle_mode: mode(text_column(row, 18)),
                guards: None,
            })
        })();
        Self {
            native_session: text_column(row, 0),
            instance: text_column(row, 1).map(Text::from),
            started_at: instant(text_column(row, 7)),
            completed_at,
            usage_updated_at,
            model: text_column(row, 12),
            tier: text_column(row, 13),
            fast: bool_column(row, 14),
            tier_count: integer_column(row, 15),
            fast_count: integer_column(row, 16),
            event_time: instant(text_column(row, 17)),
            thread: text_column(row, 2),
            run: text_column(row, 3),
            compactable: bool_column(row, 19) == Some(true),
            evidence: evidence.map(std::sync::Arc::new),
        }
    }

    /// The evidence this turn gives one native request, or None when it is invalid for it.
    fn join(&self, r: &NativeRequest<'_>, bound_ms: Millis) -> Option<Joined> {
        let native_session = self.native_session.as_deref()?;
        let instance = self.instance.clone()?;
        let tier_count = self.tier_count?;
        let fast_count = self.fast_count?;
        let (completed_at, usage_updated_at) = (self.completed_at, self.usage_updated_at);
        let requested = if tier_count <= 1
            && fast_count <= 1
            && self.model.as_deref() == r.model
            && self.model.is_some()
            && self
                .event_time
                .is_some_and(|t| t <= r.time && t <= bound_ms)
        {
            let tier = self.tier.clone().filter(|s| {
                matches!(
                    s.as_str(),
                    "auto"
                        | "default"
                        | "standard"
                        | "fast"
                        | "priority"
                        | "ultrafast"
                        | "flex"
                        | "scale"
                )
            });
            if (tier_count > 0 && tier.is_none()) || (fast_count > 0 && self.fast.is_none()) {
                None
            } else if r.harness == Harness::Codex {
                Some(RequestedSpeed {
                    tier: tier
                        .or_else(|| (self.fast == Some(true)).then(|| "fast".into()))
                        .map(Text::from),
                    speed: None,
                })
            } else {
                self.fast.map(|v| RequestedSpeed {
                    tier: None,
                    speed: Some(if v { "fast" } else { "standard" }.into()),
                })
            }
        } else {
            None
        };
        let evidence = self.evidence.clone()?;
        let valid = native_session == r.native_session
            && completed_at.is_none_or(|t| t <= bound_ms)
            && usage_updated_at.is_none_or(|t| t <= bound_ms)
            && self.started_at.is_some_and(|t| t <= r.time);
        (valid && (evidence.status != "completed" || completed_at.is_some())).then_some(Joined {
            instance,
            evidence,
            requested,
        })
    }
}

/// Records a provider turn under each of its native ids a selected request names.
fn add_turn(
    found: &mut HashMap<String, Vec<String>>,
    wanted: &HashSet<&str>,
    turn: String,
    natives: Vec<String>,
) {
    for native in natives {
        if wanted.contains(native.as_str()) {
            let turns = found.entry(native).or_default();
            if !turns.contains(&turn) {
                turns.push(turn.clone());
            }
        }
    }
}

fn text_column(row: &rusqlite::Row<'_>, index: usize) -> Option<String> {
    row.get::<_, Option<String>>(index).ok().flatten()
}
fn integer_column(row: &rusqlite::Row<'_>, index: usize) -> Option<i64> {
    use rusqlite::types::ValueRef;
    match row.get_ref(index).ok()? {
        ValueRef::Integer(n) => Some(n),
        ValueRef::Real(n)
            if n.is_finite()
                && n.fract() == 0.0
                && n.abs() <= crate::safe::MAX_SAFE_INTEGER as f64 =>
        {
            Some(n as i64)
        }
        _ => None,
    }
}
fn bool_column(row: &rusqlite::Row<'_>, index: usize) -> Option<bool> {
    match integer_column(row, index) {
        Some(0) => Some(false),
        Some(1) => Some(true),
        _ => None,
    }
}

fn instant(s: Option<String>) -> Option<Millis> {
    s.as_deref().and_then(parse_instant)
}
fn mode(s: Option<String>) -> Option<T3IdleMode> {
    match s.as_deref() {
        Some("beforeCacheExpires") => Some(T3IdleMode::BeforeCacheExpires),
        Some("onReturn") => Some(T3IdleMode::OnReturn),
        _ => None,
    }
}

fn metadata(
    tx: &Transaction<'_>,
    requests: &[&NativeRequest<'_>],
    bound: &str,
    since: Option<Millis>,
) -> Result<Projection, Unavailable> {
    // Reject views and unknown/missing schemas before issuing any projection queries.
    for table in [
        "orchestration_events",
        "orchestration_v2_projection_provider_threads",
        "orchestration_v2_projection_provider_turns",
        "orchestration_v2_projection_run_attempts",
        "orchestration_v2_projection_runs",
        "orchestration_v2_projection_messages",
        "orchestration_v2_projection_threads",
        "orchestration_v2_projection_runtime_requests",
        "orchestration_v2_projection_context_transfers",
        "orchestration_v2_projection_turn_items",
        "orchestration_v2_projection_metadata",
    ] {
        let valid: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name=?1 AND type='table')",
            [table],
            |r| r.get(0),
        )?;
        if !valid {
            return Err(Unavailable::Schema(format!("no table {table}")));
        }
    }
    let bound_ms = parse_instant(bound)
        .ok_or_else(|| Unavailable::Schema(format!("invalid native bound {bound}")))?;
    let boundary = guards::boundary(tx)?;
    let driver = |h: Harness| -> &'static str {
        match h {
            Harness::Claude => "claudeAgent",
            Harness::Codex => "codex",
            Harness::OpenCode => "opencode",
        }
    };
    // The native turn and item ids each selected native session needs.
    let mut wanted: HashMap<(&str, &str), HashSet<&str>> = HashMap::default();
    for r in requests {
        wanted
            .entry((driver(r.harness), r.native_session))
            .or_default()
            .extend(&r.native_turns);
    }
    /// A provider thread whose current native ref is a selected session.
    struct Thread {
        id: String,
        instance: Option<String>,
        /// Linked to the session within the native bound, so it may own it.
        linked: bool,
    }
    let mut threads: HashMap<(&str, String), Vec<Thread>> = HashMap::default();
    let mut owner_event = tx.prepare(OWNER_EVENT)?;
    let mut query = tx.prepare(PROVIDER_THREADS)?;
    let mut rows = query.query([bound])?;
    while let Some(row) = rows.next()? {
        let (Some(id), Some(driver), Some(native)) = (
            text_column(row, 0),
            text_column(row, 1),
            text_column(row, 2),
        ) else {
            continue;
        };
        let driver = match driver.as_str() {
            "claudeAgent" => "claudeAgent",
            "opencode" => "opencode",
            _ => "codex",
        };
        if !wanted.contains_key(&(driver, native.as_str())) {
            continue;
        }
        let linked = bool_column(row, 4) == Some(true)
            || owner_event.query_row(rusqlite::params![id, bound], |r| r.get(0))?;
        threads.entry((driver, native)).or_default().push(Thread {
            id,
            instance: text_column(row, 3),
            linked,
        });
    }
    // Claude root sessions no provider thread names, owned by the threads their turn items
    // name instead: one per provider thread, as for provider threads above. Evidence from
    // requests inside the window decides; earlier requests' only fills in.
    #[derive(Default)]
    struct Items {
        /// Each provider thread's instance, and whether it was linked within the native bound.
        threads: HashMap<String, (String, bool)>,
        /// An item whose provider turn names no Claude provider thread.
        orphaned: bool,
    }
    let mut items: HashMap<((Harness, String), bool), Items> = HashMap::default();
    let mut item_thread = tx.prepare(ITEM_THREAD)?;
    for r in requests {
        if r.harness != Harness::Claude
            || threads.contains_key(&("claudeAgent", r.native_session.to_string()))
        {
            continue;
        }
        let key = (
            (r.harness, r.root.to_string()),
            since.is_none_or(|s| r.time >= s),
        );
        for native in &r.native_turns {
            let (start, end) = item_range(native);
            let mut rows = item_thread.query(rusqlite::params![start, end, bound])?;
            while let Some(row) = rows.next()? {
                let found = items.entry(key.clone()).or_default();
                match (text_column(row, 0), text_column(row, 1).as_deref()) {
                    (Some(thread), Some("claudeAgent")) => {
                        if let std::collections::hash_map::Entry::Vacant(e) =
                            found.threads.entry(thread)
                        {
                            let linked = bool_column(row, 3) == Some(true)
                                || owner_event
                                    .query_row(rusqlite::params![e.key(), bound], |r| r.get(0))?;
                            // Legacy nullable owners block uniqueness, not other rows.
                            e.insert((text_column(row, 2).unwrap_or_default(), linked));
                        }
                    }
                    _ => found.orphaned = true,
                }
            }
        }
    }
    let (start, end) = ("turn-item:provider:", "turn-item:provider;");
    let items_since = tx
        .query_row(FIRST_ITEM, rusqlite::params![start, end], |r| {
            Ok(instant(text_column(r, 0)))
        })
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            e => Err(e),
        })?;
    // Provider turns per native turn/item id, within each selected session's threads.
    let mut refs: HashMap<&(&str, String), HashMap<String, Vec<String>>> = HashMap::default();
    // Codex joins by native turn id alone, reading its provider threads' turns.
    let mut provider_turns = tx.prepare(PROVIDER_TURNS)?;
    for (key, list) in threads.iter().filter(|(key, _)| key.0 == "codex") {
        let found = refs.entry(key).or_default();
        let wanted = &wanted[&(key.0, key.1.as_str())];
        for thread in list {
            let mut rows = provider_turns.query([&thread.id])?;
            while let Some(row) = rows.next()? {
                if let Some(turn) = text_column(row, 0)
                    && text_column(row, 1).as_deref() == Some(key.0)
                {
                    add_turn(
                        found,
                        wanted,
                        turn,
                        text_column(row, 2).into_iter().collect(),
                    );
                }
            }
        }
    }
    // Claude turns also join by item id, and T3 numbers item rows per T3 thread, so the join
    // walks the T3 threads holding the selected Claude provider threads' turns: each one's
    // item ids are read once and dropped before the next.
    let mut claude_threads: HashMap<&str, &(&str, String)> = HashMap::default();
    let mut t3_threads: HashSet<String> = HashSet::default();
    let mut turn_threads = tx.prepare(TURN_THREADS)?;
    for (key, list) in threads.iter().filter(|(key, _)| key.0 == "claudeAgent") {
        for thread in list {
            claude_threads.insert(&thread.id, key);
            let mut rows = turn_threads.query([&thread.id])?;
            while let Some(row) = rows.next()? {
                t3_threads.extend(text_column(row, 0));
            }
        }
    }
    let mut thread_turns = tx.prepare(THREAD_TURNS)?;
    let mut turn_ref = tx.prepare(TURN_REF)?;
    let mut turn_item_rows = tx.prepare(TURN_ITEM_ROWS)?;
    let mut thread_item_ids = tx.prepare(THREAD_ITEM_IDS)?;
    // Only item keys a selected request names are kept.
    let claude: HashSet<&str> = wanted
        .iter()
        .filter(|((driver, _), _)| *driver == "claudeAgent")
        .flat_map(|(_, natives)| natives.iter().copied())
        .collect();
    for t3 in &t3_threads {
        let mut keys: HashMap<i64, String> = HashMap::default();
        let mut items = thread_item_ids.query([t3])?;
        while let Some(item) = items.next()? {
            if let Some(native) = text_column(item, 1).and_then(|i| item_key(&i))
                && claude.contains(native.as_str())
            {
                keys.insert(item.get(0)?, native);
            }
        }
        let mut rows = thread_turns.query([t3])?;
        while let Some(row) = rows.next()? {
            let (Some(turn), Some(key)) = (
                text_column(row, 0),
                text_column(row, 1).and_then(|p| claude_threads.get(p.as_str()).copied()),
            ) else {
                continue;
            };
            let (driver, native) =
                turn_ref.query_row([&turn], |r| Ok((text_column(r, 0), text_column(r, 1))))?;
            if driver.as_deref() != Some(key.0) {
                continue;
            }
            let mut natives: Vec<String> = native.into_iter().collect();
            let mut items = turn_item_rows.query([&turn])?;
            while let Some(item) = items.next()? {
                natives.extend(keys.get(&item.get::<_, i64>(0)?).cloned());
            }
            add_turn(
                refs.entry(key).or_default(),
                &wanted[&(key.0, key.1.as_str())],
                turn,
                natives,
            );
        }
    }
    let mut attempt_query = tx.prepare(ATTEMPT)?;
    let mut selections_query = tx.prepare(SELECTIONS)?;
    let mut selections: HashMap<String, Selections> = HashMap::default();
    let mut turn_query = tx.prepare(TURN)?;
    let mut turn_rows: HashMap<String, Option<TurnRow>> = HashMap::default();
    let mut result = Projection {
        items_since,
        retained_since: boundary.since,
        ..Projection::default()
    };
    for r in requests {
        let key = (driver(r.harness), r.native_session.to_string());
        let root = (r.harness, r.root.to_string());
        if !result.owners.contains_key(&root) {
            let list = threads.get(&key).map_or(&[][..], Vec::as_slice);
            if list.iter().any(|t| !t.linked) {
                result.unlinked.insert(root.clone());
            }
            let mut owners: Vec<String> = list
                .iter()
                .filter(|t| t.linked)
                // Legacy nullable owners block uniqueness, not other rows.
                .map(|t| t.instance.clone().unwrap_or_default())
                .collect();
            let found = [true, false]
                .into_iter()
                .find_map(|selected| items.get(&(root.clone(), selected)));
            if let Some(found) = found.filter(|_| list.is_empty()) {
                // An item proves its thread ran the request, so a thread whose owner the
                // bound cannot confirm blocks the others rather than dropping out.
                if found.threads.values().all(|(_, linked)| *linked) {
                    owners.extend(found.threads.values().map(|(i, _)| i.clone()));
                } else {
                    result.unlinked.insert(root.clone());
                }
                if owners.is_empty() && found.orphaned {
                    result.orphaned.insert(root.clone());
                }
            }
            result.owners.insert(root, owners);
        }
        if r.native_turns.is_empty() || result.turns.contains_key(r.id) {
            continue;
        }
        let mut matched = None;
        let mut turns: Vec<&String> = Vec::new();
        for native in &r.native_turns {
            for turn in refs
                .get(&key)
                .and_then(|f| f.get(*native))
                .into_iter()
                .flatten()
            {
                if !turns.contains(&turn) {
                    turns.push(turn);
                }
            }
        }
        let joined = match turns[..] {
            [turn] => {
                matched = Some(turn.clone());
                if !turn_rows.contains_key(turn) {
                    let attempt = attempt_query
                        .query_row([turn], |row| {
                            Ok((
                                text_column(row, 0),
                                text_column(row, 1),
                                text_column(row, 2),
                            ))
                        })
                        .map(Some)
                        .or_else(|e| match e {
                            rusqlite::Error::QueryReturnedNoRows => Ok(None),
                            e => Err(e),
                        })?;
                    let row = match attempt {
                        None => None,
                        Some(attempt) => {
                            let selection = match attempt {
                                (Some(thread), Some(run), Some(attempt)) => {
                                    if !selections.contains_key(&thread) {
                                        let read = Selections::read(&mut selections_query, &thread);
                                        selections.insert(thread.clone(), read);
                                    }
                                    selections[&thread].first(&run, &attempt)?
                                }
                                _ => None,
                            };
                            turn_query
                                .query_row(rusqlite::params![turn, selection], |row| {
                                    Ok(TurnRow::read(row, bound_ms))
                                })
                                .map(Some)
                                .or_else(|e| match e {
                                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                                    e => Err(e),
                                })?
                        }
                    };
                    turn_rows.insert(turn.clone(), row);
                }
                turn_rows[turn].as_ref().and_then(|t| t.join(r, bound_ms))
            }
            _ => None,
        };
        result.turns.insert(
            r.id.to_string(),
            TurnMatch {
                count: turns.len(),
                joined,
                turn: matched,
            },
        );
    }
    // Only a uniquely joined turn's run can describe the gap after its request.
    let candidates: HashMap<&str, guards::Candidate<'_>> = turn_rows
        .iter()
        .filter_map(|(turn, row)| {
            let row = row.as_ref()?;
            Some((
                turn.as_str(),
                guards::Candidate {
                    thread: row.thread.as_deref()?,
                    run: row.run.as_deref()?,
                    usage_at: row.usage_updated_at.filter(|t| *t <= bound_ms),
                    compactable: row.compactable,
                },
            ))
        })
        .collect();
    let rebuilt = guards::rebuild(tx, boundary, &candidates, bound_ms)?;
    // Every request a turn joins still shares one evidence, now carrying the turn's guards.
    let evidence: HashMap<&str, std::sync::Arc<T3Request>> = turn_rows
        .iter()
        .filter_map(|(turn, row)| {
            let guards = rebuilt.get(turn.as_str())?;
            let mut evidence = T3Request::clone(row.as_ref()?.evidence.as_deref()?);
            evidence.guards = Some(guards.clone());
            Some((turn.as_str(), std::sync::Arc::new(evidence)))
        })
        .collect();
    for m in result.turns.values_mut() {
        if let (Some(turn), Some(joined)) = (&m.turn, &mut m.joined)
            && let Some(evidence) = evidence.get(turn.as_str())
        {
            joined.evidence = evidence.clone();
        }
    }
    Ok(result)
}

/// One thread's run selections, read once for every turn that joins a run of it: a lookup
/// per turn would reread the thread's whole event log each time.
struct Selections {
    /// The earliest running selection of each run and attempt.
    first: HashMap<(String, String), i64>,
    /// Why SQLite failed to read the event after the last one read, if it did.
    failed: Option<Unavailable>,
}

impl Selections {
    /// Read `thread`'s selections in order, up to the first event SQLite cannot read.
    fn read(query: &mut rusqlite::Statement<'_>, thread: &str) -> Self {
        let mut first = HashMap::default();
        let failed = (|| {
            let mut rows = query.query([thread])?;
            while let Some(row) = rows.next()? {
                if let (Some(run), Some(attempt), Some(true)) = (
                    text_column(row, 1),
                    text_column(row, 2),
                    bool_column(row, 3),
                ) {
                    first.entry((run, attempt)).or_insert(row.get(0)?);
                }
            }
            Ok::<_, rusqlite::Error>(())
        })()
        .err()
        .map(Unavailable::from);
        Self { first, failed }
    }

    /// The sequence of `run`'s earliest running selection of `attempt`. A lookup that has to
    /// pass an unreadable event fails as reading it does.
    fn first(&self, run: &str, attempt: &str) -> Result<Option<i64>, Unavailable> {
        match (
            self.first.get(&(run.to_string(), attempt.to_string())),
            &self.failed,
        ) {
            (Some(sequence), _) => Ok(Some(*sequence)),
            (None, Some(failed)) => Err(failed.clone()),
            (None, None) => Ok(None),
        }
    }
}

/// The one-owner profile set for `owner`, shared by every record it owns.
fn shared(owned: &mut HashMap<String, Profiles>, owner: &str) -> Profiles {
    if let Some(profiles) = owned.get(owner) {
        return profiles.clone();
    }
    let profiles = Profiles::from([owner.to_string()]);
    owned.insert(owner.to_string(), profiles.clone());
    profiles
}

/// Whether `enrich` reads a database: only a selected state's existing `statev2.sqlite`.
pub(crate) fn reads_database(states: &[T3State]) -> bool {
    states
        .iter()
        .any(|s| s.status == "selected" && Path::new(&s.path).join("statev2.sqlite").is_file())
}

/// Selected requests T3 left out of every current profile's checks, each counted once however
/// many states or owners it touches; each state's own counts break them down.
#[derive(Default)]
pub(crate) struct Unplaced {
    /// Requests whose owner stays unresolved.
    pub unattributed: u64,
    /// Requests every T3 owner of which was removed from its settings.
    pub removed: u64,
}

/// Freeze optional projection metadata in one read-only WAL transaction, then close it before
/// analysis. Returns the selected requests it could not place in any current profile.
pub(crate) fn enrich(
    states: &mut [T3State],
    profiles: &mut [Profile],
    requests: &mut [Request],
    events: &mut [StructuralEvent],
    ids: &NativeIds,
    bound: &str,
    since: Option<Millis>,
) -> Unplaced {
    let mut unplaced = Unplaced::default();
    let mut projections: Vec<NativeRequest<'_>> = requests
        .iter()
        .filter_map(|r| native_request(r, ids))
        .collect();
    // Join keys are text, and only the few events T3 can link need theirs.
    let linkable: Vec<(&StructuralEvent, String)> = events
        .iter()
        .filter(|e| ids.events.contains_key(&e.id))
        .map(|e| (e, e.id.to_string()))
        .collect();
    for (e, key) in &linkable {
        let root = ids.root(e.session.as_ref());
        if let Some(session) = ids.ids.get(root) {
            projections.push(NativeRequest {
                id: key,
                root,
                native_session: session,
                native_turns: vec![&ids.events[&e.id]],
                harness: e.harness,
                time: e.time,
                model: e.model.as_deref(),
                profiles: &e.profiles,
            });
        }
    }
    let harnesses: HashMap<&str, Harness> = projections.iter().map(|r| (r.id, r.harness)).collect();
    let mut invalid_joins = HashSet::default();
    let mut joins: HashMap<String, Vec<(String, Joined)>> = HashMap::default();
    let mut owners: HashMap<(Harness, String), Vec<String>> = HashMap::default();
    let mut unlinked = HashSet::default();
    let mut orphaned = HashSet::default();
    // When each read state began recording provider turn items.
    let mut items_since: HashMap<String, Option<Millis>> = HashMap::default();
    let mut read = HashSet::default();
    // Owners no longer in their state's settings, by profile key: (state, instance).
    let mut removed: HashMap<String, (String, String)> = HashMap::default();
    for state in states.iter_mut().filter(|s| s.status == "selected") {
        let path = Path::new(&state.path).join("statev2.sqlite");
        if !path.is_file() {
            state.metadata =
                "unavailable: no existing statev2.sqlite; native usage is retained".into();
            continue;
        }
        state.snapshot_started_at = Some(iso(crate::reader::now()));
        let data = (|| {
            let mut db = Connection::open_with_flags(
                &path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            db.busy_timeout(std::time::Duration::ZERO)?;
            db.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH, VALUE_BOUND)?;
            db.execute_batch("PRAGMA query_only=ON; PRAGMA trusted_schema=OFF;")?;
            let tx = db.transaction()?;
            let relevant: Vec<&NativeRequest> = projections
                .iter()
                .filter(|r| {
                    r.profiles.iter().any(|id| {
                        profiles.iter().any(|p| {
                            &p.id == id && p.t3.as_ref().is_some_and(|t| t.state == state.path)
                        })
                    })
                })
                .collect();
            let result = metadata(&tx, &relevant, bound, since)?;
            tx.rollback()?;
            Ok::<_, Unavailable>(result)
        })();
        state.snapshot_finished_at = Some(iso(crate::reader::now()));
        let data = match data {
            Ok(data) => {
                state.metadata = "read-only consistent SQLite snapshot; allowlisted native links and controls only".into();
                read.insert(state.path.clone());
                unlinked.extend(data.unlinked.iter().cloned());
                orphaned.extend(data.orphaned.iter().cloned());
                items_since.insert(state.path.clone(), data.items_since);
                for t3 in profiles.iter_mut().filter_map(|p| p.t3.as_mut()) {
                    if t3.state == state.path {
                        t3.retained_since = data.retained_since;
                    }
                }
                data
            }
            Err(e) => {
                state.metadata = format!("unavailable: {e}; native usage is retained");
                continue;
            }
        };
        let profile_id = |instance: &str, harness: Harness| {
            profiles
                .iter()
                .find(|p| {
                    p.harness == harness
                        && p.t3
                            .as_ref()
                            .is_some_and(|t| t.state == state.path && t.instance == instance)
                })
                .map(|p| p.id.clone())
        };
        // A selected profile, a removed instance's key, or empty for an owner neither names.
        let mut owner_id = |instance: &str, harness: Harness| {
            profile_id(instance, harness)
                .or_else(|| {
                    (!instance.is_empty() && !state.instances.contains(instance)).then(|| {
                        let key = profile_key(&state.path, instance);
                        removed.insert(key.clone(), (state.path.clone(), instance.into()));
                        key
                    })
                })
                .unwrap_or_default()
        };
        for ((harness, session), instances) in data.owners {
            if instances.is_empty() {
                continue;
            }
            let target = owners.entry((harness, session)).or_default();
            for instance in instances {
                target.push(owner_id(&instance, harness));
            }
        }
        for (request, matched) in data.turns {
            if matched.count > 1 {
                state.ambiguous_requests += 1;
            }
            if let Some(row) = matched.joined {
                let harness = harnesses[request.as_str()];
                joins
                    .entry(request)
                    .or_default()
                    .push((owner_id(&row.instance, harness), row));
            } else if matched.count > 0 {
                // Preserve invalid/duplicate matches as blockers, without inventing metadata.
                invalid_joins.insert(request);
            }
        }
    }
    let t3_ids: HashSet<String> = profiles
        .iter()
        .filter(|p| p.t3.is_some())
        .map(|p| p.id.clone())
        .chain(removed.keys().cloned())
        .collect();
    let state_of = |id: &String| {
        profiles
            .iter()
            .find(|p| &p.id == id)
            .and_then(|p| p.t3.as_ref())
            .map(|t| t.state.clone())
    };
    // The possible owners of each selected request T3 could not settle.
    let mut unsettled: HashSet<Unsettled> = HashSet::default();
    let mut unresolved: HashMap<(String, &'static str), u64> = HashMap::default();
    // How each root session's own requests resolved, which its subagents inherit: one
    // profile, or why not. Requests inside the window decide; earlier ones only fill in.
    type Resolution = Result<String, &'static str>;
    let mut roots: HashMap<((Harness, String), bool), Resolution> = HashMap::default();
    let resolution = |roots: &HashMap<((Harness, String), bool), Resolution>,
                      root: &(Harness, String)| {
        roots
            .get(&(root.clone(), true))
            .or_else(|| roots.get(&(root.clone(), false)))
            .cloned()
    };
    // Roots whose own requests all excluded T3, which their subagents inherit even
    // when the root stays among several native profiles.
    let mut native_roots: HashMap<((Harness, String), bool), bool> = HashMap::default();
    // Shared profile sets, so requests resolved alike keep sharing one copy.
    let mut owned: HashMap<String, Profiles> = HashMap::default();
    let mut native_only: HashMap<Profiles, Profiles> = HashMap::default();
    // Sessions every owner of which was removed from settings ran under controls that no
    // longer exist: they stay billed under those owners, outside every current profile.
    let mut removed_roots: HashMap<(Harness, String), Profiles> = HashMap::default();
    for (root, o) in &owners {
        if !o.is_empty() && o.iter().all(|p| removed.contains_key(p)) {
            let mut keys = o.clone();
            keys.sort_unstable();
            keys.dedup();
            let profiles = match &keys[..] {
                [one] => shared(&mut owned, one),
                _ => keys.into(),
            };
            removed_roots.insert(root.clone(), profiles);
        }
    }
    let mut removed_requests: HashMap<&(String, String), u64> = HashMap::default();
    let order: Vec<usize> = (0..requests.len())
        .filter(|&i| requests[i].agent != Agent::Subagent)
        .chain((0..requests.len()).filter(|&i| requests[i].agent == Agent::Subagent))
        .collect();
    for i in order {
        let r = &mut requests[i];
        let subagent = r.agent == Agent::Subagent;
        let root = (r.harness, ids.root(&r.session).to_string());
        let rows = joins.get(&*r.id);
        let session_owners = owners.get(&root);
        let removed_owners = removed_roots.get(&root);
        let unique = rows
            .filter(|v| v.len() == 1)
            .and_then(|v| v.first())
            .filter(|(p, _)| {
                !invalid_joins.contains(&*r.id)
                    && !p.is_empty()
                    && (r.profiles.contains(p) || removed_owners.is_some_and(|o| o.contains(p)))
            });
        let selected = since.is_none_or(|s| r.time >= s);
        let root_resolution = if subagent {
            resolution(&roots, &root)
        } else {
            None
        };
        let inherited = match &root_resolution {
            Some(Ok(p)) if r.profiles.contains(p) => Some(p.clone()),
            _ => None,
        };
        let owner = unique
            .map(|(p, _)| p)
            .or_else(|| {
                session_owners
                    .filter(|o| o.len() == 1)
                    .and_then(|o| o.first())
                    .filter(|p| !p.is_empty() && r.profiles.contains(p))
            })
            .or(inherited.as_ref());
        let mut cause = None;
        if let Some(owner) = owner {
            r.profiles = shared(&mut owned, owner);
            if let Some((_, joined)) = unique {
                r.t3 = Some(joined.evidence.clone());
                if joined.evidence.idle_mode.is_some() {
                    for part in &mut r.parts {
                        if part.kind == PartKind::Message {
                            part.kind = PartKind::Compaction;
                        }
                    }
                }
                if let Some(requested) = &joined.requested {
                    let native = r.requested_speed.get_or_insert_with(|| requested.clone());
                    let conflict = [
                        (&native.tier, &requested.tier),
                        (&native.speed, &requested.speed),
                    ]
                    .into_iter()
                    .any(|(a, b)| {
                        a.as_deref().zip(b.as_deref()).is_some_and(|(a, b)| {
                            crate::prices::tier_name(a) != crate::prices::tier_name(b)
                        })
                    });
                    if conflict {
                        r.issues.push("conflicting requested service tiers".into());
                    } else {
                        native.tier = native.tier.take().or_else(|| requested.tier.clone());
                        native.speed = native.speed.take().or_else(|| requested.speed.clone());
                    }
                }
            } else if subagent {
                r.issues.push("subagent shares its root session's owner; T3 records no per-request run/speed/idle metadata for it".into());
            } else {
                r.issues.push("T3 native session owner is unique; per-request run/speed/idle metadata is unavailable".into());
            }
        } else if let Some(o) = removed_owners {
            r.profiles = o.clone();
        } else if r.profiles.iter().any(|p| t3_ids.contains(p)) {
            // Native REPL or CLI provenance excludes T3 even when its state was not read.
            let native_source = (r.harness == Harness::Claude
                && r.client.as_deref() == Some("cli")
                && r.source.as_deref() == Some("repl_main_thread"))
                || (r.harness == Harness::Codex
                    && matches!(r.source.as_deref(), Some("cli" | "exec"))
                    && r.client.as_deref() != Some("T3 Code"));
            // T3's Codex originator, or none: a missing label proves nothing. Claude labels
            // decide nothing, since `claude -p` from a T3 agent's shell inherits T3's.
            let t3_label = r.harness == Harness::Codex
                && matches!(r.client.as_deref(), None | Some("T3 Code"));
            // T3 records a turn item for each Claude request after it began recording them.
            let recorded = r
                .profiles
                .iter()
                .filter(|p| t3_ids.contains(*p))
                .filter_map(state_of)
                .all(|s| {
                    items_since
                        .get(&s)
                        .copied()
                        .flatten()
                        .is_some_and(|t| t <= r.time)
                });
            let native_root = subagent
                && native_roots
                    .get(&(root.clone(), true))
                    .or_else(|| native_roots.get(&(root.clone(), false)))
                    == Some(&true);
            let native = r.profiles.iter().any(|p| !t3_ids.contains(p));
            let stray = rows.is_some_and(|r| !r.is_empty())
                || invalid_joins.contains(&*r.id)
                || orphaned.contains(&root);
            let found = match session_owners.map_or(0, Vec::len) {
                0 if (native_source || native_root) && native && !stray => None,
                0 => match root_resolution {
                    Some(Err(c)) => Some(c),
                    _ if r
                        .profiles
                        .iter()
                        .filter(|p| t3_ids.contains(*p))
                        .any(|p| state_of(p).is_none_or(|s| !read.contains(&s))) =>
                    {
                        Some("the T3 state database was not read")
                    }
                    _ if unlinked.contains(&root) => Some(
                        "the native session's T3 provider thread was linked after the native snapshot",
                    ),
                    _ if stray => Some(
                        "a T3 turn matches the request, but no T3 thread owns its native session",
                    ),
                    // T3's state was read with no provider thread or turn item for this
                    // session. A Claude request T3 ran would have a turn item unless it
                    // predates them; Codex has no such record, so its label decides.
                    // A subagent records no turn items of its own, so only its root can
                    // exclude T3.
                    _ if r.harness == Harness::Claude && subagent => Some(
                        "the subagent's root session has no transcript and no T3 provider thread",
                    ),
                    _ if r.harness == Harness::Claude && !recorded => {
                        Some("the request predates T3's oldest retained turn item")
                    }
                    _ if native && !t3_label => None,
                    _ if t3_label => Some(
                        "no T3 provider thread owns the native session, but its client label is one T3 writes",
                    ),
                    None if subagent => Some(
                        "the subagent's root session has no transcript and no T3 provider thread",
                    ),
                    _ => Some(
                        "no T3 provider thread owns the native session, and no native profile shares its home",
                    ),
                },
                1 => Some(
                    "the native session's only T3 owner is not a selected instance for this home",
                ),
                _ => Some("several T3 provider threads own the native session"),
            };
            match found {
                None => {
                    r.profiles = native_only
                        .entry(r.profiles.clone())
                        .or_insert_with_key(|all| {
                            all.iter()
                                .filter(|p| !t3_ids.contains(*p))
                                .cloned()
                                .collect()
                        })
                        .clone();
                }
                Some(c) => {
                    cause = Some(c);
                    // T3's database may name an owner none of these profiles is,
                    // such as an instance now under another home.
                    let outside: Vec<String> = session_owners
                        .into_iter()
                        .flatten()
                        .filter(|o| !r.profiles.contains(o))
                        .cloned()
                        .collect();
                    let also = if outside.is_empty() { "" } else { OUTSIDE };
                    r.issues.push(
                        format!("{UNSETTLED}: {c}{also}; current control guidance is advisory")
                            .into(),
                    );
                    // Context before the window prices cache state but says nothing about
                    // which controls ran the selected requests.
                    if selected {
                        let states: HashSet<String> =
                            r.profiles.iter().filter_map(state_of).collect();
                        if !states.is_empty() {
                            unplaced.unattributed += 1;
                        }
                        for state in states {
                            *unresolved.entry((state, c)).or_default() += 1;
                        }
                        unsettled.insert(Unsettled {
                            profiles: r.profiles.clone(),
                            outside,
                        });
                    }
                }
            }
        }
        if selected && !removed.is_empty() {
            if r.profiles.iter().any(|p| removed.contains_key(p)) {
                unplaced.removed += 1;
            }
            for p in r.profiles.iter() {
                if let Some(owner) = removed.get(p) {
                    *removed_requests.entry(owner).or_default() += 1;
                }
            }
        }
        if !subagent {
            const MIXED: &str = "the subagent's root session resolves to more than one owner";
            let native_only = cause.is_none() && r.profiles.iter().all(|p| !t3_ids.contains(p));
            *native_roots.entry((root.clone(), selected)).or_insert(true) &= native_only;
            let resolved = match (cause, &r.profiles[..]) {
                (Some(c), _) => Err(c),
                (None, [p]) => Ok(p.clone()),
                (None, _) => Err(MIXED),
            };
            let key = (root, selected);
            let merged = match (roots.remove(&key), resolved) {
                (None, resolved) => resolved,
                (Some(Err(c)), _) | (Some(Ok(_)), Err(c)) => Err(c),
                (Some(Ok(p)), Ok(q)) => (p == q).then_some(p).ok_or(MIXED),
            };
            roots.insert(key, merged);
        }
    }
    for state in states.iter_mut() {
        let mut causes: Vec<(&str, u64)> = unresolved
            .iter()
            .filter(|((path, _), _)| *path == state.path)
            .map(|((_, c), n)| (*c, *n))
            .collect();
        causes.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        state.unresolved = causes.into_iter().map(|(c, n)| (c.into(), n)).collect();
        let mut owners: Vec<(&str, u64)> = removed_requests
            .iter()
            .filter(|((path, _), _)| *path == state.path)
            .map(|((_, instance), n)| (instance.as_str(), *n))
            .collect();
        owners.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        state.removed = owners.into_iter().map(|(i, n)| (i.into(), n)).collect();
    }
    // A uniquely owned session also owns its structural evidence, without assuming a run.
    let mut sessions: HashMap<(Harness, &str), HashSet<&str>> = HashMap::default();
    for r in requests.iter() {
        sessions
            .entry((r.harness, &r.session))
            .or_default()
            .extend(r.profiles.iter().map(String::as_str));
    }
    for e in events {
        let key = ids.events.contains_key(&e.id).then(|| e.id.to_string());
        if let Some(key) = &key
            && let Some(rows) = joins.get(key)
            && rows.len() == 1
            && !invalid_joins.contains(key)
            && !rows[0].0.is_empty()
        {
            let (owner, joined) = &rows[0];
            e.profiles = shared(&mut owned, owner);
            e.t3 = Some(joined.evidence.clone());
            if joined.evidence.idle_mode.is_some() {
                e.idle_attribution = IdleAttribution::T3;
            }
        }
        if let Some(owners) = sessions.get(&(e.harness, e.session.as_ref()))
            && owners.len() == 1
            && let Some(owner) = owners.iter().next()
        {
            let profiles = shared(&mut owned, owner);
            if e.profiles != profiles {
                e.profiles = profiles;
            }
        }
    }
    // A Claude profile's TTL is one value only across the projects its own requests ran
    // in, which attribution has now settled.
    for p in profiles
        .iter_mut()
        .filter(|p| !p.ttl_directories.is_empty())
    {
        let directories: HashSet<&str> = requests
            .iter()
            .filter(|r| r.profiles.contains(&p.id))
            .filter_map(|r| r.cwd.as_deref())
            .collect();
        crate::discovery::settle_ttls(p, directories.iter().copied());
    }
    let open = |control: Control| unsettled_for(profiles, &unsettled, control);
    let [speed, main_ttl, subagent_ttl, compaction, bash] = [
        Control::Speed,
        Control::Ttl(Bucket::Main),
        Control::Ttl(Bucket::Subagent),
        Control::Compaction,
        Control::Bash,
    ]
    .map(open);
    const REASON: &str = "shared native history has unresolved T3 control ownership";
    for p in profiles.iter_mut() {
        if p.t3.is_some() || speed.contains(&p.id) {
            p.speed_directories = None;
            if let Some(speed) = &mut p.speed {
                speed.resolved = false;
            }
        }
        if compaction.contains(&p.id) {
            p.compaction.default.comparison_unknown = Some(REASON.into());
            for c in p.compaction.models.values_mut() {
                c.comparison_unknown = p.compaction.default.comparison_unknown.clone();
            }
            if p.t3.is_some() {
                // Its directory windows would otherwise bypass the unresolved profile window.
                p.idle_directories = None;
            }
        }
        for (open, ttl) in [
            (&main_ttl, &mut p.ttl.main),
            (&subagent_ttl, &mut p.ttl.subagent),
        ] {
            if open.contains(&p.id) {
                ttl.value = None;
                ttl.reason = REASON.into();
            }
        }
        if let Some(cap) = p.bash.as_mut().filter(|_| bash.contains(&p.id)) {
            cap.unresolved = Some(REASON.into());
        }
    }
    unplaced
}

/// A control whose current value T3's unsettled ownership can leave unknown. Idle
/// compaction and the background features act for one owner alone, so their checks
/// extrapolate over the requests T3 could not settle instead; see [`Split`].
#[derive(Clone, Copy)]
enum Control {
    Speed,
    Ttl(Bucket),
    /// The compaction windows, with both TTLs their replay applies.
    Compaction,
    /// The Bash cap, with the main TTL its check compares it with.
    Bash,
}

impl Control {
    /// The owner that puts a profile on this control's line, as its check groups them, or
    /// None when no line of this control covers the profile.
    fn owner(self, p: &Profile) -> Option<String> {
        match self {
            Control::Speed => p.speed.as_ref().map(|s| s.owner.clone()),
            Control::Ttl(bucket) => Some(crate::rows::ttl_owner(p, bucket)),
            Control::Compaction => Some(crate::rows::compaction_owner(p)),
            Control::Bash => crate::bash::owner(p),
        }
    }
}

/// A request T3 could not settle between its possible owners.
#[derive(Debug, PartialEq, Eq, Hash)]
struct Unsettled {
    /// The profiles it stays billed to.
    profiles: Profiles,
    /// Owners T3's database names that none of `profiles` is; it is billed to none of them,
    /// but whichever ran it read that owner's controls.
    outside: Vec<String>,
}

/// The profiles whose current value of `control` stays unknown: those an unsettled request
/// is billed to when its possible owners fall on more than one of the control's lines. A
/// request left open only between profiles that read the control from one file and key gets
/// that file's value whichever of them ran it.
fn unsettled_for(
    profiles: &[Profile],
    unsettled: &HashSet<Unsettled>,
    control: Control,
) -> HashSet<String> {
    let line = |id: &String| {
        profiles
            .iter()
            .find(|p| &p.id == id)
            .and_then(|p| Some(crate::rows::key(p, &control.owner(p)?)))
    };
    let mut open = HashSet::default();
    for owners in unsettled {
        // A removed instance, or one no line of the control covers, is a line of its own.
        let lines: HashSet<Option<String>> = owners
            .profiles
            .iter()
            .chain(&owners.outside)
            .map(line)
            .collect();
        if lines.len() > 1 {
            open.extend(owners.profiles.iter().cloned());
        }
    }
    // A check reads its line's lead, so an opened profile opens every profile on its line.
    let opened: HashSet<String> = open.iter().filter_map(line).collect();
    open.extend(
        profiles
            .iter()
            .filter(|p| line(&p.id).is_some_and(|l| opened.contains(&l)))
            .map(|p| p.id.clone()),
    );
    open
}

/// How a line that acts for one owner alone, such as native idle compaction, T3's idle
/// modes or the background features, extrapolates over the main-agent requests T3 left open
/// between its profiles and others: their sessions are left out of its comparison, and each
/// stands in at the line's share of the requests T3 settled between that session's own
/// possible owners, so another home on the same line never dilutes it.
#[derive(Debug, Default)]
pub(crate) struct Split<'a> {
    /// Each session with a selected request whose owner T3 left open, with its share; None
    /// when T3 settled no request between its possible owners.
    pub sessions: HashMap<&'a str, Option<f64>>,
}

impl<'a> Split<'a> {
    /// The split of `line`'s selected main-agent requests from `since`.
    pub fn new(
        profiles: &[Profile],
        requests: &'a [Request],
        line: &[String],
        since: Option<Millis>,
    ) -> Self {
        let selected = || {
            requests
                .iter()
                .filter(|r| r.agent == Agent::Main && since.is_none_or(|s| r.time >= s))
        };
        // A T3 instance's, or a removed one's, which no current profile names.
        let instance = |id: &str| {
            profiles
                .iter()
                .find(|p| p.id == id)
                .is_none_or(|p| p.t3.is_some())
        };
        // T3 left the owner open between the line's profiles and others.
        let open = |r: &Request| {
            r.profiles.iter().any(|p| line.contains(p))
                && r.profiles.iter().any(|p| !line.contains(p))
                && r.profiles.iter().any(|p| instance(p))
        };
        let mut owners: HashMap<&'a str, std::collections::BTreeSet<&'a str>> = HashMap::default();
        for r in selected().filter(|r| open(r)) {
            owners
                .entry(&r.session)
                .or_default()
                .extend(r.profiles.iter().map(String::as_str));
        }
        let mut shares: HashMap<&std::collections::BTreeSet<&str>, Option<f64>> =
            HashMap::default();
        for cohort in owners.values() {
            shares.entry(cohort).or_insert_with(|| {
                let (mut own, mut other) = (0u64, 0u64);
                for r in selected().filter(|r| {
                    !r.profiles.is_empty() && r.profiles.iter().all(|p| cohort.contains(&**p))
                }) {
                    if r.profiles.iter().all(|p| line.contains(p)) {
                        own += 1;
                    } else if r.profiles.iter().all(|p| !line.contains(p)) {
                        other += 1;
                    }
                }
                (own + other > 0).then(|| own as f64 / (own + other) as f64)
            });
        }
        let sessions = owners
            .iter()
            .map(|(&session, cohort)| (session, shares[cohort]))
            .collect();
        Split { sessions }
    }

    /// Whether the line leaves `session` out, and if so its share.
    pub fn left_out(&self, session: &str) -> Option<Option<f64>> {
        self.sessions.get(session).copied()
    }
}

/// How many left-out sessions or turns, each with its share, stand in for the line beside
/// the `settled` ones they are extrapolated from; None when any has no share, or when some
/// share is positive but nothing settled stands for them, even if it rounds to none.
pub(crate) fn stand_ins(
    shares: impl IntoIterator<Item = Option<f64>>,
    settled: u64,
) -> Option<u64> {
    let mut sum = 0.0;
    for share in shares {
        sum += share?;
    }
    (settled > 0 || sum == 0.0).then(|| sum.round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::scan;
    use crate::safe::timestamp;
    use crate::test_support::{Workspace, make_request};
    use rusqlite::params;
    use serde_json::json;

    fn write(path: impl AsRef<Path>, text: impl AsRef<[u8]>) {
        let path = path.as_ref();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    fn state(w: &Workspace, settings: Value) -> PathBuf {
        let root = w.home.join(".t3/userdata");
        write(root.join("settings.json"), settings.to_string());
        root
    }
    fn db(root: &Path) -> Connection {
        let db = Connection::open(root.join("statev2.sqlite")).unwrap();
        db.execute_batch(include_str!("../test/fixtures/t3-schema.sql"))
            .unwrap();
        db
    }
    fn seed(db: &Connection, driver: &str, instance: &str) {
        let native = "native-session";
        let start = "2026-01-01T00:00:00Z";
        let end = "2026-01-01T00:00:10Z";
        db.execute("INSERT INTO orchestration_v2_projection_provider_threads VALUES('pt','thread',?1,?2,?3,?4)",params![driver,instance,end,json!({"nativeThreadRef":{"nativeId":native},"private":"PRIVATE_PROJECTION"}).to_string()]).unwrap();
        db.execute("INSERT INTO orchestration_v2_projection_run_attempts VALUES('attempt','thread','pt','run',?1,?2)",params![instance,json!({"nativeThreadId":native,"private":"PRIVATE_PROJECTION"}).to_string()]).unwrap();
        db.execute("INSERT INTO orchestration_v2_projection_provider_turns VALUES('turn','thread','pt','attempt','completed',?1,?2,?3,1)",params![start,end,json!({"nativeTurnRef":{"driver":driver,"nativeId":"native-turn"},"tokenUsage":{"updatedAt":end,"usedTokens":300000,"promptCacheExpiresAt":"2026-01-01T00:05:10Z"},"private":"PRIVATE_PROJECTION"}).to_string()]).unwrap();
        db.execute(
            "INSERT INTO orchestration_v2_projection_runs VALUES('run','thread',1,'completed',?1)",
            [json!({"userMessageId":"message"}).to_string()],
        )
        .unwrap();
        db.execute("INSERT INTO orchestration_v2_projection_threads VALUES('thread',?1,'pt',?2,NULL,NULL,?3)",params![instance,end,json!({"lineage":{"relationshipToParent":"root"},"private":"PRIVATE_PROJECTION"}).to_string()]).unwrap();
        db.execute(
            "INSERT INTO orchestration_v2_projection_messages VALUES('message','thread',?1)",
            [json!({"text":"PRIVATE_MESSAGE","private":"PRIVATE_PROJECTION"}).to_string()],
        )
        .unwrap();
        db.execute("INSERT INTO orchestration_events VALUES(1,'thread',2,'thread','run.updated',?1,?2)",params![start,json!({"id":"run","activeAttemptId":"attempt","status":"running","modelSelection":{"model":"gpt-6.1-sol","options":[{"id":"serviceTier","value":"fast"}]},"private":"PRIVATE_PROJECTION"}).to_string()]).unwrap();
    }
    fn native(w: &Workspace) {
        write(
            w.codex.join("sessions/test.jsonl"),
            concat!(
                "{\"type\":\"session_meta\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"payload\":{\"id\":\"native-session\",\"model_provider\":\"openai\",\"source\":\"exec\"}}\n",
                "{\"type\":\"turn_context\",\"timestamp\":\"2026-01-01T00:00:01Z\",\"payload\":{\"turn_id\":\"native-turn\",\"model\":\"gpt-6.1-sol\"}}\n",
                "{\"type\":\"token_usage_record\",\"timestamp\":\"2026-01-01T00:00:05Z\",\"payload\":{\"response_id\":\"response\",\"turn_id\":\"native-turn\",\"usage\":{\"input_tokens\":300000,\"output_tokens\":10,\"cached_input_tokens\":200000,\"cache_write_input_tokens\":0}}}\n"
            ),
        );
    }
    #[test]
    fn native_history_is_shared_and_exact_run_evidence_never_fills_billed_tiers() {
        let w = Workspace::new();
        native(&w);
        let root = state(
            &w,
            json!({"providerInstances":{"codex":{"driver":"codex","config":{"homePath":w.codex},"environment":[{"name":"API_KEY","value":"PRIVATE_SECRET"}]},"unsupported":{"driver":"grok","config":{"credential":"PRIVATE_SECRET"}}},"connections":[{"secret":"PRIVATE_CONNECTION"}]}),
        );
        let db = db(&root);
        seed(&db, "codex", "codex");
        let before = fs::read(root.join("statev2.sqlite")).unwrap();
        let reading = std::sync::Arc::new(crate::reader::Reading::default());
        let scan = scan(&crate::reader::ScanOptions {
            reading: Some(reading.clone()),
            ..w.scan_options()
        })
        .unwrap();
        // The database step leaves the progress line once the read is done.
        assert!(reads_database(&scan.t3));
        assert!(reading.step.lock().unwrap().is_none());
        assert_eq!(scan.requests.len(), 1);
        let r = &scan.requests[0];
        assert_eq!(r.parts[0].tier, None);
        assert_eq!(
            r.requested_speed.as_ref().unwrap().tier.as_deref(),
            Some("fast")
        );
        assert_eq!(r.t3.as_ref().unwrap().run, Id::from(identity("run")));
        assert_eq!(r.profiles.len(), 1);
        assert!(
            scan.profiles
                .iter()
                .any(|p| p.id == r.profiles[0]
                    && p.t3.as_ref().is_some_and(|t| t.instance == "codex"))
        );
        assert_eq!(scan.snapshots.len(), 1);
        assert!(
            scan.t3[0]
                .exclusions
                .iter()
                .any(|s| s.contains("unsupported"))
        );
        // Session facts keep native session IDs by design, for the `sessions` export.
        let exported = format!(
            "{:?}",
            Scan {
                sessions: Default::default(),
                ..scan.clone()
            }
        );
        for secret in [
            "PRIVATE_SECRET",
            "PRIVATE_CONNECTION",
            "PRIVATE_PROJECTION",
            "PRIVATE_MESSAGE",
            "native-session",
            "native-turn",
        ] {
            assert!(!exported.contains(secret), "{secret}");
        }
        assert_eq!(fs::read(root.join("statev2.sqlite")).unwrap(), before);
    }
    #[test]
    fn unsettled_owners_leave_open_only_controls_they_read_from_different_lines() {
        use crate::test_support::{make_profile, make_t3_profile};
        let root = "/home/.claude";
        let mut native = make_profile("native", root);
        native.speed = Some(SpeedControl {
            value: Some("standard".into()),
            owner: format!("{root}/settings.json:fastMode"),
            replacement: String::new(),
            resolved: true,
            reason: String::new(),
        });
        let mut agent = make_t3_profile("agent", root);
        agent.speed = Some(SpeedControl {
            owner: "T3 instance agent: thread modelSelection.options.fastMode".into(),
            ..native.speed.clone().unwrap()
        });
        // T3's own window outranks the native file's.
        let mut windowed = make_t3_profile("windowed", root);
        windowed.compaction.default.owner =
            "/state/settings.json:providerInstances.windowed.config.autoCompactWindow".into();
        // An instance environment sets the subagent TTL for T3's requests alone.
        let mut env = make_t3_profile("env", root);
        env.ttl.subagent.owner = "T3 instance env: CLAUDE_CODE_SUBAGENT_PROMPT_CACHE_TTL".into();
        let profiles = [native, agent, windowed, env];
        let open_with = |owners: &[&str], outside: &[&str]| {
            let unsettled = HashSet::from_iter([Unsettled {
                profiles: owners.iter().map(|o| o.to_string()).collect(),
                outside: outside.iter().map(|o| o.to_string()).collect(),
            }]);
            let mut open: Vec<&str> = [
                ("speed", Control::Speed),
                ("main ttl", Control::Ttl(Bucket::Main)),
                ("subagent ttl", Control::Ttl(Bucket::Subagent)),
                ("compaction", Control::Compaction),
            ]
            .into_iter()
            .filter(|(_, c)| !unsettled_for(&profiles, &unsettled, *c).is_empty())
            .map(|(name, _)| name)
            .collect();
            open.sort_unstable();
            open
        };
        let open = |owners: &[&str]| open_with(owners, &[]);
        // Both read the TTLs and windows from one file; only T3 sets the speed.
        assert_eq!(open(&["native", "agent"]), ["speed"]);
        assert_eq!(open(&["native", "windowed"]), ["compaction", "speed"]);
        // The window replay applies the TTL the environment sets.
        assert_eq!(
            open(&["native", "env"]),
            ["compaction", "speed", "subagent ttl"]
        );
        // A request open between a line's member and T3 opens the whole line, whose
        // check reads the lead.
        let mut lines = profiles.to_vec();
        let mut member = make_profile("member", root);
        member.speed = lines[0].speed.clone();
        lines.push(member);
        let open_speed = unsettled_for(
            &lines,
            &HashSet::from_iter([Unsettled {
                profiles: Profiles::from_iter(["member".to_string(), "agent".to_string()]),
                outside: vec![],
            }]),
            Control::Speed,
        );
        assert!(open_speed.contains("native"), "{open_speed:?}");
        // An owner T3 names beyond the request's profiles counts by its own lines. One under
        // another home reads the TTL elsewhere, though only these profiles are opened.
        lines.push(make_t3_profile("elsewhere", "/work/.claude"));
        let unsettled = HashSet::from_iter([Unsettled {
            profiles: Profiles::from_iter(["native".to_string(), "agent".to_string()]),
            outside: vec!["elsewhere".into()],
        }]);
        let opened = unsettled_for(&lines, &unsettled, Control::Ttl(Bucket::Main));
        assert!(
            opened.contains("native") && !opened.contains("elsewhere"),
            "{opened:?}"
        );
        // One no profile names is a line of its own.
        assert_eq!(
            open_with(&["native", "agent"], &[""]),
            ["compaction", "main ttl", "speed", "subagent ttl"]
        );
        // One on the same line changes nothing.
        let member = Unsettled {
            profiles: Profiles::from_iter(["native".to_string(), "agent".to_string()]),
            outside: vec!["member".into()],
        };
        let ttl = Control::Ttl(Bucket::Main);
        assert!(unsettled_for(&lines, &HashSet::from_iter([member]), ttl).is_empty());
        // A removed instance's controls are gone, so nothing it could share is known.
        assert_eq!(
            open(&["native", "gone"]),
            ["compaction", "main ttl", "speed", "subagent ttl"]
        );
    }

    #[test]
    fn an_instance_environment_reads_every_claude_provider() {
        let w = Workspace::new();
        let root = state(
            &w,
            json!({"providerInstances":{"claudeAgent":{"driver":"claudeAgent","environment":[{"name":"CLAUDE_CODE_USE_MANTLE","value":"true"}]}}}),
        );
        seed(&db(&root), "claudeAgent", "claudeAgent");
        let data = scan(&w.scan_options()).unwrap();
        let providers: Vec<_> = data
            .profiles
            .iter()
            .filter(|p| p.harness == Harness::Claude)
            .map(|p| p.provider.as_deref())
            .collect();
        assert!(providers.contains(&Some("amazon-bedrock")), "{providers:?}");
    }

    #[test]
    fn a_bash_cap_only_one_owner_sets_stays_advisory_for_requests_t3_left_open() {
        for env in [false, true] {
            let w = Workspace::new();
            let mut instance = json!({"driver":"claudeAgent"});
            if env {
                instance["environment"] = json!([{"name":"BASH_MAX_TIMEOUT_MS","value":"3600000"}]);
            }
            let root = state(&w, json!({"providerInstances":{"claudeAgent":instance}}));
            // A known TTL, so the cap has a TTL to compare with and only ownership is open.
            write(w.claude.join("settings.json"), r#"{"promptCacheTtl":"1h"}"#);
            let database = db(&root);
            seed(&database, "claudeAgent", "claudeAgent");
            // Dates when T3 began recording turn items, after the session below.
            database.execute("INSERT INTO orchestration_v2_projection_turn_items VALUES(?1,'thread','turn','assistant_message','completed','{}','2026-01-01T00:00:00Z',NULL,0)",[format!("{ITEM_PREFIX}other-uuid")]).unwrap();
            let row = json!({"type":"assistant","uuid":"early-uuid","sessionId":"early","timestamp":"2025-12-31T00:00:00Z","entrypoint":"sdk-ts","message":{"id":"early-message","model":"claude-opus-5-5","content":[],"usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}});
            write(w.claude.join("projects/p/early.jsonl"), format!("{row}\n"));
            let data = scan(&w.scan_options()).unwrap();
            assert_eq!(data.requests[0].profiles.len(), 2, "{env}");
            let checks = crate::bash::bash_caps(&crate::bash::BashInput {
                scan: &data,
                catalog: &crate::test_support::prices(),
                since: None,
            });
            // One file's cap is the cap whichever owner ran the request; the instance's own
            // cap leaves both lines' requests to an unknown cap.
            let open = env || managed_policy_unresolved();
            for p in data
                .profiles
                .iter()
                .filter(|p| p.harness == Harness::Claude)
            {
                let bash = p.bash.as_ref().unwrap();
                assert_eq!(bash.unresolved.is_some(), open, "{env} {}", p.id);
            }
            for c in &checks {
                if open {
                    assert_eq!(c.suggested, None, "{env}");
                    assert!(
                        c.unresolved
                            .as_deref()
                            .is_some_and(|u| u.contains("unresolved T3 control ownership"))
                    );
                }
            }
            // The whole doctor report renders beside a cap that suggests nothing but has a TTL.
            let report = crate::replay::analyze(crate::replay::AnalyzeOptions {
                scan: data,
                catalog: &crate::test_support::prices(),
                since: None,
                now: timestamp(&json!("2026-01-02T00:00:00Z")).unwrap(),
                max_cost_per_use_usd: 1.0,
                reading: None,
            })
            .unwrap();
            let bash = report.bash.iter().find(|b| b.suggested.is_none());
            // An unresolved machine policy, as on Windows, opens the TTL too, so the cap has no
            // TTL there; only the instance's own variable leaves the TTL known and opens the cap.
            let ownership_only = env && !managed_policy_unresolved();
            assert_eq!(
                bash.is_some_and(|b| b.ttl.is_some()),
                ownership_only,
                "{env}"
            );
            // The measured report says nothing of the cap, and its card says why it suggests
            // none: it never claims a margin under the TTL.
            for details in [false, true] {
                let text = crate::report::rendered(
                    &report,
                    crate::report::ReportOptions {
                        details,
                        ..Default::default()
                    },
                );
                assert!(!text.contains("Bash timeout cap"), "{env} {details}");
            }
            let cards = crate::report::advice(&report, Default::default());
            let unowned = cards
                .iter()
                .filter(|c| c.text.contains("unresolved T3 control ownership"))
                .collect::<Vec<_>>();
            assert_eq!(!unowned.is_empty(), open, "{env}");
            for card in unowned {
                assert!(card.quiet && card.how.is_none(), "{env}");
                assert!(card.text.contains("No cap is suggested"), "{env}");
            }
        }
    }

    #[test]
    fn a_split_stands_in_unsettled_sessions_at_the_settled_share() {
        use crate::test_support::{make_profile, make_t3_profile};
        // Current instances' ids are digests like native ones; only the profiles tell them apart.
        let profiles = [
            make_profile("native", "/home/.claude"),
            make_profile("other", "/home/.claude"),
            make_profile("elsewhere", "/work/.claude"),
            make_t3_profile("agent", "/home/.claude"),
        ];
        let request = |id: &str, session: &str, owners: &[&str]| {
            let mut r = make_request();
            r.id = id.into();
            r.session = session.into();
            r.profiles = owners.iter().map(|p| p.to_string()).collect();
            r
        };
        let mut requests = vec![
            request("a", "native-1", &["native"]),
            request("b", "native-2", &["native"]),
            request("c", "native-3", &["native"]),
            request("d", "t3-1", &["agent"]),
            request("e", "open", &["native", "agent"]),
        ];
        let line = ["native".to_string()];
        let split = Split::new(&profiles, &requests, &line, None);
        assert_eq!(split.sessions.len(), 1);
        assert_eq!(split.left_out("open"), Some(Some(0.75)));
        assert_eq!(split.left_out("native-1"), None);
        assert_eq!(stand_ins([Some(0.75); 4], 1), Some(3));
        // Even a share that rounds to none needs a settled item to count it by.
        assert_eq!(stand_ins([Some(0.25)], 1), Some(0));
        assert_eq!(stand_ins([Some(0.25)], 0), None);
        assert_eq!(stand_ins([Some(0.0)], 0), Some(0));
        // T3's line sees the same requests from its side.
        let t3 = ["agent".to_string()];
        let split = Split::new(&profiles, &requests, &t3, None);
        assert_eq!(split.left_out("open"), Some(Some(0.25)));
        // Another home on a shared line has no say in this home's share.
        let shared = ["native".to_string(), "elsewhere".to_string()];
        requests.extend((0..100).map(|i| request(&format!("w{i}"), "work", &["elsewhere"])));
        let split = Split::new(&profiles, &requests, &shared, None);
        assert_eq!(split.left_out("open"), Some(Some(0.75)));
        // Requests open between native profiles alone are not T3's to settle.
        requests.push(request("f", "copied", &["native", "other"]));
        let split = Split::new(&profiles, &requests, &line, None);
        assert_eq!(split.left_out("copied"), None);
        // A removed instance is no current profile, and counts as T3's.
        requests.push(request("g", "removed", &["native", "t3:/state:gone"]));
        let split = Split::new(&profiles, &requests, &line, None);
        assert_eq!(split.left_out("removed"), Some(Some(1.0)));
        // With nothing settled to estimate a share from, none can be priced.
        let open = [request("e", "open", &["native", "agent"])];
        let split = Split::new(&profiles, &open, &line, None);
        assert_eq!(split.left_out("open"), Some(None));
        assert_eq!(stand_ins([None], 1), None);
        assert_eq!(stand_ins([], 0), Some(0));
    }

    #[test]
    fn unread_instances_are_counted() {
        let w = Workspace::new();
        native(&w);
        fs::remove_dir_all(w.home.join(".claude")).unwrap();
        state(
            &w,
            json!({"providerInstances":{"codex":{"driver":"codex","config":{"homePath":w.codex}},"away":{"driver":"codex","config":{"homePath":w.home.join("unmounted")}},"other":{"driver":"grok"}}}),
        );
        let scan = scan(&w.scan_options()).unwrap();
        // The built-in claudeAgent's default home is absent too, but no setting names it.
        assert!(
            scan.t3[0]
                .exclusions
                .iter()
                .any(|e| e.contains("instance claudeAgent"))
        );
        assert_eq!((scan.t3[0].missing, scan.t3[0].unsupported), (1, 1));
    }

    #[test]
    fn a_request_unresolved_in_two_states_counts_once() {
        let w = Workspace::new();
        native(&w);
        let settings =
            json!({"providerInstances":{"codex":{"driver":"codex","config":{"homePath":w.codex}}}});
        // Both states, sharing the home, claim the native session, so the request stays
        // unresolved in each.
        let first = state(&w, settings.clone());
        seed(&db(&first), "codex", "codex");
        let second = w.home.join("t3-dev");
        write(second.join("settings.json"), settings.to_string());
        seed(&db(&second), "codex", "codex");
        let mut options = w.scan_options();
        options.discovery.t3_dirs = vec![
            first.to_string_lossy().into(),
            second.to_string_lossy().into(),
        ];
        let scan = scan(&options).unwrap();
        assert_eq!(scan.requests.len(), 1);
        let per_state: u64 = scan.t3.iter().flat_map(|s| s.unresolved.values()).sum();
        assert_eq!(per_state, 2, "{:?}", scan.t3);
        assert_eq!(scan.coverage.t3_unattributed, 1);
    }

    #[test]
    fn removed_owners_stay_billed_outside_current_checks() {
        // `gone` is no longer in settings; `elsewhere` is, but under another home.
        for (owner, removed) in [("gone", true), ("elsewhere", false)] {
            let w = Workspace::new();
            native(&w);
            let other = w.home.join("other-codex");
            fs::create_dir_all(&other).unwrap();
            let root = state(
                &w,
                json!({"providerInstances":{"codex":{"driver":"codex","config":{"homePath":w.codex}},"elsewhere":{"driver":"codex","config":{"homePath":other}}}}),
            );
            seed(&db(&root), "codex", owner);
            let session = w.codex.join("sessions/test.jsonl");
            let mut transcript = fs::read_to_string(&session).unwrap();
            transcript.push_str("{\"type\":\"response_item\",\"timestamp\":\"2026-01-01T00:00:06Z\",\"payload\":{\"type\":\"custom_tool_call\",\"name\":\"exec\",\"call_id\":\"k1\"}}\n");
            write(&session, transcript);
            let scan = scan(&w.scan_options()).unwrap();
            let r = &scan.requests[0];
            // Recorded uses follow the request's resolved owners.
            let mut owners = r.profiles.to_vec();
            owners.sort();
            let mut credited = scan.exposure.tool_calls[0].profiles.clone();
            credited.sort();
            assert_eq!(credited, owners);
            let t3 = &scan.t3[0];
            let current = scan
                .profiles
                .iter()
                .find(|p| p.t3.as_ref().is_some_and(|t| t.instance == "codex"))
                .unwrap();
            // Settings resolution also reflects platform policy (always unresolved on
            // Windows), so check the native speed T3 overrides, which shared-history
            // ambiguity leaves unresolved. So does the TTL: T3 names `elsewhere`, which
            // reads it from another home, as the session's owner.
            let native = scan
                .profiles
                .iter()
                .find(|p| p.t3.is_none() && p.harness == Harness::Codex)
                .unwrap();
            assert_eq!(
                !native.speed.as_ref().unwrap().resolved,
                !removed,
                "{owner}"
            );
            assert_eq!(
                current.ttl.main.reason.contains("shared native history"),
                !removed,
                "{owner}"
            );
            if removed {
                assert_eq!(*r.profiles, [profile_key(&t3.path, "gone")]);
                assert!(r.t3.is_some(), "joined evidence still applies");
                assert_eq!(t3.removed, IndexMap::from_iter([("gone".into(), 1)]));
                assert!(t3.unresolved.is_empty(), "{:?}", t3.unresolved);
                assert_eq!(
                    (scan.coverage.t3_removed, scan.coverage.t3_unattributed),
                    (1, 0)
                );
            } else {
                assert!(r.profiles.contains(&current.id));
                assert!(t3.removed.is_empty());
                assert_eq!(
                    (scan.coverage.t3_removed, scan.coverage.t3_unattributed),
                    (0, 1)
                );
                // The request says T3 names an owner outside it, for checks that count it.
                assert!(
                    r.issues.iter().any(|i| i.contains(OUTSIDE)),
                    "{:?}",
                    r.issues
                );
                assert!(
                    t3.unresolved
                        .keys()
                        .any(|c| c.contains("not a selected instance")),
                    "{:?}",
                    t3.unresolved
                );
            }
        }
    }
    #[test]
    fn codex_requested_tiers_share_aliases_and_preserve_partial_evidence() {
        let w = Workspace::new();
        native(&w);
        let path = w.codex.join("sessions/test.jsonl");
        let mut lines: Vec<Value> = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let root = state(
            &w,
            json!({"providerInstances":{"codex":{"driver":"codex"}}}),
        );
        let db = db(&root);
        seed(&db, "codex", "codex");
        let catalog = crate::test_support::prices();
        for (native, selected, expected) in [
            (None, Some("fast"), Some(0.8803)),
            (Some("priority"), Some("fast"), Some(0.8803)),
            (Some("fast"), Some("priority"), Some(0.8803)),
            (Some("standard"), Some("default"), Some(0.44015)),
            (Some("fast"), None, Some(0.8803)),
            (Some("default"), Some("fast"), None),
        ] {
            lines[1]["payload"]["service_tier"] = json!(native);
            write(
                &path,
                format!(
                    "{}\n",
                    lines
                        .iter()
                        .map(Value::to_string)
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
            );
            let options: Vec<Value> = selected
                .map(|tier| json!({"id":"serviceTier","value":tier}))
                .into_iter()
                .collect();
            db.execute(
                "UPDATE orchestration_events SET payload_json=?1",
                [json!({"id":"run","activeAttemptId":"attempt","status":"running","modelSelection":{"model":"gpt-6.1-sol","options":options}}).to_string()],
            )
            .unwrap();
            let scan = scan(&w.scan_options()).unwrap();
            let r = &scan.requests[0];
            assert!(r.t3.is_some());
            assert_eq!(r.parts[0].tier, None);
            assert_eq!(scan.coverage.tier_known, 0);
            let cost = crate::prices::price_request(&catalog, r);
            assert_eq!(
                cost.total_usd
                    .map(|usd| (usd.to_f64() * 1_000_000.0).round()),
                expected.map(|usd: f64| (usd * 1_000_000.0).round()),
                "native={native:?}, T3={selected:?}: {cost:?}"
            );
            assert!(crate::prices::tier_sensitive(&cost));
        }
    }
    #[test]
    fn duplicate_turns_and_first_missing_selection_do_not_supply_requested_speed() {
        let w = Workspace::new();
        native(&w);
        let root = state(
            &w,
            json!({"providerInstances":{"codex":{"driver":"codex"}}}),
        );
        let db = db(&root);
        seed(&db, "codex", "codex");
        db.execute(
            "UPDATE orchestration_events SET payload_json=?1",
            [json!({"id":"run","activeAttemptId":"attempt","status":"running"}).to_string()],
        )
        .unwrap();
        db.execute("INSERT INTO orchestration_events SELECT 2,aggregate_kind,application_event_version,stream_id,event_type,occurred_at,?1 FROM orchestration_events",[json!({"id":"run","activeAttemptId":"attempt","status":"running","modelSelection":{"model":"gpt-6.1-sol","options":[{"id":"serviceTier","value":"ultrafast"}]}}).to_string()]).unwrap();
        let read = || scan(&w.scan_options()).unwrap();
        assert_eq!(
            read().requests[0].requested_speed.as_ref().unwrap().tier,
            None
        );
        db.execute("INSERT INTO orchestration_v2_projection_provider_turns SELECT 'duplicate',thread_id,provider_thread_id,run_attempt_id,status,started_at,completed_at,payload_json,ordinal+1 FROM orchestration_v2_projection_provider_turns",[]).unwrap();
        let scan = read();
        assert!(scan.requests[0].t3.is_none());
        assert!(scan.t3[0].ambiguous_requests > 0);
        assert_eq!(
            scan.requests[0].requested_speed.as_ref().unwrap().tier,
            None
        );
    }
    #[test]
    fn defaults_keep_each_instance_home_and_drop_its_settings() {
        let w = Workspace::new();
        let alternate = w.home.join(".codex-alt");
        fs::create_dir_all(alternate.join("sessions")).unwrap();
        state(
            &w,
            json!({"idleCompaction":"off","providerInstances":{
                "codex":{"driver":"codex","environment":[
                    {"name":"CODEX_HOME","value":alternate},
                    {"name":"FORCE_PROMPT_CACHING_5M","value":"1"}
                ]},
                "claudeAgent":{"driver":"claudeAgent","config":{"autoCompactWindow":"200000","idleCompaction":"off"}}
            }}),
        );
        let mut options = w.scan_options().discovery;
        options.pretend_defaults();
        let mut profiles = vec![];
        discover(&options, &mut profiles).unwrap();
        let instance = |id: &str| {
            profiles
                .iter()
                .find(|p| p.t3.as_ref().is_some_and(|t| t.instance == id))
                .unwrap()
        };
        let codex = instance("codex");
        // The variable that chose its home stays.
        assert_eq!(
            codex.root,
            crate::discovery::realpath(&alternate.to_string_lossy()).unwrap()
        );
        let claude = instance("claudeAgent");
        let t3 = claude.t3.as_ref().unwrap();
        assert_eq!(t3.idle_mode, T3IdleMode::OnReturn);
        // Unread machine policy, as on Windows, leaves the instance unresolved.
        assert_eq!(
            t3.env.is_some() && t3.inherited_window,
            !managed_policy_unresolved()
        );
        let speed = claude.speed.as_ref().unwrap();
        assert_eq!(
            (speed.value.as_deref(), speed.resolved),
            (Some("standard"), !managed_policy_unresolved())
        );
    }
    #[test]
    fn unavailable_optional_database_and_sensitive_environment_remain_explicit() {
        let w = Workspace::new();
        native(&w);
        state(
            &w,
            json!({"providerInstances":{"codex":{"driver":"codex","environment":[{"name":"CODEX_HOME","value":"PRIVATE_HOME","sensitive":true}]}}}),
        );
        let scan = scan(&w.scan_options()).unwrap();
        assert_eq!(scan.requests.len(), 1);
        assert!(scan.t3[0].metadata.contains("unavailable"));
        assert!(
            scan.profiles
                .iter()
                .filter_map(|p| p.t3.as_ref())
                .any(|t| t.settings_unresolved)
        );
        assert!(!format!("{scan:?}").contains("PRIVATE_HOME"));
    }
    #[test]
    fn an_instances_subagent_variables_are_its_own_and_a_secret_one_leaves_only_them_unresolved() {
        let w = Workspace::new();
        native(&w);
        state(
            &w,
            json!({"providerInstances":{"claudeAgent":{"driver":"claudeAgent","environment":[
                {"name":"CLAUDE_CODE_SUBAGENT_EFFORT","value":"max"},
                {"name":"CLAUDE_CODE_SUBAGENT_MODEL","value":"PRIVATE_MODEL","sensitive":true}
            ]}}}),
        );
        let scan = scan(&w.scan_options()).unwrap();
        let p = scan
            .profiles
            .iter()
            .find(|p| p.t3.as_ref().is_some_and(|t| t.instance == "claudeAgent"))
            .expect("the instance's profile");
        assert_eq!(
            p.t3.as_ref().unwrap().settings_unresolved,
            managed_policy_unresolved()
        );
        if managed_policy_unresolved() {
            // Unread machine policy, as on Windows, leaves every entry the instance sets unread.
            assert!(p.agents.effort.is_none() && p.agents.model.is_none());
            for (why, key) in [
                (&p.agents.effort_unresolved, "CLAUDE_CODE_SUBAGENT_EFFORT"),
                (&p.agents.unresolved, "CLAUDE_CODE_SUBAGENT_MODEL"),
            ] {
                assert!(why.as_deref().is_some_and(|w| w.contains(key)), "{why:?}");
            }
            assert!(!format!("{scan:?}").contains("PRIVATE_MODEL"));
            return;
        }
        let effort = p.agents.effort.as_ref().expect("the instance's effort");
        assert_eq!(effort.value, "max");
        assert!(
            effort
                .owner
                .ends_with("settings.json:providerInstances.claudeAgent.environment.CLAUDE_CODE_SUBAGENT_EFFORT"),
            "{}",
            effort.owner
        );
        assert!(p.agents.model.is_none());
        assert!(
            p.agents
                .unresolved
                .as_deref()
                .is_some_and(|why| why.contains("CLAUDE_CODE_SUBAGENT_MODEL")),
            "{:?}",
            p.agents.unresolved
        );
        assert!(!format!("{scan:?}").contains("PRIVATE_MODEL"));
        assert_eq!(p.agents.effort_unresolved, None);

        // A secret effort leaves only the effort unknown; the model still attributes runs.
        state(
            &w,
            json!({"providerInstances":{"claudeAgent":{"driver":"claudeAgent","environment":[
                {"name":"CLAUDE_CODE_SUBAGENT_EFFORT","value":"PRIVATE_EFFORT","sensitive":true},
                {"name":"CLAUDE_CODE_SUBAGENT_MODEL","value":"claude-haiku-5-5"}
            ]}}}),
        );
        let secret = crate::reader::scan(&w.scan_options()).unwrap();
        let p = secret
            .profiles
            .iter()
            .find(|p| p.t3.as_ref().is_some_and(|t| t.instance == "claudeAgent"))
            .expect("the instance's profile");
        assert!(p.agents.effort.is_none());
        assert_eq!(p.agents.unresolved, None);
        assert_eq!(
            p.agents.model.as_ref().map(|m| m.value.as_str()),
            Some("claude-haiku-5-5")
        );
        assert!(
            p.agents
                .effort_unresolved
                .as_deref()
                .is_some_and(|why| why.contains("CLAUDE_CODE_SUBAGENT_EFFORT")),
            "{:?}",
            p.agents.effort_unresolved
        );
        assert!(!format!("{secret:?}").contains("PRIVATE_EFFORT"));
    }
    #[test]
    fn a_secret_plugins_directory_leaves_only_the_plugins_unread() {
        let w = Workspace::new();
        native(&w);
        let instance = |entry: Value| {
            state(
                &w,
                json!({"providerInstances":{"claudeAgent":{"driver":"claudeAgent","environment":[entry]}}}),
            );
            let scan = scan(&w.scan_options()).unwrap();
            assert!(!format!("{scan:?}").contains("PRIVATE_DIR"));
            scan.profiles
                .into_iter()
                .find(|p| p.t3.is_some() && p.harness == Harness::Claude)
                .unwrap()
        };
        let secret = instance(
            json!({"name":"CLAUDE_CODE_PLUGIN_CACHE_DIR","value":"PRIVATE_DIR","sensitive":true}),
        );
        // Every other line still reads the instance's settings.
        assert_eq!(
            secret.t3.as_ref().unwrap().settings_unresolved,
            managed_policy_unresolved()
        );
        assert_eq!(secret.plugins.unwrap().dir, None);
        // So does a value that isn't a string, without failing the scan.
        let invalid = instance(json!({"name":"CLAUDE_CODE_PLUGIN_CACHE_DIR","value":false}));
        assert_eq!(invalid.plugins.unwrap().dir, None);
        let plain =
            instance(json!({"name":"CLAUDE_CODE_PLUGIN_CACHE_DIR","value":"~/plugin-cache"}));
        assert_eq!(
            plain.plugins.unwrap().dir,
            Some(crate::test_support::native_path(
                w.home.join("plugin-cache")
            ))
        );
    }
    #[test]
    fn claude_exact_sdk_items_link_idle_runs_without_exporting_text() {
        let w = Workspace::new();
        let root = state(
            &w,
            json!({"providerInstances":{"claudeAgent":{"driver":"claudeAgent","config":{"autoCompactWindow":"300000"}}}}),
        );
        let database = db(&root);
        seed(&database, "claudeAgent", "claudeAgent");
        database.execute("UPDATE orchestration_v2_projection_provider_turns SET payload_json=json_set(payload_json,'$.nativeTurnRef.nativeId','assistant-uuid')",[]).unwrap();
        database
            .execute(
                "UPDATE orchestration_v2_projection_messages SET payload_json=?1",
                [json!({"text":"/compact","idleCompaction":"beforeCacheExpires"}).to_string()],
            )
            .unwrap();
        database.execute("UPDATE orchestration_events SET payload_json=json_set(payload_json,'$.modelSelection.model','claude-opus-5-5','$.modelSelection.options',json('[{\"id\":\"fastMode\",\"value\":false}]'))",[]).unwrap();
        write(
            w.claude.join("projects/p/session.jsonl"),
            format!(
                "{}\n{}\n",
                json!({"type":"assistant","uuid":"assistant-uuid","sessionId":"native-session","timestamp":"2026-01-01T00:00:05Z","entrypoint":"t3-code","message":{"id":"native-message","model":"claude-opus-5-5","content":[{"type":"text","text":"PRIVATE_REPLY"}],"usage":{"input_tokens":0,"output_tokens":2000,"cache_read_input_tokens":300000,"cache_creation_input_tokens":0,"speed":"standard"}}}),
                json!({"type":"system","subtype":"compact_boundary","uuid":"boundary-uuid","sessionId":"native-session","timestamp":"2026-01-01T00:00:06Z","compactMetadata":{"trigger":"manual","requestId":"native-message","preTokens":300000,"postTokens":2000}})
            ),
        );
        let mut scan = scan(&w.scan_options()).unwrap();
        assert_eq!(scan.requests.len(), 1);
        let r = &scan.requests[0];
        assert_eq!(
            r.t3.as_ref().unwrap().idle_mode,
            Some(T3IdleMode::BeforeCacheExpires)
        );
        assert_eq!(
            r.requested_speed.as_ref().unwrap().speed.as_deref(),
            Some("standard")
        );
        assert_eq!(r.parts[0].kind, PartKind::Compaction);
        let charges = crate::replay::compaction_charges(&mut scan, &crate::test_support::prices());
        attribute_compactions(&mut scan, &charges);
        assert_eq!(scan.events[0].idle_attribution, IdleAttribution::T3);
        assert!(scan.events[0].t3.is_some());
        let debug = format!("{scan:?}");
        for private in [
            "PRIVATE_REPLY",
            "PRIVATE_MESSAGE",
            "PRIVATE_PROJECTION",
            "native-message",
            "assistant-uuid",
        ] {
            assert!(!debug.contains(private), "{private}");
        }
    }

    #[test]
    fn global_legacy_instance_modes_and_window_speed_ownership_override_native_controls() {
        let w = Workspace::new();
        write(
            w.claude.join("settings.json"),
            r#"{"fastMode":true,"autoCompactWindow":400000,"promptCacheTtl":"5m","modelSettings":{"claude-opus-5-5":{"autoCompactWindow":350000}}}"#,
        );
        let root = state(
            &w,
            json!({"compactIdleThreads":true,"providerInstances":{"claudeAgent":{"driver":"claudeAgent","config":{"autoCompactWindow":"300000","idleCompaction":"default"}},"off":{"driver":"claudeAgent","enabled":false,"config":{"idleCompaction":"off"}}}}),
        );
        let data = crate::discovery::discover(&w.scan_options().discovery).unwrap();
        let t3 = data
            .profiles
            .iter()
            .find(|p| p.t3.as_ref().is_some_and(|t| t.instance == "claudeAgent"))
            .unwrap();
        assert_eq!(t3.compaction.default.value, Some(300000));
        assert!(t3.compaction.models.is_empty());
        assert!(
            t3.compaction
                .default
                .owner
                .contains("providerInstances.claudeAgent.config.autoCompactWindow")
        );
        assert_eq!(t3.speed.as_ref().unwrap().value, None);
        assert!(!t3.speed.as_ref().unwrap().resolved);
        assert!(t3.background.is_empty());
        assert_eq!(
            t3.t3.as_ref().unwrap().idle_mode,
            T3IdleMode::BeforeCacheExpires
        );
        let off = data
            .profiles
            .iter()
            .find(|p| p.t3.as_ref().is_some_and(|t| t.instance == "off"))
            .unwrap();
        assert!(!off.t3.as_ref().unwrap().enabled);
        assert!(!off.histories.is_empty());
        write(
            root.join("settings.json"),
            r#"{"idleCompaction":"onReturn","compactIdleThreads":true}"#,
        );
        let data = crate::discovery::discover(&w.scan_options().discovery).unwrap();
        assert!(
            data.profiles
                .iter()
                .filter_map(|p| p.t3.as_ref())
                .all(|t| t.idle_mode == T3IdleMode::OnReturn)
        );
        let inherited = data
            .profiles
            .iter()
            .find(|p| p.t3.is_some() && p.harness == Harness::Claude)
            .unwrap();
        // Without its own window, T3 inherits the native files' windows unchanged.
        assert_eq!(inherited.compaction.default.value, Some(400000));
        assert_eq!(
            inherited.compaction.models["claude-opus-5-5"].value,
            Some(350000)
        );
        assert_eq!(
            inherited.compaction.default.comparison_unknown.is_some(),
            managed_policy_unresolved()
        );
    }

    #[test]
    fn launch_flags_parse_like_t3() {
        for (args, flags) in [
            ("", &[][..]),
            ("--chrome", &["chrome"][..]),
            (
                r#"--settings '{"autoCompactWindow":350000}'"#,
                &["settings"],
            ),
            ("--settings=path.json --verbose", &["settings", "verbose"]),
            ("--setting-sources user,project", &["setting-sources"]),
            // An option missing its value leaves the next flag a flag.
            ("--model --settings x", &["model", "settings"]),
            ("--model '--settings'", &["model", "settings"]),
            // Operands after `--`, escaped and quoted values are not flags.
            ("--chrome -- --settings x", &["chrome"]),
            (
                r#"--append-system-prompt "a \"--settings\" b""#,
                &["append-system-prompt"],
            ),
            (r"--add-dir a\ --settings", &["add-dir"]),
            (r"--add-dir a \--settings", &["add-dir"]),
            ("-p --settings", &["settings"]),
            // An unterminated quote runs to the end, as in T3.
            ("--add-dir 'a --settings", &["add-dir"]),
            ("--chrome\u{85}--settings", &["chrome\u{85}--settings"]),
            ("--chrome\u{feff}--settings", &["chrome", "settings"]),
        ] {
            let mut parsed: Vec<String> = launch_flags(args).into_iter().collect();
            parsed.sort();
            let mut flags: Vec<&str> = flags.to_vec();
            flags.sort();
            assert_eq!(parsed, flags, "{args}");
        }
    }

    #[test]
    fn inherited_window_resolves_per_project_unless_launch_settings_can_replace_it() {
        if managed_policy_unresolved() {
            return;
        }
        let w = Workspace::new();
        write(
            w.claude.join("settings.json"),
            r#"{"autoCompactWindow":400000}"#,
        );
        let project = w.home.join("project");
        fs::create_dir_all(project.join(".git")).unwrap();
        write(
            project.join(".claude/settings.json"),
            r#"{"autoCompactWindow":250000}"#,
        );
        state(
            &w,
            json!({"providerInstances":{
                "claudeAgent":{"driver":"claudeAgent","config":{"launchArgs":"--chrome"}},
                "pinned":{"driver":"claudeAgent","config":{"launchArgs":"--chrome","autoCompactWindow":"300000"}},
                "flagged":{"driver":"claudeAgent","config":{"launchArgs":"--setting-sources=user"}},
                "codex":{"driver":"codex","config":{"launchArgs":"-c model=x"}}
            }}),
        );
        let options = w.scan_options();
        let mut data = crate::discovery::discover(&options.discovery).unwrap();
        let instance = |data: &crate::discovery::Discovery, id: &str| {
            data.profiles
                .iter()
                .find(|p| p.t3.as_ref().is_some_and(|t| t.instance == id))
                .unwrap()
                .clone()
        };
        let chrome = instance(&data, "claudeAgent");
        assert_eq!(chrome.compaction.default.value, Some(400000));
        assert_eq!(chrome.compaction.default.comparison_unknown, None);
        let flagged = instance(&data, "flagged");
        assert!(flagged.compaction.default.comparison_unknown.is_some());
        // Launch arguments that can replace settings leave them unresolved; sharing a
        // root alone does not.
        assert!(!chrome.t3.as_ref().unwrap().settings_unresolved);
        assert!(flagged.t3.as_ref().unwrap().settings_unresolved);
        assert!(instance(&data, "codex").t3.unwrap().settings_unresolved);

        let missing = w.home.join("removed");
        let requests: Vec<Request> = [Some(&project), Some(&missing), None]
            .into_iter()
            .flat_map(|cwd| {
                ["claudeAgent", "pinned", "flagged"].map(|id| {
                    let mut r = make_request();
                    r.cwd = cwd.map(|cwd| cwd.to_string_lossy().as_ref().into());
                    r.profiles = vec![instance(&data, id).id].into();
                    r
                })
            })
            .collect();
        crate::discovery::discover_claude_controls(
            &options.discovery,
            &mut data.profiles,
            &requests,
        )
        .unwrap();
        let directories = instance(&data, "claudeAgent").idle_directories.unwrap();
        let window = &directories[project.to_string_lossy().as_ref()]
            .compaction
            .as_ref()
            .unwrap()
            .default;
        assert_eq!(window.value, Some(250000));
        assert_eq!(window.comparison_unknown, None);
        // A missing or unrecorded directory keeps the home's window, as a native profile does.
        assert_eq!(directories.len(), 1);
        // T3's own window and an unresolved launch keep the profile's window everywhere.
        assert!(instance(&data, "pinned").idle_directories.is_none());
        assert!(instance(&data, "flagged").idle_directories.is_none());
    }
    #[cfg(unix)]
    #[test]
    fn custom_state_aliases_shadow_homes_and_disabled_instances_deduplicate() {
        let w = Workspace::new();
        native(&w);
        let shadow = w.home.join("shadow");
        fs::create_dir_all(&shadow).unwrap();
        std::os::unix::fs::symlink(w.codex.join("sessions"), shadow.join("sessions")).unwrap();
        let root = state(
            &w,
            json!({"providerInstances":{"codex":{"driver":"codex","config":{"homePath":w.codex,"shadowHomePath":shadow}},"retired":{"driver":"codex","enabled":false,"config":{"homePath":shadow}}}}),
        );
        let alias = w.home.join("state-alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        let mut options = w.scan_options();
        options.discovery.t3_dirs = vec![
            root.to_string_lossy().into(),
            alias.to_string_lossy().into(),
        ];
        let scan = scan(&options).unwrap();
        assert_eq!(scan.t3.len(), 1);
        assert_eq!(scan.requests.len(), 1);
        assert_eq!(scan.snapshots.len(), 1);
        assert!(
            scan.profiles
                .iter()
                .any(|p| p.t3.as_ref().is_some_and(|t| !t.enabled))
        );
    }
    #[test]
    fn wal_snapshot_is_consistent_and_no_database_is_created() {
        let w = Workspace::new();
        let root = state(&w, json!({}));
        let absent = root.join("statev2.sqlite");
        let captured = scan(&w.scan_options()).unwrap();
        assert!(!absent.exists());
        assert!(captured.t3[0].metadata.contains("unavailable"));
        let mut writer = db(&root);
        writer.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        seed(&writer, "codex", "codex");
        let mut reader =
            Connection::open_with_flags(&absent, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let tx = reader.transaction().unwrap();
        let count = || {
            tx.query_row("SELECT count(*) FROM orchestration_events", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap()
        };
        assert_eq!(count(), 1);
        let write = writer.transaction().unwrap();
        write
            .execute("DELETE FROM orchestration_events", [])
            .unwrap();
        write.commit().unwrap();
        assert_eq!(count(), 1);
        tx.rollback().unwrap();
        assert_eq!(
            reader
                .query_row("SELECT count(*) FROM orchestration_events", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert!(root.join("statev2.sqlite-wal").exists());
        // Corrupt optional schemas never dump SQLite errors or JSON contents.
        writer
            .execute_batch("DROP TABLE orchestration_v2_projection_messages;")
            .unwrap();
        let result = scan(&w.scan_options()).unwrap();
        assert!(result.t3[0].metadata.contains(
            "incompatible or malformed schema (no table orchestration_v2_projection_messages)"
        ));
    }
    #[test]
    fn malformed_turn_rows_are_isolated_and_integral_real_counts_are_supported() {
        let w = Workspace::new();
        let root = state(&w, json!({}));
        let mut database = db(&root);
        seed(&database, "codex", "codex");
        database.execute("INSERT INTO orchestration_v2_projection_provider_threads SELECT 'legacy',thread_id,driver,NULL,updated_at,payload_json FROM orchestration_v2_projection_provider_threads",[]).unwrap();
        database.execute("UPDATE orchestration_v2_projection_provider_turns SET payload_json=json_set(payload_json,'$.tokenUsage.usedTokens',300000.0)",[]).unwrap();
        // Codex joins never read item payloads, so an oversized one cannot sink the snapshot.
        database.execute("INSERT INTO orchestration_v2_projection_turn_items VALUES('item','thread','turn','assistant_message','completed',?1,'2026-01-01T00:00:00Z',NULL,0)",[json!({"nativeItemRef":{"driver":"codex","nativeId":"item"},"text":"x".repeat(2_000_000)}).to_string()]).unwrap();
        database
            .set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH, 1_048_576)
            .unwrap();
        database.execute("INSERT INTO orchestration_v2_projection_provider_turns VALUES('orphan','thread','pt','absent',NULL,'2026-01-01T00:00:00Z',NULL,?1,2)",[json!({"nativeTurnRef":{"driver":"codex","nativeId":"orphan"},"tokenUsage":{"usedTokens":"PRIVATE_BAD_VALUE"}}).to_string()]).unwrap();
        let valid = NativeRequest {
            id: "valid",
            root: "opaque-session",
            native_session: "native-session",
            native_turns: vec!["missing", "native-turn", "native-turn"],
            harness: Harness::Codex,
            time: 1_767_225_605_000,
            model: Some("gpt-6.1-sol"),
            profiles: &[],
        };
        let orphan = NativeRequest {
            id: "orphan",
            native_turns: vec!["orphan"],
            ..valid
        };
        let tx = database.transaction().unwrap();
        let data = metadata(&tx, &[&valid, &orphan], "2026-01-01T00:00:20Z", None).unwrap();
        assert_eq!(
            data.owners[&(Harness::Codex, "opaque-session".into())].len(),
            2
        );
        assert!(data.owners[&(Harness::Codex, "opaque-session".into())].contains(&String::new()));
        assert_eq!(data.turns["valid"].count, 1);
        assert_eq!(
            data.turns["valid"]
                .joined
                .as_ref()
                .unwrap()
                .evidence
                .used_tokens,
            Some(300000)
        );
        assert_eq!(
            data.turns["valid"]
                .joined
                .as_ref()
                .unwrap()
                .requested
                .as_ref()
                .unwrap()
                .tier
                .as_deref(),
            Some("fast")
        );
        assert_eq!(data.turns["orphan"].count, 1);
        assert!(data.turns["orphan"].joined.is_none());
    }

    #[test]
    fn shared_history_ambiguity_keeps_background_controls_but_prices_nothing_without_a_share() {
        let w = Workspace::new();
        let cwd = w.home.join("project");
        fs::create_dir_all(cwd.join(".git")).unwrap();
        write(
            cwd.join(".claude/settings.local.json"),
            r#"{"env":{"CLAUDE_CODE_ENABLE_AWAY_SUMMARY":"1"}}"#,
        );
        write(
            cwd.join(".codex/config.toml"),
            "[tui]\nauto_recap = false\n",
        );
        native(&w);
        let claude = w.claude.join("projects/project/native.jsonl");
        w.fixture(&claude, "claude.jsonl");
        let mut options = w.scan_options();
        options
            .discovery
            .env
            .insert("CLAUDE_CODE_ENABLE_AWAY_SUMMARY".into(), "0".into());
        let codex = w.codex.join("sessions/test.jsonl");
        for path in [&claude, &codex] {
            let rows: Vec<Value> = fs::read_to_string(path)
                .unwrap()
                .lines()
                .map(|line| {
                    let mut row: Value = serde_json::from_str(line).unwrap();
                    if path == &claude {
                        row["cwd"] = json!(cwd);
                        row["entrypoint"] = json!("t3-code");
                    } else {
                        row["payload"]["cwd"] = json!(cwd);
                        if row["type"] == "session_meta" {
                            row["payload"]["source"] = json!("app-server");
                            row["payload"]["originator"] = json!("T3 Code");
                        }
                    }
                    row
                })
                .collect();
            write(
                path,
                rows.iter()
                    .map(|row| format!("{row}\n"))
                    .collect::<String>(),
            );
        }
        let native = scan(&options).unwrap();
        assert!(
            native
                .profiles
                .iter()
                .all(|p| !p.background_directories.is_empty())
        );
        assert!(
            native
                .profiles
                .iter()
                .any(|p| !p.background_requests.is_empty())
        );
        let root = state(&w, json!({}));
        db(&root);
        let data = scan(&options).unwrap();
        assert_eq!(data.requests.len(), 3);
        assert!(data.requests.iter().all(|r| r.profiles.len() == 2));
        let catalog = crate::test_support::prices();
        let checks = crate::background::background_checks(&crate::background::BackgroundInput {
            scan: &data,
            catalog: &catalog,
            since: None,
            now: timestamp(&json!("2026-11-02T00:00:00Z")).unwrap(),
            max_cost_per_use_usd: 1.0,
        });
        // The settings are still read; T3 settled none of the requests, so no share of
        // them can be priced on the native lines.
        assert!(!checks.is_empty());
        for check in &checks {
            assert_eq!(check.base.action, Action::Advisory, "{check:?}");
            assert_eq!(check.base.savings_usd, None, "{check:?}");
        }
        assert!(
            data.profiles
                .iter()
                .filter(|p| p.t3.is_none())
                .all(|p| !p.background_directories.is_empty())
        );
    }

    #[test]
    fn native_cli_provenance_survives_readable_t3_database_and_explicit_roots_stay_named() {
        let w = Workspace::new();
        let root = state(&w, json!({}));
        db(&root);
        native(&w);
        write(
            w.claude.join("projects/p/native.jsonl"),
            format!(
                "{}\n{}\n",
                json!({"type":"api-request","id":"api","sessionId":"native-claude","timestamp":"2026-01-01T00:00:00Z","querySource":"repl_main_thread","params":{"model":"claude-opus-5-5"}}),
                json!({"type":"assistant","uuid":"native-uuid","sessionId":"native-claude","timestamp":"2026-01-01T00:00:01Z","entrypoint":"cli","requestRef":"api","message":{"id":"native-message","model":"claude-opus-5-5","usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}})
            ),
        );
        // Subagents carry no REPL or CLI provenance of their own; they follow their root.
        write(
            w.claude
                .join("projects/p/native-claude/subagents/agent-a1.jsonl"),
            format!(
                "{}\n",
                json!({"type":"assistant","uuid":"sub-uuid","sessionId":"native-claude","isSidechain":true,"timestamp":"2026-01-01T00:00:02Z","entrypoint":"cli","message":{"id":"sub-message","model":"claude-opus-5-5","usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}})
            ),
        );
        write(
            w.codex.join("sessions/subagent.jsonl"),
            concat!(
                "{\"type\":\"session_meta\",\"timestamp\":\"2026-01-01T00:00:02Z\",\"payload\":{\"id\":\"sub-thread\",\"session_id\":\"native-session\",\"parent_thread_id\":\"native-session\",\"model_provider\":\"openai\",\"originator\":\"codex_exec\",\"source\":{\"subagent\":\"review\"}}}\n",
                "{\"type\":\"turn_context\",\"timestamp\":\"2026-01-01T00:00:03Z\",\"payload\":{\"turn_id\":\"sub-turn\",\"model\":\"gpt-6.1-sol\"}}\n",
                "{\"type\":\"token_usage_record\",\"timestamp\":\"2026-01-01T00:00:04Z\",\"payload\":{\"response_id\":\"sub-response\",\"turn_id\":\"sub-turn\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1,\"cached_input_tokens\":0,\"cache_write_input_tokens\":0}}}\n"
            ),
        );
        let data = scan(&w.scan_options()).unwrap();
        assert_eq!(data.requests.len(), 4);
        assert_eq!(
            data.requests
                .iter()
                .filter(|r| r.agent == Agent::Subagent)
                .count(),
            2
        );
        for r in &data.requests {
            assert_eq!(r.profiles.len(), 1, "{r:?}");
            let p = data
                .profiles
                .iter()
                .find(|p| p.id == r.profiles[0])
                .unwrap();
            assert!(p.t3.is_none(), "{r:?}");
            assert!(!r.issues.iter().any(|s| s.contains("unresolved")));
        }
        assert!(data.t3[0].unresolved.is_empty());
        let mut options = w.scan_options();
        options.discovery.t3_dirs = vec![root.to_string_lossy().into_owned()];
        let explicit = scan(&options).unwrap();
        assert_eq!(explicit.requests.len(), 4);
        for r in &explicit.requests {
            assert!(!r.profiles.is_empty());
            assert!(r.issues.iter().any(|s| s.contains("advisory")));
        }
    }

    #[test]
    fn subagents_share_their_root_sessions_t3_owner() {
        for harness in [Harness::Claude, Harness::Codex] {
            let w = Workspace::new();
            let root = state(
                &w,
                json!({"providerInstances":{"instance":{"driver":if harness == Harness::Claude {"claudeAgent"} else {"codex"}}}}),
            );
            let database = db(&root);
            seed(
                &database,
                if harness == Harness::Claude {
                    "claudeAgent"
                } else {
                    "codex"
                },
                "instance",
            );
            if harness == Harness::Claude {
                write(
                    w.claude
                        .join("projects/p/native-session/subagents/agent-a1.jsonl"),
                    format!(
                        "{}\n",
                        json!({"type":"assistant","uuid":"sub-uuid","sessionId":"native-session","isSidechain":true,"timestamp":"2026-01-01T00:00:05Z","entrypoint":"sdk-ts","message":{"id":"sub-message","model":"claude-opus-5-5","usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}})
                    ),
                );
            } else {
                // A nested spawn names only its parent, which names the root.
                write(
                    w.codex.join("sessions/subagent.jsonl"),
                    concat!(
                        "{\"type\":\"session_meta\",\"timestamp\":\"2026-01-01T00:00:02Z\",\"payload\":{\"id\":\"sub-thread\",\"session_id\":\"native-session\",\"model_provider\":\"openai\",\"source\":{\"subagent\":\"review\"}}}\n",
                        "{\"type\":\"session_meta\",\"timestamp\":\"2026-01-01T00:00:03Z\",\"payload\":{\"id\":\"nested-thread\",\"model_provider\":\"openai\",\"source\":{\"subagent\":{\"thread_spawn\":{\"parent_thread_id\":\"sub-thread\",\"depth\":2}}}}}\n",
                        "{\"type\":\"turn_context\",\"timestamp\":\"2026-01-01T00:00:03Z\",\"payload\":{\"turn_id\":\"sub-turn\",\"model\":\"gpt-6.1-sol\"}}\n",
                        "{\"type\":\"token_usage_record\",\"timestamp\":\"2026-01-01T00:00:04Z\",\"payload\":{\"response_id\":\"sub-response\",\"turn_id\":\"sub-turn\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1,\"cached_input_tokens\":0,\"cache_write_input_tokens\":0}}}\n"
                    ),
                );
            }
            let data = scan(&w.scan_options()).unwrap();
            let t3 = data.profiles.iter().find(|p| p.t3.is_some()).unwrap();
            // A T3 Codex instance's instruction budget is never assumed from its home alone.
            assert_eq!(
                t3.instruction_budget_unresolved.is_some(),
                harness == Harness::Codex
            );
            let owner = t3.id.clone();
            assert_eq!(data.requests.len(), 1, "{harness:?}");
            let r = &data.requests[0];
            assert_eq!(r.agent, Agent::Subagent, "{harness:?}");
            assert_eq!(*r.profiles, [owner], "{harness:?}");
            assert!(r.t3.is_none());
            assert!(data.t3[0].unresolved.is_empty(), "{:?}", data.t3[0]);
        }
    }

    #[test]
    fn a_window_follows_a_nested_subagent_through_its_earlier_parent_to_the_root_owner() {
        let w = Workspace::new();
        let root = state(
            &w,
            json!({"providerInstances":{"instance":{"driver":"codex"}}}),
        );
        seed(&db(&root), "codex", "instance");
        // The parent's only record precedes the window, in a transcript of its own.
        write(
            w.codex.join("sessions/parent.jsonl"),
            "{\"type\":\"session_meta\",\"timestamp\":\"2026-01-01T00:00:02Z\",\"payload\":{\"id\":\"sub-thread\",\"session_id\":\"native-session\",\"model_provider\":\"openai\",\"source\":{\"subagent\":\"review\"}}}\n",
        );
        write(
            w.codex.join("sessions/nested.jsonl"),
            concat!(
                "{\"type\":\"session_meta\",\"timestamp\":\"2026-01-01T00:00:03Z\",\"payload\":{\"id\":\"nested-thread\",\"model_provider\":\"openai\",\"source\":{\"subagent\":{\"thread_spawn\":{\"parent_thread_id\":\"sub-thread\",\"depth\":2}}}}}\n",
                "{\"type\":\"turn_context\",\"timestamp\":\"2026-01-01T00:00:03Z\",\"payload\":{\"turn_id\":\"sub-turn\",\"model\":\"gpt-6.1-sol\"}}\n",
                "{\"type\":\"token_usage_record\",\"timestamp\":\"2026-01-01T00:00:04Z\",\"payload\":{\"response_id\":\"sub-response\",\"turn_id\":\"sub-turn\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1,\"cached_input_tokens\":0,\"cache_write_input_tokens\":0}}}\n"
            ),
        );
        let options = |reach| crate::reader::ScanOptions {
            since: timestamp(&json!("2026-01-01T00:00:03Z")),
            reach,
            ..w.scan_options()
        };
        let history = scan(&options(crate::reader::Reach::History)).unwrap();
        let window = scan(&options(crate::reader::Reach::Window)).unwrap();
        let owner = &history.profiles.iter().find(|p| p.t3.is_some()).unwrap().id;
        assert_eq!(history.requests.len(), 1);
        assert_eq!(history.requests[0].profiles[..], [owner.as_str()]);
        assert_eq!(window.requests, history.requests);
        assert_eq!(window.t3[0].unresolved, history.t3[0].unresolved);
        assert_eq!(window.snapshots.len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn subagents_of_a_native_only_root_exclude_t3() {
        let w = Workspace::new();
        let root = state(&w, json!({}));
        // Any driver's item dates T3's records, so the session without one is native.
        db(&root).execute("INSERT INTO orchestration_v2_projection_turn_items VALUES('turn-item:provider:codex:native-item:x','thread','turn','assistant_message','completed','{}','2025-01-01T00:00:00Z',NULL,0)",[]).unwrap();
        let usage = json!({"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0});
        write(
            w.claude.join("projects/p/native.jsonl"),
            format!(
                "{}\n",
                json!({"type":"assistant","uuid":"print-uuid","sessionId":"native","timestamp":"2026-01-01T00:00:00Z","entrypoint":"sdk-cli","message":{"id":"print-message","model":"claude-opus-5-5","usage":usage}})
            ),
        );
        write(
            w.claude.join("projects/p/native/subagents/agent-a1.jsonl"),
            format!(
                "{}\n",
                json!({"type":"assistant","uuid":"sub-uuid","sessionId":"native","isSidechain":true,"timestamp":"2026-01-01T00:00:01Z","message":{"id":"sub-message","model":"claude-opus-5-5","usage":usage}})
            ),
        );
        let other = w.home.join("other-claude");
        fs::create_dir_all(&other).unwrap();
        std::os::unix::fs::symlink(w.claude.join("projects"), other.join("projects")).unwrap();
        let mut options = w.scan_options();
        options.discovery.claude_dirs = vec![
            w.claude.to_string_lossy().into(),
            other.to_string_lossy().into(),
        ];
        options.discovery.t3_dirs = vec![root.to_string_lossy().into()];
        let data = scan(&options).unwrap();
        let t3: HashSet<&String> = data
            .profiles
            .iter()
            .filter(|p| p.t3.is_some())
            .map(|p| &p.id)
            .collect();
        assert_eq!(data.requests.len(), 2);
        for r in &data.requests {
            assert_eq!(r.profiles.len(), 2, "{r:?}");
            assert!(r.profiles.iter().all(|p| !t3.contains(p)), "{r:?}");
        }
        assert!(data.t3[0].unresolved.is_empty(), "{:?}", data.t3[0]);
    }

    #[test]
    fn subagents_inherit_their_roots_resolution_inside_the_window() {
        let w = Workspace::new();
        let root = state(&w, json!({}));
        // Any driver's item dates T3's records, so the session without one is native.
        db(&root).execute("INSERT INTO orchestration_v2_projection_turn_items VALUES('turn-item:provider:codex:native-item:x','thread','turn','assistant_message','completed','{}','2025-01-01T00:00:00Z',NULL,0)",[]).unwrap();
        let usage = json!({"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0});
        write(
            w.claude.join("projects/p/native.jsonl"),
            format!(
                "{}\n{}\n{}\n",
                json!({"type":"assistant","uuid":"print-uuid","sessionId":"native-claude","timestamp":"2026-01-01T00:00:00Z","entrypoint":"sdk-cli","message":{"id":"print-message","model":"claude-opus-5-5","usage":usage}}),
                json!({"type":"api-request","id":"api","sessionId":"native-claude","timestamp":"2026-01-03T00:00:00Z","querySource":"repl_main_thread","params":{"model":"claude-opus-5-5"}}),
                json!({"type":"assistant","uuid":"native-uuid","sessionId":"native-claude","timestamp":"2026-01-03T00:00:01Z","entrypoint":"cli","requestRef":"api","message":{"id":"native-message","model":"claude-opus-5-5","usage":usage}})
            ),
        );
        write(
            w.claude
                .join("projects/p/native-claude/subagents/agent-a1.jsonl"),
            format!(
                "{}\n",
                json!({"type":"assistant","uuid":"sub-uuid","sessionId":"native-claude","isSidechain":true,"timestamp":"2026-01-03T00:00:02Z","entrypoint":"cli","message":{"id":"sub-message","model":"claude-opus-5-5","usage":usage}})
            ),
        );
        let mut options = w.scan_options();
        options.since = timestamp(&json!("2026-01-02T00:00:00Z"));
        let data = scan(&options).unwrap();
        let sub = data
            .requests
            .iter()
            .find(|r| r.agent == Agent::Subagent)
            .unwrap();
        assert_eq!(sub.profiles.len(), 1, "{sub:?}");
        assert!(data.t3[0].unresolved.is_empty(), "{:?}", data.t3[0]);
    }

    #[test]
    fn sessions_without_a_t3_thread_resolve_by_turn_items_or_codex_label() {
        let w = Workspace::new();
        let root = state(&w, json!({}));
        let database = db(&root);
        seed(&database, "claudeAgent", "claudeAgent");
        database.execute("INSERT INTO orchestration_v2_projection_provider_threads VALUES('pt2','thread2','claudeAgent','claudeAgent','2026-01-01T00:00:10Z',?1)",[json!({"nativeThreadRef":{"nativeId":"other-session"}}).to_string()]).unwrap();
        database.execute("INSERT INTO orchestration_v2_projection_provider_turns VALUES('turn2','thread2','pt2',NULL,'completed',NULL,NULL,'{}',1)",[]).unwrap();
        // Inserted first, so it dates when T3 began recording provider turn items.
        for (native, turn) in [
            ("cleared-uuid", "turn"),
            ("think-message:thinking:3", "turn"),
            ("orphan-uuid", "missing-turn"),
            ("split-uuid", "turn"),
            ("split-tool", "turn2"),
            ("window-early-uuid", "turn2"),
            ("window-uuid", "turn"),
            ("late-uuid", "turn3"),
            ("late-tool", "turn"),
        ] {
            let id = format!("{ITEM_PREFIX}{}", native.replace(':', "%3A"));
            let thread = match turn {
                "turn2" => "thread2",
                "turn3" => "thread3",
                _ => "thread",
            };
            database.execute("INSERT INTO orchestration_v2_projection_turn_items VALUES(?1,?2,?3,'assistant_message','completed','{}','2026-01-01T00:00:00Z',NULL,0)",params![id,thread,turn]).unwrap();
        }
        // Linked after the native snapshot, with no earlier event to prove the link.
        database.execute("INSERT INTO orchestration_v2_projection_provider_threads VALUES('pt3','thread3','claudeAgent','claudeAgent','2999-01-01T00:00:00Z',?1)",[json!({"nativeThreadRef":{"nativeId":"third-session"}}).to_string()]).unwrap();
        database.execute("INSERT INTO orchestration_v2_projection_provider_turns VALUES('turn3','thread3','pt3',NULL,'completed',NULL,NULL,'{}',1)",[]).unwrap();
        // Each session gets its own second so its request can be found again.
        let row = |session: &str, time: &str, entrypoint: &str, content: Value| json!({"type":"assistant","uuid":format!("{session}-uuid"),"sessionId":session,"timestamp":time,"entrypoint":entrypoint,"message":{"id":format!("{session}-message"),"model":"claude-opus-5-5","content":content,"usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}});
        for (session, time, entrypoint, content) in [
            // `/clear` in a T3 thread: a new native id, items under the old provider thread.
            ("cleared", "2026-01-01T00:00:05Z", "sdk-ts", json!([])),
            (
                "think",
                "2026-01-01T00:00:06Z",
                "sdk-ts",
                json!([{"type":"thinking","thinking":""}]),
            ),
            // `claude -p` from a T3 agent's shell inherits T3's label but has no item.
            ("print", "2026-01-01T00:00:07Z", "t3-code", json!([])),
            ("orphan", "2026-01-01T00:00:08Z", "sdk-ts", json!([])),
            (
                "split",
                "2026-01-01T00:00:09Z",
                "sdk-ts",
                json!([{"type":"tool_use","id":"split-tool","name":"Bash","input":{}}]),
            ),
            ("old", "2025-12-31T00:00:00Z", "cli", json!([])),
            // Its other item's thread is linked, but cannot make the session's owner unique.
            (
                "late",
                "2026-01-01T00:00:12Z",
                "sdk-ts",
                json!([{"type":"tool_use","id":"late-tool","name":"Bash","input":{}}]),
            ),
        ] {
            write(
                w.claude.join(format!("projects/p/{session}.jsonl")),
                format!("{}\n", row(session, time, entrypoint, content)),
            );
        }
        // Inside a later window only the second request decides the owner.
        let mut early = row("window", "2026-01-01T00:00:03Z", "sdk-ts", json!([]));
        early["uuid"] = json!("window-early-uuid");
        early["message"]["id"] = json!("window-early-message");
        write(
            w.claude.join("projects/p/window.jsonl"),
            format!(
                "{early}\n{}\n",
                row("window", "2026-01-01T00:00:11Z", "sdk-ts", json!([]))
            ),
        );
        // A subagent records no items, so a missing root cannot exclude T3.
        let mut sub = row("rootless", "2026-01-01T00:00:13Z", "sdk-ts", json!([]));
        sub["isSidechain"] = json!(true);
        write(
            w.claude
                .join("projects/p/rootless/subagents/agent-a1.jsonl"),
            format!("{sub}\n"),
        );
        // A missing originator proves nothing.
        for (name, originator) in [
            ("vscode", json!("codex_vscode")),
            ("t3", json!("T3 Code")),
            ("bare", Value::Null),
        ] {
            write(
                w.codex.join(format!("sessions/{name}.jsonl")),
                format!(
                    "{}\n{}\n{}\n",
                    json!({"type":"session_meta","timestamp":"2026-01-01T00:00:00Z","payload":{"id":format!("{name}-thread"),"model_provider":"openai","originator":originator,"source":"vscode"}}),
                    json!({"type":"turn_context","timestamp":"2026-01-01T00:00:01Z","payload":{"turn_id":format!("{name}-turn"),"model":"gpt-6.1-sol"}}),
                    json!({"type":"token_usage_record","timestamp":"2026-01-01T00:00:02Z","payload":{"response_id":format!("{name}-response"),"turn_id":format!("{name}-turn"),"usage":{"input_tokens":10,"output_tokens":1,"cached_input_tokens":0,"cache_write_input_tokens":0}}})
                ),
            );
        }
        let data = scan(&w.scan_options()).unwrap();
        assert_eq!(data.requests.len(), 13);
        let t3 = |p: &String| data.profiles.iter().any(|q| &q.id == p && q.t3.is_some());
        let at = |time: &str| {
            let time = timestamp(&json!(time)).unwrap();
            data.requests
                .iter()
                .find(|r| r.harness == Harness::Claude && r.time == time)
                .unwrap()
        };
        for time in ["2026-01-01T00:00:05Z", "2026-01-01T00:00:06Z"] {
            let r = at(time);
            assert!(r.profiles.len() == 1 && t3(&r.profiles[0]), "{r:?}");
        }
        let print = at("2026-01-01T00:00:07Z");
        assert!(
            !print.profiles.is_empty() && !print.profiles.iter().any(t3),
            "{print:?}"
        );
        for time in [
            "2026-01-01T00:00:08Z",
            "2026-01-01T00:00:09Z",
            "2025-12-31T00:00:00Z",
            "2026-01-01T00:00:11Z",
            "2026-01-01T00:00:12Z",
            "2026-01-01T00:00:13Z",
        ] {
            let r = at(time);
            assert!(r.profiles.len() > 1, "{r:?}");
        }
        for r in data.requests.iter().filter(|r| r.harness == Harness::Codex) {
            let labelled = r.client.is_none() || r.client.as_deref() == Some("T3 Code");
            assert_eq!(r.profiles.iter().any(t3), labelled, "{r:?}");
            assert_eq!(r.profiles.len() > 1, labelled, "{r:?}");
        }
        let mut causes: Vec<(&str, u64)> = data.t3[0]
            .unresolved
            .iter()
            .map(|(c, n)| (c.as_ref(), *n))
            .collect();
        causes.sort();
        assert_eq!(
            causes,
            [
                (
                    "a T3 turn matches the request, but no T3 thread owns its native session",
                    1
                ),
                (
                    "no T3 provider thread owns the native session, but its client label is one T3 writes",
                    2
                ),
                ("several T3 provider threads own the native session", 3),
                (
                    "the native session's T3 provider thread was linked after the native snapshot",
                    1
                ),
                ("the request predates T3's oldest retained turn item", 1),
                (
                    "the subagent's root session has no transcript and no T3 provider thread",
                    1
                ),
            ]
        );
        let mut options = w.scan_options();
        options.since = timestamp(&json!("2026-01-01T00:00:10Z"));
        let window = scan(&options).unwrap();
        let r = window
            .requests
            .iter()
            .find(|r| r.time == timestamp(&json!("2026-01-01T00:00:11Z")).unwrap())
            .unwrap();
        let t3 = |p: &String| window.profiles.iter().any(|q| &q.id == p && q.t3.is_some());
        assert!(r.profiles.len() == 1 && t3(&r.profiles[0]), "{r:?}");
        // Settings resolution also reflects platform policy (always unresolved on Windows),
        // so check the native speed T3 overrides, which shared-history ambiguity leaves
        // unresolved, while the TTL every possible owner reads from one file stays known,
        // unless a machine policy may replace T3's settings and so its line.
        let ambiguous = |s: &Scan| {
            s.profiles
                .iter()
                .filter(|p| p.t3.is_none())
                .any(|p| p.speed.as_ref().is_some_and(|s| !s.resolved))
        };
        assert!(ambiguous(&data));
        assert_eq!(
            data.profiles.iter().any(|p| {
                p.ttl
                    .main
                    .reason
                    .contains("unresolved T3 control ownership")
            }),
            managed_policy_unresolved()
        );
        // History before the window leaves current ownership alone.
        let mut options = w.scan_options();
        options.since = timestamp(&json!("2026-01-02T00:00:00Z"));
        let later = scan(&options).unwrap();
        assert!(later.t3[0].unresolved.is_empty());
        assert!(!ambiguous(&later));
    }

    #[test]
    fn text_generation_resolves_defaults_fallbacks_and_overrides() {
        let w = Workspace::new();
        let resolve = |settings: Value| {
            let root = state(&w, settings);
            let data = scan(&w.scan_options()).unwrap();
            assert!(!format!("{data:?}").contains("PRIVATE_KEY"));
            let g = data.t3[0].text_generation.clone().unwrap();
            assert_eq!(
                g.owner,
                format!(
                    "{}:textGenerationModelSelection",
                    Path::new(&crate::discovery::realpath(&root.to_string_lossy()).unwrap())
                        .join("settings.json")
                        .display()
                )
            );
            g
        };
        let pick = |g: &T3TextGeneration| (g.selection.instance.clone(), g.selection.model.clone());
        let pair = |i: &str, m: &str| (i.to_string(), m.to_string());

        let g = resolve(json!({}));
        assert_eq!(
            (pick(&g), g.resolution.as_str()),
            (pair("codex", "gpt-6-luna"), "T3 default")
        );
        assert!(g.overrides.is_empty() && g.project_overrides == 0);

        // Legacy `provider` keys still route; an enabled selection is kept as set.
        let g = resolve(
            json!({"textGenerationModelSelection":{"provider":"claudeAgent","model":" claude-x "}}),
        );
        assert_eq!(
            (pick(&g), g.resolution.as_str()),
            (pair("claudeAgent", "claude-x"), "set")
        );

        // Either enabled flag disables codex; the fallback keeps a custom thread default.
        let g = resolve(
            json!({"providerInstances":{"codex":{"driver":"codex","config":{"enabled":false}}}}),
        );
        assert_eq!(pick(&g), pair("claudeAgent", "claude-haiku-5-5"));
        assert!(g.resolution.contains("falls back to instance claudeAgent"));
        let g = resolve(json!({"providers":{"codex":{"enabled":false}}}));
        assert_eq!(pick(&g), pair("claudeAgent", "claude-haiku-5-5"));
        let g = resolve(json!({
            "defaultModelSelection":{"instanceId":"claudeAgent","model":"custom-claude"},
            "providerInstances":{"codex":{"driver":"codex","enabled":false},"claudeAgent":{"driver":"claudeAgent","config":{"customModels":[{"slug":"custom-claude"}]}}}
        }));
        assert_eq!(pick(&g), pair("claudeAgent", "custom-claude"));

        // Default-disabled drivers fall back only when enabled; none enabled keeps the selection.
        let g = resolve(json!({"providerInstances":{
            "codex":{"driver":"codex","enabled":false},"claudeAgent":{"driver":"claudeAgent","enabled":false},
            "cursor":{"driver":"cursor"},"grok":{"driver":"grok","enabled":true}}}));
        assert_eq!(pick(&g), pair("grok", "grok-build"));
        let g = resolve(json!({"providerInstances":{
            "codex":{"driver":"codex","enabled":false},"claudeAgent":{"driver":"claudeAgent","enabled":false}}}));
        assert_eq!(pick(&g), pair("codex", "gpt-6-luna"));
        assert!(g.resolution.contains("no enabled instance"));

        // A text generator takes precedence over an instance id; a disabled one falls back.
        let g = resolve(json!({
            "textGenerators":{"api":{"kind":"openai","apiKey":"PRIVATE_KEY"}},
            "textGenerationModelSelection":{"instanceId":"api","model":"small"},
            "sourceControlWriterModelSelection":{"instanceId":"api","model":"writer"},
            "reviewGuideModelSelection":{"instanceId":"api","model":"guide"},
            "speechPostProcessingModelSelection":{"instanceId":"cursor","model":"off"},
            "projectSettingsOverrides":{"a":{"reviewGuideModelSelection":null},"b":{"defaultAutoPull":true}}
        }));
        assert_eq!(
            (pick(&g), g.resolution.as_str()),
            (pair("api", "small"), "set")
        );
        // Review guides never use text generators, and cursor is disabled by default.
        assert_eq!(
            g.overrides.keys().collect::<Vec<_>>(),
            ["sourceControlWriterModelSelection"]
        );
        assert_eq!(g.project_overrides, 1);
        let g = resolve(json!({
            "textGenerators":{"api":{"kind":"openai","enabled":false}},
            "textGenerationModelSelection":{"instanceId":"api","model":"small"}
        }));
        assert_eq!(pick(&g), pair("codex", "gpt-6-luna"));

        for selection in [
            json!({"model":"x"}),
            json!({"instanceId":"codex","model":" "}),
            json!({"instanceId":"codex","model":"a\u{1b}b"}),
        ] {
            state(&w, json!({"textGenerationModelSelection":selection}));
            let error = scan(&w.scan_options()).unwrap_err();
            assert!(error.message.contains("invalid T3 settings"));
        }
    }

    #[test]
    fn unknown_provider_fields_are_skipped_but_known_settings_fail_loudly() {
        let w = Workspace::new();
        let root = state(
            &w,
            json!({"providerInstances":{"other.provider":{"driver":"other","enabled":"true","environment":[{"value":"PRIVATE_ENV"},{"name":"CODEX_HOME","value":42}],"config":{"homePath":{"PRIVATE_CONFIG":42}}}}}),
        );
        let data = scan(&w.scan_options()).unwrap();
        assert!(
            data.t3[0]
                .exclusions
                .iter()
                .any(|s| s.contains("unsupported"))
        );
        let debug = format!("{data:?}");
        assert!(!debug.contains("PRIVATE_ENV") && !debug.contains("PRIVATE_CONFIG"));
        write(
            root.join("settings.json"),
            r#"{"idleCompaction":"unsupported-new-mode"}"#,
        );
        let error = scan(&w.scan_options()).unwrap_err();
        assert!(error.message.contains("invalid T3 settings"));
    }

    #[test]
    fn owner_updates_after_native_bound_need_a_prior_matching_link_event() {
        let w = Workspace::new();
        let root = state(&w, json!({}));
        let mut database = db(&root);
        seed(&database, "codex", "codex");
        database.execute("UPDATE orchestration_v2_projection_provider_threads SET updated_at='2026-01-01T00:01:00Z'",[]).unwrap();
        let request = NativeRequest {
            id: "request",
            root: "opaque-session",
            native_session: "native-session",
            native_turns: vec![],
            harness: Harness::Codex,
            time: 1_767_225_605_000,
            model: None,
            profiles: &[],
        };
        let tx = database.transaction().unwrap();
        let data = metadata(&tx, &[&request], "2026-01-01T00:00:20Z", None).unwrap();
        assert!(data.owners[&(Harness::Codex, "opaque-session".into())].is_empty());
        tx.rollback().unwrap();
        database.execute("INSERT INTO orchestration_events VALUES(2,'thread',2,'thread','provider-thread.updated','2026-01-01T00:00:01Z',?1)",[json!({"id":"pt","providerInstanceId":"codex","nativeThreadRef":{"driver":"codex","nativeId":"native-session"},"private":"PRIVATE_HISTORY"}).to_string()]).unwrap();
        let tx = database.transaction().unwrap();
        let data = metadata(&tx, &[&request], "2026-01-01T00:00:20Z", None).unwrap();
        assert_eq!(
            data.owners[&(Harness::Codex, "opaque-session".into())],
            vec!["codex"]
        );
    }

    #[test]
    fn native_items_join_by_item_id_without_reading_payloads_and_name_the_resource_bound() {
        let w = Workspace::new();
        let root = state(&w, json!({}));
        let mut database = db(&root);
        seed(&database, "claudeAgent", "claude");
        let item = |database: &Connection, kind: &str, native: &str, extra: &str| {
            database.execute("INSERT INTO orchestration_v2_projection_turn_items VALUES(?1,'thread','turn',?2,'completed',?3,'2026-01-01T00:00:00Z',NULL,0)",params![format!("turn-item:provider:claudeAgent:native-item:{native}"),kind,json!({"output":extra}).to_string()]).unwrap();
        };
        item(&database, "assistant_message", "assistant-uuid", "");
        // Tool outputs can exceed the length bound; the join never reads them.
        item(
            &database,
            "command_execution",
            "toolu_1",
            &"x".repeat(2_000_000),
        );
        item(&database, "reasoning", "msg_1%3Athinking%3A1", "");
        // An item naming the turn from another thread is not the turn's.
        database.execute("INSERT INTO orchestration_v2_projection_turn_items VALUES(?1,'elsewhere','turn','assistant_message','completed','{}','2026-01-01T00:00:00Z',NULL,0)",[format!("{ITEM_PREFIX}stray-uuid")]).unwrap();
        // The selected provider thread's turns can sit in more than one T3 thread, and a
        // walked T3 thread can hold an unselected provider thread's turns, whose payload
        // past the length bound the join never reads.
        for (turn, thread, provider, native, large) in [
            ("moved-turn", "moved", "pt", "moved-uuid", ""),
            (
                "unselected-turn",
                "thread",
                "pt-unselected",
                "unselected-uuid",
                &*"x".repeat(2_000_000),
            ),
        ] {
            database.execute("INSERT INTO orchestration_v2_projection_provider_turns VALUES(?1,?2,?3,NULL,'completed',NULL,NULL,?4,2)",params![turn,thread,provider,json!({"nativeTurnRef":{"driver":"claudeAgent"},"large":large}).to_string()]).unwrap();
            database.execute("INSERT INTO orchestration_v2_projection_turn_items VALUES(?1,?2,?3,'assistant_message','completed','{}','2026-01-01T00:00:00Z',NULL,1)",params![format!("{ITEM_PREFIX}{native}"),thread,turn]).unwrap();
        }
        let request = NativeRequest {
            id: "assistant",
            root: "opaque-session",
            native_session: "native-session",
            native_turns: vec!["assistant-uuid"],
            harness: Harness::Claude,
            time: 1_767_225_605_000,
            model: None,
            profiles: &[],
        };
        let tool = NativeRequest {
            id: "tool",
            native_turns: vec!["record-uuid", "toolu_1"],
            ..request
        };
        let thinking = NativeRequest {
            id: "thinking",
            native_turns: vec!["msg_1:thinking"],
            ..request
        };
        let other = NativeRequest {
            id: "other",
            native_session: "other-session",
            native_turns: vec!["assistant-uuid"],
            ..request
        };
        let stray = NativeRequest {
            id: "stray",
            native_turns: vec!["stray-uuid"],
            ..request
        };
        let moved = NativeRequest {
            id: "moved",
            native_turns: vec!["moved-uuid"],
            ..request
        };
        let unselected = NativeRequest {
            id: "unselected",
            native_turns: vec!["unselected-uuid"],
            ..request
        };
        database
            .set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH, 1_048_576)
            .unwrap();
        let tx = database.transaction().unwrap();
        let data = metadata(
            &tx,
            &[
                &request,
                &tool,
                &thinking,
                &other,
                &stray,
                &moved,
                &unselected,
            ],
            "2026-01-01T00:00:20Z",
            None,
        )
        .unwrap();
        for id in ["assistant", "tool", "thinking"] {
            assert_eq!(data.turns[id].count, 1, "{id}");
            assert!(data.turns[id].joined.is_some(), "{id}");
        }
        assert_eq!(data.turns["moved"].turn.as_deref(), Some("moved-turn"));
        for id in ["other", "stray", "unselected"] {
            assert_eq!(data.turns[id].count, 0, "{id}");
        }
        tx.rollback().unwrap();
        let writer = Connection::open(root.join("statev2.sqlite")).unwrap();
        writer
            .execute(
                "UPDATE orchestration_v2_projection_provider_turns SET payload_json=json_set(payload_json,'$.large',?1)",
                [&"x".repeat(2_000_000)],
            )
            .unwrap();
        let tx = database.transaction().unwrap();
        let error = metadata(&tx, &[&request], "2026-01-01T00:00:20Z", None)
            .err()
            .unwrap();
        assert!(error.to_string().contains("1 MiB resource bound"));
    }

    #[test]
    fn a_selection_is_a_runs_earliest_running_attempt_before_any_unreadable_event() {
        let w = Workspace::new();
        let database = db(&state(&w, json!({})));
        let event = |sequence: i64, thread: &str, payload: Value| {
            database.execute("INSERT INTO orchestration_events VALUES(?1,'thread',2,?2,'run.updated','2026-01-01T00:00:00Z',?3)",params![sequence,thread,payload.to_string()]).unwrap();
        };
        let run = |id: &str, attempt: &str, status: &str| json!({"id":id,"activeAttemptId":attempt,"status":status});
        event(1, "thread", run("a", "a1", "queued"));
        event(2, "thread", run("a", "a1", "running"));
        event(3, "thread", run("a", "a1", "running"));
        event(4, "thread", run("b", "b1", "running"));
        event(5, "other", run("c", "c1", "running"));
        event(6, "thread", json!({"large":"x".repeat(2_000_000)}));
        event(7, "thread", run("c", "c1", "running"));
        // A nested id never leaves SQLite, so it cannot match its JSON text.
        event(
            8,
            "other",
            json!({"id":{"text":"a"},"activeAttemptId":"a1","status":"running"}),
        );
        database
            .set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH, VALUE_BOUND)
            .unwrap();
        let mut query = database.prepare(SELECTIONS).unwrap();
        let selections = Selections::read(&mut query, "thread");
        assert_eq!(selections.first("a", "a1").unwrap(), Some(2));
        assert_eq!(selections.first("b", "b1").unwrap(), Some(4));
        // Every later selection lies past the oversized event, which reading fails on.
        for (run, attempt) in [("a", "a2"), ("c", "c1")] {
            let error = selections.first(run, attempt).unwrap_err();
            assert!(error.to_string().contains("1 MiB resource bound"));
        }
        let selections = Selections::read(&mut query, "other");
        assert_eq!(selections.first("c", "c1").unwrap(), Some(5));
        assert_eq!(selections.first("a", "a1").unwrap(), None);
        assert_eq!(selections.first(r#"{"text":"a"}"#, "a1").unwrap(), None);
        assert_eq!(selections.first.len(), 1);
    }

    #[test]
    fn projection_queries_follow_t3_indexes() {
        let w = Workspace::new();
        let root = state(&w, json!({}));
        let database = db(&root);
        // The item queries read indexes alone, since an item row holds its tool output.
        for (query, scans, covering) in [
            (PROVIDER_THREADS.to_string(), 1, false),
            (OWNER_EVENT.to_string(), 0, false),
            (PROVIDER_TURNS.to_string(), 0, false),
            (TURN_THREADS.to_string(), 0, false),
            (THREAD_TURNS.to_string(), 0, false),
            (TURN_REF.to_string(), 0, false),
            (TURN_ITEM_ROWS.to_string(), 0, true),
            (THREAD_ITEM_IDS.to_string(), 0, true),
            (ATTEMPT.to_string(), 0, false),
            (SELECTIONS.to_string(), 0, false),
            (TURN.to_string(), 0, false),
            (ITEM_THREAD.to_string(), 0, false),
            (FIRST_ITEM.to_string(), 0, false),
            (guards::BOUNDARY.to_string(), 0, false),
            (guards::IDLE_RUNS.to_string(), 0, false),
            (guards::events_query(), 0, false),
            (guards::projection_query(), 0, false),
        ] {
            let mut plan = database
                .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
                .unwrap();
            let unbound = vec![rusqlite::types::Null; plan.parameter_count()];
            let details: Vec<String> = plan
                .query_map(rusqlite::params_from_iter(unbound), |r| r.get(3))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            let full = details
                .iter()
                .filter(|d| {
                    d.starts_with("SCAN")
                        && !d.contains("VIRTUAL TABLE")
                        && *d != "SCAN CONSTANT ROW"
                })
                .count();
            assert_eq!(full, scans, "{details:#?}");
            if covering {
                assert!(
                    details.iter().all(|d| d.contains("COVERING INDEX")),
                    "{details:#?}"
                );
            }
        }
    }

    #[test]
    fn native_keys_skip_null_message_ids_keep_all_blocks_and_ignore_codex_response_ids() {
        let mut ids = NativeIds::default();
        for uuid in ["first", "second", "first"] {
            ids.observe(json!({"type":"assistant","uuid":uuid,"sessionId":"session","requestId":"request","message":{"id":null}}).as_object().unwrap(),Harness::Claude,&identity("session"));
        }
        assert_eq!(
            ids.items[&format!("claude:{}", identity("request"))],
            vec!["first", "second"]
        );
        ids.observe(
            json!({"type":"response_item","payload":{"id":"PRIVATE_RESPONSE_ID"}})
                .as_object()
                .unwrap(),
            Harness::Codex,
            "session",
        );
        assert!(!ids.ids.values().any(|id| id == "PRIVATE_RESPONSE_ID"));
        ids.observe(json!({"type":"assistant","uuid":"tool-record","sessionId":"session","message":{"id":"msg_1","content":[{"type":"thinking","thinking":""},{"type":"tool_use","id":"toolu_1"},{"type":"server_tool_use","id":"srvtoolu_1"}]}}).as_object().unwrap(),Harness::Claude,&identity("session"));
        assert_eq!(
            ids.items[&format!("claude:{}", identity("msg_1"))],
            vec!["tool-record", "msg_1:thinking", "toolu_1", "srvtoolu_1"]
        );
        for (id, key) in [
            (
                "turn-item:provider:claudeAgent:native-item:toolu_1",
                Some("toolu_1"),
            ),
            (
                "turn-item:provider:claudeAgent:native-item:msg_1%3Athinking%3A12",
                Some("msg_1:thinking"),
            ),
            (
                "turn-item:provider:claudeAgent:native-item:msg_1%3Athinking%3A",
                Some("msg_1:thinking:"),
            ),
            ("turn-item:provider:codex:native-item:toolu_1", None),
            ("turn-item:provider:claudeAgent:native-item:bad%G1", None),
            ("turn-item:provider:claudeAgent:native-item:cut%3", None),
        ] {
            assert_eq!(item_key(id).as_deref(), key, "{id}");
        }
        // Each range holds exactly the ids `item_key` maps back to its key.
        for (native, inside, outside) in [
            ("a b/é", "a%20b%2F%C3%A9", "a%20b%2F%C3%A9x"),
            (
                "msg_1:thinking",
                "msg_1%3Athinking%3A12",
                "msg_1%3Athinking",
            ),
        ] {
            let (start, end) = item_range(native);
            let inside = format!("{ITEM_PREFIX}{inside}");
            let outside = format!("{ITEM_PREFIX}{outside}");
            assert!(start <= inside && inside < end, "{inside}");
            assert!(!(start <= outside && outside < end), "{outside}");
            assert_eq!(item_key(&inside).as_deref(), Some(native));
        }
    }

    #[test]
    fn evidence_outside_the_native_snapshot_cannot_join() {
        let w = Workspace::new();
        let root = state(&w, json!({}));
        let db = db(&root);
        seed(&db, "codex", "codex");
        let mut request = make_request();
        request.harness = Harness::Codex;
        request.session = identity("native-session").into();
        request.turn = Some(identity("native-turn").into());
        request.model = Some("gpt-6.1-sol".into());
        request.time = 1_767_225_605_000;
        let mut ids = NativeIds::default();
        ids.ids
            .insert(identity("native-session"), "native-session".into());
        ids.ids
            .insert(identity("native-turn"), "native-turn".into());
        let mut db = Connection::open_with_flags(
            root.join("statev2.sqlite"),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let tx = db.transaction().unwrap();
        let data = metadata(
            &tx,
            &[&native_request(&request, &ids).unwrap()],
            "2026-01-01T00:00:06Z",
            None,
        )
        .unwrap();
        assert!(data.turns[&*request.id].joined.is_none());
    }

    #[test]
    fn a_t3_opencode_instance_owns_the_sessions_its_threads_name_and_their_subagents() {
        use crate::test_support::{OPENCODE_START, opencode_database};
        let w = Workspace::new();
        let file = opencode_database(&w.home);
        // A session OpenCode's own CLI ran, in the same database.
        let native_db = Connection::open(&file).unwrap();
        native_db
            .execute(
                "INSERT INTO session_v2 VALUES ('ses_cli', NULL, '/work/repo', '2.2.0', ?1)",
                [OPENCODE_START + 10_000],
            )
            .unwrap();
        let step = json!({"model": {"id": "zen-flash", "providerID": "opencode"},
            "tokens": {"input": 1, "output": 1, "reasoning": 0, "cache": {"read": 0, "write": 0}}});
        native_db
            .execute(
                "INSERT INTO session_message VALUES ('msg_cli', 'ses_cli', 'assistant', 1, ?1, ?2)",
                params![OPENCODE_START + 10_000, step.to_string()],
            )
            .unwrap();
        drop(native_db);
        let root = state(
            &w,
            json!({"providerInstances": {
                "opencode": {"driver": "opencode", "enabled": true},
                "remote": {"driver": "opencode", "enabled": true,
                    "config": {"serverUrl": "http://127.0.0.1:4096"}}
            }}),
        );
        let db = db(&root);
        db.execute(
            "INSERT INTO orchestration_v2_projection_provider_threads VALUES('pt','thread','opencode','opencode','2026-01-01T00:00:10Z',?1)",
            [json!({"nativeThreadRef": {"nativeId": "ses_main"}}).to_string()],
        )
        .unwrap();
        let scan = scan(&w.scan_options()).unwrap();
        let instance = scan
            .profiles
            .iter()
            .find(|p| p.t3.as_ref().is_some_and(|t| t.instance == "opencode"))
            .unwrap();
        let native = scan
            .profiles
            .iter()
            .find(|p| p.harness == Harness::OpenCode && p.t3.is_none())
            .unwrap();
        assert_eq!(instance.histories, native.histories);
        let owners = |session: &str| -> Vec<Vec<String>> {
            let key = identity(session);
            scan.requests
                .iter()
                .filter(|r| *r.session == *key)
                .map(|r| r.profiles.to_vec())
                .collect()
        };
        // The thread T3 ran, and the subagent it started, are the instance's alone.
        assert_eq!(
            owners("ses_main"),
            [[instance.id.as_str()], [instance.id.as_str()]]
        );
        assert_eq!(owners("ses_sub"), [[instance.id.as_str()]]);
        assert_eq!(owners("ses_cli"), [[native.id.as_str()]]);
        assert!(
            scan.t3[0]
                .exclusions
                .iter()
                .any(|e| e.starts_with("T3 instance remote connects to an OpenCode server"))
        );
    }
}
