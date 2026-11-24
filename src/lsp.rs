//! Code intelligence plugins: the language of each file Claude read or edited, whether the
//! profile has the official plugin that runs that language's server, and `vibes`' card when
//! it doesn't.
//!
//! A session's language comes from the files its tool calls named, not marker files at its
//! directory: most recorded directories are worktrees deleted since, and a `build.gradle` can't
//! tell Java from Kotlin.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use serde_json::{Map, Value};

use crate::collections::{HashMap, HashSet, IndexMap, IndexSet};
use crate::discovery::{dirname, error, git_dir, join};
use crate::doctor::{Edit, How, Op, Patch};
use crate::model::{EventKind, Harness, Millis, Profile, Scan, Text};
use crate::report::plural;
use crate::safe::ScanError;
use crate::vibes::Item;

/// One language server plugin of the official marketplace.
pub struct Server {
    pub language: &'static str,
    /// Its marketplace entry name, such as `rust-analyzer-lsp`.
    pub plugin: &'static str,
    /// The program it starts, which the plugin does not include.
    pub binary: &'static str,
    /// Where its README says how to install the binary.
    pub readme: &'static str,
    /// The file extensions it handles, as its marketplace entry maps them.
    extensions: &'static [&'static str],
}

const MARKETPLACE: &str = "claude-plugins-official";

/// Anthropic's costs page, which recommends these plugins for typed languages.
pub const COSTS: &str =
    "https://code.claude.com/docs/en/costs#install-code-intelligence-plugins-for-typed-languages";
/// The page that lists the official plugins, their binaries and how to install them.
pub const GUIDE: &str = "https://code.claude.com/docs/en/plugins/code-intelligence";

macro_rules! official {
    ($language:expr, $plugin:expr, $binary:expr, $extensions:expr) => {
        Server {
            language: $language,
            plugin: $plugin,
            binary: $binary,
            readme: concat!(
                "https://github.com/anthropics/claude-plugins-official/tree/main/plugins/",
                $plugin
            ),
            extensions: $extensions,
        }
    };
}

/// Every plugin the code intelligence page lists, with the extensions each marketplace entry
/// maps, checked on 2026-11-23.
pub const SERVERS: [Server; 13] = [
    official!(
        "C/C++",
        "clangd-lsp",
        "clangd",
        &["c", "h", "cpp", "cc", "cxx", "hpp", "hxx", "C", "H"]
    ),
    official!("C#", "csharp-lsp", "csharp-ls", &["cs"]),
    official!("Go", "gopls-lsp", "gopls", &["go"]),
    official!("Java", "jdtls-lsp", "jdtls", &["java"]),
    official!("Kotlin", "kotlin-lsp", "kotlin-lsp", &["kt", "kts"]),
    Server {
        language: "Liquid",
        plugin: "liquid-lsp",
        binary: "shopify",
        readme: "https://github.com/Shopify/liquid-skills/tree/main/plugins/liquid-lsp",
        extensions: &["liquid"],
    },
    official!("Lua", "lua-lsp", "lua-language-server", &["lua"]),
    official!("PHP", "php-lsp", "intelephense", &["php"]),
    official!(
        "Python",
        "pyright-lsp",
        "pyright-langserver",
        &["py", "pyi"]
    ),
    official!(
        "Ruby",
        "ruby-lsp",
        "ruby-lsp",
        &["rb", "rake", "gemspec", "ru", "erb"]
    ),
    official!("Rust", "rust-analyzer-lsp", "rust-analyzer", &["rs"]),
    official!("Swift", "swift-lsp", "sourcekit-lsp", &["swift"]),
    official!(
        "TypeScript and JavaScript",
        "typescript-lsp",
        "typescript-language-server",
        &["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"]
    ),
];

/// A language of [`SERVERS`], by its index there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Lsp(u8);

impl Lsp {
    /// The language of the file at `path`, read from its extension; None for any other file.
    pub fn of(path: &str) -> Option<Lsp> {
        // A Windows path recorded on another host still splits at its backslashes.
        let name = path.rsplit(['/', '\\']).next()?;
        let (stem, extension) = name.rsplit_once('.')?;
        if stem.is_empty() {
            return None;
        }
        let index = SERVERS
            .iter()
            .position(|s| s.extensions.contains(&extension))?;
        Some(Lsp(index as u8))
    }

    pub fn server(self) -> &'static Server {
        &SERVERS[self.0 as usize]
    }
}

/// Whether a profile has a language's plugin, as its files say now.
#[derive(Clone, Debug, PartialEq)]
pub enum Plugin {
    /// Installed for every project and turned on, or forced on by managed settings.
    On,
    /// Installed only at project or local scope, for these projects; `blocked` when managed
    /// settings turn the official copy off, so it can't be installed for the rest.
    Projects {
        projects: Vec<String>,
        blocked: bool,
    },
    /// Installed for every project, and turned off by `enabledPlugins` in the user settings
    /// file, or not listed there, which no record shows turns it on.
    Off {
        id: String,
        listed: bool,
    },
    /// Turned off by `enabledPlugins` in managed settings, which nothing else overrides.
    Blocked,
    Missing,
}

impl Plugin {
    /// Whether a session at `cwd` loads it.
    fn covers(&self, cwd: &str) -> bool {
        match self {
            Plugin::On => true,
            Plugin::Projects { projects, .. } => projects.iter().any(|p| within(cwd, p)),
            _ => false,
        }
    }

