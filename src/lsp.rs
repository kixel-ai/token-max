//! Code intelligence plugins: the language of each file Claude read or edited, and `vibes`'
//! card for each typed language whose official plugin a new session wouldn't load.
//!
//! A session's language comes from the files its tool calls named, not marker files at its
//! directory: most recorded directories are worktrees deleted since, and a `build.gradle` can't
//! tell Java from Kotlin.
//!
//! The card judges only what a new session gets now from the user and managed settings: whether
//! the official plugin is installed and turned on. It doesn't check that the plugin's server
//! is on the `PATH` claude starts with, which token-max can only guess at and Claude Code's
//! `/plugin` Errors tab reports from the real one. It doesn't replay which plugins past sessions loaded, which would mean copying Claude Code's
//! rules for project installs and project settings. The plugins are read only when the report
//! is built, never during the scan, and what can't be read stays with the card.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use serde_json::{Map, Value};

use crate::collections::{HashMap, HashSet, IndexMap};
use crate::discovery::{git_dir, join, main_checkout, project_directories, realpath};
use crate::doctor::{Edit, How, Op, Patch};
use crate::model::{EventKind, Harness, Id, Millis, Profile, Scan, StructuralEvent, Text};
use crate::report::plural;
use crate::vibes::Item;

/// One language server plugin of the official marketplace.
pub struct Server {
    pub language: &'static str,
    /// Its marketplace entry name, such as `rust-analyzer-lsp`.
    pub plugin: &'static str,
    /// The program it starts, which the plugin does not include.
    pub binary: &'static str,
    /// The file extensions it handles, as its marketplace entry maps them.
    extensions: &'static [&'static str],
}

impl Server {
    /// Where its README says how to install the binary.
    pub fn readme(&self) -> String {
        format!(
            "https://github.com/anthropics/claude-plugins-official/tree/main/plugins/{}",
            self.plugin
        )
    }
}

const MARKETPLACE: &str = "claude-plugins-official";

/// The variable that moves Claude Code's plugins directory.
pub const CACHE_DIR: &str = "CLAUDE_CODE_PLUGIN_CACHE_DIR";

/// Anthropic's costs page, which recommends these plugins for typed languages.
pub const COSTS: &str =
    "https://code.claude.com/docs/en/costs#install-code-intelligence-plugins-for-typed-languages";
/// The page that lists the official plugins, their binaries and how to install them.
pub const GUIDE: &str = "https://code.claude.com/docs/en/plugins/code-intelligence";

/// Every plugin the code intelligence page lists for a statically typed language, which the
/// costs page's advice names, with the command and extensions each marketplace entry maps,
/// checked on 2026-11-23. Lua, PHP, Python, Ruby and Liquid are left out.
pub const SERVERS: [Server; 8] = [
    Server {
        language: "C/C++",
        plugin: "clangd-lsp",
        binary: "clangd",
        extensions: &["c", "h", "cpp", "cc", "cxx", "hpp", "hxx", "C", "H"],
    },
    Server {
        language: "C#",
        plugin: "csharp-lsp",
        binary: "csharp-ls",
        extensions: &["cs"],
    },
    Server {
        language: "Go",
        plugin: "gopls-lsp",
        binary: "gopls",
        extensions: &["go"],
    },
    Server {
        language: "Java",
        plugin: "jdtls-lsp",
        binary: "jdtls",
        extensions: &["java"],
    },
    Server {
        language: "Kotlin",
        plugin: "kotlin-lsp",
        binary: "kotlin-lsp",
        extensions: &["kt", "kts"],
    },
    Server {
        language: "Rust",
        plugin: "rust-analyzer-lsp",
        binary: "rust-analyzer",
        extensions: &["rs"],
    },
    Server {
        language: "Swift",
        plugin: "swift-lsp",
        binary: "sourcekit-lsp",
        extensions: &["swift"],
    },
    Server {
        language: "TypeScript and JavaScript",
        plugin: "typescript-lsp",
        binary: "typescript-language-server",
        extensions: &["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"],
    },
];

/// A language of [`SERVERS`], by its index there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
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

    /// The official plugin's ID, such as `rust-analyzer-lsp@claude-plugins-official`.
    fn official(self) -> String {
        format!("{}@{MARKETPLACE}", self.server().plugin)
    }
}

/// Whether a plugin ID is one of [`SERVERS`]' official plugins.
fn official(id: &str) -> bool {
    id.split_once('@').is_some_and(|(name, market)| {
        market == MARKETPLACE && SERVERS.iter().any(|s| s.plugin == name)
    })
}

/// What a Claude tool event says for code intelligence, never a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum File {
    /// The language of the file a `Read`, `Edit`, `MultiEdit` or `Write` call named.
    Language(Lsp),
    /// The call returned a result that wasn't an error.
    Done,
}

impl File {
    /// What a tool call named `tool` with `path` as its `file_path` says, if anything.
    pub fn call(tool: Option<&str>, path: Option<&str>) -> Option<File> {
        match tool? {
            "Read" | "Edit" | "MultiEdit" | "Write" => Lsp::of(path?).map(File::Language),
            _ => None,
        }
    }
}

/// A path with a leading `~` read as `home`, as Claude Code reads its plugins directory.
pub(crate) fn expand(home: &str, path: &str) -> String {
    match path.strip_prefix('~') {
        Some("") => home.to_string(),
        Some(rest) if rest.starts_with(['/', '\\']) => join(&[home, &rest[1..]]),
        _ => path.to_string(),
    }
}

/// The first of `settings` whose `permissions.deny` turns off the `LSP` tool, by its file.
pub(crate) fn denies_lsp(settings: &[(&Map<String, Value>, &str)]) -> Option<String> {
    settings
        .iter()
        .find(|(s, _)| {
            s.get("permissions")
                .and_then(|p| p.get("deny"))
                .and_then(Value::as_array)
                .is_some_and(|rules| rules.iter().any(|r| r == "LSP"))
        })
        .map(|(_, file)| file.to_string())
}

/// A Claude profile's plugin settings, as discovery found them without reading the plugins.
#[derive(Clone, Debug, PartialEq)]
pub struct Plugins {
    /// The plugins directory; None when a T3 environment value token-max can't read sets it.
    pub dir: Option<String>,
    /// `CLAUDE_CODE_PLUGIN_CACHE_DIR`, `~` expanded, when it is set.
    pub cache_dir: Option<String>,
    /// Whether a step must set `CLAUDE_CONFIG_DIR` to reach the profile: it isn't the default
    /// `~/.claude`, or the shell's variable names it, which keeps Claude Code's state inside.
    pub named: bool,
    /// The settings file whose `permissions.deny` turns off the `LSP` tool, if one does.
    pub denied: Option<String>,
    /// The home directory, which no project's repository may claim.
    pub home: String,
    /// The official plugins' `enabledPlugins` entries in the user settings.
    pub user: Result<Map<String, Value>, Unread>,
    /// The official plugins' `enabledPlugins` entries in managed settings.
    pub managed: Result<Map<String, Value>, Unread>,
}

/// Why a profile's plugins can't be read.
#[derive(Clone, Debug, PartialEq)]
pub enum Unread {
    /// A T3 environment value token-max can't read, such as a secret, sets
    /// `CLAUDE_CODE_PLUGIN_CACHE_DIR`.
    Secret,
    /// A file Claude Code reads holds what it would not write, or can't be read.
    Invalid { file: String, problem: String },
    /// `CLAUDE_CODE_PLUGIN_CACHE_DIR` is this relative path, which claude reads from
    /// wherever it starts.
    Relative(String),
}

