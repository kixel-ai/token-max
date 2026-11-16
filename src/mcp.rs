//! Where an MCP server is configured, so `token-max doctor` can offer to remove one that
//! is off: Claude Code's user- and local-scope servers in its state file, such as
//! `~/.claude.json`, and Codex's `[mcp_servers.<id>]` tables in `config.toml`.
//!
//! A project's `.mcp.json` is shared with everyone who uses the project, and a plugin's
//! server or a claude.ai connector is not in any file token-max edits, so those are steps
//! by hand.

use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde_json::{Map, Value};

use crate::collections::IndexSet;
use crate::controls::members;
use crate::discovery::{join, realpath, settings};
use crate::model::{Control, Harness, Profile};

/// One place an MCP server is configured.
#[derive(Clone, Debug, PartialEq)]
pub enum Definition {
    /// An entry token-max can delete.
    Entry(Entry),
    /// What to do by hand, and why token-max cannot.
    Manual { step: String, reason: String },
}

/// A server's entry in a configuration file: the key at `path`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
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
        return match codex_layers(profile, id) {
            Ok(layers) if layers.is_empty() => vec![Definition::Entry(Entry {
                server: id.clone(),
                file: file.clone(),
                harness: Harness::Codex,
                path: vec![table.clone(), id.clone()],
            })],
            Ok(layers) => vec![Definition::Manual {
                step,
                reason: format!(
                    "{} also configures it and may rely on the rest of its table",
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
    if server.starts_with("plugin_") {
        return vec![Definition::Manual {
            step: "uninstall or disable the plugin that provides it, with /plugin in Claude Code"
                .into(),
            reason: "a plugin adds it".into(),
        }];
    }
    if server.starts_with("claude_ai_") {
        return vec![Definition::Manual {
            step: "disconnect it at claude.ai, under Settings > Connectors".into(),
            reason: "it is a claude.ai connector".into(),
        }];
    }
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
            if members(config.get("mcpServers"))
                .keys()
                .any(|k| tool_name(k) == server)
            {
                found.push(Definition::Manual {
                    step: format!("remove it from {shared}"),
                    reason: "a project's .mcp.json is shared with everyone who uses the project"
                        .into(),
                });
            }
        }
        files.push(file);
    }
    if found.is_empty() {
        let searched = if files.is_empty() {
            "a project's .mcp.json".to_string()
        } else {
            format!("{} or a project's .mcp.json", files.join(", "))
        };
        found.push(Definition::Manual {
            step:
                "remove it where it is configured, such as a --mcp-config file or managed-mcp.json"
                    .into(),
            reason: format!("token-max did not find it in {searched}"),
        });
    }
    found
}

/// Servers configured for `profiles` whose tools are all off already: a Claude server
/// whose tools the user's `permissions.deny` denies, or one a project turned off with
/// `/mcp`, and a Codex server with `enabled = false`. A file that cannot be read is a step
/// by hand.
pub fn turned_off(profiles: &[Profile]) -> Vec<Definition> {
    let mut found: IndexSet<Entry> = IndexSet::default();
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
                // A project or named profile that configures it too may turn it back on.
                match codex_layers(profile, id) {
                    Ok(layers) if !layers.is_empty() => continue,
                    Ok(_) => {}
                    Err(e) => {
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
        .map(Definition::Entry)
        .chain(manual)
        .collect()
}

/// The Codex layers that outrank the profile's `config.toml` for some recorded session and
/// configure the server `id` too: a project's or named profile's file, or a named profile
/// table in `config.toml` itself.
fn codex_layers(profile: &Profile, id: &str) -> Result<Vec<String>, crate::safe::ScanError> {
    let declares =
        |config: &Map<String, Value>| members(config.get("mcp_servers")).contains_key(id);
    let mut layers = vec![];
    for file in &profile.outranking_files {
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

/// A Claude server's name as its tools carry it, `mcp__<name>__<tool>`.
fn tool_name(server: &str) -> String {
    server
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
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

/// A lock not refreshed for this long belongs to a process that died, as Claude Code's
/// lock library treats it.
const STALE: Duration = Duration::from_secs(10);

/// How often a held lock's time is refreshed, well within `STALE`.
const REFRESH: Duration = Duration::from_secs(1);

/// How long to wait for a Claude Code session to finish its save.
const WAIT: Duration = Duration::from_secs(5);

impl Lock {
    /// Take the lock beside `file`, waiting for a session that holds it.
    pub(crate) fn take(file: &Path) -> io::Result<Lock> {
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
            let age = fs::metadata(&dir)
                .and_then(|m| m.modified())
                .map(|at| at.elapsed().unwrap_or_default());
            match age {
                Ok(age) if age > STALE => {
                    let _ = fs::remove_dir(&dir);
                    continue;
                }
                // Released between the two calls.
                Err(e) if e.kind() == ErrorKind::NotFound => continue,
                _ => {}
            }
            if started.elapsed().unwrap_or_default() > WAIT {
                return Err(io::Error::other(format!(
                    "Claude Code held {} for over {} seconds",
                    dir.display(),
                    WAIT.as_secs()
                )));
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
    fn a_claude_server_is_found_in_each_scope_and_named_by_hand_where_token_max_cannot_edit() {
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
        let Definition::Manual { step, .. } = &found[2] else {
            panic!("{found:?}")
        };
        assert_eq!(step, &format!("remove it from {project}/.mcp.json"));
        let Definition::Entry(local) = &found[1] else {
            unreachable!()
        };
        assert_eq!(
            local.text(),
            format!("mcpServers.docs.v2 of project {project}")
        );

        // Plugins, connectors and servers configured elsewhere are steps by hand.
        for server in ["plugin_github_github", "claude_ai_Gmail", "missing"] {
            let found = definitions(server, &deny(server), &[&p]);
            assert!(
                matches!(found.as_slice(), [Definition::Manual { .. }]),
                "{server}: {found:?}"
            );
        }
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
        p.outranking_files.clear();
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
    fn the_lock_waits_for_a_holder_and_takes_over_a_stale_one() {
        let w = Workspace::new();
        let file = w.home.join(".claude.json");
        let lock = Lock::take(&file).unwrap();
        let dir = w.home.join(".claude.json.lock");
        assert!(dir.is_dir());
        drop(lock);
        assert!(!dir.exists());
        // A lock last refreshed long ago is taken over.
        fs::create_dir(&dir).unwrap();
        let old = SystemTime::now() - Duration::from_secs(60);
        fs::File::open(&dir).unwrap().set_modified(old).unwrap();
        drop(Lock::take(&file).unwrap());
        assert!(!dir.exists());
        // A lock another holder replaced is no longer held, and is left to that holder.
        let lock = Lock::take(&file).unwrap();
        assert!(lock.held());
        fs::File::open(&dir).unwrap().set_modified(old).unwrap();
        assert!(!lock.held());
        drop(lock);
        assert!(dir.is_dir());
    }
}