    /// How close it is to loading, so the best of a language's plugin IDs speaks for it.
    fn rank(&self) -> u8 {
        match self {
            Plugin::On => 5,
            Plugin::Projects { .. } => 4,
            Plugin::Off { listed: true, .. } => 3,
            Plugin::Off { listed: false, .. } => 2,
            Plugin::Blocked => 1,
            Plugin::Missing => 0,
        }
    }
}

/// Whether `path` is `dir` or under it.
fn within(path: &str, dir: &str) -> bool {
    path.strip_prefix(dir)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(['/', '\\']))
}

/// Whether an environment variable's name is `PATH`, which Windows spells `Path` and reads in
/// any case.
pub(crate) fn is_path(name: &str) -> bool {
    name == "PATH" || (cfg!(windows) && name.eq_ignore_ascii_case("PATH"))
}

/// A Claude profile's code intelligence plugins, read when it was discovered.
#[derive(Clone, Debug, PartialEq)]
pub struct Controls {
    /// The `installed_plugins.json` read, which profiles share only when they share it.
    pub installs: String,
    /// Each of [`SERVERS`]' plugins, in its order, and whether its binary is on `PATH`.
    pub plugins: Vec<(Plugin, bool)>,
}

/// Read a Claude profile's plugins: the installs Claude Code records under its plugins
/// directory, `CLAUDE_CODE_PLUGIN_CACHE_DIR` else `plugins` in `root`, and `enabledPlugins`
/// in the user and managed settings. `path` is the `PATH` binaries are looked for on.
pub fn controls(
    root: &str,
    cache_dir: Option<&str>,
    user: &Map<String, Value>,
    managed: &Map<String, Value>,
    path: Option<&str>,
) -> Result<Controls, ScanError> {
    let plugins = match cache_dir.filter(|d| !d.is_empty()) {
        Some(dir) => dir.to_string(),
        None => join(&[root, "plugins"]),
    };
    let file = join(&[&plugins, "installed_plugins.json"]);
    let installed = installs(&file)?;
    let enabled = |settings: &Map<String, Value>, id: &str| {
        let entries = settings.get("enabledPlugins").and_then(Value::as_object);
        entries.and_then(|e| e.get(id)).and_then(Value::as_bool)
    };
    // One plugin ID's state, such as `rust-analyzer-lsp@claude-plugins-official`'s.
    let state = |id: &str| {
        match enabled(managed, id) {
            Some(false) => return Plugin::Blocked,
            Some(true) => return Plugin::On,
            None => {}
        }
        let scopes = installed.get(id).map_or(&[][..], Vec::as_slice);
        if scopes.iter().any(Option::is_none) {
            return match enabled(user, id) {
                Some(true) => Plugin::On,
                on => Plugin::Off {
                    id: id.into(),
                    listed: on.is_some(),
                },
            };
        }
        match scopes.iter().flatten().cloned().collect::<Vec<_>>() {
            projects if !projects.is_empty() => Plugin::Projects {
                projects,
                blocked: false,
            },
            _ => Plugin::Missing,
        }
    };
    let ids = |plugin: &str| -> Vec<String> {
        let listed = [managed, user]
            .into_iter()
            .filter_map(|s| s.get("enabledPlugins").and_then(Value::as_object))
            .flat_map(|e| e.keys());
        let mut ids: Vec<String> = installed
            .keys()
            .chain(listed)
            .filter(|id| name(id) == plugin)
            .cloned()
            .collect();
        ids.sort();
        ids.dedup();
        ids
    };
    Ok(Controls {
        installs: file.clone(),
        plugins: SERVERS
            .iter()
            .map(|s| {
                let states: Vec<Plugin> = ids(s.plugin).iter().map(|id| state(id)).collect();
                // Every copy's projects load it; the advice installs the official copy.
                let projects: Vec<String> = states
                    .iter()
                    .flat_map(|p| match p {
                        Plugin::Projects { projects, .. } => projects.clone(),
                        _ => vec![],
                    })
                    .collect();
                let official = format!("{}@{MARKETPLACE}", s.plugin);
                let blocked = enabled(managed, &official) == Some(false);
                let best = states
                    .into_iter()
                    .max_by_key(Plugin::rank)
                    .unwrap_or(Plugin::Missing);
                let best = match best {
                    Plugin::Projects { .. } => Plugin::Projects { projects, blocked },
                    Plugin::Missing if blocked => Plugin::Blocked,
                    other => other,
                };
                (best, on_path(s.binary, path))
            })
            .collect(),
    })
}

/// A plugin id's name, before its `@marketplace`.
fn name(id: &str) -> &str {
    id.split_once('@').map_or(id, |(name, _)| name)
}

/// Each plugin id `installed_plugins.json` records, with each install's project: None for
/// user or managed scope. A missing file records none. The first version of the file holds
/// one record per plugin, with no scope, which is the user's.
fn installs(file: &str) -> Result<IndexMap<String, Vec<Option<String>>>, ScanError> {
    let bytes = match fs::read(file) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(IndexMap::default()),
        Err(_) => return Err(error("cannot read installed plugins", file, 1)),
    };
    let invalid = || error("invalid installed plugins file", file, 1);
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    let plugins = match value.get("plugins") {
        Some(Value::Object(plugins)) => plugins,
        _ => return Err(invalid()),
    };
    let mut found = IndexMap::default();
    for (id, records) in plugins {
        let records = match records {
            Value::Array(records) => records.as_slice(),
            Value::Object(_) => std::slice::from_ref(records),
            _ => return Err(invalid()),
        };
        let mut scopes = vec![];
        for record in records {
            let scope = record.get("scope").and_then(Value::as_str);
            let project = record.get("projectPath").and_then(Value::as_str);
            scopes.push(match (scope, project) {
                (None | Some("user" | "managed"), _) => None,
                (Some("project" | "local"), Some(project)) => Some(project.to_string()),
                _ => return Err(invalid()),
            });
        }
        found.insert(id.clone(), scopes);
    }
    Ok(found)
}

