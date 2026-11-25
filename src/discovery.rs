//! Find Claude and Codex profiles, resolve their current settings and freeze history files.

use crate::collections::{HashSet, IndexMap, IndexSet};
use std::cmp::Ordering;
use std::fs;
use std::io::ErrorKind;

use serde_json::{Map, Value};

use crate::controls::{
    CLAUDE_BACKGROUND_ENV, CompactionSources, Env, SettingsScope, TURN_END_KEY, bash_cap_control,
    claude_background, claude_inbound, claude_memory, codex_background, codex_memory,
    compaction_controls, idle_controls, js_string, members, retention_control, scoped_environment,
    speed_controls,
};
use crate::effort::{claude_effort, codex_catalog, codex_effort, latest_model};
use crate::model::{
    Agent, BackgroundControl, Billing, Control, ControlValue, Harness, IdleControl, InboundControl,
    Location, MemoryControl, PerBucket, Profile, Request, RootOrigin, RootState, RootStatus,
    Scalar, Snapshot, SpeedControl, Ttl, TtlControl,
};
use crate::safe::{ScanError, identity, label};

/// Where discovery looks; explicit roots limit the entire scan, otherwise host-local profiles are discovered.
#[derive(Clone, Debug)]
pub struct DiscoveryOptions {
    /// The home directory, where host-local profiles are looked for.
    pub home: String,
    /// The environment discovery reads settings overrides from.
    pub env: Env,
    /// Explicit Claude config directories.
    pub claude_dirs: Vec<String>,
    /// Explicit Codex home directories.
    pub codex_dirs: Vec<String>,
    /// Explicit T3 state directories: settings.json, and statev2.sqlite when it exists.
    pub t3_dirs: Vec<String>,
    /// The system managed Claude settings file; overridable for host adapters and synthetic tests.
    pub managed_settings_file: String,
    /// The system Codex config file.
    pub codex_system_config_file: String,
    /// The managed Codex requirements file.
    pub codex_requirements_file: String,
    /// Read every user, project and local settings file as unset, as `doctor --defaults`
    /// shows the report; managed and system policy still apply.
    pub defaults: bool,
    /// How each Claude profile pays, by its canonical root, as doctor asked.
    pub billing: IndexMap<String, Billing>,
}

impl DiscoveryOptions {
    /// This host's home directory, process environment and system settings files.
    pub fn host() -> Result<Self, ScanError> {
        let home =
            std::env::home_dir().ok_or_else(|| ScanError::new("cannot discover home directory"))?;
        Ok(Self {
            home: home.to_string_lossy().into_owned(),
            env: std::env::vars_os()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.to_string_lossy().into_owned(),
                    )
                })
                .collect(),
            claude_dirs: vec![],
            codex_dirs: vec![],
            t3_dirs: vec![],
            managed_settings_file: managed_settings_path(std::env::consts::OS).into(),
            codex_system_config_file: "/etc/codex/config.toml".into(),
            codex_requirements_file: "/etc/codex/requirements.toml".into(),
            defaults: false,
            billing: IndexMap::default(),
        })
    }

    /// Pretend the settings files and the shell's setting variables away, keeping the
    /// variables that only say where each CLI keeps its files or sends its requests.
    pub fn pretend_defaults(&mut self) {
        self.defaults = true;
        self.env.retain(|k, _| {
            matches!(
                k.as_str(),
                "CLAUDE_CONFIG_DIR" | "CODEX_HOME" | "T3CODE_HOME"
            ) || CLAUDE_PROVIDERS.contains(&k.as_str())
        });
    }

    /// Whether `file` is managed or system policy, which `--defaults` keeps.
    fn policy(&self, file: &str) -> bool {
        [
            &self.managed_settings_file,
            &self.codex_system_config_file,
            &self.codex_requirements_file,
        ]
        .contains(&&file.to_string())
    }

    /// The switches managed settings (Claude) or the system `config.toml` (Codex) turn off,
    /// including a system MCP allowlist, which `--defaults` keeps.
    fn policy_off(&self, harness: Harness) -> Result<Vec<(String, Control)>, ScanError> {
        let (file, off) = match harness {
            Harness::Claude => (
                &self.managed_settings_file,
                switched_off(
                    Harness::Claude,
                    &settings(&self.managed_settings_file, Harness::Claude)?,
                ),
            ),
            Harness::Codex => {
                let system = settings(&self.codex_system_config_file, Harness::Codex)?;
                let mut off = switched_off(Harness::Codex, &system);
                off.extend(allowlists(&system));
                (&self.codex_system_config_file, off)
            }
        };
        Ok(off.into_iter().map(|k| (file.clone(), k)).collect())
    }

    /// Under `--defaults`, add to `on` each switch `file` really turns off that policy does
    /// not also turn off, since `--defaults` shows that feature on.
    fn pretend_on(
        &self,
        harness: Harness,
        on: &mut Vec<(String, Control)>,
        file: &str,
    ) -> Result<(), ScanError> {
        if !self.defaults {
            return Ok(());
        }
        let config = settings(file, harness)?;
        let mut found = switched_off(harness, &config);
        if harness == Harness::Codex {
            found.extend(allowlists(&config));
        }
        self.pretend_switches_on(harness, on, file, found)
    }

    /// Under `--defaults`, add to `on` each server the state file `file` turns off with
    /// `/mcp` in the project at `cwd`, which Claude Code keys by the directory or its
    /// repository's root.
    fn pretend_mcp_on(
        &self,
        on: &mut Vec<(String, Control)>,
        file: &str,
        cwd: &str,
    ) -> Result<(), ScanError> {
        if !self.defaults {
            return Ok(());
        }
        let paths = project_directories(cwd, &[".git".into()])?;
        let root = paths.last().map_or(cwd, String::as_str);
        let state = settings(file, Harness::Claude)?;
        let mut found = vec![];
        for (project, entry) in members(state.get("projects")) {
            if project != cwd && project != root {
                continue;
            }
            let names = entry.get("disabledMcpServers").and_then(Value::as_array);
            for name in names.into_iter().flatten().filter_map(Value::as_str) {
                found.push(Control {
                    path: vec![
                        "projects".into(),
                        project.clone(),
                        "disabledMcpServers".into(),
                    ],
                    value: ControlValue::Text(name.into()),
                    append: true,
                });
            }
        }
        self.pretend_switches_on(Harness::Claude, on, file, found)
    }

    /// Add to `on` each of `found`, the switches `file` turns off, that policy does not also
    /// turn off and `on` does not already hold.
    fn pretend_switches_on(
        &self,
        harness: Harness,
        on: &mut Vec<(String, Control)>,
        file: &str,
        found: Vec<Control>,
    ) -> Result<(), ScanError> {
        let policy = self.policy_off(harness)?;
        for control in found {
            // Two allowlists differ by what they list, so each tool is checked in context.
            let allowlist = control.path.last().is_some_and(|k| k == "enabled_tools");
            let kept = !allowlist
                && policy
                    .iter()
                    .any(|(_, k)| crate::defaults::covers(k, &control));
            let seen = on
                .iter()
                .any(|(f, k)| f == file && crate::defaults::same_switch(k, &control));
            if !kept && !seen {
                on.push((file.to_string(), control));
            }
        }
        Ok(())
    }

    /// Read a settings file as this run sees it. Under `--defaults` a user, project or
    /// local file sets nothing, except that a Codex file keeps the MCP servers it
    /// declares, each on, since a server is something installed, not a setting's value,
    /// and either CLI's file keeps the provider its requests went to.
    pub(crate) fn settings(
        &self,
        file: &str,
        harness: Harness,
    ) -> Result<Map<String, Value>, ScanError> {
        if !self.defaults || self.policy(file) {
            return settings(file, harness);
        }
        let mut real = settings(file, harness)?;
        let mut pretended = Map::new();
        if harness == Harness::Claude {
            // Which cloud the requests went to, and so how they were billed.
            let env: Map<String, Value> = members(real.get("env"))
                .iter()
                .filter(|(k, _)| CLAUDE_PROVIDERS.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if !env.is_empty() {
                pretended.insert("env".into(), Value::Object(env));
            }
            return Ok(pretended);
        }
        if let Some(Value::Object(mut servers)) = real.remove("mcp_servers") {
            for server in servers.values_mut() {
                if let Value::Object(server) = server {
                    for key in ["enabled", "enabled_tools", "disabled_tools"] {
                        server.remove(key);
                    }
                }
            }
            pretended.insert("mcp_servers".into(), Value::Object(servers));
        }
        for key in ["model_provider", "model_providers"] {
            if let Some(value) = real.remove(key) {
                pretended.insert(key.into(), value);
            }
        }
        Ok(pretended)
    }
}

/// The variables that send Claude Code's requests to a cloud provider, which say how they
/// were billed rather than set anything, so `--defaults` keeps them.
pub(crate) const CLAUDE_PROVIDERS: [&str; 3] = [
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

/// Every root considered, the selected profiles, and each history file's frozen identity and length.
#[derive(Clone, Debug, PartialEq)]
pub struct Discovery {
    pub roots: Vec<RootStatus>,
    pub profiles: Vec<Profile>,
    pub snapshots: Vec<Snapshot>,
    pub t3: Vec<crate::model::T3State>,
}

impl Discovery {
    /// The Claude profile roots on the Anthropic API that have transcripts, whose unset main
    /// TTL defaults by how each pays, so doctor asks; T3 Code's instances share a native root,
    /// and so its account and answer.
    pub fn billing_roots(&self) -> Vec<String> {
        // A `projects` directory with no transcript in it has nothing to price.
        let read: HashSet<&str> = self
            .snapshots
            .iter()
            .flat_map(|s| s.profiles.iter().map(String::as_str))
            .collect();
        let roots: IndexSet<&str> = self
            .profiles
            .iter()
            .filter(|p| {
                p.harness == Harness::Claude && p.provider.is_none() && read.contains(p.id.as_str())
            })
            .map(|p| p.root.as_str())
            .collect();
        roots.into_iter().map(str::to_string).collect()
    }
}

/// A safe error naming a path without its contents.
pub(crate) fn error(message: &str, file: &str, line: u64) -> ScanError {
    ScanError::at(
        message,
        Location {
            file: file.into(),
            line,
        },
    )
}

/// Node's `ENOENT` and `ENOTDIR`, which both mean the path is not there.
pub(crate) fn absent(kind: ErrorKind) -> bool {
    matches!(kind, ErrorKind::NotFound | ErrorKind::NotADirectory)
}

/// Collapse `.`, `..` and repeated separators like Node's `path.normalize`, without a trailing slash.
#[cfg(not(windows))]
fn normalize_segments(path: &str, above_root: bool) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else if above_root {
                    parts.push("..");
                }
            }
            part => parts.push(part),
        }
    }
    parts.join("/")
}

/// Node's POSIX `path.join`.
#[cfg(not(windows))]
pub(crate) fn join(parts: &[&str]) -> String {
    let joined = parts
        .iter()
        .filter(|p| !p.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("/");
    if joined.is_empty() {
        return ".".into();
    }
    let absolute = joined.starts_with('/');
    let trailing = joined.ends_with('/');
    let mut path = normalize_segments(&joined, !absolute);
    if path.is_empty() {
        return match (absolute, trailing) {
            (true, _) => "/".into(),
            (false, true) => "./".into(),
            (false, false) => ".".into(),
        };
    }
    if trailing {
        path.push('/');
    }
    if absolute { format!("/{path}") } else { path }
}

/// Node's POSIX `path.resolve` for one path, relative to the process working directory.
#[cfg(not(windows))]
pub(crate) fn resolve(path: &str) -> String {
    let full = if path.starts_with('/') {
        path.to_string()
    } else {
        let cwd = std::env::current_dir()
            .map(|cwd| cwd.to_string_lossy().into_owned())
            .unwrap_or_default();
        format!("{cwd}/{path}")
    };
    format!("/{}", normalize_segments(&full, false))
}

/// Node's POSIX `path.dirname`.
#[cfg(not(windows))]
pub(crate) fn dirname(path: &str) -> &str {
    let bytes = path.as_bytes();
    if bytes.is_empty() {
        return ".";
    }
    let root = bytes[0] == b'/';
    let mut end = None;
    let mut matched_slash = true;
    for i in (1..bytes.len()).rev() {
        if bytes[i] == b'/' {
            if !matched_slash {
                end = Some(i);
                break;
            }
        } else {
            matched_slash = false;
        }
    }
    match end {
        None if root => "/",
        None => ".",
        Some(1) if root => "//",
        Some(end) => &path[..end],
    }
}

/// `fs.realpath`, as a string; a non-UTF-8 target cannot name a file Node could open either.
#[cfg(not(windows))]
pub(crate) fn realpath(path: &str) -> std::io::Result<String> {
    fs::canonicalize(path)?
        .into_os_string()
        .into_string()
        .map_err(|_| ErrorKind::InvalidData.into())
}

/// Node's win32 `path.join`: join the non-empty parts and normalize them lexically.
#[cfg(windows)]
pub(crate) fn join(parts: &[&str]) -> String {
    let mut path = std::path::PathBuf::new();
    for part in parts.iter().filter(|p| !p.is_empty()) {
        path.push(part);
    }
    lexical(&path)
}

/// Collapse `.` and `..` without touching the file system, as Node's win32 `path.normalize` does.
#[cfg(windows)]
fn lexical(path: &std::path::Path) -> String {
    use std::path::Component;
    let mut out: Vec<Component> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.last() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => out.push(component),
            },
            other => out.push(other),
        }
    }
    let joined: std::path::PathBuf = out.iter().collect();
    match joined.to_string_lossy().into_owned() {
        empty if empty.is_empty() => ".".into(),
        text => text,
    }
}

/// Node's win32 `path.resolve` for one path, relative to the process working directory.
#[cfg(windows)]
pub(crate) fn resolve(path: &str) -> String {
    std::path::absolute(path)
        .map(|absolute| lexical(&absolute))
        .unwrap_or_else(|_| path.to_string())
}

/// Node's win32 `path.dirname`: a root is its own parent.
#[cfg(windows)]
pub(crate) fn dirname(path: &str) -> &str {
    match std::path::Path::new(path).parent().and_then(|p| p.to_str()) {
        Some("") => ".",
        Some(parent) => parent,
        None => path,
    }
}

/// `fs.realpath`, as a string. Canonical Windows paths carry a `\\?\` prefix Node never shows.
#[cfg(windows)]
pub(crate) fn realpath(path: &str) -> std::io::Result<String> {
    let canonical = fs::canonicalize(path)?
        .into_os_string()
        .into_string()
        .map_err(|_| std::io::Error::from(ErrorKind::InvalidData))?;
    Ok(match canonical.strip_prefix(r"\\?\") {
        Some(rest) => match rest.strip_prefix(r"UNC\") {
            Some(share) => format!(r"\\{share}"),
            None => rest.to_string(),
        },
        None => canonical,
    })
}

/// The ICU root collation weight of a printable ASCII character; letters share a weight across case.
fn collation_weight(c: char) -> Option<u32> {
    const ORDER: &str = " _-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$0123456789abcdefghijklmnopqrstuvwxyz";
    match c {
        '\0'..='\x1f' | '\x7f' => None,
        c if c.is_ascii() => ORDER.find(c.to_ascii_lowercase()).map(|i| i as u32),
        // Non-ASCII has no table here; it sorts after ASCII by code point.
        c => Some(0x100 + c as u32),
    }
}

/// ICU root collation for printable ASCII paths:
/// punctuation before digits before letters, then lowercase before uppercase.
pub(crate) fn locale_compare(a: &str, b: &str) -> Ordering {
    let primary = |s: &str| s.chars().filter_map(collation_weight).collect::<Vec<_>>();
    let tertiary = |s: &str| {
        s.chars()
            .filter(|c| collation_weight(*c).is_some())
            .map(|c| c.is_ascii_uppercase())
            .collect::<Vec<_>>()
    };
    primary(a)
        .cmp(&primary(b))
        .then_with(|| tertiary(a).cmp(&tertiary(b)))
}

/// Convert TOML into JSON: a date becomes an empty object, a non-finite float null.
fn toml_json(value: toml::Value) -> Option<Value> {
    Some(match value {
        toml::Value::String(s) => Value::String(s),
        toml::Value::Integer(n) => {
            // An integer outside a double's safe-integer range fails the parse.
            if n.unsigned_abs() > crate::safe::MAX_SAFE_INTEGER as u64 {
                return None;
            }
            Value::from(n)
        }
        // Non-finite floats behave like null in every check: never a number, string or boolean.
        toml::Value::Float(f) => serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number),
        toml::Value::Boolean(b) => Value::Bool(b),
        // A date is an object without enumerable members.
        toml::Value::Datetime(_) => Value::Object(Map::new()),
        toml::Value::Array(items) => {
            Value::Array(items.into_iter().map(toml_json).collect::<Option<_>>()?)
        }
        toml::Value::Table(table) => Value::Object(
            table
                .into_iter()
                .map(|(k, v)| Some((k, toml_json(v)?)))
                .collect::<Option<_>>()?,
        ),
    })
}

/// Read a settings file; a missing file is empty, anything unreadable or malformed fails.
pub(crate) fn settings(file: &str, harness: Harness) -> Result<Map<String, Value>, ScanError> {
    let bytes = match fs::read(file) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Map::new()),
        Err(_) => return Err(error("cannot read settings", file, 1)),
    };
    // Node decodes UTF-8 lossily before parsing.
    let text = String::from_utf8_lossy(&bytes);
    let value = match harness {
        Harness::Claude => serde_json::from_str::<Value>(&text).ok(),
        Harness::Codex => toml::from_str::<toml::Table>(&text)
            .ok()
            .and_then(|table| toml_json(toml::Value::Table(table))),
    };
    match value {
        Some(Value::Object(map)) => Ok(map),
        _ => Err(error("invalid settings file", file, 1)),
    }
}

fn ttl(value: Option<&Value>) -> Option<Ttl> {
    match value?.as_str()? {
        "5m" => Some(Ttl::FiveMinutes),
        "1h" => Some(Ttl::OneHour),
        _ => None,
    }
}

/// Documented system file for an `std::env::consts::OS`; other managed delivery mechanisms remain unobserved.
pub fn managed_settings_path(os: &str) -> &'static str {
    match os {
        "macos" => "/Library/Application Support/ClaudeCode/managed-settings.json",
        "windows" => "C:\\Program Files\\ClaudeCode\\managed-settings.json",
        _ => "/etc/claude-code/managed-settings.json",
    }
}

/// The settings that own a profile's TTL controls.
struct TtlSources<'a> {
    /// Merged effective settings, for the setting values themselves.
    config: &'a Map<String, Value>,
    env: &'a Env,
    file: &'a str,
    managed: &'a Map<String, Value>,
    managed_file: &'a str,
    /// Settings in precedence order; defaults to the merged config then the managed file.
    scopes: Option<&'a [SettingsScope<'a>]>,
    /// How the profile pays, which picks the main conversation's default.
    billing: Option<Billing>,
}

fn ttl_controls(sources: &TtlSources) -> PerBucket<TtlControl> {
    let TtlSources {
        config,
        env,
        file,
        managed,
        managed_file,
        scopes,
        billing,
    } = *sources;
    let default_scopes = [
        SettingsScope { config, file },
        SettingsScope {
            config: managed,
            file: managed_file,
        },
    ];
    let scopes = scopes.unwrap_or(&default_scopes);
    let setting_owner = |key: &str| {
        let source = scopes
            .iter()
            .rev()
            .find(|s| s.config.contains_key(key))
            .map_or(file, |s| s.file);
        format!("{source}:{key}")
    };
    let environment = scoped_environment(scopes, env);
    let is = |key: &str| matches!(environment.values.get(key), Some(Value::String(s)) if s == "1");
    let env_owner = |key: &str| {
        environment
            .owners
            .get(key)
            .cloned()
            .unwrap_or_else(|| format!("{file}:env.{key}"))
    };
    let bucket = |subagent: bool| {
        let (variable, key) = if subagent {
            (
                "CLAUDE_CODE_SUBAGENT_PROMPT_CACHE_TTL",
                "subagentPromptCacheTtl",
            )
        } else {
            ("CLAUDE_CODE_PROMPT_CACHE_TTL", "promptCacheTtl")
        };
        let mut value = None;
        let mut owner = setting_owner(key);
        let reason;
        if is("DISABLE_PROMPT_CACHING") {
            reason = "prompt caching is disabled";
        } else if is("FORCE_PROMPT_CACHING_5M") {
            value = Some(Ttl::FiveMinutes);
            owner = env_owner("FORCE_PROMPT_CACHING_5M");
            reason = "environment override";
        } else if let Some(t) = ttl(environment.values.get(variable)) {
            value = Some(t);
            owner = env_owner(variable);
            reason = "environment override";
        } else if let Some(t) = ttl(config.get(key)) {
            value = Some(t);
            reason = "current user/managed setting";
        } else if is("ENABLE_PROMPT_CACHING_1H") {
            value = Some(Ttl::OneHour);
            owner = env_owner("ENABLE_PROMPT_CACHING_1H");
            reason = "environment override; agent frontmatter may take precedence";
        } else if subagent {
            value = Some(Ttl::FiveMinutes);
            reason = "documented default; agent frontmatter and server helpers may override";
        } else if is("CLAUDE_CODE_USE_BEDROCK")
            || is("CLAUDE_CODE_USE_VERTEX")
            || is("CLAUDE_CODE_USE_FOUNDRY")
        {
            value = Some(Ttl::FiveMinutes);
            reason = "explicit cloud-provider billing default";
        } else if let Some(billing) = billing {
            value = Some(billing.main_ttl());
            reason = match billing {
                Billing::Plan => TTL_PLAN_DEFAULT,
                Billing::Api => TTL_API_DEFAULT,
            };
        } else {
            reason = TTL_BILLING_UNKNOWN;
        }
        TtlControl {
            value,
            owner,
            reason: reason.into(),
            varies: false,
        }
    };
    PerBucket {
        main: bucket(false),
        subagent: bucket(true),
    }
}

