//! Find Claude and Codex profiles, resolve their current settings and freeze history files.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;

use indexmap::{IndexMap, IndexSet};
use serde_json::{Map, Value};

use crate::controls::{
    CLAUDE_BACKGROUND_ENV, CompactionSources, Env, SettingsScope, bash_cap_control,
    claude_background, codex_background, compaction_controls, idle_controls, js_string, members,
    scoped_environment, speed_controls,
};
use crate::model::{
    Agent, BackgroundControl, Harness, Location, PerBucket, Profile, Request, RootOrigin,
    RootState, RootStatus, Scalar, Snapshot, SpeedControl, Ttl, TtlControl,
};
use crate::safe::{ScanError, identity, label};

/// Where discovery looks; explicit roots limit the entire scan, otherwise host-local profiles are discovered.
#[derive(Clone, Debug)]
pub struct DiscoveryOptions {
    pub home: String,
    pub env: Env,
    pub claude_dirs: Vec<String>,
    pub codex_dirs: Vec<String>,
    /// Explicit T3 state directories containing settings.json and statev2.sqlite.
    pub t3_dirs: Vec<String>,
    /// The system managed Claude settings file; overridable for host adapters and synthetic tests.
    pub managed_settings_file: String,
    /// The system Codex config file.
    pub codex_system_config_file: String,
    /// The managed Codex requirements file.
    pub codex_requirements_file: String,
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
        })
    }
}

/// Every root considered, the selected profiles, and each history file's frozen identity and length.
#[derive(Clone, Debug, PartialEq)]
pub struct Discovery {
    pub roots: Vec<RootStatus>,
    pub profiles: Vec<Profile>,
    pub snapshots: Vec<Snapshot>,
    pub t3: Vec<crate::model::T3State>,
}

/// A safe error naming a path without its contents.
fn error(message: &str, file: &str, line: u64) -> ScanError {
    ScanError::at(
        message,
        Location {
            file: file.into(),
            line,
        },
    )
}