/// Whether `binary` is a program in a directory of `path`, trying each of Windows' program
/// extensions there.
fn on_path(binary: &str, path: Option<&str>) -> bool {
    let Some(path) = path else { return false };
    let names: Vec<String> = if cfg!(windows) {
        [".exe", ".cmd", ".bat", ".com"]
            .iter()
            .map(|e| format!("{binary}{e}"))
            .collect()
    } else {
        vec![binary.to_string()]
    };
    std::env::split_paths(path).any(|dir| {
        names.iter().any(|n| {
            let file = dir.join(n);
            fs::metadata(&file).is_ok_and(|m| m.is_file() && executable(&m))
        })
    })
}

#[cfg(unix)]
fn executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_: &fs::Metadata) -> bool {
    true
}

/// The languages one settings file's Claude profiles worked in over the window, for those
/// that share one record of installed plugins.
#[derive(Clone, Debug, PartialEq)]
pub struct LspUse {
    pub profiles: Vec<String>,
    /// Sessions with a request in the window, a subagent's counted as its parent's.
    pub sessions: u64,
    /// Each language some session read or edited a file of, most sessions first.
    pub languages: Vec<LanguageUse>,
    /// The settings file an edit to `enabledPlugins` goes to.
    pub settings_file: String,
    /// The Claude home the plugins are installed in.
    pub root: String,
    /// T3 launches the profile with arguments that can replace the settings it reads.
    pub unresolved: bool,
}

/// The sessions that read or edited files of one language.
#[derive(Clone, Debug, PartialEq)]
pub struct LanguageUse {
    pub lsp: Lsp,
    pub sessions: u64,
    /// Of `sessions`, those with a directory the plugin does not load in.
    pub uncovered: u64,
    /// Each recorded directory with the sessions that touched the language there; empty when
    /// none was recorded.
    pub directories: Vec<(String, Vec<Text>)>,
    pub plugin: Plugin,
    pub binary: bool,
}

/// One line per Claude settings file and installed-plugins record: its sessions from
/// `since`, else the scan's start, to `now`, and the languages of the files their tool calls
/// named. A T3 instance whose settings are unresolved keeps a line of its own.
pub fn lsp_use(scan: &Scan, since: Option<Millis>, now: Millis) -> Vec<LspUse> {
    let window = |time: Millis| since.is_none_or(|s| time >= s) && time <= now;
    // A subagent's session is its parent's, linked by the session ID its records carry.
    let mut parents: HashMap<Text, Text> = HashMap::default();
    let root = |parents: &mut HashMap<Text, Text>, session: &Text| -> Text {
        // A lookup per request would dominate a large scan, so each session's is kept.
        parents
            .entry(session.clone())
            .or_insert_with(|| {
                scan.sessions
                    .parent(session)
                    .unwrap_or_else(|| session.clone())
            })
            .clone()
    };
    let unresolved = |p: &Profile| p.t3.as_ref().is_some_and(|t| t.settings_unresolved);
    let mut lines: IndexMap<String, Vec<&Profile>> = IndexMap::default();
    for p in &scan.profiles {
        let Some(controls) = p.lsp.as_ref().filter(|_| p.harness == Harness::Claude) else {
            continue;
        };
        let key = if unresolved(p) {
            format!("profile:{}", p.id)
        } else {
            format!("{}\n{}", p.settings_file, controls.installs)
        };
        lines.entry(key).or_default().push(p);
    }
    lines
        .values()
        .map(|members| {
            let lead = members[0];
            let ids: Vec<&str> = members.iter().map(|p| p.id.as_str()).collect();
            let mine = |profiles: &[String]| profiles.iter().any(|p| ids.contains(&p.as_str()));
            let mut sessions: HashSet<Text> = HashSet::default();
            for r in &scan.requests {
                if r.harness == Harness::Claude && window(r.time) && mine(&r.profiles) {
                    sessions.insert(root(&mut parents, &r.session));
                }
            }
            // Each language's sessions, with every directory each touched it in.
            let mut touched: IndexMap<Lsp, IndexMap<Text, IndexSet<&str>>> = IndexMap::default();
            for e in &scan.events {
                let Some(lsp) = e.language else { continue };
                if e.kind != EventKind::ToolStart
                    || e.harness != Harness::Claude
                    || !window(e.time)
                    || !mine(&e.profiles)
                {
                    continue;
                }
                let session = root(&mut parents, &e.session);
                let cwd = e.cwd.as_deref().unwrap_or_default();
                touched
                    .entry(lsp)
                    .or_default()
                    .entry(session)
                    .or_default()
                    .insert(cwd);
            }
            sessions.extend(
                touched
                    .values()
                    .flat_map(|by_session| by_session.keys().cloned()),
            );
            let controls = lead.lsp.as_ref().expect("filtered on its controls");
            let mut languages: Vec<LanguageUse> = touched
                .into_iter()
                .map(|(lsp, by_session)| {
                    let (plugin, binary) = controls.plugins[lsp.0 as usize].clone();
                    let mut directories: IndexMap<&str, Vec<Text>> = IndexMap::default();
                    let mut uncovered = 0;
                    for (session, cwds) in &by_session {
                        // A directory not recorded can't be shown to load the plugin.
                        if cwds.iter().any(|c| !plugin.covers(c)) {
                            uncovered += 1;
                        }
                        for cwd in cwds.iter().filter(|c| !c.is_empty()) {
                            directories.entry(cwd).or_default().push(session.clone());
                        }
                    }
                    LanguageUse {
                        lsp,
                        sessions: by_session.len() as u64,
                        uncovered,
                        directories: directories
                            .into_iter()
                            .map(|(d, s)| (d.to_string(), s))
                            .collect(),
                        plugin,
                        binary,
                    }
                })
                .collect();
            languages.sort_by(|a, b| b.sessions.cmp(&a.sessions).then(a.lsp.0.cmp(&b.lsp.0)));
            LspUse {
                profiles: members.iter().map(|p| p.id.clone()).collect(),
                sessions: sessions.len() as u64,
                languages,
                settings_file: lead.settings_file.clone(),
                root: lead.root.clone(),
                unresolved: unresolved(lead),
            }
        })
        .collect()
}

