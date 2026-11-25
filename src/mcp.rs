//! Where an MCP server is configured, so `token-max doctor` can offer to remove one that
//! is off: Claude Code's user- and local-scope servers in its state file, such as
//! `~/.claude.json`, and Codex's `[mcp_servers.<id>]` tables in `config.toml`.
//!
//! A project's `.mcp.json` is shared with everyone who uses the project, and a plugin's
//! server, a claude.ai connector, or one from `--mcp-config` or `managed-mcp.json` is not
//! in any file token-max edits. Claude Code's `deniedMcpServers` blocks each of them by
//! name instead, where its name can be spelled exactly and the Claude Code in use matches
//! it; otherwise it is a step by hand.

use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::collections::IndexSet;
use crate::controls::members;
use crate::discovery::{join, realpath, settings};
use crate::model::{Control, Harness, Profile};

/// One place an MCP server is configured.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Definition {
    /// An entry token-max can delete.
    Entry(Entry),
    /// What to do by hand, and why token-max cannot.
    Manual { step: String, reason: String },
    /// A server token-max cannot remove, which a `deniedMcpServers` entry blocks.
    Deny(Deny),
}

/// A server `deniedMcpServers` blocks by name, and what to do by hand where it cannot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Deny {
    /// The name its entry matches, such as `claude.ai Slack` or `plugin:github:github`.
    pub name: String,
    /// The first Claude Code, as 2.`minor`.`patch`, whose name entry blocks just this server.
    pub since: (u64, u64),
    /// What to do by hand before that version.
    pub step: String,
    /// Why token-max cannot remove it.
    pub reason: String,
}

impl Deny {
    /// Its step by hand, since `why` also keeps token-max from blocking it.
    pub fn by_hand(self, why: &str) -> Definition {
        Definition::Manual {
            step: self.step,
            reason: format!("{}, and {why}", self.reason),
        }
    }
}

/// Versions measured by loading each kind of server under a name entry: a plugin's
/// server from 2.1.182 and an `--mcp-config` one from 2.1.83, while a project's
/// `.mcp.json` and `managed-mcp.json` already matched in 2.1.0, the oldest tried. Before
/// 2.1.330, a connector's name entry also hid other connectors at its URL.
const PLUGIN: (u64, u64) = (1, 182);
const CONNECTOR: (u64, u64) = (1, 330);
const PROJECT: (u64, u64) = (1, 0);
const ELSEWHERE: (u64, u64) = (1, 83);

/// A server's entry in a configuration file: the key at `path`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Entry {
    /// The server's name as its file declares it.
    pub server: String,
    pub file: String,
    pub harness: Harness,
    pub path: Vec<String>,
}

impl Entry {
    /// The entry as a person finds it in its file, such as `mcpServers.docs` or
    /// `[mcp_servers.docs]`.
    pub fn text(&self) -> String {
        match (self.harness, self.path.as_slice()) {
            (Harness::Claude, [_, project, _, key]) => {
                format!("mcpServers.{key} of project {project}")
            }
            (Harness::Claude, _) => format!("mcpServers.{}", self.server),
            (Harness::Codex, _) => {
                let key = toml_edit::Key::new(self.server.as_str());
                format!("[mcp_servers.{}]", key.display_repr())
            }
        }
    }

    /// Whether Claude Code rewrites its file while it runs, so a write takes its lock.
    pub fn live(&self) -> bool {
        self.harness == Harness::Claude
    }
}