/// The `enabledPlugins` entries of `settings`, read from `file`, for the official language
/// server plugins: each `true`, `false` or a list of version constraints, which keeps the
/// plugin on.
pub(crate) fn enabled_entries(
    settings: &Map<String, Value>,
    file: &str,
) -> Result<Map<String, Value>, Unread> {
    let mut found = Map::new();
    let entries = crate::controls::members(settings.get("enabledPlugins"));
    for (id, value) in entries.iter().filter(|(id, _)| official(id)) {
        if !matches!(value, Value::Bool(_) | Value::Array(_)) {
            return Err(Unread::Invalid {
                file: file.into(),
                problem: format!("enabledPlugins.{id} is neither true, false nor a list"),
            });
        }
        found.insert(id.clone(), value.clone());
    }
    Ok(found)
}

/// Whether an `enabledPlugins` value turns its plugin on.
fn on(value: &Value) -> bool {
    matches!(value, Value::Bool(true) | Value::Array(_))
}

/// Whether `file` exists, checked before it is opened, since opening a named pipe or a device
/// can block; anything there but a file can't be read.
fn regular(file: &str) -> Result<bool, Unread> {
    let invalid = |problem: &str| Unread::Invalid {
        file: file.into(),
        problem: problem.into(),
    };
    match fs::metadata(file) {
        Ok(meta) if meta.is_file() => Ok(true),
        Ok(_) => Err(invalid("it is not a file")),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(_) => Err(invalid("it can't be read")),
    }
}

/// Where an official plugin is installed.
#[derive(Clone, Debug, Default, PartialEq)]
struct Install {
    /// A user or managed record, for every project.
    everywhere: bool,
    /// Each project a project or local record names.
    projects: Vec<String>,
}

/// Each official language server plugin `installed_plugins.json` records. A missing file
/// records none. The first version of the file holds one record per plugin, with no scope,
/// which is the user's.
fn installs(file: &str) -> Result<IndexMap<String, Install>, Unread> {
    let invalid = |problem: &str| Unread::Invalid {
        file: file.into(),
        problem: problem.into(),
    };
    if !regular(file)? {
        return Ok(IndexMap::default());
    }
    let bytes = fs::read(file).map_err(|_| invalid("it can't be read"))?;
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|_| invalid("it is not valid JSON"))?;
    let Some(Value::Object(plugins)) = value.get("plugins") else {
        return Err(invalid("it has no plugins object"));
    };
    let mut found = IndexMap::default();
    for (id, records) in plugins.iter().filter(|(id, _)| official(id)) {
        let records = match records {
            Value::Array(records) => records.as_slice(),
            Value::Object(_) => std::slice::from_ref(records),
            _ => return Err(invalid(&format!("{id}'s record is not a list"))),
        };
        let mut install = Install::default();
        for record in records {
            let Value::Object(record) = record else {
                return Err(invalid(&format!("{id} has a record that is not an object")));
            };
            let project = record.get("projectPath").and_then(Value::as_str);
            match (record.get("scope"), project) {
                // The first version's records carry no scope.
                (None, _) => install.everywhere = true,
                (Some(Value::String(s)), _) if s == "user" || s == "managed" => {
                    install.everywhere = true
                }
                // Only named, so a path of another host, such as Windows' under WSL, is one.
                (Some(Value::String(s)), Some(path))
                    if (s == "project" || s == "local") && !path.is_empty() =>
                {
                    install.projects.push(path.into())
                }
                _ => {
                    return Err(invalid(&format!(
                        "{id} has a record whose scope or projectPath token-max doesn't know"
                    )));
                }
            }
        }
        found.insert(id.clone(), install);
    }
    Ok(found)
}

/// The main checkout of the Git repository holding the existing directory `dir`, as Claude
/// Code roots a project: a linked worktree's is its main repository's. None outside a
/// repository, or when its metadata can't be read.
fn checkout(dir: &str) -> Option<String> {
    let paths = project_directories(dir, &[".git".into()]).ok()?;
    let root = paths.last()?;
    git_dir(root).ok()??;
    main_checkout(root).ok()
}

/// `dir` by its real path, from its nearest existing ancestor's, so two spellings of one
/// directory match before it exists; a relative one stays as given.
fn real(dir: &str) -> String {
    if !Path::new(dir).is_absolute() {
        return dir.into();
    }
    // `..` climbs the path as spelled, before any link is followed, as Claude Code's path
    // join reads it.
    let dir = join(&[dir]);
    let dir = dir.as_str();
    let base = nearest(dir).and_then(|at| Some((at, realpath(at).ok()?)));
    match base {
        Some((at, base)) => match dir[at.len()..].trim_matches(std::path::is_separator) {
            "" => base,
            rest => join(&[&base, rest]),
        },
        None => dir.into(),
    }
}

/// `dir`, or its nearest ancestor that is still a directory.
fn nearest(dir: &str) -> Option<&str> {
    let mut at = dir;
    while !Path::new(at).is_dir() {
        let up = crate::discovery::dirname(at);
        if up == at {
            return None;
        }
        at = up;
    }
    Some(at)
}

/// The project a recorded directory belongs to: the main checkout of the Git repository that
/// holds the nearest directory still there, such as a removed worktree's parent, else that
/// directory. A repository at `home` or above it, such as one of dotfiles, holds no project
/// but `home` itself.
fn project(cwd: &str, home: Option<&str>) -> String {
    let Some(dir) = nearest(cwd) else {
        return cwd.into();
    };
    // Named by its real path, so a symbolic link and its target are one project.
    let real = realpath(dir).unwrap_or_else(|_| dir.into());
    let home = home.map(|h| realpath(h).unwrap_or_else(|_| h.into()));
    match checkout(&real) {
        Some(c) if c != real && home.is_some_and(|h| Path::new(&h).starts_with(&c)) => real,
        Some(c) => c,
        None => real,
    }
}

/// One file tool call that named a language and returned a result that wasn't an error.
struct Touch<'a> {
    event: &'a StructuralEvent,
    lsp: Lsp,
    /// The session it counts for: a subagent's is its parent's.
    session: &'a Text,
    /// The directory that session started in.
    dir: Option<&'a Text>,
}

/// Each Claude subagent session's parent.
fn parents(scan: &Scan) -> HashMap<&Text, &Text> {
    scan.subagents
        .iter()
        .filter(|s| s.harness == Harness::Claude)
        .filter_map(|s| Some((&s.session, s.parent.as_ref()?)))
        .collect()
}

/// The scan's Claude file tool calls from `since` to `now` that named a language and
/// returned a result that wasn't an error.
fn touches<'a>(
    scan: &'a Scan,
    parents: &HashMap<&'a Text, &'a Text>,
    since: Option<Millis>,
    now: Millis,
) -> Vec<Touch<'a>> {
    let starts: Vec<(&StructuralEvent, Lsp, &Id)> = scan
        .events
        .iter()
        .filter_map(|e| match (e.file, &e.tool_id) {
            (Some(File::Language(lsp)), Some(id))
                if e.kind == EventKind::ToolStart
                    && e.harness == Harness::Claude
                    && since.is_none_or(|s| e.time >= s)
                    && e.time <= now =>
            {
                Some((e, lsp, id))
            }
            _ => None,
        })
        .collect();
    if starts.is_empty() {
        return vec![];
    }
    // A call with no result, such as one interrupted, read or edited nothing either.
    let wanted: HashSet<&Id> = starts.iter().map(|&(_, _, id)| id).collect();
    let done: HashSet<&Id> = scan
        .events
        .iter()
        .filter(|e| e.kind == EventKind::ToolEnd && e.file == Some(File::Done))
        .filter_map(|e| e.tool_id.as_ref())
        .filter(|id| wanted.contains(id))
        .collect();
    starts
        .into_iter()
        .filter(|(_, _, id)| done.contains(id))
        .map(|(e, lsp, _)| {
            let session = parents.get(&e.session).copied().unwrap_or(&e.session);
            Touch {
                event: e,
                lsp,
                session,
                dir: scan.sessions.cwd(session).or(e.cwd.as_ref()),
            }
        })
        .collect()
}