/// An answered billing default holds only where every recorded project defaults the same way.
/// A project that sends its requests to a cloud provider defaults the main conversation to 5m
/// under the same unset setting as a plan's 1h, so the profile's main default is unknown
/// again, as without an answer, and pinning the replay's winner covers both.
fn unanswer_mixed_defaults(profile: &mut Profile) {
    let answered = |t: &TtlControl| t.reason == TTL_PLAN_DEFAULT || t.reason == TTL_API_DEFAULT;
    let main = &profile.ttl.main;
    if !answered(main) {
        return;
    }
    let mixed = profile
        .idle_directories
        .iter()
        .flatten()
        .filter_map(|(_, c)| c.ttl.as_ref())
        .any(|t| t.main.owner == main.owner && t.main.value != main.value);
    if !mixed {
        return;
    }
    let unknown = |t: &mut TtlControl| {
        if answered(t) {
            t.value = None;
            t.reason = TTL_BILLING_UNKNOWN.into();
        }
    };
    unknown(&mut profile.ttl.main);
    for control in profile.idle_directories.iter_mut().flatten() {
        if let Some(ttl) = &mut control.1.ttl {
            unknown(&mut ttl.main);
        }
    }
    profile.billing = None;
}

/// Each existing recorded directory's cache TTLs for a Claude profile under `env`: the user,
/// project, local and managed settings Claude Code reads there. Subagents count, since they
/// read the same files. A missing directory runs nothing more, so it decides no current value.
/// `known` holds directories already read, which keep that one observation.
fn directory_ttls(
    options: &DiscoveryOptions,
    profile: &Profile,
    env: &Env,
    requests: &[Request],
    known: IndexMap<String, PerBucket<TtlControl>>,
) -> Result<IndexMap<String, PerBucket<TtlControl>>, ScanError> {
    let mut result = known;
    let unread: Vec<&str> =
        directories(requests.iter().filter(|r| r.profiles.contains(&profile.id)))
            .into_iter()
            .filter(|cwd| !result.contains_key(*cwd))
            .collect();
    if unread.is_empty() {
        return Ok(result);
    }
    let managed_file = options.managed_settings_file.as_str();
    let user = options.settings(&profile.settings_file, Harness::Claude)?;
    let managed = settings(managed_file, Harness::Claude)?;
    for cwd in unread {
        if !existing_directory(cwd)? {
            continue;
        }
        let [project_file, legacy_file, local_file] = claude_project_files(cwd, options)?;
        let project = options.settings(&project_file, Harness::Claude)?;
        let legacy = options.settings(&legacy_file, Harness::Claude)?;
        let local = options.settings(&local_file, Harness::Claude)?;
        let scopes = [
            SettingsScope {
                config: &user,
                file: &profile.settings_file,
            },
            SettingsScope {
                config: &project,
                file: &project_file,
            },
            SettingsScope {
                config: &legacy,
                file: &legacy_file,
            },
            SettingsScope {
                config: &local,
                file: &local_file,
            },
            SettingsScope {
                config: &managed,
                file: managed_file,
            },
        ];
        let config = merged(&[&user, &project, &legacy, &local, &managed]);
        let ttl = ttl_controls(&TtlSources {
            config: &config,
            env,
            file: &profile.settings_file,
            managed: &managed,
            managed_file,
            scopes: Some(&scopes),
            billing: profile.billing,
        });
        result.insert(cwd.to_string(), ttl);
    }
    Ok(result)
}

/// A profile's TTL is one value only when every recorded project its requests ran in gets
/// it from the same owner at the same value; otherwise it `varies`, and each directory keeps
/// its own. An unset default whose billing is unknown does not vary while every project
/// leaves it to the same setting, since pinning it covers them.
pub(crate) fn settle_ttls<'d>(
    profile: &mut Profile,
    directories: impl Iterator<Item = &'d str> + Clone,
) {
    for bucket in [crate::model::Bucket::Main, crate::model::Bucket::Subagent] {
        let ttl = profile.ttl.get(bucket);
        let pinnable = ttl.value.is_none() && ttl.reason == TTL_BILLING_UNKNOWN;
        let owner = crate::rows::canonical(&ttl.owner);
        // Pinning a TTL covers another default, but not a project that turns caching off.
        let varies = directories
            .clone()
            .filter_map(|cwd| profile.ttl_directories.get(cwd))
            .map(|d| d.get(bucket))
            .any(|d| {
                crate::rows::canonical(&d.owner) != owner
                    || (d.value != ttl.value && !pinnable)
                    || (pinnable && d.value.is_none() && d.reason != ttl.reason)
            });
        if varies {
            let ttl = profile.ttl.get_mut(bucket);
            ttl.varies = true;
            ttl.reason = TTL_PROJECTS_DIFFER.into();
            // The window replay applies one TTL across the profile, so its comparison is
            // unknown too, as under unresolved T3 ownership.
            let compaction = &mut profile.compaction;
            compaction.default.comparison_unknown = Some(TTL_PROJECTS_DIFFER.into());
            for c in compaction.models.values_mut() {
                c.comparison_unknown = Some(TTL_PROJECTS_DIFFER.into());
            }
        }
    }
}

/// Why a profile's TTL has no one value: its recorded projects set it differently.
pub const TTL_PROJECTS_DIFFER: &str = "recorded projects set it differently, so no one value describes every request; each project's own settings decide its sessions";

/// Why an unset main-conversation TTL has no known value: Claude Code picks it by billing.
pub const TTL_BILLING_UNKNOWN: &str = "billing state is unknown; main default can change from a Claude plan's included usage to usage credits";
/// An unset main-conversation TTL on a profile answered as paid by a Claude plan.
pub const TTL_PLAN_DEFAULT: &str = "Claude plan default, as answered; requests past the plan's included usage are paid with usage credits and get 5m";
/// An unset main-conversation TTL on a profile answered as paid by an API key or usage credits.
pub const TTL_API_DEFAULT: &str = "API key or usage credits default, as answered";

/// Codex's byte budget for the AGENTS.md chain it loads.
const AGENTS_BUDGET: &str = "project_doc_max_bytes";

/// Settings that current or planned checks consume; anything else is never retained.
const STRUCTURAL_SETTINGS: [&str; 20] = [
    "project_doc_max_bytes",
    "bashOutputMaxChars",
    "tool_output_token_limit",
    "promptCacheTtl",
    "subagentPromptCacheTtl",
    "idleCompaction",
    "fastMode",
    "fastModePerSessionOptIn",
    "autoCompact",
    "autoCompactEnabled",
    "autoCompactWindow",
    "model",
    "model_provider",
    "service_tier",
    "model_auto_compact_token_limit",
    "model_auto_compact_token_limit_scope",
    "model_post_turn_compact_threshold_percent",
    "model_context_window",
    "effort",
    "realtime_active",
];

/// Retain only settings that current or planned checks consume.
pub fn structural_settings(config: &Map<String, Value>) -> IndexMap<String, Scalar> {
    let mut result = IndexMap::default();
    for key in STRUCTURAL_SETTINGS {
        let value = config.get(key).unwrap_or(&Value::Null);
        let scalar = match value {
            Value::Bool(b) => Scalar::Bool(*b),
            Value::Number(n) => Scalar::Number(n.as_f64().unwrap_or_default()),
            other => match label(other) {
                Some(text) => Scalar::Text(text),
                None => continue,
            },
        };
        result.insert(key.to_string(), scalar);
    }
    result
}

/// Merge settings objects key by key like `Object.assign`, with their `env` objects merged too.
fn merged(configs: &[&Map<String, Value>]) -> Map<String, Value> {
    let mut result = Map::new();
    let mut env = Map::new();
    for config in configs {
        result.extend(config.iter().map(|(k, v)| (k.clone(), v.clone())));
        env.extend(
            members(config.get("env"))
                .iter()
                .map(|(k, v)| (k.clone(), v.clone())),
        );
    }
    result.insert("env".into(), Value::Object(env));
    result
}

/// Each distinct recorded working directory, in first-seen order.
pub(crate) fn directories<'a>(requests: impl Iterator<Item = &'a Request>) -> IndexSet<&'a str> {
    requests
        .filter_map(|r| r.cwd.as_deref().filter(|cwd| !cwd.is_empty()))
        .collect()
}

/// Directories from the recorded cwd up to the nearest project marker, closest first.
pub(crate) fn project_directories(cwd: &str, markers: &[String]) -> Result<Vec<String>, ScanError> {
    let mut paths = vec![];
    let mut path = cwd;
    loop {
        paths.push(path.to_string());
        if markers.is_empty() {
            return Ok(paths);
        }
        for marker in markers {
            match fs::metadata(std::path::Path::new(path).join(marker)) {
                Ok(_) => return Ok(paths),
                Err(e) if absent(e.kind()) => {}
                Err(_) => return Err(error("cannot inspect project scope", path, 0)),
            }
        }
        if dirname(path) == path {
            return Ok(vec![cwd.to_string()]);
        }
        path = dirname(path);
    }
}

/// Where a checkout's Git metadata lives, read from Git's path metadata alone.
#[derive(Debug, PartialEq)]
pub(crate) struct GitDir {
    /// The directory holding the repository's shared `config`.
    pub common: String,
    /// Whether the checkout is a linked worktree, whose common directory belongs to another.
    pub linked: bool,
}

/// The Git directory of the checkout rooted at `root`, or None when it has no `.git`.
pub(crate) fn git_dir(root: &str) -> Result<Option<GitDir>, ScanError> {
    let dotgit = join(&[root, ".git"]);
    let meta = match fs::metadata(&dotgit) {
        Ok(meta) => meta,
        Err(e) if absent(e.kind()) => return Ok(None),
        Err(_) => return Err(error("cannot inspect git directory", &dotgit, 0)),
    };
    let own = |common: String| {
        Ok(Some(GitDir {
            common,
            linked: false,
        }))
    };
    if meta.is_dir() {
        return own(dotgit);
    }
    if !meta.is_file() {
        return Err(error("unsupported git directory pointer", &dotgit, 0));
    }
    let text = fs::read_to_string(&dotgit)
        .map_err(|_| error("cannot read git directory pointer", &dotgit, 0))?;
    let gitdir = text
        .trim()
        .strip_prefix("gitdir: ")
        .filter(|p| !p.is_empty() && !p.contains(['\n', '\r']))
        .ok_or_else(|| error("invalid git directory pointer", &dotgit, 0))?;
    let gitdir = std::path::Path::new(root).join(gitdir);
    let common_file = gitdir.join("commondir");
    let common = match fs::read_to_string(&common_file) {
        Ok(text) => text,
        // A submodule's own directory, not a linked worktree.
        Err(e) if absent(e.kind()) => return own(gitdir.to_string_lossy().into_owned()),
        Err(_) => {
            return Err(error(
                "cannot read git common directory",
                &common_file.to_string_lossy(),
                0,
            ));
        }
    };
    let common = common.trim();
    if common.is_empty() || common.contains(['\n', '\r']) {
        return Err(error(
            "invalid git common directory",
            &common_file.to_string_lossy(),
            0,
        ));
    }
    let common = realpath(&gitdir.join(common).to_string_lossy()).map_err(|_| {
        error(
            "cannot resolve git common directory",
            &common_file.to_string_lossy(),
            0,
        )
    })?;
    Ok(Some(GitDir {
        common,
        linked: true,
    }))
}

/// The main checkout of a linked worktree, reading only Git's path metadata.
#[cfg(unix)]
fn main_checkout(root: &str) -> Result<String, ScanError> {
    let Some(GitDir {
        common,
        linked: true,
    }) = git_dir(root)?
    else {
        return Ok(root.into());
    };
    // Like Claude Code, a common directory outside a checkout (a bare repository, or a
    // submodule under .git/modules) is itself the root; lacking .git, it owns no local file.
    if std::path::Path::new(&common)
        .file_name()
        .is_some_and(|n| n == ".git")
    {
        Ok(dirname(&common).into())
    } else {
        Ok(common)
    }
}

/// Claude keeps the local file at the main root only for user-owned Unix repositories.
fn claude_local_root(root: &str, options: &DiscoveryOptions) -> Result<Option<String>, ScanError> {
    #[cfg(windows)]
    {
        let _ = (root, options);
        Ok(None)
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let main = main_checkout(root)?;
        if realpath(&main).map_err(|_| error("cannot resolve local settings root", &main, 0))?
            == realpath(&options.home)
                .map_err(|_| error("cannot resolve home directory", &options.home, 0))?
        {
            return Ok(None);
        }
        for path in [&main, &join(&[&main, ".git"]), &join(&[&main, ".claude"])] {
            match fs::metadata(path) {
                // getuid has no preconditions and does not mutate process state.
                Ok(meta) if meta.uid() == unsafe { libc::getuid() } => {}
                Ok(_) => return Ok(None),
                Err(e) if absent(e.kind()) && path.ends_with(".claude") => {}
                Err(e) if absent(e.kind()) => return Ok(None),
                Err(_) => return Err(error("cannot inspect local settings ownership", path, 0)),
            }
        }
        Ok(Some(main))
    }
}

/// Leave each control unresolved for `reason`, which `scope` names in a few words so a report
/// can tell the groups of directories that resolved apart.
fn unresolved_background(controls: &mut [BackgroundControl], scope: &str, reason: &str) {
    for control in controls {
        control.value = None;
        control.reason = reason.into();
        control.scope = Some(scope.into());
    }
}

/// Whether a recorded working directory still exists as an absolute directory.
pub(crate) fn existing_directory(cwd: &str) -> Result<bool, ScanError> {
    match fs::metadata(cwd) {
        Ok(meta) => Ok(meta.is_dir() && std::path::Path::new(cwd).is_absolute()),
        Err(e) if absent(e.kind()) => Ok(false),
        Err(_) => Err(error("cannot inspect working directory", cwd, 0)),
    }
}

/// The project, legacy local and local settings files Claude reads at an existing `cwd`.
pub(crate) fn claude_project_files(
    cwd: &str,
    options: &DiscoveryOptions,
) -> Result<[String; 3], ScanError> {
    let paths = project_directories(cwd, &[".git".into()])?;
    let root = paths.last().expect("cwd is included");
    let local_root = claude_local_root(root, options)?;
    Ok([
        join(&[cwd, ".claude", "settings.json"]),
        join(&[cwd, ".claude", "settings.local.json"]),
        join(&[
            local_root.as_deref().unwrap_or(cwd),
            ".claude",
            "settings.local.json",
        ]),
    ])
}

/// Resolve each Claude profile's `crossSessionInbound` at its latest main-session request's
/// directory, as auto memory's is; without an existing one, user and managed settings decide.
/// It runs once T3 has attributed requests, so an instance reads its own sessions' directory,
/// and a request whose owner T3 can't settle decides for no profile.
pub fn discover_inbound(
    options: &DiscoveryOptions,
    profiles: &mut [Profile],
    requests: &[Request],
) -> Result<(), ScanError> {
    // Read only for Claude profiles: a Codex-only scan never reads Claude's policy.
    if !profiles.iter().any(|p| p.harness == Harness::Claude) {
        return Ok(());
    }
    let managed_file = options.managed_settings_file.as_str();
    let managed = settings(managed_file, Harness::Claude)?;
    let t3 = t3_instances(profiles);
    for profile in profiles.iter_mut().filter(|p| p.harness == Harness::Claude) {
        let user = options.settings(&profile.settings_file, Harness::Claude)?;
        let mine: Vec<&Request> = requests
            .iter()
            .filter(|r| owned_by(r, profile, &t3))
            .collect();
        let latest = crate::memory::deciding_request(&mine).and_then(|r| r.cwd.as_deref());
        let directory = match latest {
            Some(cwd) if existing_directory(cwd)? => Some(cwd),
            _ => None,
        };
        let mut repository = vec![];
        if let Some(cwd) = directory {
            // A session in the home directory reads the user's own file as its project's.
            for file in claude_project_files(cwd, options)? {
                if !same_file(&file, &profile.settings_file) {
                    let config = options.settings(&file, Harness::Claude)?;
                    repository.push((file, config));
                }
            }
        }
        let scopes: Vec<SettingsScope> = repository
            .iter()
            .map(|(file, config)| SettingsScope { config, file })
            .collect();
        profile.inbound = Some(InboundControl {
            directory: directory.map(String::from),
            ..claude_inbound(
                SettingsScope {
                    config: &managed,
                    file: managed_file,
                },
                SettingsScope {
                    config: &user,
                    file: &profile.settings_file,
                },
                &scopes,
            )?
        });
    }
    Ok(())
}

/// Every T3 instance's profile ID.
fn t3_instances(profiles: &[Profile]) -> HashSet<String> {
    profiles
        .iter()
        .filter(|p| p.t3.is_some())
        .map(|p| p.id.to_string())
        .collect()
}

/// Whether T3 settled that `profile` owns `r`. A request whose owner T3 can't settle keeps
/// every candidate, so a native profile counts only requests that name no T3 instance, and
/// an instance only those that name it alone.
fn owned_by(r: &Request, profile: &Profile, t3: &HashSet<String>) -> bool {
    if !r.profiles.iter().any(|p| **p == *profile.id) {
        return false;
    }
    if profile.t3.is_some() {
        r.profiles.iter().all(|p| **p == *profile.id)
    } else {
        !r.profiles.iter().any(|p| t3.contains(p))
    }
}

/// Resolve each native Claude profile's auto memory, main-session effort and advisor at its
/// latest main-session request's directory; without an existing one, the user and managed
/// files decide. It runs once T3 has attributed requests, so a T3 thread that shares the
/// profile's history never decides them, nor does a request whose owner T3 can't settle.
pub fn discover_latest_claude_controls(
    options: &DiscoveryOptions,
    profiles: &mut [Profile],
    requests: &[Request],
) -> Result<(), ScanError> {
    let env = &options.env;
    let empty = Env::default();
    let default_root = join(&[&options.home, ".claude"]);
    let configured = env.get("CLAUDE_CONFIG_DIR").unwrap_or(&default_root);
    let active = realpath(&resolve(configured)).unwrap_or_default();
    let managed_file = options.managed_settings_file.as_str();
    let t3 = t3_instances(profiles);
    for profile in profiles
        .iter_mut()
        .filter(|p| p.harness == Harness::Claude && p.t3.is_none())
    {
        let user = options.settings(&profile.settings_file, Harness::Claude)?;
        let managed = settings(managed_file, Harness::Claude)?;
        let local_env = if active == profile.root { env } else { &empty };
        let mine: Vec<&Request> = requests
            .iter()
            .filter(|r| r.agent == Agent::Main && owned_by(r, profile, &t3))
            .collect();
        let recorded = latest_model(&mine);
        let latest = crate::memory::deciding_request(&mine).and_then(|r| r.cwd.as_deref());
        let directory = match latest {
            Some(cwd) if !cwd.is_empty() && existing_directory(cwd)? => Some(cwd),
            _ => None,
        };
        let mut project = vec![];
        if let Some(cwd) = directory {
            for file in claude_project_files(cwd, options)? {
                let config = options.settings(&file, Harness::Claude)?;
                project.push((file, config));
            }
        }
        let scopes: Vec<SettingsScope> = std::iter::once(SettingsScope {
            config: &user,
            file: &profile.settings_file,
        })
        .chain(
            project
                .iter()
                .map(|(file, config)| SettingsScope { config, file }),
        )
        .chain(std::iter::once(SettingsScope {
            config: &managed,
            file: managed_file,
        }))
        .collect();
        profile.memory = Some(MemoryControl {
            directory: directory.map(Into::into),
            ..claude_memory(&scopes, local_env)?
        });
        profile.effort = Some(claude_effort(
            &scopes,
            &profile.settings_file,
            local_env,
            recorded,
        )?);
        // Read where effort is: the advisor card ranks against that main-session default.
        profile.advisor = Some(crate::advisor::control(
            &scopes,
            local_env,
            &profile.settings_file,
        )?);
    }
    Ok(())
}

