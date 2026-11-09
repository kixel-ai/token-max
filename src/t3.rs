//! T3 discovery and allowlisted metadata, never a second usage or billing ledger.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use rusqlite::{Connection, OpenFlags, Transaction};
use serde::Deserialize;
use serde_json::{Map, Value, value::RawValue};

use crate::controls::Env;
use crate::discovery::DiscoveryOptions;
use crate::model::*;
use crate::safe::{ScanError, identity, iso, timestamp};

/// The upstream implementation whose settings and projections this adapter supports.
pub const SOURCE: &str = "https://github.com/pingdotgg/t3code/tree/c4ce7b0fd74f6beb37ed6034f5c8416e5aa7e165 (2026-11-08)";
const AUXILIARY: &str = "T3 titles, branches, PR text and other helper generation can use ephemeral sessions; absent retained native billing records leave their cost unmeasured, not free";
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
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
    "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE",
    "BASH_MAX_OUTPUT_LENGTH",
    "MAX_MCP_OUTPUT_TOKENS",
    "BASH_MAX_TIMEOUT_MS",
    "BASH_DEFAULT_TIMEOUT_MS",
];

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
}

#[derive(Deserialize)]
#[serde(untagged)]
enum LegacyIdle {
    Bool(bool),
    Mode(T3IdleMode),
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Instance {
    driver: String,
    enabled: Option<bool>,
    config: Option<Box<RawValue>>,
    environment: Vec<Environment>,
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
    enabled: Option<bool>,
    home_path: String,
    shadow_home_path: String,
    setup_mode: Option<String>,
    auto_compact_window: String,
    idle_compaction: Option<String>,
    #[serde(deserialize_with = "nonempty")]
    launch_args: bool,
}

fn nonempty<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    String::deserialize(d).map(|s| !s.trim().is_empty())
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
    let mut seen = HashSet::new();
    for (path, required) in paths {
        let path = absolute(path)?;
        let path = match fs::canonicalize(&path) {
            Ok(path) => path,
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
                });
                continue;
            }
            Err(_) => return Err(fail("selected T3 state is missing or inaccessible", &path)),
        };
        if !seen.insert(path.clone()) {
            continue;
        }
        let file = path.join("settings.json");
        let settings = match fs::read(&file) {
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
        let mut instances = settings.provider_instances;
        for (driver, config) in settings.providers {
            instances.entry(driver.clone()).or_insert(Instance {
                driver,
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
            exclusions: vec![AUXILIARY.into(), "Native terminal suggestions, away summaries, goal check-ins, Codex TUI recaps and terminal keepalive guidance do not apply to T3; their native toggles do not control T3 helpers".into()],
            metadata: "not read".into(),
            snapshot_started_at: None,
            snapshot_finished_at: None,
            joined_requests: 0,
            ambiguous_requests: 0,
        };
        for (id, instance) in instances {
            if !slug(&id) || !slug(&instance.driver) {
                return Err(fail("invalid T3 provider identity", &file));
            }
            let harness = match instance.driver.as_str() {
                "claudeAgent" => Harness::Claude,
                "codex" => Harness::Codex,
                _ => {
                    state.exclusions.push(format!(
                        "T3 instance {id} ({}) is unsupported; its spend is excluded",
                        instance.driver
                    ));
                    continue;
                }
            };
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
            let key = format!("t3:{}:{id}", path.display());
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
            let enabled = instance.enabled != Some(false) && config.enabled != Some(false);
            let mut limitations = vec![AUXILIARY.into(), "T3's current thread selection and new-thread browser default are unavailable with the server closed; historical requested speed does not establish current selection or billed speed".into()];
            if config.launch_args {
                limitations.push("T3 launchArgs are present; arbitrary CLI overrides are not inspected, so affected native fallback controls remain advisory".into());
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
                };
            } else if harness == Harness::Claude {
                profile.compaction.default.comparison_unknown = Some("T3 inherits native per-model/project/launch window controls; effective launch scope is unresolved".into());
            }
            if !enabled || unresolved_env || config.launch_args || policy_unresolved {
                profile.compaction.default.comparison_unknown = Some("T3 instance is disabled or effective environment/launch overrides are unresolved".into());
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
                ownership_resolved: enabled
                    && !unresolved_env
                    && !config.launch_args
                    && !policy_unresolved,
                limitations,
            });
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
                    .find(|r| &r.id == id)
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
    items: HashMap<String, String>,
    events: HashMap<String, String>,
}
impl NativeIds {
    pub(crate) fn observe(&mut self, raw: &Map<String, Value>, harness: Harness) {
        let payload = raw.get("payload").and_then(Value::as_object);
        for value in [
            raw.get("sessionId"),
            payload.and_then(|p| p.get("id")),
            payload.and_then(|p| p.get("session_id")),
            payload.and_then(|p| p.get("turn_id")),
        ]
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        {
            self.ids
                .entry(identity(value))
                .or_insert_with(|| value.to_string());
        }
        if harness == Harness::Claude
            && raw.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
            && let Some(uuid) = raw.get("uuid").and_then(Value::as_str)
        {
            self.events
                .insert(identity(&format!("claude:compaction:{uuid}")), uuid.into());
        }
        if harness == Harness::Claude
            && raw.get("type").and_then(Value::as_str) == Some("assistant")
        {
            let message = raw.get("message").and_then(Value::as_object);
            let name = message
                .and_then(|m| m.get("id"))
                .or_else(|| raw.get("requestId"))
                .or_else(|| raw.get("requestRef"))
                .and_then(Value::as_str);
            if let (Some(name), Some(uuid)) = (name, raw.get("uuid").and_then(Value::as_str)) {
                self.items
                    .insert(format!("claude:{}", identity(name)), uuid.into());
            }
        }
    }
}

// Native identities and the earliest running selection are the join. No Usage totals,
// messages, prompts, credentials, or arbitrary JSON objects leave SQLite.
const OWNERS: &str = "SELECT provider_instance_id FROM orchestration_v2_projection_provider_threads
 WHERE driver=?1 AND json_extract(payload_json,'$.nativeThreadRef.nativeId')=?2 AND updated_at<=?3 LIMIT 2";
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
 WHERE json_extract(t.payload_json,'$.nativeTurnRef.driver')=?1
   AND (json_extract(t.payload_json,'$.nativeTurnRef.nativeId')=?2 OR EXISTS (
     SELECT 1 FROM orchestration_v2_projection_turn_items item
     WHERE item.provider_turn_id=t.provider_turn_id AND item.thread_id=t.thread_id
       AND json_extract(item.payload_json,'$.nativeItemRef.driver')=?1
       AND json_extract(item.payload_json,'$.nativeItemRef.nativeId')=?2))
 LIMIT 2)
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
   AND (json_extract(th.payload_json,'$.snoozedUntil') IS NULL OR json_extract(th.payload_json,'$.snoozedUntil')<=?3)
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
     WHERE json_extract(c.payload_json,'$.targetThreadId')=m.thread_id
       AND json_extract(c.payload_json,'$.type')='merge_back' AND c.status='pending')
   AND NOT EXISTS (SELECT 1 FROM orchestration_v2_projection_turn_items i
     WHERE i.thread_id=m.thread_id AND i.status IN ('pending','running','waiting'))
   AND COALESCE(json_array_length(pt.payload_json,'$.pendingBackgroundTasks'),0)=0)
 FROM matched m
 LEFT JOIN orchestration_events e ON e.sequence=m.selection_sequence
 LEFT JOIN orchestration_v2_projection_runs r ON r.run_id=m.run_id AND r.thread_id=m.thread_id
 LEFT JOIN orchestration_v2_projection_messages msg ON msg.message_id=json_extract(r.payload_json,'$.userMessageId') AND msg.thread_id=m.thread_id
 LEFT JOIN orchestration_v2_projection_threads th ON th.thread_id=m.thread_id
 LEFT JOIN orchestration_v2_projection_provider_threads pt ON pt.provider_thread_id=m.provider_thread_id";