/// Which settings turn the official plugin off for a new session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Off {
    /// The user settings, which the card can edit.
    User,
    /// Managed settings, which nothing overrides.
    Managed,
}

/// The languages one settings file's Claude profiles worked in over the window, for those
/// that share one plugins directory, and what a new session there gets for each.
#[derive(Clone, Debug, PartialEq)]
pub struct LspUse {
    pub profiles: Vec<String>,
    /// Sessions with a request in the window, a subagent's counted as its parent's.
    pub sessions: u64,
    /// Each language some session read or edited a file of, most sessions first.
    pub languages: Vec<LanguageUse>,
    /// The user settings file, which an edit to `enabledPlugins` goes to.
    pub settings_file: String,
    /// The Claude home the plugins are installed for.
    pub root: String,
    /// Where plugins are installed; None when a T3 value token-max can't read sets it.
    pub plugins_dir: Option<String>,
    /// `CLAUDE_CODE_PLUGIN_CACHE_DIR`, when it moves the plugins directory.
    pub cache_dir: Option<String>,
    /// Whether a step must set `CLAUDE_CONFIG_DIR` to reach the profile.
    pub named: bool,
    /// Whether settings token-max doesn't read may replace the user's, such as a T3
    /// instance's launch arguments.
    pub unresolved: bool,
    /// The settings file whose `permissions.deny` turns off the LSP tool, which Claude looks
    /// symbols up with, if one does.
    pub denied: Option<String>,
    /// Why the plugins can't be read, which leaves the card nothing else to say.
    pub unread: Option<Unread>,
}

/// The sessions that read or edited files of one language, and its plugin now.
#[derive(Clone, Debug, PartialEq)]
pub struct LanguageUse {
    pub lsp: Lsp,
    pub sessions: u64,
    /// Sessions that may have been another profile's, left out of every count.
    pub shared: u64,
    /// Each project the sessions ran in, with how many, most first.
    pub projects: Vec<(String, u64)>,
    /// Whether a user or managed install of the official plugin is recorded.
    pub installed: bool,
    /// The projects a project or local install of it names.
    pub installed_for: Vec<String>,
    /// The settings that turn it off for a new session, if any do.
    pub off: Option<Off>,
}

/// One line per Claude settings file and plugins directory: its sessions from `since`, else
/// the scan's start, to `now`, the languages of the files their tool calls named, and each
/// language's plugin as the line's plugins directory and settings have it now. A disabled T3
/// instance, which starts nothing, has none; `--defaults` reads no plugins.
pub fn lsp_use(scan: &Scan, since: Option<Millis>, now: Millis) -> Vec<LspUse> {
    let claude: Vec<&Profile> = scan
        .profiles
        .iter()
        .filter(|p| {
            p.harness == Harness::Claude
                && p.plugins.is_some()
                && p.t3.as_ref().is_none_or(|t| t.enabled)
        })
        .collect();
    if claude.is_empty() {
        return vec![];
    }
    let window = |time: Millis| since.is_none_or(|s| time >= s) && time <= now;
    let parents = parents(scan);
    let touched = touches(scan, &parents, since, now);
    // Each plugins directory by its real path, so profiles reaching one through a link share
    // a card and its registry is read from the directory the card names; resolved here
    // rather than in discovery, which the `sessions` export runs too.
    let dirs: HashMap<&str, Option<String>> = claude
        .iter()
        .map(|p| {
            let dir = p.plugins.as_ref().and_then(|d| d.dir.as_deref());
            (p.id.as_str(), dir.map(real))
        })
        .collect();
    let owner = |p: &Profile| {
        format!(
            "{}:enabledPlugins\n{}",
            p.settings_file,
            dirs[p.id.as_str()].as_deref().unwrap_or("unread plugins")
        )
    };
    let mut projects: HashMap<&Text, String> = HashMap::default();
    crate::rows::groups(claude.iter().copied(), owner)
        .iter()
        .filter_map(|line| {
            // A record any other line may hold is left out, not counted twice.
            let own = |profiles: &[String]| profiles.iter().all(|p| line.ids.contains(p));
            let mut by_language: IndexMap<Lsp, IndexMap<&Text, Option<&Text>>> =
                IndexMap::default();
            let mut shared: HashMap<Lsp, HashSet<&Text>> = HashMap::default();
            for t in &touched {
                if own(&t.event.profiles) {
                    by_language
                        .entry(t.lsp)
                        .or_default()
                        .entry(t.session)
                        .or_insert(t.dir);
                } else if line.covers(&t.event.profiles) {
                    shared.entry(t.lsp).or_default().insert(t.session);
                }
            }
            if by_language.is_empty() && shared.is_empty() {
                return None;
            }
            // A language only sessions of uncertain owner worked in still gets its card.
            for &lsp in shared.keys() {
                by_language.entry(lsp).or_default();
            }
            let mut sessions: HashSet<&Text> = scan
                .requests
                .iter()
                .filter(|r| r.harness == Harness::Claude && window(r.time) && own(&r.profiles))
                .map(|r| parents.get(&r.session).copied().unwrap_or(&r.session))
                .collect();
            sessions.extend(by_language.values().flat_map(|s| s.keys().copied()));
            let lead = line.lead;
            let plugins = lead.plugins.as_ref().expect("filtered on its plugins");
            let read = read(plugins, dirs[lead.id.as_str()].as_deref());
            let languages = match &read {
                Err(_) => vec![],
                Ok(installs) => {
                    let mut languages: Vec<LanguageUse> = by_language
                        .iter()
                        .map(|(&lsp, by_session)| {
                            let shared = shared.get(&lsp).map_or(0, |s| {
                                s.iter().filter(|s| !by_session.contains_key(*s)).count()
                            });
                            let mut counts: IndexMap<String, u64> = IndexMap::default();
                            for dir in by_session.values().flatten() {
                                let name = projects
                                    .entry(dir)
                                    .or_insert_with(|| project(dir, Some(&plugins.home)));
                                *counts.entry(name.clone()).or_default() += 1;
                            }
                            language(lsp, plugins, installs, by_session.len(), shared, counts)
                        })
                        .collect();
                    languages.sort_by(|a, b| b.sessions.cmp(&a.sessions).then(a.lsp.cmp(&b.lsp)));
                    languages
                }
            };
            Some(LspUse {
                profiles: line.ids.clone(),
                sessions: sessions.len() as u64,
                languages,
                settings_file: lead.settings_file.clone(),
                root: lead.root.clone(),
                plugins_dir: dirs[lead.id.as_str()].clone(),
                cache_dir: plugins.cache_dir.clone(),
                named: plugins.named,
                unresolved: line
                    .members
                    .iter()
                    .any(|p| p.t3.as_ref().is_some_and(|t| t.settings_unresolved)),
                denied: plugins.denied.clone(),
                unread: read.err(),
            })
        })
        .collect()
}

/// The official plugins the resolved plugins directory `dir` records, once the profile's
/// settings' entries are known to be ones Claude Code would write.
fn read(plugins: &Plugins, dir: Option<&str>) -> Result<IndexMap<String, Install>, Unread> {
    let dir = dir.ok_or(Unread::Secret)?;
    if !Path::new(dir).is_absolute() {
        return Err(Unread::Relative(dir.into()));
    }
    plugins.user.as_ref().map_err(Clone::clone)?;
    plugins.managed.as_ref().map_err(Clone::clone)?;
    installs(&join(&[dir, "installed_plugins.json"]))
}