/// Resolve current project/local controls at recorded directories, preserving every owner.
pub fn discover_claude_controls(
    options: &DiscoveryOptions,
    profiles: &mut [Profile],
    requests: &[Request],
) -> Result<(), ScanError> {
    let env = &options.env;
    let empty = Env::default();
    let default_root = join(&[&options.home, ".claude"]);
    let configured = env.get("CLAUDE_CONFIG_DIR").unwrap_or(&default_root);
    let active = realpath(&resolve(configured)).unwrap_or_default();
    let managed_file = options.managed_settings_file.as_str();
    for profile in profiles
        .iter_mut()
        .filter(|p| p.harness == Harness::Claude && p.t3.is_none())
    {
        profile.background_requests.clear();
        let user = options.settings(&profile.settings_file, Harness::Claude)?;
        let managed = settings(managed_file, Harness::Claude)?;
        let local_env = if active == profile.root { env } else { &empty };
        let mut outranking: Vec<String> = vec![];
        if !managed.is_empty() {
            outranking.push(managed_file.into());
        }
        let mut speed_directories = IndexMap::default();
        let mut idle_directories = IndexMap::default();
        // Existing directories whose settings the idle controls read.
        let mut read: Vec<&str> = vec![];
        let relevant: Vec<&Request> = requests
            .iter()
            .filter(|r| r.agent == Agent::Main && r.profiles.contains(&profile.id))
            .collect();
        if relevant
            .iter()
            .any(|r| r.cwd.as_deref().is_none_or(str::is_empty))
            && let Some(idle) = &mut profile.idle
        {
            idle.resolved = false;
            idle.reason = "recorded working directory is absent; current project/local ownership cannot be resolved".into();
        }
        let mut unknown = profile.background.clone();
        unresolved_background(
            &mut unknown,
            "working directory absent",
            "recorded working directory is absent; current project/local ownership and launch selection are unresolved",
        );
        profile
            .background_directories
            .insert(String::new(), unknown);
        for cwd in directories(relevant.into_iter()) {
            if !existing_directory(cwd)? {
                let mut controls = profile.background.clone();
                unresolved_background(
                    &mut controls,
                    "working directory missing",
                    "recorded working directory is missing or not absolute; current project/local ownership and launch selection are unresolved",
                );
                profile.background_directories.insert(cwd.into(), controls);
                if let Some(mut speed) = profile.speed.clone() {
                    speed.resolved = false;
                    speed.value = None;
                    speed.reason =
                        "recorded working directory no longer exists; current ownership unresolved"
                            .into();
                    speed_directories.insert(cwd.into(), speed);
                }
                if let Some(mut idle) = profile.idle.clone() {
                    idle.resolved = false;
                    idle.reason = "recorded working directory no longer exists; current project/local ownership is unresolved".into();
                    idle_directories.insert(cwd.into(), idle);
                }
                continue;
            }
            let [project_file, legacy_file, local_file] = claude_project_files(cwd, options)?;
            let project = options.settings(&project_file, Harness::Claude)?;
            let legacy = options.settings(&legacy_file, Harness::Claude)?;
            let local = options.settings(&local_file, Harness::Claude)?;
            for file in [&project_file, &legacy_file, &local_file] {
                options.pretend_on(Harness::Claude, &mut profile.pretended_on, file)?;
            }
            if let Some(state) = &profile.claude_config {
                options.pretend_mcp_on(&mut profile.pretended_on, state, cwd)?;
            }
            for (file, config) in [
                (&project_file, &project),
                (&legacy_file, &legacy),
                (&local_file, &local),
            ] {
                // A session in the home directory reads the user's own file as its project's.
                // Under --defaults a project file sets no switch.
                if !config.is_empty()
                    && !options.defaults
                    && !same_file(file, &profile.settings_file)
                    && !outranking.contains(file)
                {
                    outranking.push(file.clone());
                }
            }
            let scopes = [
                SettingsScope {
                    config: &user,
                    file: &profile.settings_file,
                },
                SettingsScope {
                    config: &project,
                    file: &project_file,
                },
                SettingsScope {
                    config: &legacy,
                    file: &legacy_file,
                },
                SettingsScope {
                    config: &local,
                    file: &local_file,
                },
                SettingsScope {
                    config: &managed,
                    file: managed_file,
                },
            ];
            let speed = speed_controls(Harness::Claude, &scopes, local_env)?;
            let mut idle = idle_controls(&scopes, local_env)?;
            let background = claude_background(&scopes, local_env)?;
            let settings_env = scoped_environment(&scopes, &empty);
            let launch_conflict = CLAUDE_BACKGROUND_ENV.iter().any(|key| {
                settings_env
                    .values
                    .get(*key)
                    .zip(local_env.get(*key))
                    .is_some_and(|(setting, inherited)| js_string(Some(setting)) != *inherited)
            });
            let mut conflicted = None;
            for request in requests.iter().filter(|r| {
                launch_conflict
                    && r.agent == Agent::Main
                    && r.profiles.contains(&profile.id)
                    && r.cwd.as_deref() == Some(cwd)
                    && r.client.as_deref() != Some("cli")
            }) {
                let controls = conflicted.get_or_insert_with(|| {
                    let mut controls = background.clone();
                    unresolved_background(
                        &mut controls,
                        "launch environment conflict",
                        "settings env and inherited environment conflict; the recorded client does not establish terminal versus hosted launch precedence",
                    );
                    std::sync::Arc::<[BackgroundControl]>::from(controls)
                });
                profile
                    .background_requests
                    .insert(request.id.clone(), controls.clone());
            }
            let config = merged(&[&user, &project, &legacy, &local, &managed]);
            read.push(cwd);
            idle.ttl = Some(ttl_controls(&TtlSources {
                config: &config,
                env: local_env,
                file: &profile.settings_file,
                managed: &managed,
                managed_file,
                scopes: Some(&scopes),
                billing: profile.billing,
            }));
            idle.compaction = Some(compaction_controls(&CompactionSources {
                harness: Harness::Claude,
                user: &user,
                managed: &managed,
                env: local_env,
                file: &profile.settings_file,
                managed_file,
                scopes: Some(&scopes),
            })?);
            profile
                .background_directories
                .insert(cwd.to_string(), background);
            speed_directories.insert(cwd.to_string(), speed);
            idle_directories.insert(cwd.to_string(), idle);
        }
        profile.speed_directories = Some(speed_directories);
        profile.idle_directories = Some(idle_directories);
        unanswer_mixed_defaults(profile);
        // The main-agent directories keep the TTLs idle compaction read, so both see one
        // observation of each file.
        let known = read
            .into_iter()
            .filter_map(|cwd| {
                let idle = profile.idle_directories.as_ref()?.get(cwd)?;
                Some((cwd.to_string(), idle.ttl.clone()?))
            })
            .collect();
        profile.ttl_directories = directory_ttls(options, profile, local_env, requests, known)?;
        profile.outranking_files = outranking;
        // /cd reapplies the new env over the old one; removed keys can survive it.
        let mut session_cwds = crate::collections::HashMap::default();
        // Every request whose session changed directory shares its directory's unresolved list.
        let mut carried: crate::collections::HashMap<&str, std::sync::Arc<[BackgroundControl]>> =
            crate::collections::HashMap::default();
        for request in requests
            .iter()
            .filter(|r| r.agent == Agent::Main && r.profiles.contains(&profile.id))
        {
            let (previous, changed) = session_cwds
                .entry(&request.session)
                .or_insert((request.cwd.as_deref(), false));
            *changed |= *previous != request.cwd.as_deref();
            *previous = request.cwd.as_deref();
            let cwd = request.cwd.as_deref().unwrap_or_default();
            if *changed && let Some(controls) = profile.background_directories.get(cwd) {
                let controls = carried.entry(cwd).or_insert_with(|| {
                    let mut controls = controls.clone();
                    unresolved_background(
                        &mut controls,
                        "working directory changed",
                        "session working directory changed; /cd environment carry-over and launch selection are not recorded",
                    );
                    controls.into()
                });
                profile
                    .background_requests
                    .insert(request.id.clone(), controls.clone());
            }
        }
    }
    pretend_t3_projects_on(options, profiles, requests, Harness::Claude)?;
    // A T3 instance that inherits its window reads it from the same project/local files,
    // under the instance's own environment; its other controls stay T3's.
    for profile in profiles.iter_mut().filter(|p| p.harness == Harness::Claude) {
        let Some(t3) = &profile.t3 else {
            continue;
        };
        let Some(env) = t3.env.clone() else {
            continue;
        };
        let inherited = t3.inherited_window;
        let t3_file = std::path::Path::new(&t3.state)
            .join("settings.json")
            .display()
            .to_string();
        let (instance, environment) = (t3.instance.clone(), t3.environment.clone());
        let env = &env;
        let ttls = directory_ttls(options, profile, env, requests, IndexMap::default())?;
        profile.ttl_directories = ttls.clone();
        if !inherited {
            continue;
        }
        let user = options.settings(&profile.settings_file, Harness::Claude)?;
        let managed = settings(managed_file, Harness::Claude)?;
        let user_scope = SettingsScope {
            config: &user,
            file: &profile.settings_file,
        };
        let managed_scope = SettingsScope {
            config: &managed,
            file: managed_file,
        };
        // T3 reads only the window and TTLs from a directory's control.
        let base = IdleControl {
            ttl: Some(profile.ttl.clone()),
            ..idle_controls(&[user_scope, managed_scope], env)?
        };
        let mut directories_controls = IndexMap::default();
        // As for a native profile, a missing or unrecorded directory keeps the home's window.
        for cwd in directories(
            requests
                .iter()
                .filter(|r| r.agent == Agent::Main && r.profiles.contains(&profile.id)),
        ) {
            if !existing_directory(cwd)? {
                continue;
            }
            let [project_file, legacy_file, local_file] = claude_project_files(cwd, options)?;
            let project = options.settings(&project_file, Harness::Claude)?;
            let legacy = options.settings(&legacy_file, Harness::Claude)?;
            let local = options.settings(&local_file, Harness::Claude)?;
            let scopes = [
                user_scope,
                SettingsScope {
                    config: &project,
                    file: &project_file,
                },
                SettingsScope {
                    config: &legacy,
                    file: &legacy_file,
                },
                SettingsScope {
                    config: &local,
                    file: &local_file,
                },
                managed_scope,
            ];
            let mut compaction = compaction_controls(&CompactionSources {
                harness: Harness::Claude,
                user: &user,
                managed: &managed,
                env,
                file: &profile.settings_file,
                managed_file,
                scopes: Some(&scopes),
            })?;
            crate::controls::t3_percent(
                &mut compaction,
                &environment,
                &t3_file,
                &instance,
                false,
                &options.env,
            );
            directories_controls.insert(
                cwd.to_string(),
                IdleControl {
                    compaction: Some(compaction),
                    ttl: ttls.get(cwd).cloned().or_else(|| base.ttl.clone()),
                    ..base.clone()
                },
            );
        }
        profile.idle_directories = Some(directories_controls);
    }
    Ok(())
}

/// Codex keys that let a profile or project file change speed, model or gates.
const CODEX_SPEED_KEYS: [&str; 4] = ["model", "model_provider", "service_tier", "features"];

/// Codex's project root markers from its system and user settings, `.git` by default.
pub(crate) fn codex_markers(
    scopes: &[SettingsScope],
    file: &str,
) -> Result<Vec<String>, ScanError> {
    match scopes
        .iter()
        .rev()
        .find_map(|s| s.config.get("project_root_markers"))
    {
        None => Ok(vec![".git".into()]),
        Some(Value::Array(values)) if values.iter().all(Value::is_string) => Ok(values
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()),
        Some(_) => Err(error("invalid project_root_markers", file, 1)),
    }
}

/// A directory's current Codex trust level: the exact directory's, then its project root's.
/// Trust is never inferred from the fact that requests exist.
pub(crate) fn codex_trust<'a>(
    scopes: &[SettingsScope<'a>],
    cwd: &str,
    root: &str,
) -> Option<&'a str> {
    [cwd, root].into_iter().find_map(|path| {
        scopes.iter().rev().find_map(|scope| {
            members(scope.config.get("projects"))
                .get(path)
                .and_then(|p| p.get("trust_level"))
                .and_then(Value::as_str)
        })
    })
}

/// Resolve Codex project controls without inferring trust or a selected launch profile.
pub fn discover_codex_controls(
    options: &DiscoveryOptions,
    profiles: &mut [Profile],
    requests: &[Request],
) -> Result<(), ScanError> {
    for profile in profiles
        .iter_mut()
        .filter(|p| p.harness == Harness::Codex && p.t3.is_none())
    {
        pretend_named_profiles_on(options, profile)?;
        let user = options.settings(&profile.settings_file, Harness::Codex)?;
        let system = options.settings(&options.codex_system_config_file, Harness::Codex)?;
        let base_scopes = [
            SettingsScope {
                config: &system,
                file: &options.codex_system_config_file,
            },
            SettingsScope {
                config: &user,
                file: &profile.settings_file,
            },
        ];
        let mut base = codex_background(&base_scopes)?;
        let legacy_selection =
            user.contains_key("profile") || !members(user.get("profiles")).is_empty();
        if legacy_selection {
            unresolved_background(
                &mut base,
                "legacy profile configuration",
                "legacy profile configuration exists; current Codex removed it in 0.134.0, but installed version and launch selection are unresolved",
            );
        }
        let mut marker_selection = legacy_selection;
        // Files that can replace the system and user instruction budget for some runs.
        let mut budget_overrides: Vec<String> = members(user.get("profiles"))
            .iter()
            .filter(|(_, p)| members(Some(p)).contains_key(AGENTS_BUDGET))
            .map(|(name, _)| format!("{}:profiles.{name}", profile.settings_file))
            .collect();
        // Profiles that set memories, which a --profile selection could apply.
        let mut memory_profiles: Vec<String> = members(user.get("profiles"))
            .values()
            .any(|p| sets_codex_memory(members(Some(p))))
            .then(|| profile.settings_file.clone())
            .into_iter()
            .collect();
        let mut outranking: Vec<String> = vec![];
        // Other files that set the turn-end compaction threshold, which a --profile selection
        // or a project could apply.
        let mut turn_end_files: Vec<String> = members(user.get("profiles"))
            .iter()
            .filter(|(_, p)| members(Some(p)).contains_key(TURN_END_KEY))
            .map(|(name, _)| format!("{}:profiles.{name}", profile.settings_file))
            .collect();
        // Named profile files that set the model or its effort, which a --profile selection could apply.
        let mut effort_profile_files: Vec<String> = vec![];
        for name in named_profile_files(options, &profile.root)? {
            let file = join(&[&profile.root, &name]);
            let config = options.settings(&file, Harness::Codex)?;
            if !config.is_empty() {
                outranking.push(file.clone());
            }
            if sets_codex_memory(&config) {
                memory_profiles.push(file.clone());
            }
            if sets_codex_effort(&config) {
                effort_profile_files.push(file.clone());
            }
            if config.contains_key(TURN_END_KEY) {
                turn_end_files.push(file.clone());
            }
            if config.contains_key(AGENTS_BUDGET) {
                budget_overrides.push(file.clone());
            }
            marker_selection |=
                config.contains_key("project_root_markers") || config.contains_key("projects");
            if members(config.get("tui")).contains_key("auto_recap") {
                // Validate only the allowlisted control; profile selection is not in history.
                codex_background(&[SettingsScope {
                    config: &config,
                    file: &file,
                }])?;
                unresolved_background(
                    &mut base,
                    "named config profiles",
                    "named config profiles can override automatic recaps; --profile and CLI/session selection are not recorded",
                );
                base[0].owner = format!(
                    "{}; {file}:tui.auto_recap (--profile selection unresolved)",
                    base[0].owner
                );
            }
        }
        // Named profiles can set memories too, and which one launched is not recorded.
        let profile_memory = (!memory_profiles.is_empty()).then_some(
            "a named config profile sets memories, and --profile selection is not recorded",
        );
        if let Some(memory) = &mut profile.memory {
            memory.unresolved = profile_memory.map(String::from);
            memory.overrides = memory_profiles.clone();
        }
        profile.background = base.clone();
        let mut unknown = base.clone();
        unresolved_background(
            &mut unknown,
            "working directory absent",
            "recorded working directory is absent; current project trust, ownership and launch selection are unresolved",
        );
        profile
            .background_directories
            .insert(String::new(), unknown);
        let markers = codex_markers(&base_scopes, &profile.settings_file)?;
        let mine: Vec<&Request> = requests
            .iter()
            .filter(|r| r.profiles.contains(&profile.id))
            .collect();
        // Memories' setting is read at the latest main-agent request's directory, as
        // for Claude.
        let latest = crate::memory::deciding_request(&mine).and_then(|r| r.cwd.clone());
        // Effort is read where memories are, from the layers Codex trusts there.
        let catalog = codex_catalog(&profile.root);
        let recorded = latest_model(&mine);
        let effort_profiles: Vec<String> = members(user.get("profiles"))
            .iter()
            .filter(|(_, p)| sets_codex_effort(members(Some(p))))
            .map(|(name, _)| format!("{}:profiles.{name}", profile.settings_file))
            .chain(effort_profile_files.iter().cloned())
            .collect();
        let effort_unresolved = |control: &mut crate::model::EffortControl, files: &[String]| {
            if !files.is_empty() && control.unresolved.is_none() {
                control.unresolved = Some(format!(
                    "{} can set the model or model_reasoning_effort for some runs, and which applied is not recorded",
                    files.join(", ")
                ));
            }
        };
        let mut effort = codex_effort(
            &base_scopes,
            &profile.settings_file,
            catalog.as_ref(),
            recorded,
        )?;
        effort_unresolved(&mut effort, &effort_profiles);
        profile.effort = Some(effort);
        let cwds = directories(mine.iter().copied());
        for cwd in cwds {
            if !existing_directory(cwd)? {
                let mut controls = base.clone();
                unresolved_background(
                    &mut controls,
                    "working directory missing",
                    "recorded working directory is missing or not absolute; current project trust and ownership are unresolved",
                );
                profile.background_directories.insert(cwd.into(), controls);
                continue;
            }
            let paths = project_directories(cwd, &markers)?;
            let root = paths.last().expect("cwd is included");
            let trust = codex_trust(&base_scopes, cwd, root);
            let mut layers = Vec::new();
            for path in &paths {
                if trust == Some("untrusted") {
                    continue;
                }
                let file = join(&[path, ".codex", "config.toml"]);
                let config = options.settings(&file, Harness::Codex)?;
                options.pretend_on(Harness::Codex, &mut profile.pretended_on, &file)?;
                let overrides = CODEX_SPEED_KEYS.iter().any(|k| config.contains_key(*k));
                let known = profile
                    .speed_directories
                    .as_ref()
                    .is_some_and(|d| d.contains_key(cwd));
                if overrides && !known {
                    let speed = profile
                        .speed
                        .clone()
                        .expect("every discovered profile has a speed control");
                    profile.speed_directories.get_or_insert_default().insert(cwd.into(), SpeedControl {
                        value: None, resolved: false, owner: file.clone(),
                        reason: "current project overrides exist; trust, profile and CLI/session selection are not established".into(),
                        ..speed
                    });
                }
                if config.contains_key(AGENTS_BUDGET)
                    && file != profile.settings_file
                    && !budget_overrides.contains(&file)
                {
                    budget_overrides.push(file.clone());
                }
                if config.contains_key(TURN_END_KEY)
                    && !same_file(&file, &profile.settings_file)
                    && !turn_end_files.contains(&file)
                {
                    turn_end_files.push(file.clone());
                }
                // A layer Codex may read, unless it is the user's own file again. Under
                // --defaults one holds only server declarations, which set no switch.
                if !config.is_empty()
                    && !options.defaults
                    && !same_file(&file, &profile.settings_file)
                    && !outranking.contains(&file)
                {
                    outranking.push(file.clone());
                }
                layers.push((file, config));
            }
            // A named profile's project_root_markers or projects can trust or root a
            // project differently, so any project config up to the home may apply.
            if marker_selection {
                let keep = |config: &Map<String, Value>| !config.is_empty();
                for file in ancestor_codex_configs(
                    options,
                    cwd,
                    &profile.settings_file,
                    &options.home,
                    keep,
                )? {
                    if same_file(&file, &profile.settings_file) {
                        continue;
                    }
                    if options
                        .settings(&file, Harness::Codex)?
                        .contains_key(TURN_END_KEY)
                        && !turn_end_files.contains(&file)
                    {
                        turn_end_files.push(file.clone());
                    }
                    if !outranking.contains(&file) {
                        outranking.push(file);
                    }
                }
            }
            let mut scopes = base_scopes.to_vec();
            if trust != Some("untrusted") {
                scopes.extend(
                    layers
                        .iter()
                        .rev()
                        .map(|(file, config)| SettingsScope { config, file }),
                );
            }
            if latest.as_deref() == Some(cwd) {
                // Codex reads a project's config only once the project is trusted.
                let trusted = if trust == Some("trusted") {
                    &scopes[..]
                } else {
                    &base_scopes[..]
                };
                let untrusted: Vec<String> = layers
                    .iter()
                    .filter(|(_, config)| trust.is_none() && sets_codex_memory(config))
                    .map(|(file, _)| file.clone())
                    .collect();
                let mut effort =
                    codex_effort(trusted, &profile.settings_file, catalog.as_ref(), recorded)?;
                let mut effort_files = effort_profiles.clone();
                effort_files.extend(
                    layers
                        .iter()
                        .filter(|(_, config)| trust.is_none() && sets_codex_effort(config))
                        .map(|(file, _)| file.clone()),
                );
                // A named profile's project_root_markers or projects can change which
                // project layers apply, as for memories.
                if marker_selection {
                    for file in ancestor_codex_configs(
                        options,
                        cwd,
                        &profile.settings_file,
                        &options.home,
                        sets_codex_effort,
                    )? {
                        if !effort_files.iter().any(|f| same_file(f, &file)) {
                            effort_files.push(file);
                        }
                    }
                }
                effort_unresolved(&mut effort, &effort_files);
                profile.effort = Some(effort);
                let untrusted_layer = !untrusted.is_empty();
                // A named profile's project_root_markers or projects can change which
                // project layers apply, which matters only where one sets memories.
                let moved = if marker_selection {
                    ancestors_setting_codex_memory(
                        options,
                        cwd,
                        &profile.settings_file,
                        &options.home,
                    )?
                } else {
                    vec![]
                };
                let moved_layer = !moved.is_empty();
                let mut overrides = memory_profiles.clone();
                // The walk names files by resolved path, the project layers by the
                // recorded one, so compare them resolved.
                let same = |a: &str, b: &str| {
                    a == b || realpath(a).is_ok_and(|a| realpath(b).is_ok_and(|b| a == b))
                };
                for file in untrusted.into_iter().chain(moved) {
                    if !overrides.iter().any(|o| same(o, &file)) {
                        overrides.push(file);
                    }
                }
                let reasons: Vec<&str> = [
                    untrusted_layer.then_some(
                        "a project .codex/config.toml sets memories, but the project's trust is not recorded",
                    ),
                    moved_layer.then_some(
                        "a .codex/config.toml above the latest session's directory sets memories, and a named config profile that can change which project config applies may have been selected",
                    ),
                    profile_memory,
                ]
                .into_iter()
                .flatten()
                .collect();
                profile.memory = Some(MemoryControl {
                    directory: Some(cwd.into()),
                    unresolved: (!reasons.is_empty()).then(|| reasons.join("; ")),
                    overrides,
                    stored: codex_memory_stored(&profile.root),
                    ..codex_memory(trusted, &profile.settings_file)?
                });
            }
            let has_override = trust != Some("untrusted")
                && layers
                    .iter()
                    .any(|(_, c)| members(c.get("tui")).contains_key("auto_recap"));
            let mut controls = if has_override {
                codex_background(&scopes)?
            } else {
                base.clone()
            };
            if has_override && trust != Some("trusted") {
                unresolved_background(
                    &mut controls,
                    "project trust unknown",
                    "project automatic recap overrides exist, but current project trust is unknown; historical trust and CLI/session selection are not recorded",
                );
            }
            if marker_selection {
                unresolved_background(
                    &mut controls,
                    if legacy_selection {
                        "legacy profile configuration"
                    } else {
                        "named profiles change root or trust"
                    },
                    if legacy_selection {
                        "legacy profile configuration exists; installed version and launch selection are unresolved"
                    } else {
                        "named profiles can change project root or trust discovery; --profile and CLI/session selection are not recorded"
                    },
                );
            }
            if trust == Some("untrusted") {
                for c in &mut controls {
                    c.reason +=
                        "; current project is untrusted, so project config layers are ignored";
                }
            }
            profile.background_directories.insert(cwd.into(), controls);
        }
        if let Some(control) = &mut profile.turn_end {
            control.unresolved = (!turn_end_files.is_empty()).then(|| {
                format!(
                    "{} can set {TURN_END_KEY} for some runs, and which applied is not recorded",
                    turn_end_files.join(", ")
                )
            });
        }
        profile.outranking_files = outranking;
        profile.instruction_budget_unresolved = (!budget_overrides.is_empty()).then(|| {
            format!(
                "{} can set {AGENTS_BUDGET} for some runs, which the transcripts do not record",
                budget_overrides.join(", ")
            )
        });
    }
    pretend_t3_projects_on(options, profiles, requests, Harness::Codex)
}