/// The project a recorded directory belongs to: the main checkout of the Git repository that
/// holds the nearest directory still there, such as a removed worktree's parent, else that
/// directory. A repository at `home` or above it, such as one of dotfiles, holds no project
/// but `home` itself, and Git metadata that can't be read leaves the directory.
fn project(cwd: &str, home: Option<&str>) -> String {
    let mut dir = cwd;
    while !Path::new(dir).is_dir() {
        let up = dirname(dir);
        if up == dir {
            return cwd.into();
        }
        dir = up;
    }
    let mut at = dir;
    loop {
        if home.is_some_and(|h| h == at) && at != dir {
            return dir.into();
        }
        match git_dir(at) {
            // A linked worktree's common directory is its main checkout's `.git`.
            Ok(Some(git)) if git.linked => {
                let common = Path::new(&git.common);
                return match (common.file_name(), common.parent()) {
                    (Some(name), Some(main)) if name == ".git" => {
                        main.to_string_lossy().into_owned()
                    }
                    _ => at.into(),
                };
            }
            Ok(Some(_)) => return at.into(),
            Ok(None) => {}
            Err(_) => return dir.into(),
        }
        let up = dirname(at);
        if up == at {
            return dir.into();
        }
        at = up;
    }
}

/// How a card prints: paths from `~` under `home`, and the report's arrow, such as "->".
pub struct View<'a> {
    pub home: Option<&'a str>,
    pub arrow: &'a str,
}

/// Each project of `directories`, with how many sessions touched it, most first.
fn projects(directories: &[(String, Vec<Text>)], home: Option<&str>) -> Vec<(String, u64)> {
    let mut found: IndexMap<String, HashSet<&Text>> = IndexMap::default();
    for (dir, sessions) in directories {
        found
            .entry(project(dir, home))
            .or_default()
            .extend(sessions);
    }
    let mut found: Vec<(String, u64)> = found
        .into_iter()
        .map(|(p, s)| (p, s.len() as u64))
        .collect();
    found.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    found
}

/// How many projects a card names before it counts the rest.
const NAMED: usize = 3;

/// `vibes`' cards for one line: one for each language whose sessions ran without its
/// plugin, or whose plugin can't start its server. Read on the host that recorded them,
/// whose files name each project.
pub fn items(u: &LspUse, view: &View<'_>) -> Vec<Item> {
    u.languages
        .iter()
        .filter_map(|l| item(u, l, view))
        .collect()
}

