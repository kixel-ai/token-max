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
use crate::safe::{ScanError, digest, identity, iso, timestamp};

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
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "DISABLE_PROMPT_CACHING",
    "FORCE_PROMPT_CACHING_5M",
    "ENABLE_PROMPT_CACHING_1H",
    "CLAUDE_CODE_PROMPT_CACHE_TTL",
    "CLAUDE_CODE_SUBAGENT_PROMPT_CACHE_TTL",
    "CLAUDE_CODE_DISABLE_FAST_MODE",
    "CLAUDE_CODE_DISABLE_AUTO_MEMORY",
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
    "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE",
    "BASH_MAX_OUTPUT_LENGTH",
    "MAX_MCP_OUTPUT_TOKENS",
    "BASH_MAX_TIMEOUT_MS",
    "BASH_DEFAULT_TIMEOUT_MS",
];

/// The profile id of a T3 instance, which a removed instance keeps for its retained history.
pub(crate) fn profile_key(state: &str, instance: &str) -> String {
    format!("t3:{state}:{instance}")
}

/// Retained history is still priced when a current instance cannot receive guidance.
pub(crate) fn constrain<T>(base: &mut CheckBase<T>, profiles: &[Profile]) {
    if let Some(instance) = profiles
        .iter()
        .find(|p| p.id == base.profile)
        .and_then(|p| p.t3.as_ref())
        && (!instance.enabled || !instance.ownership_resolved)
    {
        base.action = Action::Advisory;
        base.recommended = None;
        base.savings_usd = None;
        base.monthly_savings_usd = None;
        base.uncertainty.push("T3 instance is disabled or its effective control ownership is unresolved; retained historical costs do not establish an active setting to change".into());
    }
}

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
        if !matches!(driver.as_str(), "claudeAgent" | "codex") {
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
        let allowed = ENVIRONMENT.contains(&entry.name.as_str())
            && !entry.sensitive
            && entry.secret.is_none();
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
    let explicit = !options.claude_dirs.is_empty()
        || !options.codex_dirs.is_empty()
        || !options.t3_dirs.is_empty();
    let mut paths = Vec::new();
    if !explicit {
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
                    joined_requests: 0,
                    ambiguous_requests: 0,
                    unresolved: IndexMap::default(),
                    removed: IndexMap::default(),
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
        let global = settings
            .idle_compaction
            .unwrap_or(match settings.compact_idle_threads {
                Some(LegacyIdle::Mode(mode)) => mode,
                Some(LegacyIdle::Bool(true)) => T3IdleMode::BeforeCacheExpires,
                _ => T3IdleMode::OnReturn,
            });
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
            joined_requests: 0,
            ambiguous_requests: 0,
            unresolved: IndexMap::default(),
            removed: IndexMap::default(),
            instances: instances.keys().cloned().collect(),
            text_generation: Some(text_generation(&settings, &instances, &file)),
        };
        for (id, instance) in instances {
            let enabled = instance.resolved_enabled();
            let harness = match instance.driver.as_str() {
                "claudeAgent" => Harness::Claude,
                "codex" => Harness::Codex,
                _ => {
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
                .filter(|(k, _)| ENVIRONMENT.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let mut unresolved_env = false;
            for entry in instance.environment {
                if ENVIRONMENT.contains(&entry.name.as_str()) {
                    match entry.value {
                        Some(value) => {
                            env.insert(entry.name, value);
                        }
                        None => {
                            env.remove(&entry.name);
                            unresolved_env = true;
                        }
                    }
                }
            }
            let config = instance
                .config
                .map(|raw| serde_json::from_str::<Config>(raw.get()))
                .transpose()
                .map_err(|_| fail("invalid supported T3 provider controls", &file))?
                .unwrap_or_default();
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
                let gap = "T3 machine managed policy is present, inaccessible or unsupported on this platform; it may override homes and settings, so discovery coverage and current control ownership are unresolved";
                limitations.push(gap.into());
                if !state.exclusions.iter().any(|s| s == gap) {
                    state.exclusions.push(gap.into());
                }
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
            if harness == Harness::Claude {
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
            speed.resolved = false;
            // These terminal-client features do not run in T3's SDK/app-server client.
            profile.background.clear();
            profile.idle = None;
            profile.t3 = Some(T3Instance {
                state: state.path.clone(),
                instance: id,
                enabled,
                idle_mode,
                idle_owner,
                ownership_resolved: resolved,
                settings_unresolved: unresolved_env || launch_unresolved || policy_unresolved,
                limitations,
                // Without its own window, T3 passes no window, so Claude Code reads the
                // same user, project and local settings as a native session.
                inherited_window: (resolved
                    && harness == Harness::Claude
                    && config.auto_compact_window.is_empty())
                .then(|| local.env.clone()),
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
        native_turns: if r.agent == Agent::Subagent {
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
 FROM orchestration_v2_projection_provider_threads WHERE driver IN ('claudeAgent','codex')";
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
// Native turn and item references of one provider thread: a native turn id, or for Claude a
// turn item id that `item_key` decodes. Codex joins by turn id alone, so only Claude reads
// items. Item ids avoid reading item payloads, which hold tool outputs.
const NATIVE_REFS: &str = "SELECT provider_turn_id, json_extract(payload_json,'$.nativeTurnRef.driver'),
 json_extract(payload_json,'$.nativeTurnRef.nativeId'), 0
 FROM orchestration_v2_projection_provider_turns WHERE provider_thread_id=?1
 UNION ALL
 SELECT t.provider_turn_id, json_extract(t.payload_json,'$.nativeTurnRef.driver'), i.turn_item_id, 1
 FROM orchestration_v2_projection_provider_turns t
 JOIN orchestration_v2_projection_turn_items i ON i.provider_turn_id=t.provider_turn_id AND i.thread_id=t.thread_id
 WHERE ?2='claudeAgent' AND t.provider_thread_id=?1";
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
// Native identities and the earliest running selection are the join.
const TURN: &str = "WITH matched AS (
 SELECT t.*, a.attempt_id, a.run_id, a.provider_instance_id,
 json_extract(a.payload_json,'$.nativeThreadId') AS native_session,
 (SELECT e.sequence FROM orchestration_events e
  WHERE e.aggregate_kind='thread' AND e.application_event_version=2 AND e.stream_id=a.thread_id
    AND e.event_type='run.updated' AND json_extract(e.payload_json,'$.id')=a.run_id
    AND json_extract(e.payload_json,'$.activeAttemptId')=a.attempt_id
    AND json_extract(e.payload_json,'$.status')='running'
  ORDER BY e.sequence LIMIT 1) AS selection_sequence
 FROM orchestration_v2_projection_provider_turns t
 LEFT JOIN orchestration_v2_projection_run_attempts a ON a.attempt_id=t.run_attempt_id
   AND a.thread_id=t.thread_id AND a.provider_thread_id=t.provider_thread_id
 WHERE t.provider_turn_id=?1)
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
 MAX(th.updated_at, pt.updated_at),
 (th.archived_at IS NULL AND th.deleted_at IS NULL
   AND json_extract(th.payload_json,'$.settledAt') IS NULL
   AND COALESCE(json_extract(th.payload_json,'$.settledOverride'),'')!='settled'
   AND COALESCE(json_extract(th.payload_json,'$.lineage.relationshipToParent'),'')!='subagent'
   AND COALESCE(json_extract(th.payload_json,'$.lineage.sideChat'),0)=0
   AND (json_extract(th.payload_json,'$.snoozedUntil') IS NULL OR json_extract(th.payload_json,'$.snoozedUntil')<=?2)
   AND th.active_provider_thread_id=m.provider_thread_id
   AND th.provider_instance_id=m.provider_instance_id
   AND r.status='completed' AND m.completed_at IS NOT NULL
   AND msg.message_id IS NOT NULL AND lower(trim(json_extract(msg.payload_json,'$.text'))) != '/compact'
   AND NOT EXISTS (SELECT 1 FROM orchestration_v2_projection_provider_turns newer_usage
     WHERE newer_usage.provider_thread_id=m.provider_thread_id
       AND json_extract(newer_usage.payload_json,'$.tokenUsage.updatedAt')>json_extract(m.payload_json,'$.tokenUsage.updatedAt'))
   AND NOT EXISTS (SELECT 1 FROM orchestration_v2_projection_runs newer
     WHERE newer.thread_id=m.thread_id AND newer.ordinal>r.ordinal
       AND (newer.status!='cancelled' OR json_extract(newer.payload_json,'$.startedAt') IS NOT NULL))
   AND NOT EXISTS (SELECT 1 FROM orchestration_v2_projection_runtime_requests q
     WHERE q.thread_id=m.thread_id AND q.status='pending')
   AND NOT EXISTS (SELECT 1 FROM orchestration_v2_projection_context_transfers c
     WHERE c.target_thread_id=m.thread_id AND c.type='merge_back' AND c.status='pending')
   AND NOT EXISTS (SELECT 1 FROM orchestration_v2_projection_turn_items i
     WHERE i.thread_id=m.thread_id AND i.status IN ('pending','running','waiting'))
   AND COALESCE(json_array_length(pt.payload_json,'$.pendingBackgroundTasks'),0)=0)
 FROM matched m
 LEFT JOIN orchestration_events e ON e.sequence=m.selection_sequence
 LEFT JOIN orchestration_v2_projection_runs r ON r.run_id=m.run_id AND r.thread_id=m.thread_id
 LEFT JOIN orchestration_v2_projection_messages msg ON msg.message_id=json_extract(r.payload_json,'$.userMessageId') AND msg.thread_id=m.thread_id
 LEFT JOIN orchestration_v2_projection_threads th ON th.thread_id=m.thread_id
 LEFT JOIN orchestration_v2_projection_provider_threads pt ON pt.provider_thread_id=m.provider_thread_id";

/// Why optional T3 metadata could not be read.
#[derive(Debug)]
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
    turns: HashMap<String, TurnMatch>,
}

#[derive(Default)]
struct TurnMatch {
    count: usize,
    joined: Option<Joined>,
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
    /// What the turn records for every request it joins, read once; None when incomplete.
    evidence: Option<std::sync::Arc<T3Request>>,
}
impl TurnRow {
    fn read(row: &rusqlite::Row<'_>, bound_ms: Millis) -> Self {
        let completed_at = instant(text_column(row, 8));
        let usage_updated_at = instant(text_column(row, 9));
        let id = |index| text_column(row, index).map(|text| Id::from(identity(&text)));
        let evidence = (|| {
            Some(T3Request {
                thread: id(2)?,
                run: id(3)?,
                attempt: id(4)?,
                provider_turn: id(5)?,
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
                    (0..=9_007_199_254_740_991).contains(n)
                        && usage_updated_at.is_some_and(|t| t <= bound_ms)
                }),
                cache_expires_at: usage_updated_at
                    .filter(|t| *t <= bound_ms)
                    .and(instant(text_column(row, 11))),
                idle_mode: mode(text_column(row, 18)),
                lifecycle_eligible: instant(text_column(row, 19))
                    .filter(|t| *t <= bound_ms)
                    .and(bool_column(row, 20)),
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

fn text_column(row: &rusqlite::Row<'_>, index: usize) -> Option<String> {
    row.get::<_, Option<String>>(index).ok().flatten()
}
fn integer_column(row: &rusqlite::Row<'_>, index: usize) -> Option<i64> {
    use rusqlite::types::ValueRef;
    match row.get_ref(index).ok()? {
        ValueRef::Integer(n) => Some(n),
        ValueRef::Real(n)
            if n.is_finite() && n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_991.0 =>
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
    s.and_then(|s| timestamp(&Value::String(s)))
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
    let bound_ms = timestamp(&Value::String(bound.into()))
        .ok_or_else(|| Unavailable::Schema(format!("invalid native bound {bound}")))?;
    let driver = |h: Harness| -> &'static str {
        if h == Harness::Claude {
            "claudeAgent"
        } else {
            "codex"
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
    let mut native_refs = tx.prepare(NATIVE_REFS)?;
    for (key, list) in &threads {
        let found = refs.entry(key).or_default();
        let wanted = &wanted[&(key.0, key.1.as_str())];
        for thread in list {
            let mut rows = native_refs.query(rusqlite::params![thread.id, key.0])?;
            while let Some(row) = rows.next()? {
                let (Some(turn), Some(native)) = (text_column(row, 0), text_column(row, 2)) else {
                    continue;
                };
                let native = if bool_column(row, 3) == Some(true) {
                    let Some(native) = item_key(&native) else {
                        continue;
                    };
                    native
                } else {
                    native
                };
                if text_column(row, 1).as_deref() == Some(key.0) && wanted.contains(native.as_str())
                {
                    let turns = found.entry(native).or_default();
                    if !turns.contains(&turn) {
                        turns.push(turn);
                    }
                }
            }
        }
    }
    let mut turn_query = tx.prepare(TURN)?;
    let mut turn_rows: HashMap<String, Option<TurnRow>> = HashMap::default();
    let mut result = Projection {
        items_since,
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
                if !turn_rows.contains_key(turn) {
                    let row = turn_query
                        .query_row(rusqlite::params![turn, bound], |row| {
                            Ok(TurnRow::read(row, bound_ms))
                        })
                        .map(Some)
                        .or_else(|e| match e {
                            rusqlite::Error::QueryReturnedNoRows => Ok(None),
                            e => Err(e),
                        })?;
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
            },
        );
    }
    Ok(result)
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

/// Freeze optional projection metadata in one read-only WAL transaction, then close it before analysis.
pub(crate) fn enrich(
    states: &mut [T3State],
    profiles: &mut [Profile],
    requests: &mut [Request],
    events: &mut [StructuralEvent],
    ids: &NativeIds,
    bound: &str,
    since: Option<Millis>,
) {
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
            db.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH, 1_048_576)?;
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
    let mut ambiguous_profiles = HashSet::default();
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
                let state = profiles
                    .iter()
                    .find(|p| &p.id == owner)
                    .and_then(|p| p.t3.as_ref())
                    .map(|t| &t.state)
                    .or_else(|| removed.get(owner).map(|(state, _)| state));
                if let Some(state) =
                    state.and_then(|path| states.iter_mut().find(|s| s.path == *path))
                {
                    state.joined_requests += 1;
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
                    r.issues.push(format!("T3/shared native history ownership is unresolved: {c}; current control guidance is advisory").into());
                    // Context before the window prices cache state but says nothing about
                    // which controls ran the selected requests.
                    if selected {
                        let states: HashSet<String> =
                            r.profiles.iter().filter_map(state_of).collect();
                        for state in states {
                            *unresolved.entry((state, c)).or_default() += 1;
                        }
                        ambiguous_profiles.extend(r.profiles.iter().cloned());
                    }
                }
            }
        }
        if selected && !removed.is_empty() {
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
    for p in profiles.iter_mut() {
        if p.t3.is_some() || ambiguous_profiles.contains(&p.id) {
            p.speed_directories = None;
            if let Some(speed) = &mut p.speed {
                speed.resolved = false;
            }
        }
        if ambiguous_profiles.contains(&p.id) {
            if let Some(t3) = &mut p.t3 {
                t3.ownership_resolved = false;
                // Its directory windows would otherwise bypass the unresolved profile window.
                p.idle_directories = None;
            }
            p.compaction.default.comparison_unknown =
                Some("shared native history has unresolved T3 control ownership".into());
            for c in p.compaction.models.values_mut() {
                c.comparison_unknown = p.compaction.default.comparison_unknown.clone();
            }
            if let Some(idle) = &mut p.idle {
                idle.resolved = false;
            }
            for ttl in [&mut p.ttl.main, &mut p.ttl.subagent] {
                ttl.value = None;
                ttl.reason = "shared native history has unresolved T3 control ownership".into();
            }
            p.background.clear();
            p.background_directories.clear();
            p.background_requests.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::scan;
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
        db.execute("INSERT INTO orchestration_v2_projection_provider_turns VALUES('turn','thread','pt','attempt','completed',?1,?2,?3)",params![start,end,json!({"nativeTurnRef":{"driver":driver,"nativeId":"native-turn"},"tokenUsage":{"updatedAt":end,"usedTokens":300000,"promptCacheExpiresAt":"2026-01-01T00:05:10Z"},"private":"PRIVATE_PROJECTION"}).to_string()]).unwrap();
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
        let scan = scan(&w.scan_options()).unwrap();
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
        assert_eq!(scan.t3[0].joined_requests, 1);
        assert!(
            scan.t3[0]
                .exclusions
                .iter()
                .any(|s| s.contains("unsupported"))
        );
        let exported = serde_json::to_string(&scan).unwrap();
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
            // `ownership_resolved` also reflects platform policy (always unresolved on
            // Windows), so check the marker shared-history ambiguity leaves on the TTL.
            assert_eq!(
                current.ttl.main.reason.contains("shared native history"),
                !removed,
                "{owner}"
            );
            if removed {
                assert_eq!(*r.profiles, [profile_key(&t3.path, "gone")]);
                assert!(r.t3.is_some(), "joined evidence still applies");
                assert_eq!(t3.joined_requests, 1);
                assert_eq!(t3.removed, IndexMap::from_iter([("gone".into(), 1)]));
                assert!(t3.unresolved.is_empty(), "{:?}", t3.unresolved);
            } else {
                assert!(r.profiles.contains(&current.id));
                assert!(t3.removed.is_empty());
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
        db.execute("INSERT INTO orchestration_v2_projection_provider_turns SELECT 'duplicate',thread_id,provider_thread_id,run_attempt_id,status,started_at,completed_at,payload_json FROM orchestration_v2_projection_provider_turns",[]).unwrap();
        let scan = read();
        assert!(scan.requests[0].t3.is_none());
        assert!(scan.t3[0].ambiguous_requests > 0);
        assert_eq!(
            scan.requests[0].requested_speed.as_ref().unwrap().tier,
            None
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
                .any(|t| !t.ownership_resolved)
        );
        assert!(
            !serde_json::to_string(&scan)
                .unwrap()
                .contains("PRIVATE_HOME")
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
        let json = serde_json::to_string(&scan).unwrap();
        for private in [
            "PRIVATE_REPLY",
            "PRIVATE_MESSAGE",
            "PRIVATE_PROJECTION",
            "native-message",
            "assistant-uuid",
        ] {
            assert!(!json.contains(private), "{private}");
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
        assert!(chrome.t3.as_ref().unwrap().ownership_resolved);
        assert_eq!(chrome.compaction.default.value, Some(400000));
        assert_eq!(chrome.compaction.default.comparison_unknown, None);
        let flagged = instance(&data, "flagged");
        assert!(!flagged.t3.as_ref().unwrap().ownership_resolved);
        assert!(flagged.compaction.default.comparison_unknown.is_some());
        assert!(!instance(&data, "codex").t3.unwrap().ownership_resolved);
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
        database.execute("INSERT INTO orchestration_v2_projection_turn_items VALUES('item','thread','turn','assistant_message','completed',?1,'2026-01-01T00:00:00Z')",[json!({"nativeItemRef":{"driver":"codex","nativeId":"item"},"text":"x".repeat(2_000_000)}).to_string()]).unwrap();
        database
            .set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH, 1_048_576)
            .unwrap();
        database.execute("INSERT INTO orchestration_v2_projection_provider_turns VALUES('orphan','thread','pt','absent',NULL,'2026-01-01T00:00:00Z',NULL,?1)",[json!({"nativeTurnRef":{"driver":"codex","nativeId":"orphan"},"tokenUsage":{"usedTokens":"PRIVATE_BAD_VALUE"}}).to_string()]).unwrap();
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
    fn shared_history_ambiguity_clears_scoped_terminal_background_controls() {
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
        assert!(checks.is_empty(), "{checks:?}");
        for profile in &data.profiles {
            assert!(profile.background.is_empty());
            assert!(profile.background_directories.is_empty());
            assert!(profile.background_requests.is_empty());
        }
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

    #[cfg(unix)]
    #[test]
    fn subagents_of_a_native_only_root_exclude_t3() {
        let w = Workspace::new();
        let root = state(&w, json!({}));
        // Any driver's item dates T3's records, so the session without one is native.
        db(&root).execute("INSERT INTO orchestration_v2_projection_turn_items VALUES('turn-item:provider:codex:native-item:x','thread','turn','assistant_message','completed','{}','2025-01-01T00:00:00Z')",[]).unwrap();
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
        db(&root).execute("INSERT INTO orchestration_v2_projection_turn_items VALUES('turn-item:provider:codex:native-item:x','thread','turn','assistant_message','completed','{}','2025-01-01T00:00:00Z')",[]).unwrap();
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
        database.execute("INSERT INTO orchestration_v2_projection_provider_turns VALUES('turn2','thread2','pt2',NULL,'completed',NULL,NULL,'{}')",[]).unwrap();
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
            database.execute("INSERT INTO orchestration_v2_projection_turn_items VALUES(?1,?2,?3,'assistant_message','completed','{}','2026-01-01T00:00:00Z')",params![id,thread,turn]).unwrap();
        }
        // Linked after the native snapshot, with no earlier event to prove the link.
        database.execute("INSERT INTO orchestration_v2_projection_provider_threads VALUES('pt3','thread3','claudeAgent','claudeAgent','2999-01-01T00:00:00Z',?1)",[json!({"nativeThreadRef":{"nativeId":"third-session"}}).to_string()]).unwrap();
        database.execute("INSERT INTO orchestration_v2_projection_provider_turns VALUES('turn3','thread3','pt3',NULL,'completed',NULL,NULL,'{}')",[]).unwrap();
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
        // `ownership_resolved` also reflects platform policy (always unresolved on Windows),
        // so check the marker shared-history ambiguity leaves on the TTL instead.
        let ambiguous = |s: &Scan| {
            s.profiles.iter().any(|p| {
                p.ttl
                    .main
                    .reason
                    .contains("unresolved T3 control ownership")
            })
        };
        assert!(ambiguous(&data));
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
            assert!(
                !serde_json::to_string(&data)
                    .unwrap()
                    .contains("PRIVATE_KEY")
            );
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
        let serialized = serde_json::to_string(&data).unwrap();
        assert!(!serialized.contains("PRIVATE_ENV") && !serialized.contains("PRIVATE_CONFIG"));
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
            database.execute("INSERT INTO orchestration_v2_projection_turn_items VALUES(?1,'thread','turn',?2,'completed',?3,'2026-01-01T00:00:00Z')",params![format!("turn-item:provider:claudeAgent:native-item:{native}"),kind,json!({"output":extra}).to_string()]).unwrap();
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
        database
            .set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH, 1_048_576)
            .unwrap();
        let tx = database.transaction().unwrap();
        let data = metadata(
            &tx,
            &[&request, &tool, &thinking, &other],
            "2026-01-01T00:00:20Z",
            None,
        )
        .unwrap();
        for id in ["assistant", "tool", "thinking"] {
            assert_eq!(data.turns[id].count, 1, "{id}");
            assert!(data.turns[id].joined.is_some(), "{id}");
        }
        assert_eq!(data.turns["other"].count, 0);
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
    fn projection_queries_follow_t3_indexes() {
        let w = Workspace::new();
        let root = state(&w, json!({}));
        let database = db(&root);
        for (query, scans) in [
            (PROVIDER_THREADS, 1),
            (OWNER_EVENT, 0),
            (NATIVE_REFS, 0),
            (TURN, 0),
            (ITEM_THREAD, 0),
            (FIRST_ITEM, 0),
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
}