/// Under `--defaults`, what a Codex home's named profile files turn off, which --defaults
/// shows on, for the servers a default user has: those the files it reads declare.
fn pretend_named_profiles_on(
    options: &DiscoveryOptions,
    profile: &mut Profile,
) -> Result<(), ScanError> {
    if !options.defaults {
        return Ok(());
    }
    for name in profile_file_names(&profile.root)? {
        let file = join(&[&profile.root, &name]);
        let config = settings(&file, Harness::Codex)?;
        let mut found = switched_off(Harness::Codex, &config);
        found.extend(allowlists(&config));
        found.retain(|k| k.path[0] != "mcp_servers" || profile.mcp_servers.contains(&k.path[1]));
        options.pretend_switches_on(Harness::Codex, &mut profile.pretended_on, &file, found)?;
    }
    Ok(())
}

/// Under `--defaults`, what the project files at each directory a T3 instance's requests
/// ran in turn off, as for a native profile: the CLI it launches reads them there. A Codex
/// project reads as untrusted by no file and rooted at `.git`, since the defaults hold no
/// trust or markers.
fn pretend_t3_projects_on(
    options: &DiscoveryOptions,
    profiles: &mut [Profile],
    requests: &[Request],
    harness: Harness,
) -> Result<(), ScanError> {
    if !options.defaults {
        return Ok(());
    }
    for profile in profiles
        .iter_mut()
        .filter(|p| p.harness == harness && p.t3.is_some())
    {
        if harness == Harness::Codex {
            pretend_named_profiles_on(options, profile)?;
        }
        let mine = requests.iter().filter(|r| r.profiles.contains(&profile.id));
        let cwds: Vec<String> = directories(mine).into_iter().map(String::from).collect();
        for cwd in cwds {
            if !existing_directory(&cwd)? {
                continue;
            }
            let files = match harness {
                Harness::Claude => claude_project_files(&cwd, options)?.to_vec(),
                Harness::Codex => project_directories(&cwd, &[".git".into()])?
                    .iter()
                    .map(|path| join(&[path, ".codex", "config.toml"]))
                    .collect(),
            };
            for file in &files {
                options.pretend_on(harness, &mut profile.pretended_on, file)?;
            }
            if let Some(state) = &profile.claude_config {
                options.pretend_mcp_on(&mut profile.pretended_on, state, &cwd)?;
            }
        }
    }
    Ok(())
}

/// Whether a Codex config sets the model or its reasoning effort.
fn sets_codex_effort(config: &Map<String, Value>) -> bool {
    config.contains_key("model") || config.contains_key("model_reasoning_effort")
}

/// Whether a Codex config sets whether memories are on, used or generated.
fn sets_codex_memory(config: &Map<String, Value>) -> bool {
    let memories = members(config.get("memories"));
    members(config.get("features")).contains_key("memories")
        || memories.contains_key("use_memories")
        || memories.contains_key("generate_memories")
}

/// Whether a Codex home holds a memory store, which Codex writes only once it has
/// generated memories.
fn codex_memory_stored(root: &str) -> bool {
    ["memories", "memories_v2"]
        .iter()
        .any(|name| std::path::Path::new(&join(&[root, name])).is_dir())
}

/// Every `.codex/config.toml` from `cwd` up to the user's home, or the filesystem root
/// outside it, other than the user's own, that sets memories, whichever of them a
/// project root would include. A home's `.codex` is the user layer, and directories
/// above a home hold no one's project.
fn ancestors_setting_codex_memory(
    options: &DiscoveryOptions,
    cwd: &str,
    user: &str,
    home: &str,
) -> Result<Vec<String>, ScanError> {
    ancestor_codex_configs(options, cwd, user, home, sets_codex_memory)
}

/// Whether two paths name one file, as recorded or once resolved.
fn same_file(a: &str, b: &str) -> bool {
    a == b || realpath(a).is_ok_and(|a| realpath(b).is_ok_and(|b| a == b))
}

/// Every `.codex/config.toml` from `cwd` up to the user's home, or the filesystem root
/// outside it, other than the user's own, whose settings `keep` accepts.
fn ancestor_codex_configs(
    options: &DiscoveryOptions,
    cwd: &str,
    user: &str,
    home: &str,
    keep: impl Fn(&Map<String, Value>) -> bool,
) -> Result<Vec<String>, ScanError> {
    // Walk and stop on normalized forms, such as resolved symlinks and Windows' `\\?\`
    // prefix: a symlinked cwd's physical parents are its ancestors, not the link's. A path
    // that no longer exists is walked as recorded.
    let normal = |path: &str| realpath(path).unwrap_or_else(|_| path.to_string());
    let home = normal(home);
    let cwd = normal(cwd);
    let mut files = vec![];
    let mut path = cwd.as_str();
    loop {
        let file = join(&[path, ".codex", "config.toml"]);
        if file != user && keep(&options.settings(&file, Harness::Codex)?) {
            files.push(file);
        }
        if path == home || dirname(path) == path {
            return Ok(files);
        }
        path = dirname(path);
    }
}

/// Whether a file name selects a named Codex profile, as in `--profile NAME`.
fn named_profile_file(name: &str) -> bool {
    name.strip_suffix(".config.toml").is_some_and(|stem| {
        !stem.is_empty()
            && stem
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    })
}

/// Named profile files in stable order, shared by control discovery; none under
/// `--defaults`, whose user selects no `--profile`.
pub(crate) fn named_profile_files(
    options: &DiscoveryOptions,
    root: &str,
) -> Result<Vec<String>, ScanError> {
    if options.defaults {
        return Ok(vec![]);
    }
    profile_file_names(root)
}

/// The named profile files in a Codex home, in stable order, whatever the run pretends.
fn profile_file_names(root: &str) -> Result<Vec<String>, ScanError> {
    let unreadable = || error("cannot read settings", root, 0);
    let mut names = Vec::new();
    for entry in fs::read_dir(root).map_err(|_| unreadable())? {
        let name = entry.map_err(|_| unreadable())?.file_name();
        if let Some(name) = name.to_str().filter(|n| named_profile_file(n)) {
            names.push(name.to_string());
        }
    }
    names.sort_unstable();
    Ok(names)
}

/// The Codex files that can override or block a profile's saved speed.
struct CodexSpeedSources<'a> {
    options: &'a DiscoveryOptions,
    scopes: &'a [SettingsScope<'a>],
    user: &'a Map<String, Value>,
    requirements: &'a Map<String, Value>,
    requirements_file: &'a str,
    canonical: &'a str,
    settings_file: &'a str,
}

/// Lower a saved Codex premium tier to what managed requirements and unobserved profiles allow.
fn codex_speed(speed: &mut SpeedControl, sources: &CodexSpeedSources) -> Result<(), ScanError> {
    let CodexSpeedSources {
        options,
        scopes,
        user,
        requirements,
        requirements_file,
        canonical,
        settings_file,
    } = *sources;
    let base_tier = scopes
        .iter()
        .rev()
        .find_map(|s| s.config.get("service_tier"))
        .cloned();
    // `?? baseTier`: a null tier also falls back.
    let or_base =
        |tier: Option<&Value>| tier.filter(|t| !t.is_null()).cloned().or(base_tier.clone());
    let mut possible = vec![base_tier.clone()];
    for row in members(user.get("profiles")).values() {
        possible.push(or_base(members(Some(row)).get("service_tier")));
    }
    let features = members(requirements.get("features"));
    for key in ["fast_mode", "ultrafast_mode"] {
        let value = features.get(key);
        if value.is_some_and(|v| !v.is_boolean()) {
            return Err(error(
                "invalid required speed feature",
                requirements_file,
                1,
            ));
        }
        let current = speed.value.as_deref();
        let premium = if key == "fast_mode" {
            matches!(current, Some("fast" | "priority"))
        } else {
            current == Some("ultrafast")
        };
        if value == Some(&Value::Bool(false)) && premium {
            speed.value = Some("standard".into());
            speed.owner = format!("{requirements_file}:features.{key}");
            speed.reason = "premium tier blocked by managed requirements".into();
            speed.resolved = true;
        }
    }
    if user.contains_key("profile") || !members(user.get("profiles")).is_empty() {
        speed.value = None;
        speed.resolved = false;
        speed.reason = "legacy profile selector/tables exist; current Codex removed them in 0.134.0, installed version and selected profile are unresolved".into();
    }
    for name in named_profile_files(options, canonical)? {
        let file = join(&[canonical, &name]);
        let config = options.settings(&file, Harness::Codex)?;
        possible.push(or_base(config.get("service_tier")));
        if CODEX_SPEED_KEYS.iter().any(|k| config.contains_key(*k)) {
            speed.value = None;
            speed.resolved = false;
            speed.reason = "named profile files can override speed/model/gates; --profile selection is unobserved".into();
            speed.owner = format!("{settings_file}; {file} (--profile selection unresolved)");
        }
    }
    let disabled = |key: &str| features.get(key) == Some(&Value::Bool(false));
    let blocked = |tier: &Option<Value>| {
        (matches!(js_string(tier.as_ref()).as_str(), "fast" | "priority") && disabled("fast_mode"))
            || (tier.as_ref().and_then(Value::as_str) == Some("ultrafast")
                && disabled("ultrafast_mode"))
    };
    // An unobserved profile cannot bypass a hard policy block. A different,
    // allowed premium tier still keeps profile selection unresolved.
    if possible.iter().any(blocked)
        && possible.iter().all(|tier| {
            tier.is_none()
                || matches!(js_string(tier.as_ref()).as_str(), "standard" | "default")
                || blocked(tier)
        })
    {
        speed.value = Some("standard".into());
        speed.resolved = true;
        speed.owner = format!("{requirements_file}:features");
        speed.reason = "all locally configured premium tiers are blocked by managed requirements, including profile alternatives".into();
    }
    Ok(())
}

/// The file identity that deduplicates hard links and aliases.
type FileKey = (u64, u64, String);

/// A file's identity and length, read together so a replacement cannot pair one file's length
/// with another's identity.
#[cfg(unix)]
fn file_key(info: &fs::Metadata, _canonical: &str) -> (FileKey, u64) {
    use std::os::unix::fs::MetadataExt;
    ((info.dev(), info.ino(), String::new()), info.len())
}

/// The volume serial number and file index Node's `stat` reports as `dev` and `ino` on Windows,
/// with the length from the same handle.
#[cfg(windows)]
fn file_key(info: &fs::Metadata, canonical: &str) -> (FileKey, u64) {
    let handle = winapi_util::Handle::from_path_any(canonical);
    match handle.and_then(|h| winapi_util::file::information(&h)) {
        Ok(file) => (
            (
                file.volume_serial_number(),
                file.file_index(),
                String::new(),
            ),
            file.file_size(),
        ),
        // An unopenable file fails later at read time with a safe error.
        Err(_) => ((0, 0, canonical.to_string()), info.len()),
    }
}

/// Without device and inode numbers, the canonical path is the identity.
#[cfg(not(any(unix, windows)))]
fn file_key(info: &fs::Metadata, canonical: &str) -> (FileKey, u64) {
    ((0, 0, canonical.to_string()), info.len())
}

/// Snapshots in discovery order, and the directories each profile has already walked.
#[derive(Default)]
struct Walk {
    files: IndexMap<FileKey, Snapshot>,
    visited: HashSet<(FileKey, usize)>,
}

impl Walk {
    /// Record every `.jsonl` file under `history`, depth first in sorted order, without following a directory twice.
    fn history(&mut self, history: &str, index: usize, profile: &Profile) -> Result<(), ScanError> {
        // A known non-symlink type means the path is already canonical: its parent was.
        let mut stack: Vec<(String, Option<fs::FileType>)> = vec![(history.to_string(), None)];
        while let Some((path, kind)) = stack.pop() {
            let fail = || error("cannot snapshot selected history", &path, 0);
            let (canonical, info) = match kind {
                Some(kind) if !kind.is_symlink() => {
                    let wanted = kind.is_dir() || (kind.is_file() && path.ends_with(".jsonl"));
                    if !wanted {
                        continue;
                    }
                    let info = fs::metadata(&path).map_err(|_| fail())?;
                    (path.clone(), info)
                }
                _ => {
                    let canonical = realpath(&path).map_err(|_| fail())?;
                    let info = fs::metadata(&canonical).map_err(|_| fail())?;
                    (canonical, info)
                }
            };
            let (key, length) = file_key(&info, &canonical);
            if info.is_dir() {
                if !self.visited.insert((key, index)) {
                    continue;
                }
                let mut entries = Vec::new();
                for entry in fs::read_dir(&canonical).map_err(|_| fail())? {
                    let entry = entry.map_err(|_| fail())?;
                    let name = entry.file_name();
                    // Node names entries lossily; a non-UTF-8 name then fails to resolve.
                    let kind = match name.to_str() {
                        Some(_) => Some(entry.file_type().map_err(|_| fail())?),
                        None => None,
                    };
                    entries.push((name.to_string_lossy().into_owned(), kind));
                }
                // Names sort by UTF-16 code units.
                entries.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
                for (name, kind) in entries.into_iter().rev() {
                    stack.push((join(&[&canonical, &name]), kind));
                }
            } else if info.is_file() && path.ends_with(".jsonl") {
                let id = &profile.id;
                match self.files.get_mut(&key) {
                    Some(previous) => {
                        if !previous.profiles.contains(id) {
                            previous.profiles.push(id.clone());
                        }
                    }
                    None => {
                        let snapshot = Snapshot {
                            file: canonical,
                            harness: profile.harness,
                            profiles: vec![id.clone()],
                            bytes: length,
                            device: key.0,
                            inode: key.1,
                            deferred_bytes: 0,
                            skipped_lines: 0,
                            unreadable: Default::default(),
                            lines: 0,
                        };
                        self.files.insert(key, snapshot);
                    }
                }
            }
        }
        Ok(())
    }
}

/// Add a candidate root; an explicit root keeps its origin, and explicit selection excludes the rest.
fn add_root(
    candidates: &mut IndexMap<String, RootStatus>,
    explicit: bool,
    harness: Harness,
    path: &str,
    origin: RootOrigin,
) {
    let absolute = resolve(path);
    let key = format!("{}{absolute}", harness.as_str());
    if candidates
        .get(&key)
        .is_some_and(|previous| previous.origin == RootOrigin::Explicit)
    {
        return;
    }
    let status = if explicit && origin != RootOrigin::Explicit {
        RootState::Excluded
    } else {
        RootState::Selected
    };
    candidates.insert(
        key,
        RootStatus {
            harness,
            path: absolute,
            status,
            origin,
        },
    );
}