fn item(u: &LspUse, l: &LanguageUse, view: &View<'_>) -> Option<Item> {
    let home = |path: &str| crate::report::home_path(view.home, path);
    let arrow = view.arrow;
    let s = l.lsp.server();
    let id = format!("{}@{MARKETPLACE}", s.plugin);
    let loaded = l.sessions - l.uncovered;
    // A plugin that loads everywhere, with its server on PATH, leaves nothing to advise.
    if l.uncovered == 0 && l.binary {
        return None;
    }
    let mut text = format!(
        "{} files were read or edited in {} of {} ({}){}.",
        s.language,
        l.sessions,
        plural(u.sessions, "session"),
        share(l.sessions, u.sessions),
        named(&projects(&l.directories, view.home), &home),
    );
    let binary = if l.binary {
        format!(
            "Its language server, {}, is on the PATH token-max runs with.",
            s.binary
        )
    } else {
        format!(
            "Its language server, {}, is not on the PATH token-max runs with, and the plugin can't start without it on the PATH of the shell you start claude from.",
            s.binary
        )
    };
    // A bare install goes to whichever Claude home the shell selects, so the home is named.
    let install = format!(
        "with CLAUDE_CONFIG_DIR set to {}, run `claude plugin install {id}`, then start a new session",
        home(&u.root)
    );
    let server = format!("install {}, as the plugin's README says", s.binary);
    let (how, quiet) = match &l.plugin {
        Plugin::Projects {
            blocked: true,
            projects,
        } => {
            text += &format!(
                " The {} plugin is installed only for {}, which {loaded} of them ran in, and managed settings turn {id} off.",
                s.plugin,
                list(&projects.iter().map(|p| home(p)).collect::<Vec<_>>()),
            );
            (None, true)
        }
        Plugin::Missing | Plugin::Projects { .. } => {
            text += &match &l.plugin {
                Plugin::Projects { projects, .. } => format!(
                    " The {} plugin is installed only for {}, which {} of them ran in.",
                    s.plugin,
                    list(&projects.iter().map(|p| home(p)).collect::<Vec<_>>()),
                    loaded
                ),
                _ => format!(" The {} plugin is not installed.", s.plugin),
            };
            let step = if l.binary {
                install
            } else {
                format!("{server}, then {install}")
            };
            let how = How::Manual {
                step,
                reason: "no setting installs a plugin".into(),
            };
            (Some(how), false)
        }
        Plugin::Off { id, listed } => {
            let file = home(&u.settings_file);
            text += &if *listed {
                format!(
                    " The {} plugin is installed and turned off in {file}.",
                    s.plugin
                )
            } else {
                format!(
                    " The {} plugin is installed, and no enabledPlugins entry in {file} turns it on.",
                    s.plugin
                )
            };
            let edit = Edit {
                path: vec!["enabledPlugins".into(), id.clone()],
                value: Value::Bool(true),
                op: Op::Set,
            };
            let how = if u.unresolved {
                How::Manual {
                    step: format!("set {} in {file}", edit.text(Harness::Claude)),
                    reason: "T3 launches this instance with arguments that can replace the settings it reads".into(),
                }
            } else {
                How::Edit(Patch {
                    file: u.settings_file.clone(),
                    harness: Harness::Claude,
                    edits: vec![edit],
                })
            };
            (Some(how), false)
        }
        Plugin::Blocked => {
            text += &format!(
                " Managed settings turn the {} plugin off, and nothing else overrides them.",
                s.plugin
            );
            (None, true)
        }
        Plugin::On => {
            text += &format!(" The {} plugin is installed.", s.plugin);
            let how = How::Manual {
                step: server.clone(),
                reason: "token-max installs no programs".into(),
            };
            (Some(how), false)
        }
    };
    text += &format!(
        " {binary} With the plugin, Claude looks up a symbol's definition and references instead of searching text and reading each candidate file, and sees the type errors an edit introduced. Anthropic recommends it for typed languages; what it saves is not measured."
    );
    Some(Item {
        name: format!("{} code intelligence", s.language),
        value: match &l.plugin {
            Plugin::Off { .. } => format!("off {arrow} on"),
            _ => String::new(),
        },
        what: "A plugin that runs a language server for Claude: code navigation by symbol, and diagnostics after each edit.".into(),
        text,
        sources: vec![COSTS.into(), GUIDE.into(), s.readme.into()],
        how,
        quiet,
    })
}

/// `part` of `whole` as a whole percentage, or "under 1%".
fn share(part: u64, whole: u64) -> String {
    let percent = part as f64 * 100.0 / whole.max(1) as f64;
    if percent > 0.0 && percent < 1.0 {
        "under 1%".into()
    } else {
        format!("{percent:.0}%")
    }
}

/// Where the sessions ran, most first: ", most in ~/a (3 sessions), ~/b (1) and 2 more
/// projects"; empty when no directory was recorded.
fn named(dirs: &[(String, u64)], home: &dyn Fn(&str) -> String) -> String {
    let Some(((first, n), rest)) = dirs.split_first() else {
        return String::new();
    };
    if rest.is_empty() {
        return format!(", in {}", home(first));
    }
    let mut named = vec![format!("{} ({})", home(first), plural(*n, "session"))];
    named.extend(
        rest.iter()
            .take(NAMED - 1)
            .map(|(d, n)| format!("{} ({n})", home(d))),
    );
    let more = dirs.len().saturating_sub(NAMED);
    if more > 0 {
        let noun = if more == 1 { "project" } else { "projects" };
        named.push(format!("{more} more {noun}"));
    }
    format!(", most in {}", list(&named))
}