/// One language's plugin as a new session gets it, with the sessions that worked in it.
fn language(
    lsp: Lsp,
    plugins: &Plugins,
    installs: &IndexMap<String, Install>,
    sessions: usize,
    shared: usize,
    counts: IndexMap<String, u64>,
) -> LanguageUse {
    let id = lsp.official();
    let install = installs.get(&id).cloned().unwrap_or_default();
    let entry = |settings: &Result<Map<String, Value>, Unread>| {
        settings.as_ref().ok().and_then(|s| s.get(&id)).map(on)
    };
    // Managed settings win over the user's. With neither, a plugin starts on: its
    // `defaultEnabled` defaults to true, and no official language server plugin sets it.
    let off = match (entry(&plugins.managed), entry(&plugins.user)) {
        (Some(false), _) => Some(Off::Managed),
        (None, Some(false)) => Some(Off::User),
        _ => None,
    };
    let mut projects: Vec<(String, u64)> = counts.into_iter().collect();
    projects.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut installed_for = install.projects;
    installed_for.sort();
    installed_for.dedup();
    LanguageUse {
        lsp,
        sessions: sessions as u64,
        shared: shared as u64,
        projects,
        installed: install.everywhere,
        installed_for,
        off,
    }
}

/// How a card prints: paths from `~` under `home`, and the report's arrow, such as "->".
pub struct View<'a> {
    pub home: Option<&'a str>,
    pub arrow: &'a str,
}

/// How many projects a card names before it counts the rest.
const NAMED: usize = 3;

/// Why a T3 instance's user settings may not decide its plugins.
const UNRESOLVED: &str = "its launch arguments, a secret environment value or machine policy can replace the settings it reads, and which apply is unresolved";

const WHAT: &str = "A plugin that runs a language server for Claude: code navigation by symbol, and diagnostics after each edit.";

/// `vibes`' cards for one line: one for each language whose official plugin a new session
/// wouldn't load, or one saying why the plugins can't be read.
pub fn items(u: &LspUse, view: &View<'_>) -> Vec<Item> {
    let home = |path: &str| crate::report::home_path(view.home, path);
    if let Some(unread) = &u.unread {
        let (text, quiet) = match unread {
            Unread::Secret => (
                format!(
                    "A T3 environment value token-max can't read, such as a secret, sets {CACHE_DIR}, so it can't tell which code intelligence plugins this instance has, and advises none."
                ),
                true,
            ),
            Unread::Relative(dir) => (
                format!(
                    "{CACHE_DIR} is the relative path {dir}, which claude reads from wherever it starts, so token-max can't tell which code intelligence plugins this profile has, and advises none."
                ),
                true,
            ),
            // Loud, since fixing the file is the only way to get the advice back.
            Unread::Invalid { file, problem } => (
                format!(
                    "token-max can't read {}: {problem}. Until it can, it gives no advice on code intelligence plugins here.",
                    home(file)
                ),
                false,
            ),
        };
        return vec![Item {
            name: "Code intelligence".into(),
            value: String::new(),
            what: WHAT.into(),
            text,
            sources: vec![GUIDE.into()],
            how: None,
            quiet,
            after: vec![],
            defers: None,
        }];
    }
    u.languages
        .iter()
        .filter_map(|l| item(u, l, view))
        .collect()
}

fn item(u: &LspUse, l: &LanguageUse, view: &View<'_>) -> Option<Item> {
    let home = |path: &str| crate::report::home_path(view.home, path);
    let s = l.lsp.server();
    let id = l.lsp.official();
    let sources = vec![COSTS.into(), GUIDE.into(), s.readme()];
    // A project install for every project the sessions ran in serves them as well, though
    // those projects' settings, which the card doesn't read, decide whether it is on there.
    let covered = !l.installed
        && l.sessions > 0
        && l.projects.iter().map(|(_, n)| n).sum::<u64>() == l.sessions
        && l.projects.iter().all(|(p, _)| l.installed_for.contains(p));
    let blocked = l.off == Some(Off::Managed);
    let install = !blocked && !l.installed && !covered;
    // The user settings decide only a user or managed install.
    let enable = !blocked && l.installed && l.off == Some(Off::User);
    if !blocked && !install && !enable {
        return None;
    }
    if l.sessions == 0 {
        return Some(Item {
            name: format!("{} code intelligence", s.language),
            value: String::new(),
            what: WHAT.into(),
            text: format!(
                "{} files were read or edited in {} that may have been another profile's, so token-max advises nothing on them.",
                s.language,
                plural(l.shared, "session"),
            ),
            sources,
            how: None,
            quiet: true,
            after: vec![],
            defers: None,
        });
    }
    let mut text = format!(
        "{} files were read or edited in {} of {} ({}%){}.",
        s.language,
        l.sessions,
        plural(u.sessions, "session"),
        crate::report::percent(l.sessions, u.sessions),
        named(&l.projects, &home),
    );
    if l.shared > 0 {
        let (noun, verb) = if l.shared == 1 {
            ("session", "is")
        } else {
            ("sessions", "are")
        };
        text += &format!(
            " {} more {noun} that may have been another profile's {verb} left out.",
            l.shared
        );
    }
    text += &if blocked {
        format!(" Managed settings turn {id} off, and nothing overrides them.")
    } else if install && l.installed_for.is_empty() {
        format!(" The {} plugin is not installed.", s.plugin)
    } else if install {
        format!(
            " The {} plugin is installed only for {}, which leaves out every other project.",
            s.plugin,
            listed(&l.installed_for, &home)
        )
    } else if u.unresolved {
        format!(
            " The {} plugin is installed, and enabledPlugins in {} turns it off, if the T3 instance reads that file: {UNRESOLVED}.",
            s.plugin,
            home(&u.settings_file)
        )
    } else {
        format!(
            " The {} plugin is installed, and enabledPlugins in {} turns it off.",
            s.plugin,
            home(&u.settings_file)
        )
    };
    if install && u.unresolved {
        text += &format!(
            " token-max read {}, which the T3 instance may not use: {UNRESOLVED}.",
            u.plugins_dir
                .as_deref()
                .map_or_else(|| "its plugins directory".into(), home)
        );
    }
    let denied = u.denied.as_deref().filter(|_| !blocked);
    if let Some(file) = denied {
        text += &format!(
            " permissions.deny in {} turns off the LSP tool, which Claude looks symbols up with, so the plugin would give only diagnostics after edits{}.",
            home(file),
            if u.unresolved {
                ", if the T3 instance reads that file"
            } else {
                ""
            }
        );
    }
    // token-max can't know the PATH claude starts with, so it names the server without
    // checking it; Claude Code's /plugin Errors tab reports one it can't find.
    if !blocked {
        text += &format!(
            " The plugin starts {}, which it doesn't include; /plugin's Errors tab says when it isn't on the PATH claude starts with.",
            s.binary
        );
    }
    const LOOKUP: &str = "looks up a symbol's definition and references instead of searching text and reading each candidate file";
    text += &match (denied, u.unresolved) {
        (None, _) => {
            format!(" With the plugin, Claude {LOOKUP}, and sees the errors an edit introduced.")
        }
        (Some(_), false) => " With the plugin, Claude sees the errors an edit introduced.".into(),
        (Some(_), true) => format!(
            " With the plugin, Claude sees the errors an edit introduced, and where the LSP tool is on, {LOOKUP}."
        ),
    };
    text += " Anthropic recommends it for typed languages; what it saves is not measured.";
    let on = Edit {
        path: vec!["enabledPlugins".into(), id.clone()],
        value: Value::Bool(true),
        op: Op::Set,
    };
    let server = format!(
        "install {} on the PATH claude starts with if it isn't, as the plugin's README says",
        s.binary
    );
    // The one change the walk writes: turning the installed plugin on in the profile's own
    // file, when that alone is known to load it.
    let edit = enable && !u.unresolved;
    let how = if edit {
        Some(How::Edit(Patch {
            file: u.settings_file.clone(),
            harness: Harness::Claude,
            edits: vec![on],
        }))
    } else if install && u.unresolved {
        Some(How::Manual {
            step: format!(
                "{server}, then if the plugins directory the T3 instance uses lacks it, run `claude plugin install {id}` with the Claude home and plugins directory the instance uses, then start a new session"
            ),
            reason: UNRESOLVED.into(),
        })
    } else if install {
        Some(How::Manual {
            step: format!(
                "{server}, then {} run `claude plugin install {id}`, then start a new session",
                shell(u, &home)
            ),
            reason: "no setting installs a plugin".into(),
        })
    } else if enable {
        Some(How::Manual {
            step: format!(
                "{server}, then set {} in the settings the T3 instance reads, if they turn it off, then start a new session",
                on.text(Harness::Claude)
            ),
            reason: UNRESOLVED.into(),
        })
    } else {
        None
    };
    Some(Item {
        name: format!("{} code intelligence", s.language),
        value: if edit {
            format!("off {} on", view.arrow)
        } else {
            String::new()
        },
        what: WHAT.into(),
        text,
        sources,
        quiet: how.is_none(),
        how,
        // It advises on enabledPlugins and installs, which no `doctor` line writes.
        after: vec![],
        defers: None,
    })
}