/// Where a server that was never used, named as its tools name it, is configured for
/// `profiles`, the profiles whose settings `control` turns it off in. None when the server
/// has no configuration of its own, such as Codex's apps.
pub fn definitions(server: &str, control: &Control, profiles: &[&Profile]) -> Vec<Definition> {
    let Some(profile) = profiles.first() else {
        return vec![];
    };
    if profile.harness == Harness::Codex {
        let [table, id, _] = control.path.as_slice() else {
            return vec![];
        };
        if table != "mcp_servers" {
            return vec![];
        }
        let file = &profile.settings_file;
        let step = format!("remove [mcp_servers.{id}] from {file}");
        // The scan saw the table, but the file may have changed since; with no table to
        // remove, removing the path would also undo the turn-off edit it follows.
        let declared = settings(file, Harness::Codex)
            .map(|user| members(user.get("mcp_servers")).contains_key(id.as_str()));
        if !matches!(declared, Ok(true)) {
            return vec![Definition::Manual {
                step: "remove it where it is configured, such as a -c override or a project's config.toml".into(),
                reason: match declared {
                    Err(e) => e.message,
                    _ => format!("{file} does not declare [mcp_servers.{id}]"),
                },
            }];
        }
        // Every profile on the line shares the file but may outrank it with layers of its own.
        let layers = profiles.iter().try_fold(vec![], |mut all: Vec<String>, p| {
            for layer in codex_layers(p, id)? {
                if !all.contains(&layer) {
                    all.push(layer);
                }
            }
            Ok::<_, crate::safe::ScanError>(all)
        });
        return match layers {
            Ok(layers) if layers.is_empty() => vec![Definition::Entry(Entry {
                server: id.clone(),
                file: file.clone(),
                harness: Harness::Codex,
                path: vec![table.clone(), id.clone()],
            })],
            Ok(layers) => vec![Definition::Manual {
                step,
                reason: format!(
                    "{} also declares it, which removing this table would leave in effect",
                    layers.join(", ")
                ),
            }],
            Err(e) => vec![Definition::Manual {
                step,
                reason: format!(
                    "{}, so whether another layer relies on it is unknown",
                    e.message
                ),
            }],
        };
    }
    // A plugin's server and a claude.ai connector live in no file token-max reads, but a
    // declared key may spell the same prefix.
    let source = if let Some(rest) = server.strip_prefix("plugin_") {
        // Each profile installs its own plugins, and another may share this one, so the step
        // names the profiles whose settings turn the server off.
        let roots: Vec<&str> = profiles.iter().map(|p| p.root.as_str()).collect();
        let step = format!(
            "uninstall or disable the plugin that provides it, with /plugin in Claude Code's {} at {}",
            if roots.len() > 1 {
                "profiles"
            } else {
                "profile"
            },
            roots.join(" and "),
        );
        let reason = "a plugin adds it";
        // Claude Code names it `plugin:<plugin>:<server>`, whose colons its tools carry as
        // `_`; with no other `_` in the prefix, neither name held one or a character read
        // as one.
        Some(match rest.split_once('_') {
            Some((plugin, name))
                if !plugin.is_empty() && !name.is_empty() && !name.contains('_') =>
            {
                Definition::Deny(Deny {
                    name: format!("plugin:{plugin}:{name}"),
                    since: PLUGIN,
                    step: step.clone(),
                    reason: reason.into(),
                })
            }
            _ => Definition::Manual {
                step,
                reason: format!("{reason}, and its tools' prefix does not spell its name"),
            },
        })
    } else if server.starts_with("claude_ai_") {
        Some(connector(server, &state_files(profiles)))
    } else {
        None
    };
    let mut found = vec![];
    let mut files = vec![];
    for file in state_files(profiles) {
        let config = match settings(&file, Harness::Claude) {
            Ok(config) => config,
            Err(e) => {
                found.push(Definition::Manual {
                    step: format!("remove it from {file}"),
                    reason: format!("{} ({file})", e.message),
                });
                continue;
            }
        };
        found.extend(
            entries(&file, &config)
                .into_iter()
                .filter(|e| tool_name(&e.server) == server)
                .map(Definition::Entry),
        );
        for project in members(config.get("projects")).keys() {
            let shared = join(&[project, ".mcp.json"]);
            // A project that no longer parses has nothing Claude Code would start.
            let Ok(config) = settings(&shared, Harness::Claude) else {
                continue;
            };
            // Each key its tools' prefix may have come from.
            for key in members(config.get("mcpServers"))
                .keys()
                .filter(|k| tool_name(k) == server)
            {
                found.push(Definition::Deny(Deny {
                    name: key.clone(),
                    since: PROJECT,
                    step: format!("remove it from {shared}"),
                    reason: "a project's .mcp.json is shared with everyone who uses the project"
                        .into(),
                }));
            }
        }
        files.push(file);
    }
    if let Some(source) = source {
        if found.is_empty() {
            return vec![source];
        }
        // Either may have given its tools this prefix, so no name is known to block them.
        let why = "a declared server and a plugin or connector give their tools the same prefix";
        found.push(source);
        return found
            .into_iter()
            .map(|d| match d {
                Definition::Deny(deny) => deny.by_hand(why),
                d => d,
            })
            .collect();
    }
    if found.is_empty() {
        let searched = if files.is_empty() {
            "a project's .mcp.json".to_string()
        } else {
            format!("{} or a project's .mcp.json", files.join(", "))
        };
        let step =
            "remove it where it is configured, such as a --mcp-config file or managed-mcp.json";
        let reason = format!("token-max did not find it in {searched}");
        // Only a prefix with no `_` is the name it came from, unchanged.
        found.push(if server.contains('_') {
            Definition::Manual {
                step: step.into(),
                reason: format!("{reason}, and its tools' prefix does not spell its name"),
            }
        } else {
            Definition::Deny(Deny {
                name: server.into(),
                since: ELSEWHERE,
                step: step.into(),
                reason,
            })
        });
    }
    found
}