/// `items` joined as prose: "a", "a and b", "a, b and c".
fn list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::scan;
    use crate::test_support::Workspace;
    use serde_json::json;
    use std::path::PathBuf;

    const START: Millis = 1_793_491_200_000; // 2026-11-01T00:00:00Z

    fn text(path: &Path) -> String {
        path.to_str().unwrap().to_string()
    }

    /// One JSON record per line, creating the file's directory.
    fn write(path: PathBuf, records: &[Value]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let lines: String = records.iter().map(|r| format!("{r}\n")).collect();
        fs::write(path, lines).unwrap();
    }

    /// A Claude request at `second` in `session`, run at `cwd`, calling each `(tool, input)`.
    fn request(session: &str, cwd: &str, second: i64, calls: &[(&str, Value)]) -> Value {
        let content: Vec<Value> = calls
            .iter()
            .enumerate()
            .map(|(i, (name, input))| {
                json!({"type": "tool_use", "id": format!("{session}-{second}-{i}"), "name": name, "input": input})
            })
            .collect();
        json!({
            "type": "assistant",
            "sessionId": session,
            "uuid": format!("{session}-{second}"),
            "timestamp": jiff::Timestamp::from_millisecond(START + second * 1000).unwrap().to_string(),
            "cwd": cwd,
            "message": {
                "id": format!("{session}-{second}"),
                "model": "claude-opus-5-5",
                "usage": {"input_tokens": 10, "output_tokens": 1},
                "content": content,
            },
        })
    }

    /// A program named `binary` in its own directory, returning that directory.
    fn program(w: &Workspace, binary: &str) -> PathBuf {
        let dir = w.home.join("bin");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join(if cfg!(windows) {
            format!("{binary}.exe")
        } else {
            binary.to_string()
        });
        fs::write(&file, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
        }
        dir
    }

    #[test]
    fn a_file_names_its_language_by_extension_only() {
        let language = |path: &str| Lsp::of(path).map(|l| l.server().language);
        assert_eq!(language("/src/main.rs"), Some("Rust"));
        assert_eq!(language("C:\\src\\lib.rs"), Some("Rust"));
        assert_eq!(language("types.d.ts"), Some("TypeScript and JavaScript"));
        // clangd maps the upper-case extensions to C++; other cases are other files.
        assert_eq!(language("a.C"), Some("C/C++"));
        assert_eq!(language("a.RS"), None);
        for other in [".rs", "Makefile", "notes.md", "/dir.rs/", ""] {
            assert_eq!(language(other), None, "{other:?}");
        }
    }

    #[test]
    fn plugin_state_follows_installs_and_enabled_plugins() {
        let w = Workspace::new();
        let root = text(&w.claude);
        let plugins = w.claude.join("plugins");
        fs::create_dir_all(&plugins).unwrap();
        let project = text(&w.home.join("project"));
        fs::write(
            plugins.join("installed_plugins.json"),
            json!({"version": 2, "plugins": {
                "rust-analyzer-lsp@claude-plugins-official": [{"scope": "user"}],
                "gopls-lsp@claude-plugins-official": [{"scope": "user"}],
                "pyright-lsp@claude-plugins-official": [{"scope": "project", "projectPath": project}],
                "clangd-lsp@claude-plugins-official": [{"scope": "user"}],
                "kotlin-lsp@claude-plugins-official": [{"scope": "user"}],
                "ruby-lsp@claude-plugins-official": [{"scope": "user"}],
                "lua-lsp@corp": [{"scope": "project", "projectPath": text(&w.home.join("a"))}],
                "lua-lsp@claude-plugins-official": [{"scope": "local", "projectPath": text(&w.home.join("b"))}],
            }})
            .to_string(),
        )
        .unwrap();
        let user = json!({"enabledPlugins": {
            "rust-analyzer-lsp@claude-plugins-official": true,
            "gopls-lsp@claude-plugins-official": false,
            "ruby-lsp@claude-plugins-official": true,
        }});
        let managed = json!({"enabledPlugins": {
            "clangd-lsp@claude-plugins-official": false,
            "swift-lsp@corp": true,
            "ruby-lsp@corp": false,
            "lua-lsp@claude-plugins-official": false,
        }});
        let bin = program(&w, "rust-analyzer");
        let path = std::env::join_paths([w.home.join("empty"), bin]).unwrap();
        let c = controls(
            &root,
            None,
            user.as_object().unwrap(),
            managed.as_object().unwrap(),
            path.to_str(),
        )
        .unwrap();
        let state = |plugin: &str| {
            let i = SERVERS.iter().position(|s| s.plugin == plugin).unwrap();
            c.plugins[i].clone()
        };
        assert_eq!(state("rust-analyzer-lsp"), (Plugin::On, true));
        assert_eq!(
            state("gopls-lsp"),
            (
                Plugin::Off {
                    id: "gopls-lsp@claude-plugins-official".into(),
                    listed: true,
                },
                false
            )
        );
        // An install that no enabledPlugins entry turns on is offered on.
        assert_eq!(
            state("kotlin-lsp").0,
            Plugin::Off {
                id: "kotlin-lsp@claude-plugins-official".into(),
                listed: false,
            }
        );
        // Policy that blocks one marketplace's copy leaves another's on.
        assert_eq!(state("ruby-lsp").0, Plugin::On);
        // Every copy's projects load the language; a blocked official copy can't be installed.
        assert_eq!(
            state("lua-lsp").0,
            Plugin::Projects {
                // The official copy's own project install is blocked too.
                projects: vec![text(&w.home.join("a"))],
                blocked: true,
            }
        );
        assert_eq!(
            state("pyright-lsp").0,
            Plugin::Projects {
                projects: vec![project.clone()],
                blocked: false
            }
        );
        let projects = Plugin::Projects {
            projects: vec![project.clone()],
            blocked: false,
        };
        assert!(projects.covers(&format!("{project}/sub")));
        assert!(!projects.covers(&format!("{project}-other")));
        // Managed settings win both ways, whatever is installed.
        assert_eq!(state("clangd-lsp").0, Plugin::Blocked);
        assert_eq!(state("swift-lsp").0, Plugin::On);
        assert_eq!(state("typescript-lsp"), (Plugin::Missing, false));

        // CLAUDE_CODE_PLUGIN_CACHE_DIR moves the records; a directory with none installs nothing.
        let empty = Map::new();
        let moved = text(&w.home.join("cache"));
        let c = controls(&root, Some(&moved), &empty, &empty, None).unwrap();
        assert!(c.plugins.iter().all(|p| *p == (Plugin::Missing, false)));
        // The first version of the file holds one record per plugin, which is the user's.
        fs::write(
            plugins.join("installed_plugins.json"),
            json!({"version": 1, "plugins": {"gopls-lsp@claude-plugins-official": {"version": "1.0.0"}}}).to_string(),
        )
        .unwrap();
        let on = json!({"enabledPlugins": {"gopls-lsp@claude-plugins-official": true}});
        let c = controls(&root, None, on.as_object().unwrap(), &empty, None).unwrap();
        assert_eq!(c.plugins[2].0, Plugin::On);
        // A record Claude Code would not write fails the scan rather than reading as missing.
        for invalid in [
            "[]",
            r#"{"plugins": {"x@y": [{"scope": "project"}]}}"#,
            r#"{"plugins": {"x@y": 1}}"#,
            "{",
        ] {
            fs::write(plugins.join("installed_plugins.json"), invalid).unwrap();
            assert!(
                controls(&root, None, &empty, &empty, None).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn sessions_count_their_subagents_files_by_project_and_cards_name_the_missing_plugin() {
        let w = Workspace::new();
        let repo = w.home.join("repo");
        fs::create_dir_all(repo.join(".git/worktrees/wt")).unwrap();
        // A linked worktree of the repository, and a removed one under it.
        let wt = w.home.join("wt");
        fs::create_dir_all(&wt).unwrap();
        fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", text(&repo.join(".git/worktrees/wt"))),
        )
        .unwrap();
        fs::write(repo.join(".git/worktrees/wt/commondir"), "../..\n").unwrap();
        let gone = repo.join(".claude/worktrees/gone");
        let other = w.home.join("other");
        fs::create_dir_all(&other).unwrap();
        let (repo_s, wt_s, gone_s, other_s) = (text(&repo), text(&wt), text(&gone), text(&other));
        let file = |name: &str| json!({"file_path": format!("/synthetic/{name}")});
        let projects = w.claude.join("projects/project");
        write(
            projects.join("a.jsonl"),
            &[
                request(
                    "a",
                    &repo_s,
                    1,
                    &[
                        ("Read", file("main.rs")),
                        ("Edit", file("tool.py")),
                        ("Read", file("lib.rs")),
                        // Only the file tools name a file worth a language.
                        ("Grep", json!({"path": "/synthetic/x.go"})),
                    ],
                ),
                // The same session later works in another directory.
                request("a", &other_s, 6, &[("Read", file("z.rs"))]),
            ],
        );
        write(projects.join("b.jsonl"), &[request("b", &gone_s, 2, &[])]);
        let mut sidechain = request("agent", &gone_s, 3, &[("Write", file("new.rs"))]);
        sidechain["isSidechain"] = json!(true);
        sidechain["sessionId"] = json!("b");
        write(projects.join("b/subagents/agent-x.jsonl"), &[sidechain]);
        write(
            projects.join("c.jsonl"),
            &[request(
                "c",
                &other_s,
                4,
                &[("Bash", json!({"command": "ls"}))],
            )],
        );
        write(
            projects.join("d.jsonl"),
            &[request("d", &wt_s, 5, &[("MultiEdit", file("x.rs"))])],
        );
        let plugins = w.claude.join("plugins");
        fs::create_dir_all(&plugins).unwrap();
        fs::write(
            plugins.join("installed_plugins.json"),
            json!({"version": 2, "plugins": {
                "pyright-lsp@claude-plugins-official": [{"scope": "local", "projectPath": other_s}],
            }})
            .to_string(),
        )
        .unwrap();
        let mut options = w.scan_options();
        let bin = program(&w, "rust-analyzer");
        options.discovery.env.insert("PATH".into(), text(&bin));
        let data = scan(&options).unwrap();
        assert!(!format!("{data:?}").contains("main.rs"), "no path is kept");
        let lines = lsp_use(&data, None, START + 3_600_000);
        assert_eq!(lines.len(), 1, "Codex gets no line");
        let u = &lines[0];
        assert_eq!(u.sessions, 4);
        let got: Vec<_> = u
            .languages
            .iter()
            .map(|l| {
                let mut dirs: Vec<(String, usize)> = l
                    .directories
                    .iter()
                    .map(|(d, s)| (d.clone(), s.len()))
                    .collect();
                dirs.sort();
                (l.lsp.server().language, l.sessions, l.uncovered, dirs)
            })
            .collect();
        assert_eq!(
            got,
            [
                ("Rust", 3, 3, {
                    let mut dirs = vec![
                        (repo_s.clone(), 1),
                        (gone_s.clone(), 1),
                        (wt_s.clone(), 1),
                        (other_s.clone(), 1),
                    ];
                    dirs.sort();
                    dirs
                }),
                ("Python", 1, 1, vec![(repo_s.clone(), 1)]),
            ]
        );
        // A later window leaves only the sessions after it.
        let later = lsp_use(&data, Some(START + 4_500), START + 3_600_000);
        assert_eq!((later[0].sessions, later[0].languages[0].sessions), (2, 2));

        let home_s = text(&w.home);
        let home = |p: &str| crate::report::home_path(Some(&home_s), p);
        let view = View {
            home: Some(&home_s),
            arrow: "->",
        };
        // The worktree and the directory removed since are the repository's.
        let rust = &u.languages[0].directories;
        assert_eq!(
            super::projects(rust, Some(&home_s)),
            [(repo_s.clone(), 3), (other_s.clone(), 1)]
        );
        let items = items(u, &view);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name, "Rust code intelligence");
        assert_eq!(
            items[0].text,
            "Rust files were read or edited in 3 of 4 sessions (75%), most in ~/repo (3 sessions) and ~/other (1). The rust-analyzer-lsp plugin is not installed. Its language server, rust-analyzer, is on the PATH token-max runs with. With the plugin, Claude looks up a symbol's definition and references instead of searching text and reading each candidate file, and sees the type errors an edit introduced. Anthropic recommends it for typed languages; what it saves is not measured."
                .replace("~/repo", &home(&repo_s))
                .replace("~/other", &home(&other_s))
        );
        assert_eq!(
            items[0].how,
            Some(How::Manual {
                step: format!(
                    "with CLAUDE_CONFIG_DIR set to {}, run `claude plugin install rust-analyzer-lsp@claude-plugins-official`, then start a new session",
                    home(&text(&w.claude))
                ),
                reason: "no setting installs a plugin".into(),
            })
        );
        assert_eq!(items[0].sources[2], SERVERS[10].readme);
        assert!(!items[0].quiet);
        let python = &items[1].text;
        assert!(
            python.contains(&format!(
                " The pyright-lsp plugin is installed only for {}, which 0 of them ran in. Its language server, pyright-langserver, is not on the PATH token-max runs with,",
                home(&other_s)
            )),
            "{python}"
        );
        let Some(How::Manual { step, .. }) = &items[1].how else {
            panic!("{:?}", items[1].how)
        };
        assert!(step.starts_with("install pyright-langserver, as the plugin's README says, then with CLAUDE_CONFIG_DIR set to "));
    }

    #[test]
    fn a_repository_at_home_holds_no_project_but_home() {
        let w = Workspace::new();
        let home = text(&w.home);
        fs::create_dir_all(w.home.join(".git")).unwrap();
        let parent = w.home.join("worktrees/x");
        fs::create_dir_all(&parent).unwrap();
        let gone = text(&parent.join("t3-gone"));
        assert_eq!(project(&gone, Some(&home)), text(&parent));
        assert_eq!(project(&home, Some(&home)), home);
        // Without a home to stop at, the dotfiles repository would claim it.
        assert_eq!(project(&gone, None), home);
    }

    fn language(
        lsp: usize,
        sessions: u64,
        uncovered: u64,
        plugin: Plugin,
        binary: bool,
    ) -> LanguageUse {
        LanguageUse {
            lsp: Lsp(lsp as u8),
            sessions,
            uncovered,
            directories: vec![],
            plugin,
            binary,
        }
    }

    #[test]
    fn a_card_names_its_change_by_plugin_state_and_none_when_nothing_is_missing() {
        let w = Workspace::new();
        let settings = w.claude.join("settings.json");
        fs::write(&settings, "{}\n").unwrap();
        let u = |l: LanguageUse| LspUse {
            profiles: vec!["p".into()],
            sessions: 300,
            languages: vec![l],
            settings_file: text(&settings),
            root: text(&w.home.join(".claude-work")),
            unresolved: false,
        };
        // Five projects, the first two with more sessions.
        let dirs: Vec<(String, Vec<Text>)> = [("a", 5), ("b", 2), ("c", 1), ("d", 1), ("e", 1)]
            .into_iter()
            .map(|(name, n)| {
                let dir = w.home.join(name);
                fs::create_dir_all(&dir).unwrap();
                let sessions = (0..n).map(|i| format!("{name}{i}").into()).collect();
                (text(&dir), sessions)
            })
            .collect();
        let view = View {
            home: None,
            arrow: "->",
        };
        let card = |l: LanguageUse| {
            let l = LanguageUse {
                directories: dirs.clone(),
                ..l
            };
            items(&u(l), &view)
        };
        // Loaded everywhere with its server on PATH: no card.
        assert!(card(language(10, 10, 0, Plugin::On, true)).is_empty());
        // Loaded, but its server is missing.
        let on = card(language(10, 10, 0, Plugin::On, false));
        assert_eq!(
            on[0].how,
            Some(How::Manual {
                step: "install rust-analyzer, as the plugin's README says".into(),
                reason: "token-max installs no programs".into(),
            })
        );
        assert!(
            on[0].text.starts_with(
                &format!("Rust files were read or edited in 10 of 300 sessions (3%), most in {} (5 sessions), {} (2), {} (1) and 2 more projects. The rust-analyzer-lsp plugin is installed. Its language server, rust-analyzer, is not on", dirs[0].0, dirs[1].0, dirs[2].0)
            ),
            "{}",
            on[0].text
        );
        // Another Claude home is named, since a bare install goes to the shell's.
        let missing = card(language(10, 1, 1, Plugin::Missing, true));
        assert!(missing[0].text.contains("(under 1%)"));
        let Some(How::Manual { step, .. }) = &missing[0].how else {
            panic!("{:?}", missing[0].how)
        };
        assert_eq!(
            step,
            &format!(
                "with CLAUDE_CONFIG_DIR set to {}, run `claude plugin install rust-analyzer-lsp@claude-plugins-official`, then start a new session",
                text(&w.home.join(".claude-work"))
            )
        );
        // Blocked by policy: shown, with nothing to act on.
        let blocked = card(language(10, 10, 10, Plugin::Blocked, true));
        assert!(blocked[0].quiet && blocked[0].how.is_none());
        // Turned off in the user file: one edit there, which the walk writes.
        let id = "rust-analyzer-lsp@claude-plugins-official".to_string();
        let off = card(language(
            10,
            10,
            10,
            Plugin::Off {
                id: id.clone(),
                listed: true,
            },
            true,
        ));
        assert_eq!(off[0].value, "off -> on");
        // T3 launch arguments may replace the settings it reads, so the edit is by hand.
        let unsettled = LspUse {
            unresolved: true,
            ..u(language(
                10,
                10,
                10,
                Plugin::Off {
                    id: id.clone(),
                    listed: false,
                },
                true,
            ))
        };
        let by_hand = items(&unsettled, &view);
        assert!(by_hand[0].text.contains("no enabledPlugins entry in"));
        assert!(
            matches!(&by_hand[0].how, Some(How::Manual { reason, .. }) if reason.starts_with("T3 launches"))
        );
        let owner = crate::vibes::owner(&off, None, 100.0);
        let plan = owner
            .plan(&[crate::doctor::Event::Choose {
                offer: 0,
                yes: true,
            }])
            .unwrap();
        crate::doctor::write(&plan).unwrap();
        let written: Value = serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(written, json!({"enabledPlugins": {id: true}}));
    }
}