/// The variables a step sets before `claude plugin install`, since a bare install goes to
/// whichever Claude home and plugins directory the shell selects: "with CLAUDE_CONFIG_DIR set
/// to ~/.claude-work and CLAUDE_CODE_PLUGIN_CACHE_DIR unset,".
fn shell(u: &LspUse, home: &dyn Fn(&str) -> String) -> String {
    let cache_dir = u.cache_dir.as_deref().map(home);
    format!(
        "with {},",
        variables(u.named, &home(&u.root), cache_dir.as_deref())
    )
}

/// The variables that reach a Claude profile and its plugins directory from a shell, such as
/// "CLAUDE_CONFIG_DIR set to ~/.claude-work and CLAUDE_CODE_PLUGIN_CACHE_DIR unset".
pub(crate) fn variables(named: bool, root: &str, cache_dir: Option<&str>) -> String {
    match (named, cache_dir) {
        (false, None) => format!("CLAUDE_CONFIG_DIR and {CACHE_DIR} unset"),
        (true, None) => format!("CLAUDE_CONFIG_DIR set to {root} and {CACHE_DIR} unset"),
        (false, Some(dir)) => format!("CLAUDE_CONFIG_DIR unset and {CACHE_DIR} set to {dir}"),
        (true, Some(dir)) => {
            format!("CLAUDE_CONFIG_DIR set to {root} and {CACHE_DIR} set to {dir}")
        }
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
        named.push(format!(
            "{more} more {}",
            if more == 1 { "project" } else { "projects" }
        ));
    }
    format!(", most in {}", list(&named))
}