#[derive(Clone)]
struct Joined {
    instance: String,
    evidence: T3Request,
    requested: Option<RequestedSpeed>,
}
#[derive(Default)]
struct Projection {
    owners: HashMap<(Harness, String), Vec<String>>,
    turns: HashMap<String, Vec<Joined>>,
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
    requests: &[Request],
    ids: &NativeIds,
    bound: &str,
) -> rusqlite::Result<Projection> {
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
            return Err(rusqlite::Error::InvalidQuery);
        }
    }
    let mut owner_query = tx.prepare(OWNERS)?;
    let mut turn_query = tx.prepare(TURN)?;
    let mut result = Projection::default();
    let bound_ms = timestamp(&Value::String(bound.into())).ok_or(rusqlite::Error::InvalidQuery)?;
    for r in requests.iter().filter(|r| r.agent == Agent::Main) {
        let Some(session) = ids.ids.get(&r.session) else {
            continue;
        };
        let driver = if r.harness == Harness::Claude {
            "claudeAgent"
        } else {
            "codex"
        };
        let owner_key = (r.harness, r.session.clone());
        if let std::collections::hash_map::Entry::Vacant(entry) = result.owners.entry(owner_key) {
            let owners = owner_query
                .query_map([driver, session, bound], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            entry.insert(owners);
        }
        let native_turn = if r.harness == Harness::Codex {
            r.turn.as_ref().and_then(|t| ids.ids.get(t))
        } else {
            ids.items.get(&r.id).or_else(|| ids.events.get(&r.id))
        };
        let Some(native_turn) = native_turn else {
            continue;
        };
        let turn_key = r.id.clone();
        if result.turns.contains_key(&turn_key) {
            continue;
        }
        let rows = turn_query
            .query_map([driver, native_turn, bound], |row| {
                let native_session: Option<String> = row.get(0)?;
                let instance: Option<String> = row.get(1)?;
                let model: Option<String> = row.get(12)?;
                let tier: Option<String> = row.get(13)?;
                let fast: Option<bool> = row.get(14)?;
                let tier_count: i64 = row.get(15)?;
                let fast_count: i64 = row.get(16)?;
                let event_time = instant(row.get(17)?);
                let completed_at = instant(row.get(8)?);
                let usage_updated_at = instant(row.get(9)?);
                let thread_updated_at = instant(row.get(19)?);
                let lifecycle: Option<bool> = row.get(20)?;
                let used_tokens: Option<i64> = row.get(10)?;
                let requested = if tier_count <= 1
                    && fast_count <= 1
                    && model == r.model
                    && event_time.is_some_and(|t| t <= r.time && t <= bound_ms)
                {
                    let tier = tier.filter(|s| {
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
                    if tier_count > 0 && tier.is_none() {
                        None
                    } else if r.harness == Harness::Codex {
                        Some(RequestedSpeed {
                            tier: tier.or_else(|| (fast == Some(true)).then(|| "fast".into())),
                            speed: None,
                        })
                    } else {
                        fast.map(|v| RequestedSpeed {
                            tier: None,
                            speed: Some(if v { "fast" } else { "standard" }.into()),
                        })
                    }
                } else {
                    None
                };
                let evidence = T3Request {
                    thread: identity(&row.get::<_, String>(2)?),
                    run: identity(&row.get::<_, String>(3)?),
                    attempt: identity(&row.get::<_, String>(4)?),
                    provider_turn: identity(&row.get::<_, String>(5)?),
                    status: match row.get::<_, String>(6)?.as_str() {
                        "completed" => "completed",
                        "running" => "running",
                        "failed" => "failed",
                        "interrupted" => "interrupted",
                        "cancelled" => "cancelled",
                        _ => "unknown",
                    }
                    .into(),
                    completed_at,
                    usage_updated_at,
                    used_tokens: used_tokens.filter(|n| (0..=9_007_199_254_740_991).contains(n)),
                    cache_expires_at: instant(row.get(11)?),
                    idle_mode: mode(row.get(18)?),
                    lifecycle_eligible: thread_updated_at.filter(|t| *t <= bound_ms).and(lifecycle),
                };
                let valid = native_session.as_deref() == Some(session)
                    && completed_at.is_none_or(|t| t <= bound_ms)
                    && usage_updated_at.is_none_or(|t| t <= bound_ms)
                    && instant(row.get(7)?).is_some_and(|t| t <= r.time);
                Ok((valid, instance, evidence, requested))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        // Count invalid/mismatched rows too: a duplicate native turn is ambiguous.
        let joined = if rows.len() == 1 {
            rows.into_iter()
                .filter_map(|(valid, instance, evidence, requested)| {
                    valid.then_some(Joined {
                        instance: instance?,
                        evidence,
                        requested,
                    })
                })
                .collect()
        } else {
            rows.into_iter()
                .map(|(_, instance, evidence, requested)| Joined {
                    instance: instance.unwrap_or_default(),
                    evidence,
                    requested,
                })
                .collect()
        };
        result.turns.insert(turn_key, joined);
    }
    Ok(result)
}

/// Freeze optional projection metadata in one read-only WAL transaction, then close it before analysis.
pub(crate) fn enrich(
    states: &mut [T3State],
    profiles: &mut [Profile],
    requests: &mut [Request],
    events: &mut [StructuralEvent],
    ids: &NativeIds,
    bound: &str,
) {
    let mut projections: Vec<Request> = requests.to_vec();
    for e in events.iter().filter(|e| ids.events.contains_key(&e.id)) {
        projections.push(Request {
            id: e.id.clone(),
            session: e.session.to_string(),
            harness: e.harness,
            agent: Agent::Main,
            time: e.time,
            end_time: e.time,
            model: e.model.as_deref().map(str::to_string),
            profiles: e.profiles.clone(),
            evidence: RequestEvidence::Unknown,
            provider: None,
            api: Api::Unknown,
            source: None,
            trigger: None,
            reference: None,
            turn: None,
            version: None,
            client: None,
            cwd: None,
            parts: vec![],
            server_tools: None,
            locations: vec![],
            accounting: Accounting::Individual,
            attempt: None,
            requested_speed: None,
            issues: vec![],
            t3: None,
        });
    }
    let mut joins: HashMap<String, Vec<(String, Joined)>> = HashMap::new();
    let mut owners: HashMap<(Harness, String), Vec<String>> = HashMap::new();
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
            // Optional metadata has a fixed total VM budget, independent of database size.
            // Oversized/unindexed histories degrade to an explicit evidence gap.
            let mut steps = 0u32;
            db.progress_handler(
                1_000,
                Some(move || {
                    steps += 1;
                    steps > 10_000
                }),
            )?;
            db.execute_batch("PRAGMA query_only=ON; PRAGMA trusted_schema=OFF;")?;
            let tx = db.transaction()?;
            let relevant: Vec<Request> = projections
                .iter()
                .filter(|r| {
                    r.profiles.iter().any(|id| {
                        profiles.iter().any(|p| {
                            &p.id == id && p.t3.as_ref().is_some_and(|t| t.state == state.path)
                        })
                    })
                })
                .cloned()
                .collect();
            let result = metadata(&tx, &relevant, ids, bound)?;
            tx.rollback()?;
            Ok::<_, rusqlite::Error>(result)
        })();
        state.snapshot_finished_at = Some(iso(crate::reader::now()));
        let data = match data {
            Ok(data) => {
                state.metadata = "read-only consistent SQLite snapshot; allowlisted native links and controls only".into();
                data
            }
            Err(_) => {
                state.metadata = "unavailable: incompatible/malformed schema, metadata resource bound, or read-only database/WAL access failed; native usage is retained".into();
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
        for ((harness, session), instances) in data.owners {
            let target = owners.entry((harness, session)).or_default();
            for instance in instances {
                // Preserve ambiguity even if a referenced instance was removed from settings.
                target.push(profile_id(&instance, harness).unwrap_or_default());
            }
        }
        for r in &projections {
            let key = if r.harness == Harness::Codex {
                r.turn.clone()
            } else {
                ids.items
                    .get(&r.id)
                    .or_else(|| ids.events.get(&r.id))
                    .map(|s| identity(s))
            };
            let Some(_) = key else {
                continue;
            };
            if let Some(rows) = data.turns.get(&r.id) {
                if rows.len() > 1 {
                    state.ambiguous_requests += 1;
                }
                for row in rows {
                    joins.entry(r.id.clone()).or_default().push((
                        profile_id(&row.instance, r.harness).unwrap_or_default(),
                        row.clone(),
                    ));
                }
            }
        }
    }
    let t3_ids: HashSet<String> = profiles
        .iter()
        .filter(|p| p.t3.is_some())
        .map(|p| p.id.clone())
        .collect();
    let mut ambiguous_profiles = HashSet::new();
    for r in requests.iter_mut() {
        let rows = joins.get(&r.id);
        let session_owners = owners.get(&(r.harness, r.session.clone()));
        let unique = rows
            .filter(|v| v.len() == 1)
            .and_then(|v| v.first())
            .filter(|(p, _)| !p.is_empty() && r.profiles.contains(p));
        let owner = unique.map(|(p, _)| p).or_else(|| {
            session_owners
                .filter(|o| o.len() == 1)
                .and_then(|o| o.first())
                .filter(|p| !p.is_empty() && r.profiles.contains(p))
        });
        if let Some(owner) = owner {
            r.profiles = vec![owner.clone()];
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
                    if r.requested_speed
                        .as_ref()
                        .is_some_and(|s| (s.tier.is_some() || s.speed.is_some()) && s != requested)
                    {
                        r.issues.push("conflicting requested service tiers".into());
                    } else {
                        r.requested_speed = Some(requested.clone());
                    }
                }
                if let Some(state) = profiles
                    .iter()
                    .find(|p| &p.id == owner)
                    .and_then(|p| p.t3.as_ref())
                    .and_then(|t| states.iter_mut().find(|s| s.path == t.state))
                {
                    state.joined_requests += 1;
                }
            } else {
                r.issues.push("T3 native session owner is unique; per-request run/speed/idle metadata is unavailable".into());
            }
        } else if r.profiles.iter().any(|p| t3_ids.contains(p)) {
            // REPL provenance excludes the SDK client. T3 can override its entrypoint
            // label, so a different label alone cannot prove a native control owner.
            if r.harness == Harness::Claude
                && r.client.as_deref() == Some("cli")
                && r.source.as_deref() == Some("repl_main_thread")
                && session_owners.is_none()
                && rows.is_none()
            {
                r.profiles.retain(|p| !t3_ids.contains(p));
            } else {
                r.issues.push("T3/shared native history ownership or metadata join is ambiguous/unavailable; current control guidance is advisory".into());
                ambiguous_profiles.extend(r.profiles.iter().cloned());
            }
        }
    }
    // A uniquely owned session also owns its structural evidence, without assuming a run.
    let mut sessions: HashMap<(Harness, &str), HashSet<&str>> = HashMap::new();
    for r in requests.iter() {
        sessions
            .entry((r.harness, &r.session))
            .or_default()
            .extend(r.profiles.iter().map(String::as_str));
    }
    for e in events {
        if let Some(rows) = joins.get(&e.id)
            && rows.len() == 1
            && !rows[0].0.is_empty()
        {
            let (owner, joined) = &rows[0];
            e.profiles = vec![owner.clone()];
            e.t3 = Some(joined.evidence.clone());
            if joined.evidence.idle_mode.is_some() {
                e.idle_attribution = IdleAttribution::T3;
            }
        }
        if let Some(owners) = sessions.get(&(e.harness, e.session.as_ref()))
            && owners.len() == 1
        {
            e.profiles = owners.iter().map(|s| s.to_string()).collect();
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
        assert_eq!(r.t3.as_ref().unwrap().run, identity("run"));
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
            r#"{"fastMode":true,"autoCompactWindow":400000,"promptCacheTtl":"5m"}"#,
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
        assert!(result.t3[0].metadata.contains("incompatible"));
    }
    #[test]
    fn evidence_outside_the_native_snapshot_cannot_join() {
        let w = Workspace::new();
        let root = state(&w, json!({}));
        let db = db(&root);
        seed(&db, "codex", "codex");
        let mut request = make_request();
        request.harness = Harness::Codex;
        request.session = identity("native-session");
        request.turn = Some(identity("native-turn"));
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
        let data = metadata(&tx, &[request.clone()], &ids, "2026-01-01T00:00:06Z").unwrap();
        assert!(data.turns[&request.id].is_empty());
    }
}