/// Resolve one canonical root's current settings into a profile, without walking its histories.
pub(crate) fn profile(
    options: &DiscoveryOptions,
    harness: Harness,
    canonical: String,
    alias: String,
    key: &str,
) -> Result<Profile, ScanError> {
    let claude = harness == Harness::Claude;
    let settings_file = join(&[
        &canonical,
        if claude {
            "settings.json"
        } else {
            "config.toml"
        },
    ]);
    let user = options.settings(&settings_file, harness)?;
    let managed_file = options.managed_settings_file.as_str();
    let managed = if claude {
        settings(managed_file, Harness::Claude)?
    } else {
        Map::new()
    };
    let combined;
    let config = if claude {
        combined = merged(&[&user, &managed]);
        &combined
    } else {
        &user
    };
    // Shell overrides belong only to the selected active profile, never every alias.
    let (variable, default_dir) = if claude {
        ("CLAUDE_CONFIG_DIR", ".claude")
    } else {
        ("CODEX_HOME", ".codex")
    };
    let default_root = join(&[&options.home, default_dir]);
    let active = resolve(options.env.get(variable).unwrap_or(&default_root));
    let active = realpath(&active).unwrap_or(active);
    let empty = Env::default();
    let local_env = if active == canonical {
        &options.env
    } else {
        &empty
    };
    let environment = scoped_environment(
        &[SettingsScope {
            config,
            file: &settings_file,
        }],
        local_env,
    );
    let provider = if claude {
        crate::effort::claude_provider(&environment)
    } else {
        label(config.get("model_provider").unwrap_or(&Value::Null))
    };
    let system_file = options.codex_system_config_file.as_str();
    let system = if claude {
        Map::new()
    } else {
        settings(system_file, Harness::Codex)?
    };
    let speed_scopes = if claude {
        [
            SettingsScope {
                config: &user,
                file: &settings_file,
            },
            SettingsScope {
                config: &managed,
                file: managed_file,
            },
        ]
    } else {
        [
            SettingsScope {
                config: &system,
                file: system_file,
            },
            SettingsScope {
                config: &user,
                file: &settings_file,
            },
        ]
    };
    let requirements_file = options.codex_requirements_file.as_str();
    let requirements = if claude {
        Map::new()
    } else {
        settings(requirements_file, Harness::Codex)?
    };
    let mut speed = speed_controls(harness, &speed_scopes, local_env)?;
    let bash = claude.then(|| bash_cap_control(&speed_scopes, local_env));
    let retention = claude.then(|| {
        retention_control(
            SettingsScope {
                config: &user,
                file: &settings_file,
            },
            SettingsScope {
                config: &managed,
                file: managed_file,
            },
        )
    });
    let turn_end = if claude {
        None
    } else {
        Some(crate::controls::turn_end_control(
            &speed_scopes,
            &settings_file,
        )?)
    };
    if !claude {
        codex_speed(
            &mut speed,
            &CodexSpeedSources {
                options,
                scopes: &speed_scopes,
                user: &user,
                requirements: &requirements,
                requirements_file,
                canonical: &canonical,
                settings_file: &settings_file,
            },
        )?;
    }
    let mut histories = Vec::new();
    let names: &[&str] = if claude {
        &["projects"]
    } else {
        &["sessions", "archived_sessions"]
    };
    for name in names {
        let path = join(&[&canonical, name]);
        match realpath(&path) {
            Ok(history) => histories.push(history),
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(_) => return Err(error("cannot inspect transcript directory", &path, 0)),
        }
    }
    // Only a Claude profile on the Anthropic API has a billing-dependent default.
    let billing = (claude && provider.is_none())
        .then(|| options.billing.get(&canonical).copied())
        .flatten();
    let ttl = ttl_controls(&TtlSources {
        config,
        env: local_env,
        file: &settings_file,
        managed: &managed,
        managed_file,
        scopes: None,
        billing,
    });
    let mut settings = structural_settings(config);
    let mut instruction_budget_file = None;
    if !claude {
        if settings.contains_key(AGENTS_BUDGET) {
            instruction_budget_file = Some(settings_file.clone());
        } else if let Some(value) = structural_settings(&system).shift_remove(AGENTS_BUDGET) {
            // The system config sets Codex's instruction budget unless the user's replaces it.
            settings.insert(AGENTS_BUDGET.into(), value);
            instruction_budget_file = Some(system_file.to_string());
        }
    }
    if claude {
        // Output caps use the same terminal environment precedence as other controls.
        for key in ["BASH_MAX_OUTPUT_LENGTH", "MAX_MCP_OUTPUT_TOKENS"] {
            let value = environment.values.get(key).and_then(Value::as_str);
            if let Some(n) = value.and_then(|v| v.trim().parse::<f64>().ok()) {
                settings.insert(key.into(), Scalar::Number(n));
            }
        }
    }
    let mcp_servers = if claude {
        vec![]
    } else {
        members(config.get("mcp_servers")).keys().cloned().collect()
    };
    let compaction = compaction_controls(&CompactionSources {
        harness,
        user: &user,
        managed: &managed,
        env: local_env,
        file: &settings_file,
        managed_file,
        scopes: None,
    })?;
    let idle = if claude {
        Some(idle_controls(
            &[
                SettingsScope {
                    config: &user,
                    file: &settings_file,
                },
                SettingsScope {
                    config: &managed,
                    file: managed_file,
                },
            ],
            local_env,
        )?)
    } else {
        None
    };
    let background = if claude {
        claude_background(&speed_scopes, local_env)?
    } else {
        codex_background(&speed_scopes)?
    };
    let memory = if claude {
        claude_memory(&speed_scopes, local_env)?
    } else {
        MemoryControl {
            stored: codex_memory_stored(&canonical),
            ..codex_memory(&speed_scopes, &settings_file)?
        }
    };
    // A shell's CLAUDE_CONFIG_DIR keeps Claude Code's state inside it, even when it names
    // the default directory.
    let explicit = options.env.contains_key(variable) && active == canonical;
    let claude_config = claude.then(|| claude_config(&options.home, &canonical, explicit));
    let mut profile = Profile {
        id: identity(key),
        harness,
        root: canonical,
        aliases: vec![alias],
        histories,
        settings_file,
        ttl,
        provider,
        billing,
        settings,
        mcp_servers,
        compaction,
        idle,
        speed: Some(speed),
        bash,
        retention,
        turn_end,
        effort: None,
        advisor: None,
        speed_directories: None,
        idle_directories: None,
        background,
        background_directories: IndexMap::default(),
        background_requests: IndexMap::default(),
        memory: Some(memory),
        inbound: None,
        t3: None,
        agents: crate::model::AgentControls::default(),
        instruction_budget_file,
        instruction_budget_unresolved: None,
        outranking_files: vec![],
        ttl_directories: IndexMap::default(),
        claude_config,
        codex_system_config: (!claude).then(|| system_file.to_string()),
        switched_off: switched_off(harness, &user),
        pretended_on: vec![],
        policy_off: vec![],
    };
    // What the real file turns off, which --defaults shows on unless policy keeps it off.
    if options.defaults {
        profile.policy_off = options.policy_off(harness)?;
    }
    let file = profile.settings_file.clone();
    options.pretend_on(harness, &mut profile.pretended_on, &file)?;
    Ok(profile)
}

/// Claude Code's state file for a canonical root: `.claude.json` inside a root the shell's
/// `CLAUDE_CONFIG_DIR` selects, else `~/.claude.json` beside the default `~/.claude`, else
/// `.claude.json` inside the root, as another `CLAUDE_CONFIG_DIR` would place it.
fn claude_config(home: &str, canonical: &str, explicit: bool) -> String {
    let default = join(&[home, ".claude"]);
    if !explicit && realpath(&default).is_ok_and(|d| d == canonical) {
        join(&[home, ".claude.json"])
    } else {
        join(&[canonical, ".claude.json"])
    }
}

/// The tools, skills and MCP servers a profile's own settings file turns off, each as the
/// control the report's lines name for it. A Claude `permissions.deny` rule turns a tool off
/// only when it names the tool alone, without a pattern in parentheses.
fn switched_off(harness: Harness, user: &Map<String, Value>) -> Vec<Control> {
    let off = |path: &[&str]| Control {
        path: path.iter().map(|p| p.to_string()).collect(),
        value: ControlValue::Bool(false),
        append: false,
    };
    let mut result = vec![];
    if harness == Harness::Claude {
        for key in ["enableArtifact", "enableWorkflows", "syncClaudeAiSkills"] {
            if user.get(key) == Some(&Value::Bool(false)) {
                result.push(off(&[key]));
            }
        }
        let rules = user.get("permissions").and_then(|p| p.get("deny"));
        for rule in rules.and_then(Value::as_array).into_iter().flatten() {
            if let Some(rule) = rule.as_str().filter(|r| !r.is_empty() && !r.contains('(')) {
                result.push(Control {
                    path: vec!["permissions".into(), "deny".into()],
                    value: ControlValue::Text(rule.into()),
                    append: true,
                });
            }
        }
        for (name, value) in members(user.get("skillOverrides")) {
            if let Some(value) = value.as_str().filter(|v| *v != "on") {
                result.push(Control {
                    path: vec!["skillOverrides".into(), name.clone()],
                    value: ControlValue::Text(value.into()),
                    append: false,
                });
            }
        }
        return result;
    }
    if user.get("features").and_then(|f| f.get("apps")) == Some(&Value::Bool(false)) {
        result.push(off(&["features", "apps"]));
    }
    for (id, server) in members(user.get("mcp_servers")) {
        if server.get("enabled") == Some(&Value::Bool(false)) {
            result.push(off(&["mcp_servers", id, "enabled"]));
            continue;
        }
        let tools = server.get("disabled_tools").and_then(Value::as_array);
        for tool in tools.into_iter().flatten().filter_map(Value::as_str) {
            result.push(Control {
                path: vec!["mcp_servers".into(), id.clone(), "disabled_tools".into()],
                value: ControlValue::Text(tool.into()),
                append: true,
            });
        }
    }
    let skills = user.get("skills").and_then(|s| s.get("config"));
    for entry in skills.and_then(Value::as_array).into_iter().flatten() {
        if entry.get("enabled") == Some(&Value::Bool(false))
            && let Some(path) = entry.get("path").and_then(Value::as_str)
        {
            result.push(Control {
                path: vec!["skills".into(), "config".into()],
                value: ControlValue::Skill {
                    path: path.into(),
                    enabled: false,
                },
                append: true,
            });
        }
    }
    result
}

/// Each Codex MCP server's `enabled_tools` allowlist, as a control whose value lists the
/// tools it keeps, one per line.
fn allowlists(config: &Map<String, Value>) -> Vec<Control> {
    members(config.get("mcp_servers"))
        .iter()
        .filter_map(|(id, server)| {
            let tools = server.get("enabled_tools")?.as_array()?;
            let kept: Vec<&str> = tools.iter().filter_map(Value::as_str).collect();
            Some(Control {
                path: vec!["mcp_servers".into(), id.clone(), "enabled_tools".into()],
                value: ControlValue::Text(kept.join("\n")),
                append: false,
            })
        })
        .collect()
}