/// Node's `ENOENT` and `ENOTDIR`, which both mean the path is not there.
fn absent(kind: ErrorKind) -> bool {
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
fn join(parts: &[&str]) -> String {
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
fn resolve(path: &str) -> String {
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
fn dirname(path: &str) -> &str {
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
fn join(parts: &[&str]) -> String {
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
fn resolve(path: &str) -> String {
    std::path::absolute(path)
        .map(|absolute| lexical(&absolute))
        .unwrap_or_else(|_| path.to_string())
}

/// Node's win32 `path.dirname`: a root is its own parent.
#[cfg(windows)]
fn dirname(path: &str) -> &str {
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

/// JavaScript's default `localeCompare` (ICU root collation) for printable ASCII paths:
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

/// Convert TOML into the JSON shapes JavaScript's TOML parser yields.
fn toml_json(value: toml::Value) -> Option<Value> {
    Some(match value {
        toml::Value::String(s) => Value::String(s),
        toml::Value::Integer(n) => {
            // Integers JavaScript cannot represent exactly are a parse error there.
            if n.unsigned_abs() > 9_007_199_254_740_991 {
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
fn settings(file: &str, harness: Harness) -> Result<Map<String, Value>, ScanError> {
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
}

fn ttl_controls(sources: &TtlSources) -> PerBucket<TtlControl> {
    let TtlSources {
        config,
        env,
        file,
        managed,
        managed_file,
        scopes,
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
        } else {
            reason = "billing state is unknown; main default can change from included subscription usage to credits";
        }
        TtlControl {
            value,
            owner,
            reason: reason.into(),
        }
    };
    PerBucket {
        main: bucket(false),
        subagent: bucket(true),
    }
}

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
    let mut result = IndexMap::new();
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
fn directories<'a>(requests: impl Iterator<Item = &'a Request>) -> IndexSet<&'a str> {
    requests
        .filter_map(|r| r.cwd.as_deref().filter(|cwd| !cwd.is_empty()))
        .collect()
}

/// Directories from the recorded cwd up to the nearest project marker, closest first.
fn project_directories(cwd: &str, markers: &[String]) -> Result<Vec<String>, ScanError> {
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

/// The main checkout of a linked worktree, reading only Git's path metadata.
#[cfg(unix)]
fn main_checkout(root: &str) -> Result<String, ScanError> {
    let dotgit = join(&[root, ".git"]);
    let meta = match fs::metadata(&dotgit) {
        Ok(meta) => meta,
        Err(e) if absent(e.kind()) => return Ok(root.into()),
        Err(_) => return Err(error("cannot inspect git directory", &dotgit, 0)),
    };
    if meta.is_dir() {
        return Ok(root.into());
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
        Err(e) if absent(e.kind()) => return Ok(root.into()),
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

fn unresolved_background(controls: &mut [BackgroundControl], reason: &str) {
    for control in controls {
        control.value = None;
        control.minutes = None;
        control.reason = reason.into();
    }
}

/// Resolve current project/local controls at recorded directories, preserving every owner.
pub fn discover_claude_controls(
    options: &DiscoveryOptions,
    profiles: &mut [Profile],
    requests: &[Request],
) -> Result<(), ScanError> {
    let env = &options.env;
    let empty = Env::new();
    let default_root = join(&[&options.home, ".claude"]);
    let configured = env.get("CLAUDE_CONFIG_DIR").unwrap_or(&default_root);
    let active = realpath(&resolve(configured)).unwrap_or_default();
    let managed_file = options.managed_settings_file.as_str();
    for profile in profiles
        .iter_mut()
        .filter(|p| p.harness == Harness::Claude && p.t3.is_none())
    {
        profile.background_requests.clear();
        let user = settings(&profile.settings_file, Harness::Claude)?;
        let managed = settings(managed_file, Harness::Claude)?;
        let local_env = if active == profile.root { env } else { &empty };
        let mut speed_directories = IndexMap::new();
        let mut idle_directories = IndexMap::new();
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
            "recorded working directory is absent; current project/local ownership and launch selection are unresolved",
        );
        profile
            .background_directories
            .insert(String::new(), unknown);
        for cwd in directories(relevant.into_iter()) {
            let exists = match fs::metadata(cwd) {
                Ok(meta) => meta.is_dir() && std::path::Path::new(cwd).is_absolute(),
                Err(e) if absent(e.kind()) => false,
                Err(_) => return Err(error("cannot inspect working directory", cwd, 0)),
            };
            if !exists {
                let mut controls = profile.background.clone();
                unresolved_background(
                    &mut controls,
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
            let paths = project_directories(cwd, &[".git".into()])?;
            let root = paths.last().expect("cwd is included");
            let local_root = claude_local_root(root, options)?;
            let project_file = join(&[cwd, ".claude", "settings.json"]);
            let legacy_file = join(&[cwd, ".claude", "settings.local.json"]);
            let local_file = join(&[
                local_root.as_deref().unwrap_or(cwd),
                ".claude",
                "settings.local.json",
            ]);
            let project = settings(&project_file, Harness::Claude)?;
            let legacy = settings(&legacy_file, Harness::Claude)?;
            let local = settings(&local_file, Harness::Claude)?;
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
            for request in requests.iter().filter(|r| {
                launch_conflict
                    && r.agent == Agent::Main
                    && r.profiles.contains(&profile.id)
                    && r.cwd.as_deref() == Some(cwd)
                    && r.client.as_deref() != Some("cli")
            }) {
                let mut controls = background.clone();
                unresolved_background(
                    &mut controls,
                    "settings env and inherited environment conflict; the recorded client does not establish terminal versus hosted launch precedence",
                );
                profile
                    .background_requests
                    .insert(request.id.clone(), controls);
            }
            let config = merged(&[&user, &project, &legacy, &local, &managed]);
            idle.ttl = Some(ttl_controls(&TtlSources {
                config: &config,
                env: local_env,
                file: &profile.settings_file,
                managed: &managed,
                managed_file,
                scopes: Some(&scopes),
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
        // /cd reapplies the new env over the old one; removed keys can survive it.
        let mut session_cwds = std::collections::HashMap::new();
        for request in requests
            .iter()
            .filter(|r| r.agent == Agent::Main && r.profiles.contains(&profile.id))
        {
            let (previous, changed) = session_cwds
                .entry(&request.session)
                .or_insert((request.cwd.as_deref(), false));
            *changed |= *previous != request.cwd.as_deref();
            *previous = request.cwd.as_deref();
            if *changed
                && let Some(controls) = profile
                    .background_directories
                    .get(request.cwd.as_deref().unwrap_or_default())
            {
                let mut controls = controls.clone();
                unresolved_background(
                    &mut controls,
                    "session working directory changed; /cd environment carry-over and launch selection are not recorded",
                );
                profile
                    .background_requests
                    .insert(request.id.clone(), controls);
            }
        }
    }
    Ok(())
}

/// Codex keys that let a profile or project file change speed, model or gates.
const CODEX_SPEED_KEYS: [&str; 4] = ["model", "model_provider", "service_tier", "features"];

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
        let user = settings(&profile.settings_file, Harness::Codex)?;
        let system = settings(&options.codex_system_config_file, Harness::Codex)?;
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
                "legacy profile configuration exists; current Codex removed it in 0.134.0, but installed version and launch selection are unresolved",
            );
        }
        let mut marker_selection = legacy_selection;
        for name in named_profile_files(&profile.root)? {
            let file = join(&[&profile.root, &name]);
            let config = settings(&file, Harness::Codex)?;
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
                    "named config profiles can override automatic recaps; --profile and CLI/session selection are not recorded",
                );
                base[0].owner = format!(
                    "{}; {file}:tui.auto_recap (--profile selection unresolved)",
                    base[0].owner
                );
            }
        }
        profile.background = base.clone();
        let mut unknown = base.clone();
        unresolved_background(
            &mut unknown,
            "recorded working directory is absent; current project trust, ownership and launch selection are unresolved",
        );
        profile
            .background_directories
            .insert(String::new(), unknown);
        let markers = base_scopes
            .iter()
            .rev()
            .find_map(|s| s.config.get("project_root_markers"));
        let markers: Vec<String> = match markers {
            None => vec![".git".into()],
            Some(Value::Array(values)) if values.iter().all(Value::is_string) => values
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect(),
            Some(_) => {
                return Err(error(
                    "invalid project_root_markers",
                    &profile.settings_file,
                    1,
                ));
            }
        };
        let cwds = directories(requests.iter().filter(|r| r.profiles.contains(&profile.id)));
        for cwd in cwds {
            let exists = match fs::metadata(cwd) {
                Ok(meta) => meta.is_dir() && std::path::Path::new(cwd).is_absolute(),
                Err(e) if absent(e.kind()) => false,
                Err(_) => return Err(error("cannot inspect working directory", cwd, 0)),
            };
            if !exists {
                let mut controls = base.clone();
                unresolved_background(
                    &mut controls,
                    "recorded working directory is missing or not absolute; current project trust and ownership are unresolved",
                );
                profile.background_directories.insert(cwd.into(), controls);
                continue;
            }
            let paths = project_directories(cwd, &markers)?;
            let root = paths.last().expect("cwd is included");
            // Exact cwd, then project root; never infer trust from the fact requests exist.
            let trust = [cwd, root.as_str()].into_iter().find_map(|path| {
                base_scopes.iter().rev().find_map(|scope| {
                    members(scope.config.get("projects"))
                        .get(path)
                        .and_then(|p| p.get("trust_level"))
                        .and_then(Value::as_str)
                })
            });
            let mut layers = Vec::new();
            for path in &paths {
                if trust == Some("untrusted") {
                    continue;
                }
                let file = join(&[path, ".codex", "config.toml"]);
                let config = settings(&file, Harness::Codex)?;
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
                layers.push((file, config));
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
                    "project automatic recap overrides exist, but current project trust is unknown; historical trust and CLI/session selection are not recorded",
                );
            }
            if marker_selection {
                unresolved_background(
                    &mut controls,
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
    }
    Ok(())
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

/// Named profile files in stable order, shared by control discovery.
fn named_profile_files(root: &str) -> Result<Vec<String>, ScanError> {
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
    for name in named_profile_files(canonical)? {
        let file = join(&[canonical, &name]);
        let config = settings(&file, Harness::Codex)?;
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
                // JavaScript's default sort compares UTF-16 code units.
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

fn harness_name(harness: Harness) -> &'static str {
    match harness {
        Harness::Claude => "claude",
        Harness::Codex => "codex",
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
    let key = format!("{}{absolute}", harness_name(harness));
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
    let user = settings(&settings_file, harness)?;
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
    let empty = Env::new();
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
    let enabled =
        |key: &str| matches!(environment.values.get(key), Some(Value::String(s)) if s == "1");
    let provider = if claude {
        // A custom ANTHROPIC_BASE_URL (gateway or proxy) still speaks the Anthropic API,
        // so its requests are priced as public Anthropic API equivalents.
        if enabled("CLAUDE_CODE_USE_BEDROCK") {
            Some("amazon-bedrock".into())
        } else if enabled("CLAUDE_CODE_USE_VERTEX") {
            Some("google-vertex-anthropic".into())
        } else if enabled("CLAUDE_CODE_USE_FOUNDRY") {
            Some("azure".into())
        } else {
            None
        }
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
    if !claude {
        codex_speed(
            &mut speed,
            &CodexSpeedSources {
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
    let ttl = ttl_controls(&TtlSources {
        config,
        env: local_env,
        file: &settings_file,
        managed: &managed,
        managed_file,
        scopes: None,
    });
    let mut settings = structural_settings(config);
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
    Ok(Profile {
        id: identity(key),
        harness,
        root: canonical,
        aliases: vec![alias],
        histories,
        settings_file,
        ttl,
        provider,
        settings,
        mcp_servers,
        compaction,
        idle,
        speed: Some(speed),
        bash,
        speed_directories: None,
        idle_directories: None,
        background,
        background_directories: IndexMap::new(),
        background_requests: IndexMap::new(),
        t3: None,
    })
}

/// Discover configuration aliases and freeze every selected file's length before parsing.
pub fn discover(options: &DiscoveryOptions) -> Result<Discovery, ScanError> {
    let home = options.home.as_str();
    let env = &options.env;
    let explicit = !options.claude_dirs.is_empty()
        || !options.codex_dirs.is_empty()
        || !options.t3_dirs.is_empty();
    let mut candidates = IndexMap::new();
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
        if candidates.contains_key(&format!("{}{}", harness_name(harness), resolve(&path))) {
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
    let mut profiles: IndexMap<String, Profile> = IndexMap::new();
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
        let key = format!("{}{canonical}", harness_name(root.harness));
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
    use crate::test_support::{Workspace, make_request};
    use std::path::Path;

    fn text(path: &Path) -> String {
        // Windows paths joined from "a/b" keep the slash; the code under test normalizes it.
        let path: std::path::PathBuf = if cfg!(windows) {
            path.components().collect()
        } else {
            path.to_path_buf()
        };
        path.to_string_lossy().into_owned()
    }

    fn canonical(path: &Path) -> String {
        realpath(&text(path)).unwrap()
    }

    fn write(path: impl AsRef<Path>, contents: &str) {
        let path = path.as_ref();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn options(w: &Workspace) -> DiscoveryOptions {
        DiscoveryOptions {
            home: text(&w.home),
            env: Env::new(),
            claude_dirs: vec![],
            codex_dirs: vec![],
            t3_dirs: vec![],
            managed_settings_file: text(&w.managed_settings_file()),
            codex_system_config_file: text(&w.home.join("system.toml")),
            codex_requirements_file: text(&w.home.join("requirements.toml")),
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
        Request {
            profiles: vec![profile.id.clone()],
            cwd: cwd.map(text),
            ..make_request()
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
        let config = settings(&text(&file), Harness::Codex).unwrap();
        assert_eq!(config["a"], serde_json::json!({}));
        assert_eq!(config["b"], Value::Null);
        assert_eq!(config["c"], serde_json::json!(2.0));
        assert_eq!(config["d"], serde_json::json!({ "e": [1, "x"] }));
        write(&file, "a = 9007199254740993\n");
        let error = settings(&text(&file), Harness::Codex).unwrap_err();
        assert_eq!(error.message, "invalid settings file");
        write(&file, "a = [");
        assert!(settings(&text(&file), Harness::Codex).is_err());
        assert!(
            settings(&text(&w.home.join("absent.toml")), Harness::Codex)
                .unwrap()
                .is_empty()
        );
        let error = settings(&text(&w.claude), Harness::Claude).unwrap_err();
        assert_eq!(error.message, "cannot read settings");
        let json = w.home.join("settings.json");
        write(&json, "[]");
        let error = settings(&text(&json), Harness::Claude).unwrap_err();
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

    // Ported from reader.test.ts: "current environment overrides are scoped to the active config root".
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
                ("CLAUDE_CONFIG_DIR", &text(&w.claude)),
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

    // Ported from reader.test.ts: "managed TTL controls retain the managed owner and API-key presence leaves billing unknown".
    #[test]
    fn managed_ttl_controls_keep_the_managed_owner() {
        let w = Workspace::new();
        let managed = text(&w.managed_settings_file());
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

    // Ported from reader.test.ts: "automatic discovery ignores a symlinked Claude config file".
    #[cfg(unix)]
    #[test]
    fn symlinked_config_files_are_not_profiles() {
        let w = Workspace::new();
        let file = w.home.join("account.json");
        write(&file, "{}");
        std::os::unix::fs::symlink(&file, w.home.join(".claude.json")).unwrap();
        assert_eq!(discover(&options(&w)).unwrap().profiles.len(), 2);
        let error = discover(&DiscoveryOptions {
            claude_dirs: vec![text(&file)],
            ..options(&w)
        })
        .unwrap_err();
        assert_eq!(error.message, "selected root is missing or inaccessible");
    }

    // Ported from reader.test.ts: "managed file locations match the supported host platforms".
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

    // Ported from the discovery half of reader.test.ts: "shared directories are read once, preserve profile aliases, and include nested subagents".
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
            claude_dirs: vec![text(&alternate)],
            ..options(&w)
        })
        .unwrap();
        assert_eq!(selected.profiles.len(), 1);
        assert!(selected.roots.iter().any(|r| r.path == text(&w.claude)
            && r.status == RootState::Excluded
            && r.origin == RootOrigin::Default));
        assert_eq!(selected.profiles[0].ttl.main.value, Some(Ttl::OneHour));
    }

    // Ported from the discovery halves of reader.test.ts: "all lengths freeze before reading" and "a shrunk snapshot fails".
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
            claude_dirs: vec![text(&absent)],
            ..options(&w)
        })
        .unwrap_err();
        assert_eq!(error.message, "selected root is missing or inaccessible");
        assert_eq!(&*error.location.unwrap().file, text(&absent));
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

    // Ported from compaction.test.ts: "discovery retains only whitelisted compaction controls and malformed selected limits fail".
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
            crate::controls::window_control(claude(&data), Some("claude-opus-5-5")).value,
            Some(400_000)
        );
        let codex = data
            .profiles
            .iter()
            .find(|p| p.harness == Harness::Codex)
            .unwrap();
        assert_eq!(codex.compaction.default.value, Some(240_000));
        assert!(!codex.compaction.default.applicable);
        assert!(
            !serde_json::to_string(&data.profiles)
                .unwrap()
                .contains("PRIVATE")
        );
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

    // Ported from the discovery half of fast.test.ts: "active-profile environment, managed policy and current project ownership suppress stale preferences".
    #[test]
    fn environment_managed_policy_and_project_ownership_suppress_fast_preferences() {
        let w = Workspace::new();
        let managed = text(&w.managed_settings_file());
        write(
            w.claude.join("settings.json"),
            r#"{"model":"opus","fastMode":true}"#,
        );
        let shell = discover(&DiscoveryOptions {
            env: env(&[
                ("CLAUDE_CONFIG_DIR", &text(&w.claude)),
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
        let directory = &claude(&data).speed_directories.as_ref().unwrap()[&text(&cwd)];
        assert_eq!(directory.value.as_deref(), Some("standard"));
        assert_eq!(directory.owner, format!("{}:fastMode", text(&local)));
    }

    // Ported from fast.test.ts: "Codex legacy/named profiles and project selection stay advisory; local requirements can block premium".
    #[test]
    fn codex_named_profiles_stay_advisory_and_requirements_block_premium() {
        let w = Workspace::new();
        let config = w.codex.join("config.toml");
        let requirements = w.home.join("requirements.toml");
        let options = DiscoveryOptions {
            codex_dirs: vec![text(&w.codex)],
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
        assert_eq!(blocked.owner, format!("{}:features", text(&requirements)));
        write(&config, "model=\"gpt-6.1-sol\"\nservice_tier=\"ultrafast\"");
        write(&requirements, "[features]\nultrafast_mode=false");
        assert_eq!(speed().value.as_deref(), Some("standard"));
        write(&requirements, "[features]\nultrafast_mode=\"no\"");
        let error = discover(&options).unwrap_err();
        assert_eq!(error.message, "invalid required speed feature");
    }

    // Ported from the discovery half of fast.test.ts: "known distinct project controls get separate cards and nearest Codex scope remains advisory owner".
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
            codex_dirs: vec![text(&w.codex)],
            ..options(&w)
        };
        let mut data = discover(&options).unwrap();
        let profile = data.profiles[0].clone();
        let requests = [
            Request {
                harness: Harness::Codex,
                ..request(&profile, Some(&nested))
            },
            request(&profile, None),
        ];
        discover_codex_controls(&options, &mut data.profiles, &requests).unwrap();
        let directories = data.profiles[0].speed_directories.as_ref().unwrap();
        assert_eq!(directories.len(), 1);
        let directory = &directories[&text(&nested)];
        assert_eq!(directory.owner, text(&nearest));
        assert_eq!(directory.value, None);
        assert_eq!(directory.replacement, profile.speed.unwrap().replacement);
    }

    // Ported from the discovery half of reader.test.ts: "a custom ANTHROPIC_BASE_URL proxy is priced as the public Anthropic API".
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
        let r = Request {
            client: Some("cli".into()),
            ..request(claude(&d), Some(&nested))
        };
        discover_claude_controls(
            &o,
            &mut d.profiles,
            &[
                r.clone(),
                Request {
                    cwd: None,
                    ..r.clone()
                },
            ],
        )
        .unwrap();
        let p = claude(&d);
        let c = &p.background_directories[&text(&nested)];
        assert_eq!(c[0].value, Some(true));
        assert_eq!(
            c[0].owner,
            format!("{}:promptSuggestionEnabled", text(&project))
        );
        assert_eq!(c[1].value, Some(true));
        assert_eq!(c[1].off_file.as_deref(), Some(text(&project).as_str()));
        assert_eq!(
            c[1].off,
            "\"env\": {\"CLAUDE_CODE_ENABLE_AWAY_SUMMARY\": \"0\"}"
        );
        #[cfg(unix)]
        assert_eq!(
            (c[2].minutes, c[2].off_file.as_deref()),
            (Some(7), Some(text(&local).as_str()))
        );
        #[cfg(windows)]
        assert_eq!(c[2].minutes, Some(3));
        assert!(
            p.background_directories[""]
                .iter()
                .all(|c| c.value.is_none())
        );
        assert!(
            !serde_json::to_string(p)
                .unwrap()
                .contains("SYNTHETIC_PRIVATE")
        );
        write(
            w.managed_settings_file(),
            r#"{"env":{"CLAUDE_CODE_ENABLE_AWAY_SUMMARY":"0","CLAUDE_CODE_GOAL_CHECKIN_MINUTES":"0"}}"#,
        );
        discover_claude_controls(&o, &mut d.profiles, std::slice::from_ref(&r)).unwrap();
        let c = &claude(&d).background_directories[&text(&nested)];
        assert_eq!((c[1].value, c[2].minutes), (Some(false), Some(0)));
        assert_eq!(
            c[2].off_file.as_deref(),
            Some(text(&w.managed_settings_file()).as_str())
        );
        let hosted = Request {
            client: Some("desktop".into()),
            ..r.clone()
        };
        discover_claude_controls(&o, &mut d.profiles, std::slice::from_ref(&hosted)).unwrap();
        let c = &claude(&d).background_requests[&hosted.id];
        assert!(c.iter().all(|c| c.value.is_none()));
        assert!(c[0].reason.contains("launch precedence"));
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
        .map(|(i, (session, cwd, client))| Request {
            id: format!("r{i}"),
            session: session.into(),
            time: i as i64,
            client: client.map(String::from),
            ..request(p, Some(cwd))
        })
        .collect();
        discover_claude_controls(&o, &mut d.profiles, &requests).unwrap();
        let p = claude(&d);
        assert_eq!(p.background_directories[&text(&b)][1].value, Some(false));
        assert!(!p.background_requests.contains_key("r0"));
        assert!(!p.background_requests.contains_key("r1"));
        for i in 2..7 {
            assert_eq!(p.background_requests[&format!("r{i}")][1].value, None);
        }
        assert!(p.background_requests["r4"][1].reason.contains("carry-over"));
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
        let c = &claude(&d).background_directories[&text(&nested)][1];
        assert_eq!(c.value, Some(true));
        assert_eq!(c.off_file.as_deref(), Some(text(&local).as_str()));
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
            &format!("gitdir: {}\n", text(&main.join(".git/worktrees/branch"))),
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
        let c = &claude(&d).background_directories[&text(&work)][1];
        assert_eq!(c.value, Some(false));
        assert_eq!(c.off_file.as_deref(), Some(text(&local).as_str()));
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
            &format!("gitdir: {}\n", text(&module.join("worktrees/branch"))),
        );
        write(module.join("worktrees/branch/commondir"), "../..\n");
        write(
            work.join(".claude/settings.local.json"),
            r#"{"awaySummaryEnabled":false}"#,
        );
        let mut d = discover(&options(&w)).unwrap();
        let r = request(claude(&d), Some(&work));
        discover_claude_controls(&options(&w), &mut d.profiles, &[r]).unwrap();
        let c = &claude(&d).background_directories[&text(&work)][1];
        assert_eq!(c.value, Some(false));
        let local = work.join(".claude/settings.local.json");
        assert_eq!(c.off_file.as_deref(), Some(text(&local).as_str()));
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
                serde_json::to_string(&text(&root)).unwrap()
            ),
        );
        let mut d = discover(&options(&w)).unwrap();
        let p = d
            .profiles
            .iter()
            .find(|p| p.harness == Harness::Codex)
            .unwrap();
        let r = Request {
            harness: Harness::Codex,
            ..request(p, Some(&cwd))
        };
        discover_codex_controls(&options(&w), &mut d.profiles, std::slice::from_ref(&r)).unwrap();
        let p = d
            .profiles
            .iter()
            .find(|p| p.harness == Harness::Codex)
            .unwrap();
        // Trusted project overrides rank above every unobserved named profile's recap value.
        let c = &p.background_directories[&text(&cwd)][0];
        assert_eq!(c.value, Some(true));
        assert_eq!(
            c.off_file.as_deref(),
            Some(text(&cwd.join(".codex/config.toml")).as_str())
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
        assert_eq!(p.background_directories[&text(&cwd)][0].value, None);
        fs::remove_file(w.codex.join("quiet.config.toml")).unwrap();
        // Empty root markers stop at cwd, including trust selection.
        write(
            w.codex.join("config.toml"),
            &format!(
                "project_root_markers = []\n[projects.{}]\ntrust_level = 'trusted'\n",
                serde_json::to_string(&text(&cwd)).unwrap()
            ),
        );
        discover_codex_controls(&options(&w), &mut d.profiles, std::slice::from_ref(&r)).unwrap();
        let p = d
            .profiles
            .iter()
            .find(|p| p.harness == Harness::Codex)
            .unwrap();
        assert_eq!(p.background_directories[&text(&cwd)][0].value, Some(true));
        // A worktree's .git file is a boundary, even inside a different trusted checkout.
        write(cwd.join(".git"), "gitdir: ../elsewhere\n");
        write(
            w.codex.join("config.toml"),
            &format!(
                "[projects.{}]\ntrust_level = 'trusted'\n",
                serde_json::to_string(&text(&root)).unwrap()
            ),
        );
        discover_codex_controls(&options(&w), &mut d.profiles, std::slice::from_ref(&r)).unwrap();
        let p = d
            .profiles
            .iter()
            .find(|p| p.harness == Harness::Codex)
            .unwrap();
        assert_eq!(
            p.background_directories[&text(&cwd)][0].value,
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
                serde_json::to_string(&text(&root)).unwrap()
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
        let req = Request {
            harness: Harness::Codex,
            profiles: vec![id.clone()],
            cwd: Some(text(&cwd)),
            ..make_request()
        };
        let recap = |d: &Discovery| {
            d.profiles
                .iter()
                .find(|p| p.id == id)
                .unwrap()
                .background_directories[&text(&cwd)][0]
                .clone()
        };
        discover_codex_controls(&options(&w), &mut d.profiles, std::slice::from_ref(&req)).unwrap();
        assert_eq!(recap(&d).value, Some(true));
        assert_eq!(recap(&d).off_file.as_deref(), Some(text(&nearest).as_str()));
        assert!(
            !serde_json::to_string(&d.profiles)
                .unwrap()
                .contains("SYNTHETIC_PRIVATE")
        );
        write(w.codex.join("config.toml"), "[tui]\nauto_recap = false\n");
        discover_codex_controls(&options(&w), &mut d.profiles, std::slice::from_ref(&req)).unwrap();
        assert_eq!(recap(&d).value, None);
        assert!(recap(&d).reason.contains("trust is unknown"));
        write(
            w.codex.join("config.toml"),
            &format!(
                "[tui]\nauto_recap = false\n[projects.{}]\ntrust_level = 'untrusted'\n",
                serde_json::to_string(&text(&root)).unwrap()
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
        let gone = Request {
            cwd: Some(text(&w.home.join("gone"))),
            ..req.clone()
        };
        let absent = Request { cwd: None, ..req };
        discover_codex_controls(&options(&w), &mut d.profiles, &[gone.clone(), absent]).unwrap();
        let p = d.profiles.iter().find(|p| p.id == id).unwrap();
        assert!(
            p.background_directories[""]
                .iter()
                .all(|c| c.value.is_none())
        );
        assert!(
            p.background_directories[&gone.cwd.unwrap()][0]
                .value
                .is_none()
        );
    }

    // Ported from the discovery half of idle.test.ts: "native informational notices are the only parsed idle attribution; project owners and threshold floor resolve".
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
            Request {
                agent: crate::model::Agent::Subagent,
                ..request(&profile, None)
            },
        ];
        discover_claude_controls(&options(&w), &mut data.profiles, &requests).unwrap();
        let profile = claude(&data);
        assert!(
            profile.idle.as_ref().unwrap().resolved,
            "subagent cwd is not consulted"
        );
        let directories = profile.idle_directories.as_ref().unwrap();
        let owner = &directories[&text(&project.join("nested"))];
        assert_eq!(owner.value, Some(false));
        assert_eq!(owner.threshold, Some(100_000));
        assert_eq!(owner.owner, format!("{}:idleCompaction", text(&local)));
        assert_eq!(
            owner.ttl.as_ref().unwrap().main.value,
            Some(Ttl::FiveMinutes)
        );
        assert_eq!(
            owner.ttl.as_ref().unwrap().main.owner,
            format!("{}:promptCacheTtl", text(&local))
        );
        assert!(owner.resolved, "the nested directory exists");
        let missing = &directories[&text(&gone)];
        assert!(!missing.resolved);
        assert_eq!(
            missing.reason,
            "recorded working directory no longer exists; current project/local ownership is unresolved"
        );
        let speed = &profile.speed_directories.as_ref().unwrap()[&text(&gone)];
        assert_eq!(speed.value, None);

        let absent = [request(claude(&data), None)];
        discover_claude_controls(&options(&w), &mut data.profiles, &absent).unwrap();
        let profile = claude(&data);
        assert!(!profile.idle.as_ref().unwrap().resolved);
        assert!(profile.idle_directories.as_ref().unwrap().is_empty());
    }
}