/// A claude.ai connector, named as its tools name it: blocked by the one display name, such
/// as `claude.ai Slack`, that a state file records it connected under and that gives its
/// tools this prefix.
fn connector(server: &str, files: &[String]) -> Definition {
    let step = "disconnect it at claude.ai, under Settings > Connectors";
    let reason = "it is a claude.ai connector";
    let mut names: IndexSet<String> = IndexSet::default();
    for file in files {
        let state = match settings(file, Harness::Claude) {
            Ok(state) => state,
            Err(e) => {
                return Definition::Manual {
                    step: step.into(),
                    reason: format!("{reason}, and {} ({file})", e.message),
                };
            }
        };
        let connected = state
            .get("claudeAiMcpEverConnected")
            .and_then(Value::as_array);
        names.extend(
            connected
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter(|name| tool_name(name) == server)
                .map(String::from),
        );
    }
    let mut names = names.into_iter();
    match (names.next(), names.next()) {
        (Some(name), None) => Definition::Deny(Deny {
            name,
            since: CONNECTOR,
            step: step.into(),
            reason: reason.into(),
        }),
        (None, _) => Definition::Manual {
            step: step.into(),
            reason: format!("{reason} whose name no state file records"),
        },
        (Some(_), Some(_)) => Definition::Manual {
            step: step.into(),
            reason: format!(
                "{reason}, and more than one of its recorded names gives its tools' prefix"
            ),
        },
    }
}

/// Servers configured for `profiles` whose tools are all off already: a Claude server
/// whose tools the user's `permissions.deny` denies, or one a project turned off with
/// `/mcp`, and a Codex server with `enabled = false`. A file that cannot be read is a step
/// by hand.
pub fn turned_off(profiles: &[Profile]) -> Vec<Definition> {
    let mut found: IndexSet<Entry> = IndexSet::default();
    // Codex tables some profile sharing their file keeps, by the file's resolved path:
    // another layer of that profile declares the server, or its layers cannot be read.
    let mut kept: IndexSet<(String, String)> = IndexSet::default();
    let resolved = |file: &str| realpath(file).unwrap_or_else(|_| file.to_string());
    let mut manual = vec![];
    let mut unreadable = |file: &str, e: crate::safe::ScanError| {
        let at = e.location.as_ref().map_or(file, |l| &*l.file);
        manual.push(Definition::Manual {
            step: format!("look in {file} for MCP servers that are off"),
            reason: format!("{} ({at})", e.message),
        })
    };
    for profile in profiles {
        let config = match settings(&profile.settings_file, profile.harness) {
            Ok(config) => config,
            Err(e) => {
                unreadable(&profile.settings_file, e);
                continue;
            }
        };
        if profile.harness == Harness::Codex {
            for (id, table) in members(config.get("mcp_servers")) {
                if table.get("enabled") != Some(&Value::Bool(false)) {
                    continue;
                }
                // Another layer that declares it may turn it back on, or start it once this
                // table is gone.
                match codex_layers(profile, id) {
                    Ok(layers) if layers.is_empty() => {}
                    Ok(_) => {
                        kept.insert((resolved(&profile.settings_file), id.clone()));
                        continue;
                    }
                    Err(e) => {
                        kept.insert((resolved(&profile.settings_file), id.clone()));
                        unreadable(&profile.settings_file, e);
                        continue;
                    }
                }
                found.insert(Entry {
                    server: id.clone(),
                    file: profile.settings_file.clone(),
                    harness: Harness::Codex,
                    path: vec!["mcp_servers".into(), id.clone()],
                });
            }
            continue;
        }
        let denied: Vec<&str> = members(config.get("permissions"))
            .get("deny")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let denies = |server: &str| {
            let name = tool_name(server);
            denied
                .iter()
                .any(|rule| *rule == format!("mcp__{name}") || *rule == format!("mcp__{name}__*"))
        };
        for file in state_files(&[profile]) {
            let state = match settings(&file, Harness::Claude) {
                Ok(state) => state,
                Err(e) => {
                    unreadable(&file, e);
                    continue;
                }
            };
            let projects = members(state.get("projects"));
            for entry in entries(&file, &state) {
                // A local-scope server can be turned off in its project alone.
                let off_here = match entry.path.as_slice() {
                    [_, project, _, key] => members(projects.get(project))
                        .get("disabledMcpServers")
                        .and_then(Value::as_array)
                        .is_some_and(|list| list.iter().any(|s| s.as_str() == Some(key))),
                    _ => false,
                };
                if off_here || denies(&entry.server) {
                    found.insert(entry);
                }
            }
        }
    }
    found
        .into_iter()
        .filter(|e| {
            e.harness != Harness::Codex || !kept.contains(&(resolved(&e.file), e.path[1].clone()))
        })
        .map(Definition::Entry)
        .chain(manual)
        .collect()
}