/// The first projects of `paths`, counting the rest: "~/a, ~/b, ~/c and 2 more".
fn listed(paths: &[String], home: &dyn Fn(&str) -> String) -> String {
    let mut named: Vec<String> = paths.iter().take(NAMED).map(|p| home(p)).collect();
    if paths.len() > NAMED {
        named.push(format!("{} more", paths.len() - NAMED));
    }
    list(&named)
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
    use crate::test_support::{Workspace, native_path};
    use serde_json::json;
    use std::path::PathBuf;

    const START: Millis = 1_793_491_200_000; // 2026-11-01T00:00:00Z

    /// One JSON record per line, creating the file's directory.
    fn write(path: PathBuf, records: &[Value]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let lines: String = records.iter().map(|r| format!("{r}\n")).collect();
        fs::write(path, lines).unwrap();
    }

    fn time(second: i64) -> String {
        jiff::Timestamp::from_millisecond(START + second * 1000)
            .unwrap()
            .to_string()
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
            "timestamp": time(second),
            "cwd": cwd,
            "message": {
                "id": format!("{session}-{second}"),
                "model": "claude-opus-5-5",
                "usage": {"input_tokens": 10, "output_tokens": 1},
                "content": content,
            },
        })
    }

    /// The results of `request`'s first `calls` calls, each an error when `error` is.
    fn results(session: &str, cwd: &str, second: i64, calls: usize, error: bool) -> Value {
        let content: Vec<Value> = (0..calls)
            .map(|i| json!({"type": "tool_result", "tool_use_id": format!("{session}-{second}-{i}"), "is_error": error, "content": "synthetic"}))
            .collect();
        json!({
            "type": "user",
            "sessionId": session,
            "uuid": format!("{session}-{second}-result"),
            "timestamp": time(second + 1),
            "cwd": cwd,
            "message": {"role": "user", "content": content},
        })
    }

    fn json_file(path: PathBuf, value: Value) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, value.to_string()).unwrap();
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
        // Dynamically typed languages get no card.
        for other in [
            ".rs", "Makefile", "notes.md", "/dir.rs/", "", "tool.py", "a.rb",
        ] {
            assert_eq!(language(other), None, "{other:?}");
        }
        // Only the file tools name a file worth a language.
        let rust = Lsp::of("a.rs").map(File::Language);
        assert_eq!(File::call(Some("MultiEdit"), Some("a.rs")), rust);
        assert_eq!(File::call(Some("Grep"), Some("a.rs")), None);
        assert_eq!(File::call(Some("Read"), None), None);
    }

    #[test]
    fn a_plugins_directory_with_a_leading_tilde_is_under_home() {
        let home = native_path("/synthetic/home");
        assert_eq!(expand(&home, "~"), home);
        assert_eq!(
            expand(&home, "~/cache"),
            native_path("/synthetic/home/cache")
        );
        // Another user's home is not this one's.
        assert_eq!(expand(&home, "~alex/cache"), "~alex/cache"); // slash ok: passed through unchanged
        assert_eq!(expand(&home, "/abs/cache"), "/abs/cache");
    }

    #[test]
    fn install_records_are_read_only_for_official_language_servers_and_fail_on_what_claude_would_not_write()
     {
        let w = Workspace::new();
        let file = w.home.join("installed_plugins.json");
        let path = native_path(&file);
        assert_eq!(
            installs(&path),
            Ok(IndexMap::default()),
            "no file, no installs"
        );
        json_file(
            file.clone(),
            json!({"version": 2, "plugins": {
                "rust-analyzer-lsp@claude-plugins-official": [
                    {"scope": "user"},
                    {"scope": "local", "projectPath": "/synthetic/repo"},
                    // A path another host recorded, such as Windows' under WSL, is still named.
                    {"scope": "project", "projectPath": "C:\\repo"},
                ],
                // Another marketplace's copy, and records of other plugins, are not this card's.
                "gopls-lsp@corp": [{"scope": "someday"}],
                "formatter@claude-plugins-official": [{"scope": "someday"}],
            }}),
        );
        let found = installs(&path).unwrap();
        assert_eq!(
            found.keys().collect::<Vec<_>>(),
            ["rust-analyzer-lsp@claude-plugins-official"]
        );
        assert_eq!(
            found["rust-analyzer-lsp@claude-plugins-official"],
            Install {
                everywhere: true,
                projects: vec!["/synthetic/repo".into(), "C:\\repo".into()],
            }
        );
        // The first version of the file holds one record per plugin, which is the user's.
        json_file(
            file.clone(),
            json!({"version": 1, "plugins": {"gopls-lsp@claude-plugins-official": {"version": "1.0.0"}}}),
        );
        assert!(installs(&path).unwrap()["gopls-lsp@claude-plugins-official"].everywhere);
        let id = "gopls-lsp@claude-plugins-official";
        for invalid in [
            "[]".to_string(),
            "{".to_string(),
            json!({"plugins": {id: 1}}).to_string(),
            json!({"plugins": {id: [null]}}).to_string(),
            json!({"plugins": {id: [{"scope": 1}]}}).to_string(),
            json!({"plugins": {id: [{"scope": "someday"}]}}).to_string(),
            json!({"plugins": {id: [{"scope": "project"}]}}).to_string(),
            json!({"plugins": {id: [{"scope": "local", "projectPath": ""}]}}).to_string(),
        ] {
            fs::write(&file, &invalid).unwrap();
            assert!(
                matches!(installs(&path), Err(Unread::Invalid { file, .. }) if file == path),
                "{invalid}"
            );
        }
        // A named pipe or directory is never opened.
        fs::remove_file(&file).unwrap();
        fs::create_dir_all(&file).unwrap();
        assert_eq!(
            installs(&path),
            Err(Unread::Invalid {
                file: path.clone(),
                problem: "it is not a file".into()
            })
        );
    }

    #[test]
    fn a_plugins_directory_is_named_by_its_real_path_even_before_it_exists() {
        let w = Workspace::new();
        let root = realpath(&native_path(&w.home)).unwrap();
        let target = join(&[&root, "claude"]);
        fs::create_dir_all(&target).unwrap();
        assert_eq!(
            real(&join(&[&target, "plugins"])),
            join(&[&target, "plugins"])
        );
        assert_eq!(real(".plugin-cache"), ".plugin-cache");
        // A missing parent's `..` collapses as Claude Code's path join does, and the registry
        // is read from this path too.
        assert_eq!(
            real(&join(&[&target, "missing"]).replace("missing", "missing/../plugins")),
            join(&[&target, "plugins"])
        );
        // A trailing separator names the same directory.
        assert_eq!(
            real(&format!(
                "{}{}",
                join(&[&target, "plugins"]),
                std::path::MAIN_SEPARATOR
            )),
            join(&[&target, "plugins"])
        );
        #[cfg(unix)]
        {
            // On Unix a backslash is part of the name, not a separator.
            assert_eq!(
                real(&format!("{target}/\\plugins")),
                format!("{target}/\\plugins")
            );
            let link = join(&[&root, "link"]);
            std::os::unix::fs::symlink(&target, &link).unwrap();
            assert_eq!(
                real(&join(&[&link, "plugins"])),
                join(&[&target, "plugins"])
            );
            assert_eq!(real(&link), target);
            // Not the link target's parent's `plugins`.
            let inner = join(&[&root, "a", "link"]);
            fs::create_dir_all(join(&[&root, "a"])).unwrap();
            std::os::unix::fs::symlink(&target, &inner).unwrap();
            assert_eq!(
                real(&format!("{inner}/../plugins")),
                join(&[&root, "a", "plugins"])
            );
        }
    }

    #[test]
    fn a_repository_at_home_holds_no_project_but_home() {
        let w = Workspace::new();
        // Checkouts are named by their real path.
        let root = PathBuf::from(realpath(&native_path(&w.home)).unwrap());
        let home = native_path(&root);
        fs::create_dir_all(root.join(".git")).unwrap();
        let parent = root.join("worktrees/x");
        fs::create_dir_all(&parent).unwrap();
        let gone = native_path(parent.join("t3-gone"));
        assert_eq!(project(&gone, Some(&home)), native_path(&parent));
        assert_eq!(project(&home, Some(&home)), home);
        // Without a home to stop at, the dotfiles repository would claim it.
        assert_eq!(project(&gone, None), home);
        // A repository above home holds no project either, even for a directory outside it.
        let shared = root.join("shared/x");
        fs::create_dir_all(&shared).unwrap();
        let alex = native_path(root.join("alex"));
        assert_eq!(
            project(&native_path(&shared), Some(&alex)),
            native_path(&shared)
        );
        // A link into a checkout is in that checkout's project.
        let repo = root.join("work/repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(repo.join("crates/a")).unwrap();
        #[cfg(unix)]
        {
            let link = root.join("current");
            std::os::unix::fs::symlink(repo.join("crates/a"), &link).unwrap();
            assert_eq!(
                project(&native_path(&link), Some(&home)),
                native_path(&repo)
            );
            // So is a link to the checkout itself.
            let alias = root.join("alias");
            std::os::unix::fs::symlink(&repo, &alias).unwrap();
            assert_eq!(
                project(&native_path(&alias), Some(&home)),
                native_path(&repo)
            );
        }
    }

    #[test]
    fn sessions_count_by_language_and_cards_judge_the_plugin_a_new_session_gets() {
        let w = Workspace::new();
        // Checkouts are named by their real path.
        let root = PathBuf::from(realpath(&native_path(&w.home)).unwrap());
        let repo = root.join("repo");
        fs::create_dir_all(repo.join(".git/worktrees/wt")).unwrap();
        // A linked worktree of the repository, and a removed one under it.
        let wt = root.join("wt");
        fs::create_dir_all(&wt).unwrap();
        fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", native_path(repo.join(".git/worktrees/wt"))),
        )
        .unwrap();
        fs::write(repo.join(".git/worktrees/wt/commondir"), "../..\n").unwrap();
        let gone = repo.join(".claude/worktrees/gone");
        let other = root.join("other");
        fs::create_dir_all(&other).unwrap();
        let (repo_s, wt_s, gone_s, other_s) = (
            native_path(&repo),
            native_path(&wt),
            native_path(&gone),
            native_path(&other),
        );
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
                        ("Edit", file("tool.ts")),
                        ("Grep", json!({"path": "/synthetic/x.go"})),
                    ],
                ),
                results("a", &repo_s, 1, 3, false),
                // A later directory doesn't move the session's project.
                request("a", &other_s, 6, &[("Read", file("z.rs"))]),
                results("a", &other_s, 6, 1, false),
            ],
        );
        write(projects.join("b.jsonl"), &[request("b", &gone_s, 2, &[])]);
        let mut sidechain = request(
            "agent",
            &gone_s,
            3,
            &[("Write", file("new.rs")), ("Read", file("y.ts"))],
        );
        sidechain["isSidechain"] = json!(true);
        sidechain["sessionId"] = json!("b");
        let mut done = results("agent", &gone_s, 3, 2, false);
        done["isSidechain"] = json!(true);
        done["sessionId"] = json!("b");
        write(
            projects.join("b/subagents/agent-x.jsonl"),
            &[sidechain, done],
        );
        write(
            projects.join("c.jsonl"),
            &[
                request("c", &other_s, 4, &[("Read", file("lib.rs"))]),
                results("c", &other_s, 4, 1, false),
            ],
        );
        write(
            projects.join("d.jsonl"),
            &[
                request("d", &wt_s, 5, &[("MultiEdit", file("x.tsx"))]),
                results("d", &wt_s, 5, 1, false),
            ],
        );
        // A read of a file that isn't there, or one interrupted before its result, is no
        // Kotlin worked in.
        write(
            projects.join("e.jsonl"),
            &[
                request("e", &other_s, 7, &[("Read", file("App.kt"))]),
                results("e", &other_s, 7, 1, true),
                request("e", &other_s, 9, &[("Read", file("Main.kt"))]),
            ],
        );
        let registry = w.claude.join("plugins/installed_plugins.json");
        let rust = "rust-analyzer-lsp@claude-plugins-official";
        let typescript = "typescript-lsp@claude-plugins-official";
        json_file(
            registry.clone(),
            json!({"version": 2, "plugins": {
                rust: [{"scope": "user"}],
                typescript: [{"scope": "local", "projectPath": other_s}],
                // Another marketplace's copy is never the plugin.
                "typescript-lsp@corp": [{"scope": "user"}],
            }}),
        );
        let settings = w.claude.join("settings.json");
        json_file(
            settings.clone(),
            json!({"model": "opus", "enabledPlugins": {rust: false}}),
        );
        // A project's file is not read: a new session elsewhere still gets the user's.
        json_file(
            other.join(".claude/settings.local.json"),
            json!({"enabledPlugins": {rust: true}}),
        );
        let options = w.scan_options();
        let data = scan(&options).unwrap();
        assert!(!format!("{data:?}").contains("main.rs"), "no path is kept");
        let lines = lsp_use(&data, None, START + 3_600_000);
        assert_eq!(lines.len(), 1, "Codex gets no line");
        let u = &lines[0];
        assert_eq!(
            (u.sessions, u.unread.as_ref(), u.denied.as_deref()),
            (5, None, None)
        );
        let got: Vec<_> = u
            .languages
            .iter()
            .map(|l| (l.lsp.server().language, l.sessions, l.installed, l.off))
            .collect();
        assert_eq!(
            got,
            [
                ("Rust", 3, true, Some(Off::User)),
                ("TypeScript and JavaScript", 3, false, None),
            ]
        );
        assert_eq!(u.languages[1].installed_for, std::slice::from_ref(&other_s));
        // The worktree and the removed one are the repository's.
        assert_eq!(u.languages[1].projects, [(repo_s.clone(), 3)]);
        assert_eq!(
            u.languages[0].projects,
            [(repo_s.clone(), 2), (other_s.clone(), 1)]
        );
        // A later window leaves only the sessions after it.
        let later = lsp_use(&data, Some(START + 4_500), START + 3_600_000);
        assert_eq!((later[0].sessions, later[0].languages.len()), (3, 2));

        let home_s = native_path(&w.home);
        let home = |p: &str| crate::report::home_path(Some(&home_s), p);
        let view = View {
            home: Some(&home_s),
            arrow: "->",
        };
        let cards = items(u, &view);
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].name, "Rust code intelligence");
        assert_eq!(
            cards[0].text,
            format!(
                "Rust files were read or edited in 3 of 5 sessions (60%), most in {} (2 sessions) and {} (1). The rust-analyzer-lsp plugin is installed, and enabledPlugins in {} turns it off. The plugin starts rust-analyzer, which it doesn't include; /plugin's Errors tab says when it isn't on the PATH claude starts with. With the plugin, Claude looks up a symbol's definition and references instead of searching text and reading each candidate file, and sees the errors an edit introduced. Anthropic recommends it for typed languages; what it saves is not measured.",
                home(&repo_s),
                home(&other_s),
                home(&native_path(&settings)),
            )
        );
        // A project install for a project these sessions didn't run in leaves them out.
        assert!(
            cards[1].text.contains(&format!(
                " The typescript-lsp plugin is installed only for {}, which leaves out every other project.",
                home(&other_s)
            )),
            "{}",
            cards[1].text
        );
        assert!(
            matches!(&cards[1].how, Some(How::Manual { step, .. }) if step.starts_with("install typescript-language-server on the PATH claude starts with if it isn't, as the plugin's README says, then with CLAUDE_CONFIG_DIR")),
            "{:?}",
            cards[1].how
        );
        assert_eq!(cards[0].value, "off -> on");
        assert!(!cards[0].quiet);
        // The walk writes the one edit, keeping the rest of the file.
        let owner = crate::vibes::owner(&cards[..1], None, 100.0);
        let plan = owner
            .plan(&[crate::doctor::Event::Choose {
                offer: 0,
                yes: true,
            }])
            .unwrap();
        crate::doctor::write(&plan).unwrap();
        let written: Value = serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(
            written,
            json!({"model": "opus", "enabledPlugins": {rust: true}})
        );
        // Managed settings that turn it off win, with nothing to act on, and a denied LSP
        // tool is named.
        json_file(
            w.managed_settings_file(),
            json!({"enabledPlugins": {rust: false}, "permissions": {"deny": ["LSP"]}}),
        );
        let data = scan(&options).unwrap();
        let lines = lsp_use(&data, None, START + 3_600_000);
        assert_eq!(lines[0].languages[0].off, Some(Off::Managed));
        assert_eq!(
            lines[0].denied,
            Some(native_path(w.managed_settings_file()))
        );
        let cards = items(&lines[0], &view);
        assert!(cards[0].quiet);
        assert!(
            cards[1].text.contains(" permissions.deny in "),
            "{}",
            cards[1].text
        );
        fs::remove_file(w.managed_settings_file()).unwrap();

        // A record the card can't read costs only the card.
        fs::write(&registry, "{").unwrap();
        let data = scan(&options).unwrap();
        let lines = lsp_use(&data, None, START + 3_600_000);
        let cards = items(&lines[0], &view);
        assert_eq!(cards.len(), 1);
        assert!(!cards[0].quiet && cards[0].how.is_none());
        assert_eq!(
            cards[0].text,
            format!(
                "token-max can't read {}: it is not valid JSON. Until it can, it gives no advice on code intelligence plugins here.",
                home(&native_path(&registry))
            )
        );
        // So does an enabledPlugins entry Claude Code would not write.
        json_file(registry.clone(), json!({"version": 2, "plugins": {}}));
        json_file(settings.clone(), json!({"enabledPlugins": {rust: "yes"}}));
        let data = scan(&options).unwrap();
        assert!(matches!(
            &lsp_use(&data, None, START + 3_600_000)[0].unread,
            Some(Unread::Invalid { file, .. }) if *file == native_path(&settings)
        ));
        fs::remove_file(&settings).unwrap();
        // `--defaults`, whose report has no cards, reads no plugins.
        let mut defaults = w.scan_options();
        defaults.discovery.pretend_defaults();
        let data = scan(&defaults).unwrap();
        assert!(lsp_use(&data, None, START + 3_600_000).is_empty());

        // The default home needs no variable to reach; any other does, set in the shell or not.
        assert!(!u.named);
        let work = w.home.join(".claude-work");
        write(
            work.join("projects/p/f.jsonl"),
            &[
                request("f", &other_s, 1, &[("Read", file("lib.rs"))]),
                results("f", &other_s, 1, 1, false),
            ],
        );
        let mut options = w.scan_options();
        options.discovery.claude_dirs = vec![native_path(&work)];
        let data = scan(&options).unwrap();
        let line = lsp_use(&data, None, START + 3_600_000)
            .into_iter()
            .find(|u| u.root == native_path(&work))
            .unwrap();
        assert!(line.named);
        assert!(
            matches!(&items(&line, &view)[0].how, Some(How::Manual { step, .. }) if step.contains(&format!(" with CLAUDE_CONFIG_DIR set to {} and", home(&native_path(&work))))),
            "{:?}",
            items(&line, &view)[0].how
        );
    }

    #[test]
    fn a_card_names_what_a_new_session_needs_and_none_when_nothing_is_missing() {
        let rust = Lsp::of("a.rs").unwrap();
        let id = "rust-analyzer-lsp@claude-plugins-official";
        let settings = native_path("/synthetic/claude/settings.json");
        let u = |l: LanguageUse| LspUse {
            profiles: vec!["p".into()],
            sessions: 300,
            languages: vec![l],
            settings_file: settings.clone(),
            root: native_path("/synthetic/claude-work"),
            plugins_dir: Some(native_path("/synthetic/claude-work/plugins")),
            cache_dir: None,
            named: false,
            unresolved: false,
            denied: None,
            unread: None,
        };
        let usage = |installed: bool, off: Option<Off>| LanguageUse {
            lsp: rust,
            sessions: 10,
            shared: 0,
            projects: vec![],
            installed,
            installed_for: vec![],
            off,
        };
        let view = View {
            home: None,
            arrow: "->",
        };
        let card = |l: LanguageUse| items(&u(l), &view);
        let how = |l: LanguageUse| card(l).remove(0).how;
        // Installed and on: nothing to advise, since Claude Code reports a missing server.
        assert!(card(usage(true, None)).is_empty());
        let server = "install rust-analyzer on the PATH claude starts with if it isn't, as the plugin's README says";
        // A bare install goes to whichever home and plugins directory the shell selects, and
        // turns the plugin on in the user settings too.
        for off in [None, Some(Off::User)] {
            assert_eq!(
                how(usage(false, off)),
                Some(How::Manual {
                    step: format!(
                        "{server}, then with CLAUDE_CONFIG_DIR and CLAUDE_CODE_PLUGIN_CACHE_DIR unset, run `claude plugin install {id}`, then start a new session"
                    ),
                    reason: "no setting installs a plugin".into(),
                })
            );
        }
        let named = items(
            &LspUse {
                named: true,
                cache_dir: Some(native_path("/synthetic/cache")),
                ..u(usage(false, None))
            },
            &view,
        );
        let Some(How::Manual { step: named, .. }) = &named[0].how else {
            panic!("{named:?}")
        };
        assert!(
            named.starts_with(&format!(
                "{server}, then with CLAUDE_CONFIG_DIR set to {} and CLAUDE_CODE_PLUGIN_CACHE_DIR set to {}, run",
                native_path("/synthetic/claude-work"),
                native_path("/synthetic/cache")
            )),
            "{named}"
        );
        // Five projects hold a project install: three are named.
        let partly = card(LanguageUse {
            installed_for: ["a", "b", "c", "d", "e"]
                .map(|p| format!("/synthetic/{p}"))
                .into(),
            ..usage(false, None)
        })
        .remove(0);
        assert!(
            partly.text.contains(
                " The rust-analyzer-lsp plugin is installed only for /synthetic/a, /synthetic/b, /synthetic/c and 2 more, which leaves out every other project."
            ),
            "{}",
            partly.text
        );
        // Project installs for every project the sessions ran in leave nothing to install.
        let covered = |off| LanguageUse {
            projects: vec![("/synthetic/a".into(), 10)],
            installed_for: vec!["/synthetic/a".into()],
            ..usage(false, off)
        };
        // The user settings decide only a user or managed install, so they don't turn these off.
        assert!(card(covered(None)).is_empty());
        assert!(card(covered(Some(Off::User))).is_empty());
        // A session with no recorded directory might have run anywhere, so it isn't covered.
        let uncovered = card(LanguageUse {
            projects: vec![("/synthetic/a".into(), 9)],
            ..covered(None)
        })
        .remove(0);
        assert!(
            uncovered
                .text
                .contains(" is installed only for /synthetic/a, which leaves out"),
            "{}",
            uncovered.text
        );
        // Installed and turned off in the user settings: the one edit the walk writes.
        let off = card(usage(true, Some(Off::User))).remove(0);
        assert_eq!(off.value, "off -> on");
        assert!(
            matches!(&off.how, Some(How::Edit(p)) if p.file == settings && p.edits[0].path == ["enabledPlugins", id])
        );
        // A T3 instance's launch arguments may name other settings, so the change is by hand
        // and says so.
        let unresolved = items(
            &LspUse {
                unresolved: true,
                ..u(usage(true, Some(Off::User)))
            },
            &view,
        )
        .remove(0);
        assert_eq!(
            unresolved.how,
            Some(How::Manual {
                step: format!(
                    r#"{server}, then set "enabledPlugins": {{"{id}": true}} in the settings the T3 instance reads, if they turn it off, then start a new session"#
                ),
                reason: UNRESOLVED.into(),
            })
        );
        assert!(unresolved.text.contains(&format!(
            "if the T3 instance reads that file: {UNRESOLVED}."
        )));
        assert_eq!(unresolved.value, "");
        // Nor may such an instance use the plugins directory token-max read, so its install
        // step names none.
        let unresolved = items(
            &LspUse {
                unresolved: true,
                ..u(usage(false, None))
            },
            &view,
        )
        .remove(0);
        assert!(unresolved.text.contains(&format!(
            " token-max read {}, which the T3 instance may not use: {UNRESOLVED}.",
            native_path("/synthetic/claude-work/plugins")
        )));
        assert_eq!(
            unresolved.how,
            Some(How::Manual {
                step: format!(
                    "{server}, then if the plugins directory the T3 instance uses lacks it, run `claude plugin install {id}` with the Claude home and plugins directory the instance uses, then start a new session"
                ),
                reason: UNRESOLVED.into(),
            })
        );
        // A denied LSP tool is named beside an install.
        let denied = items(
            &LspUse {
                denied: Some(native_path("/synthetic/managed.json")),
                ..u(usage(false, None))
            },
            &view,
        )
        .remove(0);
        assert!(denied.text.contains(&format!(
            " permissions.deny in {} turns off the LSP tool,",
            native_path("/synthetic/managed.json")
        )));
        // ...and the card promises no symbol lookup, unless a T3 instance may not read it.
        assert!(
            denied
                .text
                .contains(" With the plugin, Claude sees the errors an edit introduced. Anthropic")
        );
        assert!(!denied.text.contains("looks up"));
        let maybe = items(
            &LspUse {
                denied: Some(native_path("/synthetic/managed.json")),
                unresolved: true,
                ..u(usage(false, None))
            },
            &view,
        )
        .remove(0);
        assert!(
            maybe
                .text
                .contains("only diagnostics after edits, if the T3 instance reads that file. ")
        );
        assert!(
            maybe
                .text
                .contains(", and where the LSP tool is on, looks up")
        );
        // Turned off by managed settings: shown, with nothing to act on, installed or not.
        for installed in [true, false] {
            let blocked = card(usage(installed, Some(Off::Managed))).remove(0);
            assert!(blocked.quiet && blocked.how.is_none());
            assert!(blocked.text.contains(&format!(
                " Managed settings turn {id} off, and nothing overrides them. With the plugin,"
            )));
        }
        // Sessions another profile may have run are named but not counted.
        let shared = card(LanguageUse {
            shared: 2,
            ..usage(false, None)
        })
        .remove(0);
        assert!(
            shared.text.starts_with(
                "Rust files were read or edited in 10 of 300 sessions (3.3%). 2 more sessions that may have been another profile's are left out."
            ),
            "{}",
            shared.text
        );
        // A partial share never reads as all of them.
        let most = card(LanguageUse {
            sessions: 299,
            ..usage(false, None)
        })
        .remove(0);
        assert!(most.text.contains("(99.6%)"), "{}", most.text);
        // A language only such sessions worked in says so, unless nothing is missing.
        let unowned = |installed| {
            card(LanguageUse {
                sessions: 0,
                shared: 2,
                ..usage(installed, None)
            })
        };
        assert!(unowned(true).is_empty());
        let unowned = unowned(false).remove(0);
        assert!(unowned.quiet && unowned.how.is_none());
        assert!(unowned.text.starts_with(
            "Rust files were read or edited in 2 sessions that may have been another profile's, so token-max advises nothing"
        ));
        // Plugins the card can't read: a secret is quiet, a broken file is not.
        let unread = |unread: Unread| {
            items(
                &LspUse {
                    unread: Some(unread),
                    languages: vec![],
                    ..u(usage(false, None))
                },
                &view,
            )
            .remove(0)
        };
        assert!(unread(Unread::Secret).quiet);
        let relative = unread(Unread::Relative(".plugin-cache".into()));
        assert!(relative.quiet && relative.text.contains("the relative path .plugin-cache,"));
        let invalid = unread(Unread::Invalid {
            file: settings.clone(),
            problem: "enabledPlugins.x@y is neither true, false nor a list".into(),
        });
        assert!(!invalid.quiet && invalid.how.is_none());
    }
}