/// Discover configuration aliases and freeze every selected file's length before parsing.
pub fn discover(options: &DiscoveryOptions) -> Result<Discovery, ScanError> {
    let home = options.home.as_str();
    let env = &options.env;
    let explicit = !options.claude_dirs.is_empty()
        || !options.codex_dirs.is_empty()
        || !options.t3_dirs.is_empty();
    let mut candidates = IndexMap::default();
    let add = |candidates: &mut IndexMap<String, RootStatus>, harness, path: &str, origin| {
        add_root(candidates, explicit, harness, path, origin)
    };
    let defaults = [(Harness::Claude, ".claude"), (Harness::Codex, ".codex")];
    for (harness, name) in defaults {
        add(
            &mut candidates,
            harness,
            &join(&[home, name]),
            RootOrigin::Default,
        );
    }
    for (harness, variable) in [
        (Harness::Claude, "CLAUDE_CONFIG_DIR"),
        (Harness::Codex, "CODEX_HOME"),
    ] {
        if let Some(dir) = env.get(variable).filter(|d| !d.is_empty()) {
            add(&mut candidates, harness, dir, RootOrigin::Environment);
        }
    }
    let unreadable = || error("cannot discover home directory", home, 0);
    let mut entries = Vec::new();
    for entry in fs::read_dir(home).map_err(|_| unreadable())? {
        let entry = entry.map_err(|_| unreadable())?;
        let kind = entry.file_type().map_err(|_| unreadable())?;
        entries.push((entry.file_name().to_string_lossy().into_owned(), kind));
    }
    // Node lists directories in byte order.
    entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    for (name, kind) in entries {
        if !(kind.is_dir() || kind.is_symlink()) {
            continue;
        }
        let (harness, histories) = if name.starts_with(".claude") {
            (Harness::Claude, "projects")
        } else if name.starts_with(".codex") {
            (Harness::Codex, "sessions")
        } else {
            continue;
        };
        let path = join(&[home, &name]);
        if candidates.contains_key(&format!("{}{}", harness.as_str(), resolve(&path))) {
            continue;
        }
        let found = match fs::metadata(&path) {
            Ok(info) if !info.is_dir() => false,
            Ok(_) => match fs::metadata(join(&[&path, histories])) {
                Ok(_) => true,
                Err(e) => !absent(e.kind()),
            },
            Err(e) => !absent(e.kind()),
        };
        if found {
            add(&mut candidates, harness, &path, RootOrigin::Discovered);
        }
    }
    for path in &options.claude_dirs {
        add(&mut candidates, Harness::Claude, path, RootOrigin::Explicit);
    }
    for path in &options.codex_dirs {
        add(&mut candidates, Harness::Codex, path, RootOrigin::Explicit);
    }
    let mut roots: Vec<RootStatus> = candidates.into_values().collect();
    roots.sort_by(|a, b| locale_compare(&a.path, &b.path));
    let mut profiles: IndexMap<String, Profile> = IndexMap::default();
    for root in &mut roots {
        if root.status == RootState::Excluded {
            continue;
        }
        let state = |e: std::io::Error| {
            if e.kind() == ErrorKind::NotFound {
                RootState::Missing
            } else {
                RootState::Inaccessible
            }
        };
        let canonical =
            realpath(&root.path).map_err(state).and_then(|canonical| {
                match fs::metadata(&canonical) {
                    Ok(info) if info.is_dir() => Ok(canonical),
                    Ok(_) => Err(RootState::Inaccessible),
                    Err(e) => Err(state(e)),
                }
            });
        let canonical = match canonical {
            Ok(canonical) => canonical,
            Err(status) => {
                root.status = status;
                if root.origin == RootOrigin::Default && status == RootState::Missing {
                    continue;
                }
                return Err(error(
                    "selected root is missing or inaccessible",
                    &root.path,
                    0,
                ));
            }
        };
        let key = format!("{}{canonical}", root.harness.as_str());
        if let Some(previous) = profiles.get_mut(&key) {
            previous.aliases.push(root.path.clone());
            continue;
        }
        let profile = profile(options, root.harness, canonical, root.path.clone(), &key)?;
        profiles.insert(key, profile);
    }
    let mut profiles: Vec<Profile> = profiles.into_values().collect();
    let t3 = crate::t3::discover(options, &mut profiles)?;
    let mut walk = Walk::default();
    for (index, profile) in profiles.iter().enumerate() {
        for history in &profile.histories {
            walk.history(history, index, profile)?;
        }
    }
    Ok(Discovery {
        roots,
        profiles,
        snapshots: walk.files.into_values().collect(),
        t3,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{Workspace, make_request, native_path};
    use std::path::Path;

    #[test]
    fn a_settings_file_names_the_switches_it_turns_off() {
        let map = |v: Value| v.as_object().unwrap().clone();
        let names = |harness, v: Value| -> Vec<String> {
            switched_off(harness, &map(v))
                .iter()
                .map(crate::defaults::switch_name)
                .collect()
        };
        let claude = serde_json::json!({
            "enableArtifact": false,
            "enableWorkflows": true,
            "syncClaudeAiSkills": false,
            "permissions": {"deny": ["Monitor", "Bash(rm:*)", "mcp__github", ""]},
            "skillOverrides": {"pdf": "user-invocable-only", "docx": "on"},
        });
        assert_eq!(
            names(Harness::Claude, claude),
            [
                "Artifact tool",
                "Skills synced from claude.ai",
                "Monitor tool",
                "MCP server github",
                "Skill pdf"
            ]
        );
        let codex = serde_json::json!({
            "features": {"apps": false},
            "mcp_servers": {
                "docs": {"enabled": false, "disabled_tools": ["read"]},
                "live": {"command": "x", "disabled_tools": ["write"]},
            },
            "skills": {"config": [
                {"path": "/s/imagegen/SKILL.md", "enabled": false},
                {"path": "/s/other/SKILL.md", "enabled": true},
            ]},
        });
        assert_eq!(
            names(Harness::Codex, codex),
            [
                "ChatGPT apps",
                "MCP server docs",
                "MCP tool live.write",
                "Skill imagegen"
            ]
        );
    }

    fn canonical(path: &Path) -> String {
        realpath(&native_path(path)).unwrap()
    }

    fn write(path: impl AsRef<Path>, contents: &str) {
        let path = path.as_ref();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn options(w: &Workspace) -> DiscoveryOptions {
        DiscoveryOptions {
            home: native_path(&w.home),
            env: Env::default(),
            claude_dirs: vec![],
            codex_dirs: vec![],
            t3_dirs: vec![],
            managed_settings_file: native_path(w.managed_settings_file()),
            codex_system_config_file: native_path(w.home.join("system.toml")),
            codex_requirements_file: native_path(w.home.join("requirements.toml")),
            defaults: false,
            billing: IndexMap::default(),
        }
    }

    fn env(pairs: &[(&str, &str)]) -> Env {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn claude(discovery: &Discovery) -> &Profile {
        discovery
            .profiles
            .iter()
            .find(|p| p.harness == Harness::Claude)
            .unwrap()
    }

    fn request(profile: &Profile, cwd: Option<&Path>) -> Request {
        {
            let mut built = Request { ..make_request() };
            built.profiles = vec![profile.id.clone()].into();
            built.cwd = cwd.map(|c| native_path(c).into());
            built
        }
    }

    #[test]
    fn codex_memory_ancestors_stop_at_the_home() {
        let w = Workspace::new();
        let home = w.home.join("user");
        let cwd = home.join("repo/nested");
        for dir in [&w.home, &home, &home.join("repo"), &cwd] {
            std::fs::create_dir_all(dir.join(".codex")).unwrap();
            std::fs::write(
                dir.join(".codex/config.toml"),
                "[features]\nmemories = true\n",
            )
            .unwrap();
        }
        let file = |dir: &Path| native_path(dir.join(".codex/config.toml"));
        let o = options(&w);
        let found = |stop: &Path| {
            ancestors_setting_codex_memory(&o, &native_path(&cwd), &file(&home), &native_path(stop))
                .unwrap()
        };
        // The home's own is the user layer, and above the home is no one's project.
        assert_eq!(found(&home), [file(&cwd), file(&home.join("repo"))]);
        // Outside the home, the walk reaches the filesystem root, still skipping the user's.
        let outside = found(&cwd.join("elsewhere"));
        assert!(outside.contains(&file(&w.home)) && !outside.contains(&file(&home)));
        // A symlinked cwd outside the home walks its target's ancestors, not the link's.
        #[cfg(unix)]
        {
            let outside = w.home.join("outside");
            std::fs::create_dir_all(outside.join(".codex")).unwrap();
            std::fs::write(
                outside.join(".codex/config.toml"),
                "[features]\nmemories = true\n",
            )
            .unwrap();
            let link = outside.join("link");
            std::os::unix::fs::symlink(&cwd, &link).unwrap();
            let linked = ancestors_setting_codex_memory(
                &o,
                &native_path(&link),
                &file(&home),
                &native_path(&home),
            )
            .unwrap();
            assert_eq!(linked, [file(&cwd), file(&home.join("repo"))]);
        }
    }

    #[cfg(windows)]
    #[test]
    fn node_win32_path_semantics() {
        assert_eq!(join(&[r"C:\a\b", r"..\c", "d"]), r"C:\a\c\d");
        assert_eq!(join(&["a", "..", ".."]), "..");
        assert_eq!(join(&["", ""]), ".");
        assert_eq!(dirname(r"C:\a\b"), r"C:\a");
        assert_eq!(dirname(r"C:\a"), r"C:\");
        assert_eq!(dirname(r"C:\"), r"C:\");
        assert_eq!(dirname("a"), ".");
        assert!(!realpath(".").unwrap().starts_with(r"\\?\"));
    }

    #[cfg(not(windows))]
    #[test]
    fn node_path_semantics() {
        assert_eq!(join(&["/a/b/", "../c", "d"]), "/a/c/d");
        assert_eq!(join(&["a", "..", ".."]), "..");
        assert_eq!(join(&["", ""]), ".");
        assert_eq!(resolve("/a//b/./c/.."), "/a/b");
        assert_eq!(resolve("/.."), "/");
        assert_eq!(dirname("/a/b/"), "/a");
        assert_eq!(dirname("/a"), "/");
        assert_eq!(dirname("/"), "/");
        assert_eq!(dirname("//a"), "//");
        assert_eq!(dirname("a"), ".");
        assert_eq!(dirname(""), ".");
    }

    #[test]
    fn roots_sort_like_javascript_locale_compare() {
        let mut paths = vec![
            "/x/.claudeB",
            "/x/.claude/a",
            "/x/.claude1",
            "/x/.claude-x",
            "/x/.claudea",
            "/x/.claude.a",
            "/x/.claude_x",
            "/x/.claude",
        ];
        paths.sort_by(|a, b| locale_compare(a, b));
        assert_eq!(
            paths,
            [
                "/x/.claude",
                "/x/.claude_x",
                "/x/.claude-x",
                "/x/.claude.a",
                "/x/.claude/a",
                "/x/.claude1",
                "/x/.claudea",
                "/x/.claudeB",
            ]
        );
        let mut cases = vec!["A/B", "a/B", "A/b", "a/b"];
        cases.sort_by(|a, b| locale_compare(a, b));
        assert_eq!(cases, ["a/b", "a/B", "A/b", "A/B"]);
    }

    #[test]
    fn toml_values_take_javascript_shapes() {
        let w = Workspace::new();
        let file = w.home.join("config.toml");
        write(
            &file,
            "a = 1979-05-27\nb = nan\nc = 2.0\n[d]\ne = [1, 'x']\n",
        );
        let config = settings(&native_path(&file), Harness::Codex).unwrap();
        assert_eq!(config["a"], serde_json::json!({}));
        assert_eq!(config["b"], Value::Null);
        assert_eq!(config["c"], serde_json::json!(2.0));
        assert_eq!(config["d"], serde_json::json!({ "e": [1, "x"] }));
        write(&file, "a = 9007199254740993\n");
        let error = settings(&native_path(&file), Harness::Codex).unwrap_err();
        assert_eq!(error.message, "invalid settings file");
        write(&file, "a = [");
        assert!(settings(&native_path(&file), Harness::Codex).is_err());
        assert!(
            settings(&native_path(w.home.join("absent.toml")), Harness::Codex)
                .unwrap()
                .is_empty()
        );
        let error = settings(&native_path(&w.claude), Harness::Claude).unwrap_err();
        assert_eq!(error.message, "cannot read settings");
        let json = w.home.join("settings.json");
        write(&json, "[]");
        let error = settings(&native_path(&json), Harness::Claude).unwrap_err();
        assert_eq!(error.message, "invalid settings file");
        assert_eq!(error.location.unwrap().line, 1);
    }

    #[test]
    fn structural_settings_keep_only_whitelisted_metadata() {
        let config = serde_json::json!({
            "model": "claude-opus-5-5",
            "fastMode": true,
            "effort": "free text",
            "model_context_window": 400000,
            "privateText": "PRIVATE",
        });
        let settings = structural_settings(crate::safe::object(&config));
        assert_eq!(
            settings.keys().collect::<Vec<_>>(),
            ["fastMode", "model", "model_context_window"]
        );
        assert_eq!(settings["model"], Scalar::Text("claude-opus-5-5".into()));
    }

    #[test]
    fn claude_state_lives_beside_the_default_root_unless_the_shell_selects_it() {
        let w = Workspace::new();
        let home = native_path(&w.home);
        let state = |env: Env| {
            let data = discover(&DiscoveryOptions { env, ..options(&w) }).unwrap();
            claude(&data).claude_config.clone().unwrap()
        };
        assert_eq!(state(Env::default()), join(&[&home, ".claude.json"]));
        let root = realpath(&native_path(&w.claude)).unwrap();
        assert_eq!(
            state(env(&[("CLAUDE_CONFIG_DIR", &native_path(&w.claude))])),
            join(&[&root, ".claude.json"])
        );
    }

    // Current environment overrides are scoped to the active config root.
    #[test]
    fn environment_overrides_are_scoped_to_the_active_root() {
        let w = Workspace::new();
        let other = w.home.join(".claude-other");
        fs::create_dir_all(other.join("projects")).unwrap();
        write(
            other.join("settings.json"),
            r#"{"promptCacheTtl":"1h","subagentPromptCacheTtl":"1h"}"#,
        );
        let data = discover(&DiscoveryOptions {
            env: env(&[
                ("CLAUDE_CONFIG_DIR", &native_path(&w.claude)),
                ("FORCE_PROMPT_CACHING_5M", "1"),
                ("CLAUDE_CODE_PROMPT_CACHE_TTL", "1h"),
            ]),
            ..options(&w)
        })
        .unwrap();
        let root = |path: &Path| {
            let path = canonical(path);
            data.profiles.iter().find(|p| p.root == path).unwrap()
        };
        assert_eq!(root(&w.claude).ttl.main.value, Some(Ttl::FiveMinutes));
        assert_eq!(root(&w.claude).ttl.main.owner, "FORCE_PROMPT_CACHING_5M");
        assert_eq!(root(&other).ttl.main.value, Some(Ttl::OneHour));
        assert_eq!(root(&other).ttl.main.reason, "current user/managed setting");
    }

    #[test]
    fn defaults_pretend_user_files_and_setting_variables_away_but_keep_policy() {
        let w = Workspace::new();
        write(
            w.claude.join("settings.json"),
            r#"{"promptCacheTtl":"5m","permissions":{"deny":["WebFetch","WebSearch"]}}"#,
        );
        write(
            w.codex.join("config.toml"),
            "service_tier = 'fast'\n[mcp_servers.docs]\ncommand = 'docs'\nenabled = false\n[mcp_servers.kit]\ncommand = 'kit'\nenabled_tools = ['a']\n",
        );
        // The system allowlist keeps tool b of kit off whatever the user's files say.
        write(
            w.home.join("system.toml"),
            "[mcp_servers.kit]\nenabled_tools = ['a']\n",
        );
        // A --profile selection would leave speed unresolved; a default user makes none.
        write(w.codex.join("work.config.toml"), "service_tier = 'fast'\n");
        write(
            w.managed_settings_file(),
            r#"{"subagentPromptCacheTtl":"1h","permissions":{"deny":["WebSearch"]}}"#,
        );
        let mut o = options(&w);
        o.env = env(&[
            ("CLAUDE_CONFIG_DIR", &native_path(&w.claude)),
            ("CLAUDE_CODE_PROMPT_CACHE_TTL", "5m"),
        ]);
        o.pretend_defaults();
        assert_eq!(
            o.env.keys().collect::<Vec<_>>(),
            [&"CLAUDE_CONFIG_DIR".to_string()]
        );
        let data = discover(&o).unwrap();
        let claude = claude(&data);
        assert_eq!(claude.ttl.main.value, None);
        // Managed policy still sets what it sets.
        assert_eq!(claude.ttl.subagent.value, Some(Ttl::OneHour));
        assert!(claude.switched_off.is_empty());
        assert_eq!(
            claude
                .pretended_on
                .iter()
                .map(|(_, k)| crate::defaults::switch_name(k))
                .collect::<Vec<_>>(),
            ["WebFetch tool"]
        );
        let codex = data
            .profiles
            .iter()
            .find(|p| p.harness == Harness::Codex)
            .unwrap();
        // The servers stay declared, and on, with every tool.
        assert_eq!(codex.mcp_servers, ["docs", "kit"]);
        assert!(codex.switched_off.is_empty());
        let tool = |name: &str| Control {
            path: vec!["mcp_servers".into(), "kit".into(), "disabled_tools".into()],
            value: ControlValue::Text(name.into()),
            append: true,
        };
        let policy = |t| {
            codex
                .policy_off
                .iter()
                .any(|(_, k)| crate::defaults::covers(k, &tool(t)))
        };
        assert_eq!((policy("a"), policy("b")), (false, true));
        let on: Vec<&[String]> = codex
            .pretended_on
            .iter()
            .map(|(_, k)| k.path.as_slice())
            .collect();
        assert_eq!(
            on,
            [
                ["mcp_servers", "docs", "enabled"],
                ["mcp_servers", "kit", "enabled_tools"]
            ]
        );
        assert_eq!(
            codex.speed.as_ref().unwrap().value.as_deref(),
            Some("standard")
        );
        // Where requests went decides how they are priced, from the shell or the file.
        for (shell, file, provider) in [
            (Some("CLAUDE_CODE_USE_BEDROCK"), "{}", "amazon-bedrock"),
            (
                None,
                r#"{"env":{"CLAUDE_CODE_USE_VERTEX":"1","CLAUDE_CODE_PROMPT_CACHE_TTL":"1h"}}"#,
                "google-vertex-anthropic",
            ),
        ] {
            write(w.claude.join("settings.json"), file);
            let mut o = options(&w);
            o.env = env(&[("CLAUDE_CONFIG_DIR", &native_path(&w.claude))]);
            if let Some(variable) = shell {
                o.env.insert(variable.into(), "1".into());
            }
            o.pretend_defaults();
            let data = discover(&o).unwrap();
            let profile = super::tests::claude(&data);
            assert_eq!(profile.provider.as_deref(), Some(provider));
            assert_eq!(profile.ttl.main.value, Some(Ttl::FiveMinutes), "{provider}");
        }
    }

    #[test]
    fn defaults_restore_project_named_profile_and_mcp_state_switches() {
        let w = Workspace::new();
        let repo = w.home.join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        write(
            repo.join(".claude/settings.json"),
            r#"{"permissions":{"deny":["WebFetch"]}}"#,
        );
        // A project layer that turns a server back on sets nothing a default user has.
        write(
            repo.join(".codex/config.toml"),
            "[mcp_servers.docs]\ncommand = 'docs'\nenabled = true\n",
        );
        write(
            w.codex.join("config.toml"),
            "[mcp_servers.docs]\ncommand = 'docs'\n",
        );
        write(
            w.codex.join("work.config.toml"),
            "[mcp_servers.docs]\nenabled = false\n[mcp_servers.lint]\ncommand = 'lint'\nenabled = false\n",
        );
        let state = claude(&discover(&options(&w)).unwrap())
            .claude_config
            .clone()
            .unwrap();
        // /mcp turned two servers off in the repo; policy denies one's tools by wildcard.
        write(
            &state,
            &serde_json::json!({"projects": {native_path(&repo): {"disabledMcpServers": ["notes", "blocked"]}}})
                .to_string(),
        );
        write(
            w.managed_settings_file(),
            r#"{"permissions":{"deny":["mcp__blocked__*"]}}"#,
        );
        let mut o = options(&w);
        o.pretend_defaults();
        let restored = |t3: bool| {
            let mut d = discover(&o).unwrap();
            for p in &mut d.profiles {
                p.t3 = t3.then(|| crate::model::T3Instance {
                    state: String::new(),
                    instance: "synthetic".into(),
                    enabled: true,
                    idle_mode: crate::model::T3IdleMode::Off,
                    idle_owner: String::new(),
                    settings_unresolved: false,
                    limitations: vec![],
                    env: None,
                    inherited_window: true,
                    environment: vec![],
                    retained_since: None,
                });
            }
            let requests: Vec<Request> =
                d.profiles.iter().map(|p| request(p, Some(&repo))).collect();
            discover_claude_controls(&o, &mut d.profiles, &requests).unwrap();
            discover_codex_controls(&o, &mut d.profiles, &requests).unwrap();
            let names = |harness| {
                let p = d.profiles.iter().find(|p| p.harness == harness).unwrap();
                p.pretended_on
                    .iter()
                    .map(|(_, k)| crate::defaults::switch_name(k))
                    .collect::<Vec<_>>()
            };
            let codex = d
                .profiles
                .iter()
                .find(|p| p.harness == Harness::Codex)
                .unwrap();
            (
                names(Harness::Claude),
                names(Harness::Codex),
                codex.outranking_files.clone(),
            )
        };
        for t3 in [false, true] {
            let (claude, codex, outranking) = restored(t3);
            assert_eq!(claude, ["WebFetch tool", "MCP server notes"], "t3 {t3}");
            // Only the server the main file declares is a default user's.
            assert_eq!(codex, ["MCP server docs"], "t3 {t3}");
            assert!(outranking.is_empty(), "{outranking:?}");
        }
    }

    // Managed TTL controls retain the managed owner and API-key presence leaves billing unknown.
    #[test]
    fn managed_ttl_controls_keep_the_managed_owner() {
        let w = Workspace::new();
        let managed = native_path(w.managed_settings_file());
        write(w.claude.join("settings.json"), r#"{"promptCacheTtl":"5m"}"#);
        write(&managed, r#"{"promptCacheTtl":"1h"}"#);
        let profile = claude(&discover(&options(&w)).unwrap()).clone();
        assert_eq!(profile.ttl.main.value, Some(Ttl::OneHour));
        assert_eq!(profile.ttl.main.owner, format!("{managed}:promptCacheTtl"));
        write(&managed, r#"{"env":{"FORCE_PROMPT_CACHING_5M":"1"}}"#);
        let profile = claude(&discover(&options(&w)).unwrap()).clone();
        assert_eq!(profile.ttl.main.value, Some(Ttl::FiveMinutes));
        assert_eq!(
            profile.ttl.main.owner,
            format!("{managed}:env.FORCE_PROMPT_CACHING_5M")
        );
        write(&managed, "{}");
        write(w.claude.join("settings.json"), "{}");
        let data = discover(&DiscoveryOptions {
            env: env(&[("ANTHROPIC_API_KEY", "SYNTHETIC_TEST_KEY")]),
            ..options(&w)
        })
        .unwrap();
        assert_eq!(claude(&data).ttl.main.value, None);
        assert_eq!(claude(&data).ttl.subagent.value, Some(Ttl::FiveMinutes));
    }

    // Automatic discovery ignores a symlinked Claude config file.
    #[cfg(unix)]
    #[test]
    fn symlinked_config_files_are_not_profiles() {
        let w = Workspace::new();
        let file = w.home.join("account.json");
        write(&file, "{}");
        std::os::unix::fs::symlink(&file, w.home.join(".claude.json")).unwrap();
        assert_eq!(discover(&options(&w)).unwrap().profiles.len(), 2);
        let error = discover(&DiscoveryOptions {
            claude_dirs: vec![native_path(&file)],
            ..options(&w)
        })
        .unwrap_err();
        assert_eq!(error.message, "selected root is missing or inaccessible");
    }

    // Managed file locations match the supported host platforms.
    #[test]
    fn managed_file_locations_match_platforms() {
        assert_eq!(
            managed_settings_path("macos"),
            "/Library/Application Support/ClaudeCode/managed-settings.json"
        );
        assert_eq!(
            managed_settings_path("linux"),
            "/etc/claude-code/managed-settings.json"
        );
        assert_eq!(
            managed_settings_path("windows"),
            "C:\\Program Files\\ClaudeCode\\managed-settings.json"
        );
    }

    // Shared directories are read once, preserve profile aliases, and include nested subagents.
    #[cfg(unix)]
    #[test]
    fn shared_directories_are_snapshotted_once_and_explicit_roots_exclude_others() {
        let w = Workspace::new();
        let alternate = w.home.join(".claude-work");
        fs::create_dir_all(&alternate).unwrap();
        std::os::unix::fs::symlink(w.claude.join("projects"), alternate.join("projects")).unwrap();
        write(
            alternate.join("settings.json"),
            r#"{"promptCacheTtl":"1h"}"#,
        );
        w.fixture(w.claude.join("projects/project/main.jsonl"), "claude.jsonl");
        write(
            w.claude.join("projects/project/main/subagents/agent.jsonl"),
            "{}\n",
        );
        let data = discover(&options(&w)).unwrap();
        assert_eq!(data.snapshots.len(), 2);
        assert!(data.snapshots.iter().all(|s| s.profiles.len() == 2));
        let selected = discover(&DiscoveryOptions {
            claude_dirs: vec![native_path(&alternate)],
            ..options(&w)
        })
        .unwrap();
        assert_eq!(selected.profiles.len(), 1);
        assert!(
            selected
                .roots
                .iter()
                .any(|r| r.path == native_path(&w.claude)
                    && r.status == RootState::Excluded
                    && r.origin == RootOrigin::Default)
        );
        assert_eq!(selected.profiles[0].ttl.main.value, Some(Ttl::OneHour));
    }

    // All lengths freeze before reading, and a shrunk snapshot fails.
    #[test]
    fn lengths_freeze_at_discovery_and_missing_explicit_roots_fail() {
        let w = Workspace::new();
        let file = w.claude.join("projects/project/active.jsonl");
        write(
            &file,
            "{\"type\":\"custom-title\"}\n{\"private\":\"SYNTHETIC",
        );
        let data = discover(&options(&w)).unwrap();
        let snapshot = &data.snapshots[0];
        let size = fs::metadata(&file).unwrap().len();
        assert_eq!(snapshot.bytes, size);
        assert_eq!((snapshot.lines, snapshot.deferred_bytes), (0, 0));
        assert_eq!(snapshot.file, canonical(&file));
        fs::write(&file, "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n").unwrap();
        assert_eq!(snapshot.bytes, size);
        let absent = w.home.join("absent");
        let error = discover(&DiscoveryOptions {
            claude_dirs: vec![native_path(&absent)],
            ..options(&w)
        })
        .unwrap_err();
        assert_eq!(error.message, "selected root is missing or inaccessible");
        assert_eq!(&*error.location.unwrap().file, native_path(&absent));
    }

    #[cfg(unix)]
    #[test]
    fn walks_skip_loops_dedupe_links_and_sort_by_utf16() {
        let w = Workspace::new();
        let project = w.claude.join("projects/project");
        for name in [
            "b.jsonl",
            "a.jsonl",
            "\u{ff21}.jsonl",
            "\u{1f600}.jsonl",
            "notes.txt",
        ] {
            write(project.join(name), "{}\n");
        }
        write(project.join("sub/c.jsonl"), "{}\n");
        std::os::unix::fs::symlink(project.join("a.jsonl"), project.join("link.jsonl")).unwrap();
        fs::hard_link(project.join("b.jsonl"), project.join("hard.jsonl")).unwrap();
        std::os::unix::fs::symlink("..", project.join("sub/loop")).unwrap();
        let data = discover(&options(&w)).unwrap();
        let names: Vec<&str> = data
            .snapshots
            .iter()
            .map(|s| s.file.rsplit('/').next().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "a.jsonl",
                "b.jsonl",
                "c.jsonl",
                "\u{1f600}.jsonl",
                "\u{ff21}.jsonl"
            ]
        );
        std::os::unix::fs::symlink(project.join("absent"), project.join("dangling.jsonl")).unwrap();
        let error = discover(&options(&w)).unwrap_err();
        assert_eq!(error.message, "cannot snapshot selected history");
    }

    // Discovery retains only whitelisted compaction controls and malformed selected limits fail.
    #[test]
    fn discovery_keeps_whitelisted_compaction_controls_and_rejects_malformed_limits() {
        let w = Workspace::new();
        write(
            w.claude.join("settings.json"),
            r#"{"modelSettings":{"claude-opus-5-5":{"autoCompactWindow":400000,"privateText":"PRIVATE"}}}"#,
        );
        write(
            w.codex.join("config.toml"),
            "model_auto_compact_token_limit = 240000\nmodel_auto_compact_token_limit_scope = 'body_after_prefix'\n",
        );
        let data = discover(&options(&w)).unwrap();
        assert_eq!(
            crate::controls::window_control(&claude(&data).compaction, Some("claude-opus-5-5"))
                .value,
            Some(400_000)
        );
        let codex = data
            .profiles
            .iter()
            .find(|p| p.harness == Harness::Codex)
            .unwrap();
        assert_eq!(codex.compaction.default.value, Some(240_000));
        assert!(!codex.compaction.default.applicable);
        assert!(!format!("{:?}", data.profiles).contains("PRIVATE"));
        write(
            w.codex.join("config.toml"),
            "model_auto_compact_token_limit = 'bad'\n",
        );
        let error = discover(&options(&w)).unwrap_err();
        assert_eq!(error.message, "invalid compaction token limit");
        write(w.codex.join("config.toml"), "");
        write(
            w.claude.join("settings.json"),
            r#"{"modelSettings":{"opus[1m]":null}}"#,
        );
        let error = discover(&options(&w)).unwrap_err();
        assert_eq!(error.message, "invalid per-model settings");
    }

    #[test]
    fn a_turn_end_threshold_another_file_can_set_is_unresolved() {
        let w = Workspace::new();
        let config = w.codex.join("config.toml");
        let key = TURN_END_KEY;
        let resolve = || {
            let mut d = discover(&options(&w)).unwrap();
            let codex = |d: &Discovery| {
                d.profiles
                    .iter()
                    .find(|p| p.harness == Harness::Codex)
                    .unwrap()
                    .clone()
            };
            let r = {
                let mut built = request(&codex(&d), Some(&w.home.join("repo")));
                built.harness = Harness::Codex;
                built
            };
            discover_codex_controls(&options(&w), &mut d.profiles, std::slice::from_ref(&r))
                .unwrap();
            codex(&d).turn_end.unwrap()
        };
        let unset = resolve();
        assert_eq!(
            (unset.percent, unset.unset, &unset.unresolved),
            (0, true, &None)
        );
        write(&config, &format!("{key} = 60\n"));
        let set = resolve();
        assert_eq!(
            (set.percent, set.unset, &set.unresolved),
            (60, false, &None)
        );
        assert_eq!(set.owner, format!("{}:{key}", canonical(&config)));

        // A table of profiles, a named profile's file and a project's config can each set it.
        write(
            &config,
            &format!("{key} = 60\n[profiles.work]\n{key} = 80\n"),
        );
        assert!(resolve().unresolved.unwrap().contains(":profiles.work"));
        write(&config, &format!("{key} = 60\n"));
        let named = w.codex.join("work.config.toml");
        write(&named, &format!("{key} = 80\n"));
        assert!(resolve().unresolved.unwrap().contains("work.config.toml"));
        fs::remove_file(&named).unwrap();
        let project = w.home.join("repo/.codex/config.toml");
        fs::create_dir_all(w.home.join("repo/.git")).unwrap();
        write(&project, &format!("{key} = 80\n"));
        let suffix = Path::new("repo").join(".codex").join("config.toml");
        assert!(
            resolve()
                .unresolved
                .unwrap()
                .contains(&*suffix.to_string_lossy())
        );
        fs::remove_file(&project).unwrap();
        assert_eq!(resolve().unresolved, None);

        write(&config, &format!("{key} = 101\n"));
        let error = discover(&options(&w)).unwrap_err();
        assert_eq!(error.message, "invalid turn-end compaction threshold");
    }

    // Active-profile environment, managed policy and current project ownership suppress stale preferences.
    #[test]
    fn environment_managed_policy_and_project_ownership_suppress_fast_preferences() {
        let w = Workspace::new();
        let managed = native_path(w.managed_settings_file());
        write(
            w.claude.join("settings.json"),
            r#"{"model":"opus","fastMode":true}"#,
        );
        let shell = discover(&DiscoveryOptions {
            env: env(&[
                ("CLAUDE_CONFIG_DIR", &native_path(&w.claude)),
                ("CLAUDE_CODE_DISABLE_FAST_MODE", "1"),
            ]),
            ..options(&w)
        })
        .unwrap();
        let speed = claude(&shell).speed.as_ref().unwrap();
        assert_eq!(speed.value.as_deref(), Some("standard"));
        assert_eq!(speed.owner, "CLAUDE_CODE_DISABLE_FAST_MODE");
        write(&managed, r#"{"fastMode":false}"#);
        let data = discover(&options(&w)).unwrap();
        assert_eq!(
            claude(&data).speed.as_ref().unwrap().owner,
            format!("{managed}:fastMode")
        );
        write(&managed, "{}");
        fs::create_dir_all(w.home.join(".git")).unwrap();
        let cwd = w.home.join(".claude-project");
        fs::create_dir_all(cwd.join(".claude")).unwrap();
        // A repository rooted at home keeps the local file at the session directory.
        let local = cwd.join(".claude/settings.local.json");
        write(&local, r#"{"fastMode":false}"#);
        let mut data = discover(&options(&w)).unwrap();
        let profile = claude(&data).clone();
        assert_eq!(
            profile.speed.as_ref().unwrap().value.as_deref(),
            Some("fast")
        );
        let requests = [request(&profile, Some(&cwd))];
        discover_claude_controls(&options(&w), &mut data.profiles, &requests).unwrap();
        let directory = &claude(&data).speed_directories.as_ref().unwrap()[&native_path(&cwd)];
        assert_eq!(directory.value.as_deref(), Some("standard"));
        assert_eq!(directory.owner, format!("{}:fastMode", native_path(&local)));
    }

    // Codex legacy/named profiles and project selection stay advisory; local requirements can block premium.
    #[test]
    fn codex_named_profiles_stay_advisory_and_requirements_block_premium() {
        let w = Workspace::new();
        let config = w.codex.join("config.toml");
        let requirements = w.home.join("requirements.toml");
        let options = DiscoveryOptions {
            codex_dirs: vec![native_path(&w.codex)],
            ..options(&w)
        };
        let speed = || {
            discover(&options).unwrap().profiles[0]
                .speed
                .clone()
                .unwrap()
        };
        write(
            &config,
            "model=\"gpt-6.1-sol\"\nservice_tier=\"fast\"\n[profiles.special]\nservice_tier=\"default\"",
        );
        assert!(!speed().resolved);
        write(&config, "model=\"gpt-6.1-sol\"\nservice_tier=\"fast\"");
        let named = w.codex.join("special.config.toml");
        write(&named, "service_tier=\"default\"");
        let advisory = speed();
        assert!(advisory.reason.contains("--profile selection"));
        assert_eq!(
            advisory.owner,
            format!(
                "{}; {} (--profile selection unresolved)",
                canonical(&config),
                canonical(&named)
            )
        );
        fs::remove_file(&named).unwrap();
        write(&requirements, "[features]\nfast_mode=false");
        let blocked = speed();
        assert_eq!(blocked.value.as_deref(), Some("standard"));
        assert_eq!(
            blocked.owner,
            format!("{}:features", native_path(&requirements))
        );
        write(&config, "model=\"gpt-6.1-sol\"\nservice_tier=\"ultrafast\"");
        write(&requirements, "[features]\nultrafast_mode=false");
        assert_eq!(speed().value.as_deref(), Some("standard"));
        write(&requirements, "[features]\nultrafast_mode=\"no\"");
        let error = discover(&options).unwrap_err();
        assert_eq!(error.message, "invalid required speed feature");
    }

    // Known distinct project controls get separate cards and nearest Codex scope remains advisory owner.
    #[test]
    fn nearest_codex_project_scope_owns_speed() {
        let w = Workspace::new();
        let root = w.home.join("repo");
        let nested = root.join("nested");
        fs::create_dir_all(root.join(".git")).unwrap();
        write(root.join(".codex/config.toml"), "service_tier=\"fast\"");
        let nearest = nested.join(".codex/config.toml");
        write(&nearest, "service_tier=\"default\"");
        let options = DiscoveryOptions {
            codex_dirs: vec![native_path(&w.codex)],
            ..options(&w)
        };
        let mut data = discover(&options).unwrap();
        let profile = data.profiles[0].clone();
        let requests = [
            {
                let mut built = Request {
                    ..request(&profile, Some(&nested))
                };
                built.harness = Harness::Codex;
                built
            },
            request(&profile, None),
        ];
        discover_codex_controls(&options, &mut data.profiles, &requests).unwrap();
        let directories = data.profiles[0].speed_directories.as_ref().unwrap();
        assert_eq!(directories.len(), 1);
        let directory = &directories[&native_path(&nested)];
        assert_eq!(directory.owner, native_path(&nearest));
        assert_eq!(directory.value, None);
        assert_eq!(directory.replacement, profile.speed.unwrap().replacement);
    }

    #[test]
    fn billing_asks_only_for_a_root_with_a_transcript() {
        let w = Workspace::new();
        // Workspace leaves `projects` empty, so there is nothing to ask about.
        assert!(discover(&options(&w)).unwrap().billing_roots().is_empty());
        write(w.claude.join("projects/project/session.jsonl"), "");
        assert_eq!(
            discover(&options(&w)).unwrap().billing_roots(),
            vec![canonical(&w.claude)]
        );
    }

    // A custom ANTHROPIC_BASE_URL proxy is priced as the public Anthropic API.
    #[test]
    fn an_answered_billing_sets_only_an_unset_main_ttl_on_the_anthropic_api() {
        let w = Workspace::new();
        let answered = |settings: &str, billing| {
            write(w.claude.join("settings.json"), settings);
            let mut o = options(&w);
            o.billing.insert(canonical(&w.claude), billing);
            claude(&discover(&o).unwrap()).clone()
        };
        let p = answered("{}", Billing::Plan);
        assert_eq!(p.billing, Some(Billing::Plan));
        assert_eq!(p.ttl.main.value, Some(Ttl::OneHour));
        assert_eq!(p.ttl.main.reason, TTL_PLAN_DEFAULT);
        assert_eq!(p.ttl.subagent.value, Some(Ttl::FiveMinutes));
        let p = answered("{}", Billing::Api);
        assert_eq!(p.ttl.main.value, Some(Ttl::FiveMinutes));
        assert_eq!(p.ttl.main.reason, TTL_API_DEFAULT);
        // A setting outranks the default, and the answer still picks the default compared.
        let p = answered(r#"{"promptCacheTtl":"5m"}"#, Billing::Plan);
        assert_eq!(p.ttl.main.value, Some(Ttl::FiveMinutes));
        assert_eq!(p.ttl.main.reason, "current user/managed setting");
        assert_eq!(p.billing, Some(Billing::Plan));
        // A cloud provider bills per token, whatever was answered.
        let p = answered(r#"{"env":{"CLAUDE_CODE_USE_BEDROCK":"1"}}"#, Billing::Plan);
        assert_eq!(p.billing, None);
        assert_eq!(p.ttl.main.reason, "explicit cloud-provider billing default");
        // An answer for another root changes nothing.
        write(w.claude.join("settings.json"), "{}");
        let mut o = options(&w);
        o.billing.insert("/elsewhere".into(), Billing::Plan);
        let p = claude(&discover(&o).unwrap()).clone();
        assert_eq!((p.billing, p.ttl.main.value), (None, None));
        assert_eq!(p.ttl.main.reason, TTL_BILLING_UNKNOWN);
    }

    #[test]
    fn custom_base_urls_keep_the_public_provider_while_cloud_flags_set_one() {
        let w = Workspace::new();
        write(
            w.claude.join("settings.json"),
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://proxy.invalid"}}"#,
        );
        assert_eq!(claude(&discover(&options(&w)).unwrap()).provider, None);
        write(
            w.claude.join("settings.json"),
            r#"{"env":{"CLAUDE_CODE_USE_VERTEX":"1"}}"#,
        );
        assert_eq!(
            claude(&discover(&options(&w)).unwrap()).provider.as_deref(),
            Some("google-vertex-anthropic")
        );
        let mut o = options(&w);
        o.env.insert("CLAUDE_CODE_USE_VERTEX".into(), "0".into());
        let d = discover(&o).unwrap();
        let p = claude(&d);
        assert_eq!(p.provider.as_deref(), Some("google-vertex-anthropic"));
        assert_eq!(p.background[0].value, Some(false));
    }

    #[test]
    fn claude_background_uses_session_project_and_root_local_env_with_managed_precedence() {
        let w = Workspace::new();
        let root = w.home.join("repo");
        let nested = root.join("nested");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(&nested).unwrap();
        write(
            w.claude.join("settings.json"),
            r#"{"promptSuggestionEnabled":false,"env":{"CLAUDE_CODE_ENABLE_AWAY_SUMMARY":"0"}}"#,
        );
        // Shared project settings come from the session directory, not the git root.
        write(
            root.join(".claude/settings.json"),
            r#"{"promptSuggestionEnabled":false,"awaySummaryEnabled":false}"#,
        );
        let project = nested.join(".claude/settings.json");
        write(
            &project,
            r#"{"promptSuggestionEnabled":true,"env":{"CLAUDE_CODE_ENABLE_AWAY_SUMMARY":"1"},"secret":"SYNTHETIC_PRIVATE"}"#,
        );
        write(
            nested.join(".claude/settings.local.json"),
            r#"{"env":{"CLAUDE_CODE_GOAL_CHECKIN_MINUTES":"3"}}"#,
        );
        let local = root.join(".claude/settings.local.json");
        write(
            &local,
            r#"{"env":{"CLAUDE_CODE_GOAL_CHECKIN_MINUTES":"7","SECRET":"SYNTHETIC_PRIVATE"}}"#,
        );
        let mut o = options(&w);
        o.env
            .insert("CLAUDE_CODE_GOAL_CHECKIN_MINUTES".into(), "9".into());
        let mut d = discover(&o).unwrap();
        let r = {
            let mut built = Request {
                ..request(claude(&d), Some(&nested))
            };
            built.client = Some("cli".into());
            built
        };
        discover_claude_controls(
            &o,
            &mut d.profiles,
            &[r.clone(), {
                let mut built = Request { ..r.clone() };
                built.cwd = None;
                built
            }],
        )
        .unwrap();
        let p = claude(&d);
        let c = &p.background_directories[&native_path(&nested)];
        assert_eq!(c[0].value, Some(true));
        assert_eq!(
            c[0].owner,
            format!("{}:promptSuggestionEnabled", native_path(&project))
        );
        assert_eq!(c[1].value, Some(true));
        assert_eq!(
            c[1].off_file.as_deref(),
            Some(native_path(&project).as_str())
        );
        assert_eq!(
            c[1].off,
            "\"env\": {\"CLAUDE_CODE_ENABLE_AWAY_SUMMARY\": \"0\"}"
        );
        #[cfg(unix)]
        assert_eq!(
            (c[2].value, c[2].off_file.as_deref()),
            (Some(true), Some(native_path(&local).as_str()))
        );
        #[cfg(windows)]
        assert_eq!(c[2].value, Some(true));
        assert!(
            p.background_directories[""]
                .iter()
                .all(|c| c.value.is_none())
        );
        assert!(!format!("{p:?}").contains("SYNTHETIC_PRIVATE"));
        write(
            w.managed_settings_file(),
            r#"{"env":{"CLAUDE_CODE_ENABLE_AWAY_SUMMARY":"0","CLAUDE_CODE_GOAL_CHECKIN_MINUTES":"0"}}"#,
        );
        discover_claude_controls(&o, &mut d.profiles, std::slice::from_ref(&r)).unwrap();
        let c = &claude(&d).background_directories[&native_path(&nested)];
        assert_eq!((c[1].value, c[2].value), (Some(false), Some(false)));
        assert_eq!(
            c[2].off_file.as_deref(),
            Some(native_path(w.managed_settings_file()).as_str())
        );
        let hosted = {
            let mut built = Request { ..r.clone() };
            built.client = Some("desktop".into());
            built
        };
        discover_claude_controls(&o, &mut d.profiles, std::slice::from_ref(&hosted)).unwrap();
        let c = &claude(&d).background_requests[&*hosted.id];
        assert!(c.iter().all(|c| c.value.is_none()));
        assert!(c[0].reason.contains("launch precedence"));
        assert_eq!(c[0].scope.as_deref(), Some("launch environment conflict"));
    }

    #[test]
    fn claude_session_uncertainty_does_not_change_other_requests_at_the_same_directory() {
        let w = Workspace::new();
        let a = w.home.join("a");
        let b = w.home.join("b");
        fs::create_dir_all(a.join(".git")).unwrap();
        fs::create_dir_all(b.join(".git")).unwrap();
        write(
            w.claude.join("settings.json"),
            r#"{"env":{"CLAUDE_CODE_ENABLE_AWAY_SUMMARY":"0"}}"#,
        );
        let mut o = options(&w);
        o.env
            .insert("CLAUDE_CODE_ENABLE_AWAY_SUMMARY".into(), "1".into());
        let mut d = discover(&o).unwrap();
        let p = claude(&d);
        let requests: Vec<Request> = [
            ("stable", &b, Some("cli")),
            ("moving", &a, Some("cli")),
            ("moving", &b, Some("cli")),
            ("moving", &b, Some("cli")),
            ("moving", &a, Some("cli")),
            ("older", &b, None),
            ("hosted", &b, Some("desktop")),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, (session, cwd, client))| {
            let mut built = Request {
                id: format!("r{i}").into(),
                time: i as i64,
                ..request(p, Some(cwd))
            };
            built.session = session.into();
            built.client = client.map(crate::model::Text::from);
            built
        })
        .collect();
        discover_claude_controls(&o, &mut d.profiles, &requests).unwrap();
        let p = claude(&d);
        assert_eq!(
            p.background_directories[&native_path(&b)][1].value,
            Some(false)
        );
        assert!(!p.background_requests.contains_key("r0"));
        assert!(!p.background_requests.contains_key("r1"));
        for i in 2..7 {
            assert_eq!(
                p.background_requests[format!("r{i}").as_str()][1].value,
                None
            );
        }
        assert!(p.background_requests["r4"][1].reason.contains("carry-over"));
        assert_eq!(
            p.background_requests["r4"][1].scope.as_deref(),
            Some("working directory changed")
        );
        // A resolved control has no scope to name.
        assert_eq!(p.background_directories[&native_path(&b)][1].scope, None);
        assert!(
            p.background_requests["r5"][1]
                .reason
                .contains("recorded client")
        );
    }

    #[cfg(unix)]
    #[test]
    fn claude_home_repository_exception_compares_canonical_paths() {
        let w = Workspace::new();
        let alias = w.home.join("home-alias");
        std::os::unix::fs::symlink(&w.home, &alias).unwrap();
        fs::create_dir_all(w.home.join(".git")).unwrap();
        let nested = alias.join("nested");
        fs::create_dir_all(&nested).unwrap();
        write(
            w.home.join(".claude/settings.local.json"),
            r#"{"awaySummaryEnabled":false}"#,
        );
        let local = nested.join(".claude/settings.local.json");
        write(&local, r#"{"awaySummaryEnabled":true}"#);
        let o = options(&w);
        let mut d = discover(&o).unwrap();
        let r = request(claude(&d), Some(&nested));
        discover_claude_controls(&o, &mut d.profiles, &[r]).unwrap();
        let c = &claude(&d).background_directories[&native_path(&nested)][1];
        assert_eq!(c.value, Some(true));
        assert_eq!(c.off_file.as_deref(), Some(native_path(&local).as_str()));
    }

    #[cfg(unix)]
    #[test]
    fn inaccessible_recorded_scopes_fail_instead_of_returning_a_partial_report() {
        let w = Workspace::new();
        let cwd = w.home.join("loop");
        std::os::unix::fs::symlink(&cwd, &cwd).unwrap();
        let o = options(&w);
        let mut d = discover(&o).unwrap();
        let r = request(claude(&d), Some(&cwd));
        let error = discover_claude_controls(&o, &mut d.profiles, &[r]).unwrap_err();
        assert_eq!(error.message, "cannot inspect working directory");
        let p = d
            .profiles
            .iter()
            .find(|p| p.harness == Harness::Codex)
            .unwrap();
        let r = request(p, Some(&cwd));
        let error = discover_codex_controls(&o, &mut d.profiles, &[r]).unwrap_err();
        assert_eq!(error.message, "cannot inspect working directory");
    }

    #[cfg(unix)]
    #[test]
    fn claude_worktree_local_file_belongs_to_main_checkout() {
        let w = Workspace::new();
        let main = w.home.join("main");
        let work = w.home.join("worktree");
        fs::create_dir_all(main.join(".git/worktrees/branch")).unwrap();
        fs::create_dir_all(&work).unwrap();
        write(
            work.join(".git"),
            &format!(
                "gitdir: {}\n",
                native_path(main.join(".git/worktrees/branch"))
            ),
        );
        write(main.join(".git/worktrees/branch/commondir"), "../..\n");
        let local = main.join(".claude/settings.local.json");
        write(&local, r#"{"awaySummaryEnabled":false}"#);
        write(
            work.join(".claude/settings.local.json"),
            r#"{"awaySummaryEnabled":true}"#,
        );
        let mut d = discover(&options(&w)).unwrap();
        let r = request(claude(&d), Some(&work));
        discover_claude_controls(&options(&w), &mut d.profiles, &[r]).unwrap();
        let c = &claude(&d).background_directories[&native_path(&work)][1];
        assert_eq!(c.value, Some(false));
        assert_eq!(c.off_file.as_deref(), Some(native_path(&local).as_str()));
        assert_eq!(
            fs::read_to_string(&local).unwrap(),
            r#"{"awaySummaryEnabled":false}"#
        );
    }

    #[cfg(unix)]
    #[test]
    fn claude_submodule_worktree_keeps_local_file_in_place() {
        let w = Workspace::new();
        let module = w.home.join("super/.git/modules/sub");
        let work = w.home.join("worktree");
        fs::create_dir_all(module.join("worktrees/branch")).unwrap();
        fs::create_dir_all(&work).unwrap();
        write(
            work.join(".git"),
            &format!("gitdir: {}\n", native_path(module.join("worktrees/branch"))),
        );
        write(module.join("worktrees/branch/commondir"), "../..\n");
        write(
            work.join(".claude/settings.local.json"),
            r#"{"awaySummaryEnabled":false}"#,
        );
        let mut d = discover(&options(&w)).unwrap();
        let r = request(claude(&d), Some(&work));
        discover_claude_controls(&options(&w), &mut d.profiles, &[r]).unwrap();
        let c = &claude(&d).background_directories[&native_path(&work)][1];
        assert_eq!(c.value, Some(false));
        let local = work.join(".claude/settings.local.json");
        assert_eq!(c.off_file.as_deref(), Some(native_path(&local).as_str()));
    }

    #[test]
    fn codex_project_boundaries_and_profile_selection_preserve_known_and_unknown_owners() {
        let w = Workspace::new();
        let root = w.home.join("repo");
        let cwd = root.join("nested");
        fs::create_dir_all(root.join(".boundary")).unwrap();
        write(
            root.join(".codex/config.toml"),
            "[tui]\nauto_recap = false\n",
        );
        write(cwd.join(".codex/config.toml"), "[tui]\nauto_recap = true\n");
        write(
            w.codex.join("quiet.config.toml"),
            "[tui]\nauto_recap = false\n",
        );
        write(
            w.codex.join("config.toml"),
            &format!(
                "project_root_markers = ['.boundary']\n[projects.{}]\ntrust_level = 'trusted'\n",
                serde_json::to_string(&native_path(&root)).unwrap()
            ),
        );
        let mut d = discover(&options(&w)).unwrap();
        let p = d
            .profiles
            .iter()
            .find(|p| p.harness == Harness::Codex)
            .unwrap();
        let r = {
            let mut built = Request {
                ..request(p, Some(&cwd))
            };
            built.harness = Harness::Codex;
            built
        };
        discover_codex_controls(&options(&w), &mut d.profiles, std::slice::from_ref(&r)).unwrap();
        let p = d
            .profiles
            .iter()
            .find(|p| p.harness == Harness::Codex)
            .unwrap();
        // Trusted project overrides rank above every unobserved named profile's recap value.
        let c = &p.background_directories[&native_path(&cwd)][0];
        assert_eq!(c.value, Some(true));
        assert_eq!(
            c.off_file.as_deref(),
            Some(native_path(cwd.join(".codex/config.toml")).as_str())
        );
        write(
            w.codex.join("quiet.config.toml"),
            "project_root_markers = []\n",
        );
        discover_codex_controls(&options(&w), &mut d.profiles, std::slice::from_ref(&r)).unwrap();
        let p = d
            .profiles
            .iter()
            .find(|p| p.harness == Harness::Codex)
            .unwrap();
        assert_eq!(p.background_directories[&native_path(&cwd)][0].value, None);
        fs::remove_file(w.codex.join("quiet.config.toml")).unwrap();
        // Empty root markers stop at cwd, including trust selection.
        write(
            w.codex.join("config.toml"),
            &format!(
                "project_root_markers = []\n[projects.{}]\ntrust_level = 'trusted'\n",
                serde_json::to_string(&native_path(&cwd)).unwrap()
            ),
        );
        discover_codex_controls(&options(&w), &mut d.profiles, std::slice::from_ref(&r)).unwrap();
        let p = d
            .profiles
            .iter()
            .find(|p| p.harness == Harness::Codex)
            .unwrap();
        assert_eq!(
            p.background_directories[&native_path(&cwd)][0].value,
            Some(true)
        );
        // A worktree's .git file is a boundary, even inside a different trusted checkout.
        write(cwd.join(".git"), "gitdir: ../elsewhere\n");
        write(
            w.codex.join("config.toml"),
            &format!(
                "[projects.{}]\ntrust_level = 'trusted'\n",
                serde_json::to_string(&native_path(&root)).unwrap()
            ),
        );
        discover_codex_controls(&options(&w), &mut d.profiles, std::slice::from_ref(&r)).unwrap();
        let p = d
            .profiles
            .iter()
            .find(|p| p.harness == Harness::Codex)
            .unwrap();
        assert_eq!(
            p.background_directories[&native_path(&cwd)][0].value,
            None,
            "parent checkout trust is not worktree trust"
        );
        write(
            w.codex.join("config.toml"),
            "profile = 'quiet'\n[profiles.quiet.tui]\nauto_recap = false\n",
        );
        discover_codex_controls(&options(&w), &mut d.profiles, std::slice::from_ref(&r)).unwrap();
        let p = d
            .profiles
            .iter()
            .find(|p| p.harness == Harness::Codex)
            .unwrap();
        assert_eq!(p.background[0].value, None);
        assert!(p.background[0].reason.contains("legacy profile"));
    }

    #[test]
    fn outranking_files_hold_project_and_named_profile_layers_but_not_the_user_file() {
        let w = Workspace::new();
        let repo = w.home.join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        write(repo.join(".codex/config.toml"), "[features]\napps = true\n");
        write(
            repo.join(".claude/settings.json"),
            r#"{"enableArtifact": true}"#,
        );
        write(
            w.claude.join("settings.json"),
            r#"{"enableArtifact": true}"#,
        );
        let trust = |level: &str| {
            format!(
                "[projects.{}]\ntrust_level = '{level}'\n",
                serde_json::to_string(&native_path(&repo)).unwrap()
            )
        };
        write(w.codex.join("config.toml"), &trust("untrusted"));
        let outranking = |harness: Harness| {
            let mut d = discover(&options(&w)).unwrap();
            let p = d.profiles.iter().find(|p| p.harness == harness).unwrap();
            let requests: Vec<Request> = [&repo, &w.home]
                .into_iter()
                .map(|cwd| {
                    let mut r = request(p, Some(cwd.as_path()));
                    r.harness = harness;
                    r
                })
                .collect();
            discover_claude_controls(&options(&w), &mut d.profiles, &requests).unwrap();
            discover_codex_controls(&options(&w), &mut d.profiles, &requests).unwrap();
            let p = d.profiles.iter().find(|p| p.harness == harness).unwrap();
            p.outranking_files.clone()
        };
        // A session in the home reads the user's own file, which never outranks itself.
        assert_eq!(
            outranking(Harness::Claude),
            [native_path(repo.join(".claude/settings.json"))]
        );
        // An untrusted project's layer is not read.
        assert!(outranking(Harness::Codex).is_empty());
        // A named profile that trusts it can apply it, so both may outrank the user file.
        write(w.codex.join("work.config.toml"), &trust("trusted"));
        assert_eq!(
            outranking(Harness::Codex),
            [
                native_path(w.codex.join("work.config.toml")),
                native_path(repo.join(".codex/config.toml")),
            ]
        );
    }

    #[test]
    fn codex_instruction_budget_reads_system_config_and_flags_other_layers() {
        let w = Workspace::new();
        let root = w.home.join("repo");
        fs::create_dir_all(root.join(".git")).unwrap();
        write(w.home.join("system.toml"), "project_doc_max_bytes = 8192\n");
        write(w.codex.join("config.toml"), "model = 'gpt-5'\n");
        let budget = |d: &Discovery| {
            let p = d
                .profiles
                .iter()
                .find(|p| p.harness == Harness::Codex)
                .unwrap();
            (
                p.settings.get("project_doc_max_bytes").cloned(),
                p.instruction_budget_unresolved.clone(),
            )
        };
        let file = |d: &Discovery| {
            d.profiles
                .iter()
                .find(|p| p.harness == Harness::Codex)
                .unwrap()
                .instruction_budget_file
                .clone()
        };
        let mut d = discover(&options(&w)).unwrap();
        assert_eq!(budget(&d).0, Some(Scalar::Number(8192.0)));
        assert_eq!(file(&d), Some(native_path(w.home.join("system.toml"))));
        // The user's own value replaces the system's.
        write(
            w.codex.join("config.toml"),
            "project_doc_max_bytes = 65536\n",
        );
        d = discover(&options(&w)).unwrap();
        assert_eq!(budget(&d).0, Some(Scalar::Number(65536.0)));
        assert_eq!(file(&d), Some(native_path(w.codex.join("config.toml"))));
        let id = d
            .profiles
            .iter()
            .find(|p| p.harness == Harness::Codex)
            .unwrap()
            .id
            .clone();
        let req = {
            let mut built = Request { ..make_request() };
            built.harness = Harness::Codex;
            built.profiles = vec![id].into();
            built.cwd = Some(native_path(&root).into());
            built
        };
        discover_codex_controls(&options(&w), &mut d.profiles, std::slice::from_ref(&req)).unwrap();
        assert_eq!(budget(&d).1, None);
        // A project layer at a recorded directory can replace it.
        let project = root.join(".codex/config.toml");
        write(&project, "project_doc_max_bytes = 4096\n");
        discover_codex_controls(&options(&w), &mut d.profiles, std::slice::from_ref(&req)).unwrap();
        let reason = budget(&d).1.unwrap();
        assert!(reason.starts_with(&native_path(&project)), "{reason}");
        // So can a named profile, whose selection no record names.
        fs::remove_file(&project).unwrap();
        write(
            w.codex.join("big.config.toml"),
            "project_doc_max_bytes = 1\n",
        );
        discover_codex_controls(&options(&w), &mut d.profiles, std::slice::from_ref(&req)).unwrap();
        assert!(budget(&d).1.unwrap().contains("big.config.toml"));
        fs::remove_file(w.codex.join("big.config.toml")).unwrap();
        discover_codex_controls(&options(&w), &mut d.profiles, std::slice::from_ref(&req)).unwrap();
        assert_eq!(budget(&d).1, None);
    }

    #[test]
    fn codex_recaps_follow_trusted_layers_and_keep_unobserved_selection_unknown() {
        let w = Workspace::new();
        let root = w.home.join("repo");
        let cwd = root.join("nested");
        fs::create_dir_all(root.join(".git")).unwrap();
        let project = root.join(".codex/config.toml");
        let nearest = cwd.join(".codex/config.toml");
        write(&project, "[tui]\nauto_recap = false\n");
        write(
            &nearest,
            "secret = 'SYNTHETIC_PRIVATE'\n[tui]\nauto_recap = true\n",
        );
        write(
            w.codex.join("config.toml"),
            &format!(
                "[tui]\nauto_recap = false\n[projects.{}]\ntrust_level = 'trusted'\n",
                serde_json::to_string(&native_path(&root)).unwrap()
            ),
        );
        let mut d = discover(&options(&w)).unwrap();
        let id = d
            .profiles
            .iter()
            .find(|p| p.harness == Harness::Codex)
            .unwrap()
            .id
            .clone();
        let req = {
            let mut built = Request { ..make_request() };
            built.harness = Harness::Codex;
            built.profiles = vec![id.clone()].into();
            built.cwd = Some(native_path(&cwd).into());
            built
        };
        let recap = |d: &Discovery| {
            d.profiles
                .iter()
                .find(|p| p.id == id)
                .unwrap()
                .background_directories[&native_path(&cwd)][0]
                .clone()
        };
        discover_codex_controls(&options(&w), &mut d.profiles, std::slice::from_ref(&req)).unwrap();
        assert_eq!(recap(&d).value, Some(true));
        assert_eq!(
            recap(&d).off_file.as_deref(),
            Some(native_path(&nearest).as_str())
        );
        assert!(!format!("{:?}", d.profiles).contains("SYNTHETIC_PRIVATE"));
        write(w.codex.join("config.toml"), "[tui]\nauto_recap = false\n");
        discover_codex_controls(&options(&w), &mut d.profiles, std::slice::from_ref(&req)).unwrap();
        assert_eq!(recap(&d).value, None);
        assert!(recap(&d).reason.contains("trust is unknown"));
        write(
            w.codex.join("config.toml"),
            &format!(
                "[tui]\nauto_recap = false\n[projects.{}]\ntrust_level = 'untrusted'\n",
                serde_json::to_string(&native_path(&root)).unwrap()
            ),
        );
        discover_codex_controls(&options(&w), &mut d.profiles, std::slice::from_ref(&req)).unwrap();
        assert_eq!(recap(&d).value, Some(false));
        assert!(recap(&d).reason.contains("untrusted"));
        write(
            w.codex.join("quiet.config.toml"),
            "[tui]\nauto_recap = true\n",
        );
        discover_codex_controls(&options(&w), &mut d.profiles, std::slice::from_ref(&req)).unwrap();
        assert_eq!(recap(&d).value, None);
        assert!(recap(&d).reason.contains("--profile"));
        let gone = {
            let mut built = Request { ..req.clone() };
            built.cwd = Some(native_path(w.home.join("gone")).into());
            built
        };
        let absent = {
            let mut built = Request { ..req };
            built.cwd = None;
            built
        };
        discover_codex_controls(&options(&w), &mut d.profiles, &[gone.clone(), absent]).unwrap();
        let p = d.profiles.iter().find(|p| p.id == id).unwrap();
        assert!(
            p.background_directories[""]
                .iter()
                .all(|c| c.value.is_none())
        );
        assert!(
            p.background_directories[&*gone.cwd.clone().unwrap()][0]
                .value
                .is_none()
        );
    }

    #[test]
    fn a_project_on_a_cloud_provider_leaves_an_answered_plan_default_unknown() {
        let w = Workspace::new();
        let (plain, bedrock) = (w.home.join("plain"), w.home.join("bedrock"));
        for project in [&plain, &bedrock] {
            fs::create_dir_all(project.join(".git")).unwrap();
        }
        write(w.claude.join("settings.json"), "{}");
        write(
            bedrock.join(".claude/settings.json"),
            r#"{"env":{"CLAUDE_CODE_USE_BEDROCK":"1"}}"#,
        );
        let resolve = |billing, projects: &[&Path]| {
            let mut o = options(&w);
            o.billing.insert(canonical(&w.claude), billing);
            let mut data = discover(&o).unwrap();
            let requests: Vec<Request> = projects
                .iter()
                .map(|p| request(claude(&data), Some(p)))
                .collect();
            discover_claude_controls(&o, &mut data.profiles, &requests).unwrap();
            claude(&data).clone()
        };
        // Only plain projects: the plan's 1h holds everywhere.
        let p = resolve(Billing::Plan, &[&plain]);
        assert_eq!(
            (p.billing, p.ttl.main.value),
            (Some(Billing::Plan), Some(Ttl::OneHour))
        );
        // A Bedrock project defaults to 5m under the same unset setting, so neither default
        // is the profile's, as without an answer.
        let p = resolve(Billing::Plan, &[&plain, &bedrock]);
        assert_eq!((p.billing, p.ttl.main.value), (None, None));
        assert_eq!(p.ttl.main.reason, TTL_BILLING_UNKNOWN);
        let directories = p.idle_directories.as_ref().unwrap();
        let main = |d: &Path| {
            directories[&native_path(d)]
                .ttl
                .as_ref()
                .unwrap()
                .main
                .clone()
        };
        assert_eq!(main(&plain).value, None);
        assert_eq!(main(&plain).reason, TTL_BILLING_UNKNOWN);
        assert_eq!(main(&bedrock).value, Some(Ttl::FiveMinutes));
        // An API answer's 5m matches Bedrock's, so it holds.
        let p = resolve(Billing::Api, &[&plain, &bedrock]);
        assert_eq!(
            (p.billing, p.ttl.main.value),
            (Some(Billing::Api), Some(Ttl::FiveMinutes))
        );
    }

    #[test]
    fn a_ttl_resolves_for_a_profile_only_when_every_recorded_project_gets_it() {
        let w = Workspace::new();
        let (plain, pinned) = (w.home.join("plain"), w.home.join("pinned"));
        for project in [&plain, &pinned] {
            fs::create_dir_all(project.join(".git")).unwrap();
        }
        write(w.claude.join("settings.json"), r#"{"promptCacheTtl":"5m"}"#);
        write(
            pinned.join(".claude/settings.json"),
            r#"{"promptCacheTtl":"1h"}"#,
        );
        let gone = w.home.join("gone");
        let o = options(&w);
        let mut data = discover(&o).unwrap();
        // Discovery reads every recorded directory, including a deleted one's absence.
        let requests: Vec<Request> = [&plain, &pinned, &gone]
            .iter()
            .map(|p| request(claude(&data), Some(p)))
            .collect();
        discover_claude_controls(&o, &mut data.profiles, &requests).unwrap();
        // Once T3 attributes the requests, only the profile's own directories settle it.
        let settled = |projects: &[&Path]| {
            let mut p = claude(&data).clone();
            let texts: Vec<String> = projects.iter().map(native_path).collect();
            settle_ttls(&mut p, texts.iter().map(String::as_str));
            p
        };
        // A deleted project runs nothing more, so it decides no current value.
        let p = settled(&[&plain, &gone]);
        assert_eq!(p.ttl.main.one().value, Some(Ttl::FiveMinutes));
        assert_eq!(p.compaction.default.comparison_unknown, None);
        // A project's own file decides its sessions, which the user file cannot change.
        for projects in [&[pinned.as_path()][..], &[&plain, &pinned]] {
            let p = settled(projects);
            assert!(p.ttl.main.varies);
            assert_eq!(p.ttl.main.one().value, None);
            // The home's value stays for replays that read each directory's own.
            assert_eq!(p.ttl.main.value, Some(Ttl::FiveMinutes));
            assert_eq!(p.ttl.main.reason, TTL_PROJECTS_DIFFER);
            assert!(!p.ttl.subagent.varies);
            // Every check that applies one TTL across the profile goes unknown with it.
            assert_eq!(
                p.compaction.default.comparison_unknown.as_deref(),
                Some(TTL_PROJECTS_DIFFER)
            );
            let directories = p.idle_directories.as_ref().unwrap();
            let main = |d: &Path| {
                directories[&native_path(d)]
                    .ttl
                    .as_ref()
                    .unwrap()
                    .main
                    .value
            };
            assert_eq!(main(&pinned), Some(Ttl::OneHour));
        }
        // An unset main TTL with unknown billing stays pinnable across another default, such
        // as a cloud provider's, but not across a project that turns caching off.
        write(w.claude.join("settings.json"), "{}");
        let (cloud, off) = (w.home.join("cloud"), w.home.join("off"));
        for (project, env) in [
            (&cloud, "CLAUDE_CODE_USE_BEDROCK"),
            (&off, "DISABLE_PROMPT_CACHING"),
        ] {
            fs::create_dir_all(project.join(".git")).unwrap();
            write(
                project.join(".claude/settings.json"),
                &format!(r#"{{"env":{{"{env}":"1"}}}}"#),
            );
        }
        let mut data = discover(&o).unwrap();
        let requests: Vec<Request> = [&plain, &cloud, &off]
            .iter()
            .map(|p| request(claude(&data), Some(p)))
            .collect();
        discover_claude_controls(&o, &mut data.profiles, &requests).unwrap();
        let settled = |projects: &[&Path]| {
            let mut p = claude(&data).clone();
            let texts: Vec<String> = projects.iter().map(native_path).collect();
            settle_ttls(&mut p, texts.iter().map(String::as_str));
            p
        };
        let p = settled(&[&plain, &cloud]);
        assert_eq!(p.ttl.main.reason, TTL_BILLING_UNKNOWN);
        assert!(!p.ttl.main.varies);
        assert!(settled(&[&plain, &off]).ttl.main.varies);
    }

    // Native informational notices are the only parsed idle attribution; project owners and threshold floor resolve.
    #[test]
    fn project_owners_and_threshold_floor_resolve_at_recorded_directories() {
        let w = Workspace::new();
        let project = w.home.join("project");
        fs::create_dir_all(project.join(".git")).unwrap();
        fs::create_dir_all(project.join("nested")).unwrap();
        write(
            w.claude.join("settings.json"),
            r#"{"idleCompaction":true,"autoCompactWindow":400000,"promptCacheTtl":"1h"}"#,
        );
        let local = if cfg!(windows) {
            project.join("nested/.claude/settings.local.json")
        } else {
            project.join(".claude/settings.local.json")
        };
        write(
            &local,
            r#"{"idleCompaction":false,"env":{"CLAUDE_CODE_IDLE_COMPACT_MIN_TOKENS":"90000"},"promptCacheTtl":"5m"}"#,
        );
        let mut data = discover(&options(&w)).unwrap();
        let profile = claude(&data).clone();
        let gone = w.home.join("gone");
        let requests = [
            request(&profile, Some(&project.join("nested"))),
            request(&profile, Some(&gone)),
            {
                let mut built = Request {
                    ..request(&profile, None)
                };
                built.agent = crate::model::Agent::Subagent;
                built
            },
        ];
        discover_claude_controls(&options(&w), &mut data.profiles, &requests).unwrap();
        let profile = claude(&data);
        assert!(
            profile.idle.as_ref().unwrap().resolved,
            "subagent cwd is not consulted"
        );
        let directories = profile.idle_directories.as_ref().unwrap();
        let owner = &directories[&native_path(project.join("nested"))];
        assert_eq!(owner.value, Some(false));
        assert_eq!(owner.threshold, Some(100_000));
        assert_eq!(
            owner.owner,
            format!("{}:idleCompaction", native_path(&local))
        );
        assert_eq!(
            owner.ttl.as_ref().unwrap().main.value,
            Some(Ttl::FiveMinutes)
        );
        assert_eq!(
            owner.ttl.as_ref().unwrap().main.owner,
            format!("{}:promptCacheTtl", native_path(&local))
        );
        assert!(owner.resolved, "the nested directory exists");
        let missing = &directories[&native_path(&gone)];
        assert!(!missing.resolved);
        assert_eq!(
            missing.reason,
            "recorded working directory no longer exists; current project/local ownership is unresolved"
        );
        let speed = &profile.speed_directories.as_ref().unwrap()[&native_path(&gone)];
        assert_eq!(speed.value, None);

        let absent = [request(claude(&data), None)];
        discover_claude_controls(&options(&w), &mut data.profiles, &absent).unwrap();
        let profile = claude(&data);
        assert!(!profile.idle.as_ref().unwrap().resolved);
        assert!(profile.idle_directories.as_ref().unwrap().is_empty());
    }

    #[test]
    fn only_the_profiles_own_settled_latest_request_decides_its_controls() {
        let w = Workspace::new();
        let (mine, theirs) = (w.home.join("mine"), w.home.join("theirs"));
        for (dir, settings) in [
            (
                &mine,
                r#"{"advisorModel":"opus","autoMemoryEnabled":false}"#,
            ),
            (
                &theirs,
                r#"{"advisorModel":"fable","model":"claude-sonnet-5-5"}"#,
            ),
        ] {
            fs::create_dir_all(dir.join(".git")).unwrap();
            write(dir.join(".claude/settings.json"), settings);
        }
        let mut data = discover(&options(&w)).unwrap();
        let profile = claude(&data).clone();
        data.profiles.push(crate::test_support::make_t3_profile(
            "t3",
            &native_path(&w.home.join("t3")),
        ));
        let mut earlier = request(&profile, Some(&mine));
        earlier.time = 1;
        // Newer, but T3 attributed it to an instance of its own.
        let mut later = request(&profile, Some(&theirs));
        later.time = 2;
        later.profiles = vec!["t3".into()].into();
        // Newest, but T3 couldn't settle its owner, so it names both: it decides for neither.
        let mut unsettled = request(&profile, Some(&theirs));
        unsettled.time = 3;
        unsettled.profiles = vec![profile.id.clone(), "t3".into()].into();
        let requests = [earlier, later, unsettled.clone()];
        discover_latest_claude_controls(&options(&w), &mut data.profiles, &requests).unwrap();
        discover_inbound(&options(&w), &mut data.profiles, &requests).unwrap();
        let directory = |data: &Discovery, id: &str| {
            let p = data.profiles.iter().find(|p| p.id == id).unwrap();
            p.inbound.as_ref().unwrap().directory.clone()
        };
        assert_eq!(directory(&data, &profile.id), Some(native_path(&mine)));
        assert_eq!(directory(&data, "t3"), Some(native_path(&theirs)));
        let profile = claude(&data);
        let advisor = profile.advisor.as_ref().unwrap();
        assert_eq!(advisor.model.as_deref(), Some("opus"));
        assert_eq!(
            advisor.owner,
            format!(
                "{}:advisorModel",
                join(&[&native_path(&mine), ".claude", "settings.json"])
            )
        );
        let memory = profile.memory.as_ref().unwrap();
        assert_eq!(
            memory.directory.as_deref(),
            Some(native_path(&mine).as_str())
        );
        // The recorded model, not the other project's setting.
        let effort = profile.effort.as_ref().unwrap();
        assert_eq!(effort.model.as_deref(), Some("claude-opus-5-5"));

        // With only an unsettled request, the user and managed files decide.
        let requests = [unsettled];
        discover_latest_claude_controls(&options(&w), &mut data.profiles, &requests).unwrap();
        discover_inbound(&options(&w), &mut data.profiles, &requests).unwrap();
        assert_eq!(directory(&data, &claude(&data).id.clone()), None);
        assert_eq!(directory(&data, "t3"), None);
        let profile = claude(&data);
        assert_eq!(profile.memory.as_ref().unwrap().directory, None);
        let advisor = profile.advisor.as_ref().unwrap();
        assert_eq!(advisor.model, None);
        assert_eq!(
            advisor.owner,
            format!("{}:advisorModel", profile.settings_file)
        );
        assert_eq!(profile.effort.as_ref().unwrap().model, None);
    }
}