/// The other Codex layers that declare the server `id`: those that outrank the profile's
/// `config.toml` for some recorded session, a project's or named profile's file or a named
/// profile table in `config.toml` itself, which may rely on the user's table, and the
/// system `config.toml` below it, whose declaration would take effect once the user's
/// table that turns it off is gone.
fn codex_layers(profile: &Profile, id: &str) -> Result<Vec<String>, crate::safe::ScanError> {
    let declares =
        |config: &Map<String, Value>| members(config.get("mcp_servers")).contains_key(id);
    let mut layers = vec![];
    for file in profile
        .outranking_files
        .iter()
        .chain(&profile.codex_system_config)
    {
        if declares(&settings(file, Harness::Codex)?) {
            layers.push(file.clone());
        }
    }
    let user = settings(&profile.settings_file, Harness::Codex)?;
    for (name, table) in members(user.get("profiles")) {
        if declares(members(Some(table))) {
            layers.push(format!("{}:profiles.{name}", profile.settings_file));
        }
    }
    Ok(layers)
}

/// Each Claude Code state file the profiles use, once.
fn state_files(profiles: &[&Profile]) -> Vec<String> {
    let mut files: IndexSet<String> = IndexSet::default();
    let mut seen: IndexSet<String> = IndexSet::default();
    for file in profiles.iter().filter_map(|p| p.claude_config.as_ref()) {
        if seen.insert(realpath(file).unwrap_or_else(|_| file.clone())) {
            files.insert(file.clone());
        }
    }
    files.into_iter().collect()
}

/// The user- and local-scope servers a Claude Code state file declares.
fn entries(file: &str, config: &Map<String, Value>) -> Vec<Entry> {
    let entry = |path: Vec<String>, server: &str| Entry {
        server: server.into(),
        file: file.into(),
        harness: Harness::Claude,
        path,
    };
    let mut found: Vec<Entry> = members(config.get("mcpServers"))
        .keys()
        .map(|k| entry(vec!["mcpServers".into(), k.clone()], k))
        .collect();
    for (project, state) in members(config.get("projects")) {
        for key in members(state.get("mcpServers")).keys() {
            let path = vec![
                "projects".into(),
                project.clone(),
                "mcpServers".into(),
                key.clone(),
            ];
            found.push(entry(path, key));
        }
    }
    found
}

/// A Claude server's name as its tools carry it, `mcp__<name>__<tool>`. A claude.ai
/// connector's, such as `claude.ai Slack (2)`, also collapses runs of `_` and trims them
/// from its ends, to `claude_ai_Slack_2`, as Claude Code does. Its JavaScript writes a `_`
/// for each UTF-16 unit, so a character outside the BMP, such as an emoji, gives two.
pub(crate) fn tool_name(server: &str) -> String {
    let mut name = String::with_capacity(server.len());
    for c in server.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            name.push(c);
        } else {
            name.extend(std::iter::repeat_n('_', c.len_utf16()));
        }
    }
    if !server.starts_with("claude.ai ") {
        return name;
    }
    let mut collapsed = String::with_capacity(name.len());
    for c in name.chars() {
        if !(c == '_' && collapsed.ends_with('_')) {
            collapsed.push(c);
        }
    }
    collapsed.trim_matches('_').into()
}

/// Claude Code's lock on its state file, held while token-max rewrites it. Claude Code
/// saves the file under the same lock, re-reading it first, so a removal made under it
/// survives sessions that are running. Like Claude Code's lock library, it refreshes the
/// lock's time while held, so no one takes it over as stale, and releases it only while
/// it is still the one that set that time.
pub(crate) struct Lock {
    dir: PathBuf,
    /// The time this holder last gave the lock; None once another holder replaced it.
    owned: Arc<Mutex<Option<SystemTime>>>,
    refresher: Option<std::thread::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

/// A lock not refreshed for this long belongs to a process that exited, as Claude Code's
/// lock library treats it.
const STALE: Duration = Duration::from_secs(10);

/// How often a held lock's time is refreshed, well within `STALE`.
const REFRESH: Duration = Duration::from_secs(1);

/// How long to wait for a Claude Code session to finish its save.
pub(crate) const WAIT: Duration = Duration::from_secs(5);

impl Lock {
    /// Take the lock beside `file`, waiting up to `wait` for a session that holds it.
    pub(crate) fn take(file: &Path, wait: Duration) -> io::Result<Lock> {
        let mut name = file.as_os_str().to_owned();
        name.push(".lock");
        let dir = PathBuf::from(name);
        let started = SystemTime::now();
        loop {
            match fs::create_dir(&dir) {
                Ok(()) => return Lock::hold(dir),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
            if started.elapsed().unwrap_or_default() >= wait {
                // A stale lock is left for Claude Code's own lock library to take over:
                // removing it here could remove a fresh one another contender just made.
                let stale = modified(&dir).is_ok_and(|at| at.elapsed().unwrap_or_default() > STALE);
                let held = if stale {
                    "is left from a process that exited; remove it if no Claude Code is running"
                        .to_string()
                } else {
                    format!(
                        "was held by Claude Code for over {} seconds",
                        wait.as_secs()
                    )
                };
                return Err(io::Error::other(format!("{} {held}", dir.display())));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Hold the lock directory just made, refreshing its time until it is dropped.
    fn hold(dir: PathBuf) -> io::Result<Lock> {
        let set = match touch(&dir) {
            Ok(set) => set,
            Err(e) => {
                let _ = fs::remove_dir(&dir);
                return Err(e);
            }
        };
        let owned = Arc::new(Mutex::new(Some(set)));
        let stop = Arc::new(AtomicBool::new(false));
        let refresher = {
            let (dir, owned, stop) = (dir.clone(), owned.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    std::thread::park_timeout(REFRESH);
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    let mut owned = owned.lock().unwrap_or_else(|e| e.into_inner());
                    let ours = owned.is_some() && modified(&dir).ok() == *owned;
                    *owned = if ours { touch(&dir).ok() } else { None };
                    if owned.is_none() {
                        break;
                    }
                }
            })
        };
        Ok(Lock {
            dir,
            owned,
            refresher: Some(refresher),
            stop,
        })
    }

    /// Whether this holder still has the lock: it refreshed it in time, and no one
    /// replaced it.
    pub(crate) fn held(&self) -> bool {
        let owned = self.owned.lock().unwrap_or_else(|e| e.into_inner());
        owned.is_some_and(|set| {
            modified(&self.dir).ok() == Some(set) && set.elapsed().unwrap_or_default() < STALE
        })
    }
}

/// Set the lock directory's time to now, returning the time it reads back.
fn touch(dir: &Path) -> io::Result<SystemTime> {
    #[cfg(windows)]
    let handle = {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_BACKUP_SEMANTICS, which opening a directory needs.
        fs::OpenOptions::new()
            .write(true)
            .custom_flags(0x0200_0000)
            .open(dir)?
    };
    #[cfg(not(windows))]
    let handle = fs::File::open(dir)?;
    handle.set_modified(SystemTime::now())?;
    modified(dir)
}

fn modified(dir: &Path) -> io::Result<SystemTime> {
    fs::metadata(dir)?.modified()
}

impl Drop for Lock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(refresher) = self.refresher.take() {
            refresher.thread().unpark();
            let _ = refresher.join();
        }
        // A lock someone else took over is theirs to release.
        let owned = *self.owned.lock().unwrap_or_else(|e| e.into_inner());
        if owned.is_some() && modified(&self.dir).ok() == owned {
            let _ = fs::remove_dir(&self.dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ControlValue;
    use crate::test_support::{Workspace, make_profile};

    fn write(path: impl AsRef<Path>, text: &str) {
        let path = path.as_ref();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn deny(server: &str) -> Control {
        Control {
            path: vec!["permissions".into(), "deny".into()],
            value: ControlValue::Text(format!("mcp__{server}")),
            append: true,
        }
    }

    fn claude(w: &Workspace) -> Profile {
        let root = w.claude.to_string_lossy().into_owned();
        let mut p = make_profile("claude", &root);
        p.claude_config = Some(w.home.join(".claude.json").to_string_lossy().into_owned());
        p
    }

    #[test]
    fn a_claude_server_is_found_in_each_scope_its_state_file_and_projects_declare() {
        let w = Workspace::new();
        let project = w.home.join("project");
        let project = project.to_string_lossy();
        write(
            w.home.join(".claude.json"),
            &serde_json::json!({
                "mcpServers": { "docs.v2": {}, "other": {} },
                "projects": { project.as_ref(): { "mcpServers": { "docs.v2": {} } } },
            })
            .to_string(),
        );
        write(
            format!("{project}/.mcp.json"),
            r#"{"mcpServers": {"docs.v2": {}}}"#,
        );
        let p = claude(&w);
        let file = p.claude_config.clone().unwrap();
        let found = definitions("docs_v2", &deny("docs_v2"), &[&p]);
        let entry = |path: &[&str]| {
            Definition::Entry(Entry {
                server: "docs.v2".into(),
                file: file.clone(),
                harness: Harness::Claude,
                path: path.iter().map(|s| s.to_string()).collect(),
            })
        };
        assert_eq!(found.len(), 3, "{found:?}");
        assert_eq!(found[0], entry(&["mcpServers", "docs.v2"]));
        assert_eq!(
            found[1],
            entry(&["projects", &project, "mcpServers", "docs.v2"])
        );
        // A project's .mcp.json is denied by the key it declares, which its tools spell
        // as `docs_v2`.
        let Definition::Deny(shared) = &found[2] else {
            panic!("{found:?}")
        };
        assert_eq!(shared.name, "docs.v2");
        assert_eq!(shared.since, PROJECT);
        assert_eq!(
            shared.step,
            format!(
                "remove it from {}",
                Path::new(project.as_ref()).join(".mcp.json").display()
            )
        );
        let Definition::Entry(local) = &found[1] else {
            unreachable!()
        };
        assert_eq!(
            local.text(),
            format!("mcpServers.docs.v2 of project {project}")
        );
    }

    #[test]
    fn a_server_token_max_cannot_remove_is_denied_by_the_name_it_can_spell() {
        let w = Workspace::new();
        write(
            w.home.join(".claude.json"),
            &serde_json::json!({
                "claudeAiMcpEverConnected": [
                    "claude.ai Google Drive", "claude.ai Slack (2)", "claude.ai A.B", "claude.ai A B",
                ],
            })
            .to_string(),
        );
        let p = claude(&w);
        let one = |server: &str| match definitions(server, &deny(server), &[&p]).as_slice() {
            [one] => one.clone(),
            all => panic!("{server}: {all:?}"),
        };
        let denied = |server: &str| match one(server) {
            Definition::Deny(d) => (d.name, d.since),
            other => panic!("{server}: {other:?}"),
        };
        let by_hand = |server: &str| match one(server) {
            Definition::Manual { reason, .. } => reason,
            other => panic!("{server}: {other:?}"),
        };
        // A connector by the display name its tools came from, the suffix's `__` collapsed.
        assert_eq!(
            denied("claude_ai_Google_Drive"),
            ("claude.ai Google Drive".into(), CONNECTOR)
        );
        assert_eq!(
            denied("claude_ai_Slack_2"),
            ("claude.ai Slack (2)".into(), CONNECTOR)
        );
        assert!(by_hand("claude_ai_Gmail").ends_with("whose name no state file records"));
        assert!(by_hand("claude_ai_A_B").contains("more than one of its recorded names"));
        // A plugin's server only when the prefix splits into its two names one way.
        assert_eq!(
            denied("plugin_github_github"),
            ("plugin:github:github".into(), PLUGIN)
        );
        assert_eq!(
            denied("plugin_my-tools_my-docs"),
            ("plugin:my-tools:my-docs".into(), PLUGIN)
        );
        // Each profile installs its own plugins, so the step names the ones the change covers.
        let root = |p: &Profile| p.root.clone();
        let step = |profiles: &[&Profile]| match definitions(
            "plugin_github_github",
            &deny("plugin_github_github"),
            profiles,
        )
        .as_slice()
        {
            [Definition::Deny(d)] => d.step.clone(),
            all => panic!("{all:?}"),
        };
        assert_eq!(
            step(&[&p]),
            format!(
                "uninstall or disable the plugin that provides it, with /plugin in Claude Code's profile at {}",
                root(&p)
            )
        );
        let mut work = p.clone();
        work.root = format!("{}-work", p.root);
        assert!(
            step(&[&p, &work]).ends_with(&format!(
                "Claude Code's profiles at {} and {}",
                root(&p),
                root(&work)
            )),
            "{}",
            step(&[&p, &work])
        );
        for server in ["plugin_a_b_c", "plugin_github", "plugin__x"] {
            assert!(
                by_hand(server).ends_with("does not spell its name"),
                "{server}"
            );
        }
        // A declared key that spells a plugin's or connector's prefix leaves both by hand.
        let project = w.home.join("project");
        let project = project.to_string_lossy();
        write(
            format!("{project}/.mcp.json"),
            r#"{"mcpServers": {"plugin_github_github": {}, "claude_ai_Slack_2": {}}}"#,
        );
        let mut state: Value =
            serde_json::from_str(&fs::read_to_string(w.home.join(".claude.json")).unwrap())
                .unwrap();
        state["projects"] = serde_json::json!({ project.as_ref(): {} });
        write(w.home.join(".claude.json"), &state.to_string());
        for server in ["plugin_github_github", "claude_ai_Slack_2"] {
            let found = definitions(server, &deny(server), &[&p]);
            assert_eq!(found.len(), 2, "{found:?}");
            assert!(
                found.iter().all(|d| matches!(
                    d, Definition::Manual { reason, .. } if reason.ends_with("the same prefix")
                )),
                "{found:?}"
            );
        }
        assert_eq!(tool_name("docs\u{1F600}"), "docs__");
        assert_eq!(tool_name("claude.ai _A \u{1F600}_"), "claude_ai_A");
        // One found in no file only when its prefix is its name unchanged.
        assert_eq!(denied("docs-v2"), ("docs-v2".into(), ELSEWHERE));
        assert!(by_hand("my_docs").ends_with("does not spell its name"));
    }

    #[test]
    fn a_codex_server_is_its_table_and_codex_apps_have_none() {
        let w = Workspace::new();
        let root = w.codex.to_string_lossy().into_owned();
        let mut p = make_profile("codex", &root);
        p.harness = Harness::Codex;
        p.settings_file = format!("{root}/config.toml");
        let off = |path: &[&str]| Control {
            path: path.iter().map(|s| s.to_string()).collect(),
            value: ControlValue::Bool(false),
            append: false,
        };
        // A table the user's file no longer declares is not offered: removing its path
        // would only undo the turn-off edit that creates it.
        write(&p.settings_file, "model = \"m\"\n");
        let control = off(&["mcp_servers", "my.docs", "enabled"]);
        let found = definitions("my_docs", &control, &[&p]);
        let [Definition::Manual { reason, .. }] = found.as_slice() else {
            panic!("{found:?}")
        };
        assert!(
            reason.ends_with(r#"does not declare [mcp_servers.my.docs]"#),
            "{reason}"
        );
        write(
            &p.settings_file,
            "[mcp_servers.\"my.docs\"]\ncommand = \"x\"\n",
        );
        let found = definitions(
            "my_docs",
            &off(&["mcp_servers", "my.docs", "enabled"]),
            &[&p],
        );
        let [Definition::Entry(entry)] = found.as_slice() else {
            panic!("{found:?}")
        };
        assert_eq!(entry.path, ["mcp_servers", "my.docs"]);
        assert_eq!(entry.text(), r#"[mcp_servers."my.docs"]"#);
        assert!(definitions("codex_apps", &off(&["features", "apps"]), &[&p]).is_empty());

        // A project or named profile that configures it too may rely on the user's table.
        let project = w.home.join("project/.codex/config.toml");
        write(&project, "[mcp_servers.\"my.docs\"]\nenabled = true\n");
        p.outranking_files = vec![project.to_string_lossy().into_owned()];
        write(
            &p.settings_file,
            "[mcp_servers.\"my.docs\"]\nenabled = false\n",
        );
        let control = off(&["mcp_servers", "my.docs", "enabled"]);
        let found = definitions("my_docs", &control, &[&p]);
        let [Definition::Manual { reason, .. }] = found.as_slice() else {
            panic!("{found:?}")
        };
        assert!(reason.starts_with(&p.outranking_files[0]), "{reason}");
        assert!(turned_off(&[p.clone()]).is_empty());
        // So does another profile on the line that shares the file.
        let mut other = p.clone();
        p.outranking_files.clear();
        assert!(matches!(
            definitions("my_docs", &control, &[&p, &other]).as_slice(),
            [Definition::Manual { .. }]
        ));
        assert!(turned_off(&[p.clone(), other.clone()]).is_empty());
        other.outranking_files.clear();
        assert!(matches!(
            definitions("my_docs", &control, &[&p, &other]).as_slice(),
            [Definition::Entry(_)]
        ));
        assert_eq!(turned_off(&[p.clone(), other.clone()]).len(), 1);
        // The system config declares it below the user's file: removing the user's table
        // would start it again.
        let system = w.home.join("etc/codex/config.toml");
        write(&system, "[mcp_servers.\"my.docs\"]\ncommand = \"x\"\n");
        p.codex_system_config = Some(system.to_string_lossy().into_owned());
        assert!(matches!(
            definitions("my_docs", &control, &[&p]).as_slice(),
            [Definition::Manual { .. }]
        ));
        assert!(turned_off(&[p.clone()]).is_empty());
        p.codex_system_config = None;
        write(
            &p.settings_file,
            "[mcp_servers.\"my.docs\"]\nenabled = false\n[profiles.work.mcp_servers.\"my.docs\"]\nenabled = true\n",
        );
        assert!(matches!(
            definitions("my_docs", &control, &[&p]).as_slice(),
            [Definition::Manual { .. }]
        ));
        assert!(turned_off(&[p]).is_empty());
    }

    #[test]
    fn turned_off_servers_are_those_denied_disabled_in_their_project_or_not_enabled() {
        let w = Workspace::new();
        let project = w.home.join("project").to_string_lossy().into_owned();
        write(
            w.home.join(".claude.json"),
            &serde_json::json!({
                "mcpServers": { "denied": {}, "wildcard": {}, "on": {}, "one-tool": {} },
                "projects": { &project: {
                    "mcpServers": { "local": {}, "kept": {} },
                    "disabledMcpServers": ["local", "on"],
                } },
            })
            .to_string(),
        );
        write(
            w.claude.join("settings.json"),
            r#"{"permissions": {"deny": ["mcp__denied", "mcp__wildcard__*", "mcp__one-tool__read"]}}"#,
        );
        let mut codex = make_profile("codex", &w.codex.to_string_lossy());
        codex.harness = Harness::Codex;
        codex.settings_file = w.codex.join("config.toml").to_string_lossy().into_owned();
        write(
            &codex.settings_file,
            "[mcp_servers.off]\nenabled = false\n[mcp_servers.live]\ncommand = \"x\"\n",
        );
        let found = turned_off(&[claude(&w), claude(&w), codex]);
        let names: Vec<String> = found
            .iter()
            .map(|d| match d {
                Definition::Entry(e) => e.text(),
                Definition::Manual { step, .. } => step.clone(),
                Definition::Deny(d) => d.name.clone(),
            })
            .collect();
        assert_eq!(
            names,
            [
                "mcpServers.denied".to_string(),
                "mcpServers.wildcard".into(),
                format!("mcpServers.local of project {project}"),
                "[mcp_servers.off]".into(),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_lock_is_released_by_its_holder_and_a_held_or_stale_one_is_left_alone() {
        let w = Workspace::new();
        let file = w.home.join(".claude.json");
        let lock = Lock::take(&file, WAIT).unwrap();
        let dir = w.home.join(".claude.json.lock");
        assert!(dir.is_dir());
        drop(lock);
        assert!(!dir.exists());
        // Another holder's lock, fresh or stale, is never removed here.
        fs::create_dir(&dir).unwrap();
        let held = Lock::take(&file, Duration::ZERO).err().unwrap().to_string();
        assert!(
            held.ends_with("was held by Claude Code for over 0 seconds"),
            "{held}"
        );
        let old = SystemTime::now() - Duration::from_secs(60);
        fs::File::open(&dir).unwrap().set_modified(old).unwrap();
        let stale = Lock::take(&file, Duration::ZERO).err().unwrap().to_string();
        assert!(
            stale.contains("remove it if no Claude Code is running"),
            "{stale}"
        );
        assert!(dir.is_dir());
        fs::remove_dir(&dir).unwrap();
        // A lock another holder replaced is no longer held, and is left to that holder.
        let lock = Lock::take(&file, WAIT).unwrap();
        assert!(lock.held());
        fs::File::open(&dir).unwrap().set_modified(old).unwrap();
        assert!(!lock.held());
        drop(lock);
        assert!(dir.is_dir());
    }
}
