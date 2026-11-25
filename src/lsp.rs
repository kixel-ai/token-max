//! Code intelligence plugins: the language of each file Claude read or edited, whether the
//! session ran with the official plugin that runs that language's server, and `vibes`' card
//! when it didn't.
//!
//! A session's language comes from the files its tool calls named, not marker files at its
//! directory: most recorded directories are worktrees deleted since, and a `build.gradle` can't
//! tell Java from Kotlin. Whether a plugin loaded is decided at the directory the session
//! started in, as Claude Code decides it: an install record that applies there and the
//! `enabledPlugins` entry that wins there.
//!
//! The plugin state is read after the scan, for the profiles whose sessions touched a
//! language, and what can't be read stays with the card: it never fails the scan.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use serde_json::{Map, Value};

use crate::collections::{HashMap, HashSet, IndexMap, IndexSet};
use crate::discovery::{DiscoveryOptions, dirname, git_dir, join};
use crate::doctor::{Edit, How, Op, Patch};
use crate::model::{
    EventKind, Harness, Id, Millis, Profile, Scan, StructuralEvent, Subagent, Text,
};
use crate::report::plural;
use crate::sessions::Sessions;
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
    /// Whether the language is statically typed, which the costs page's advice names.
    typed: bool,
}

const MARKETPLACE: &str = "claude-plugins-official";
/// The official marketplace's source, as `strictKnownMarketplaces` and `blockedMarketplaces`
/// match it.
const MARKETPLACE_REPO: &str = "anthropics/claude-plugins-official";

/// The variable that moves Claude Code's plugins directory.
pub const CACHE_DIR: &str = "CLAUDE_CODE_PLUGIN_CACHE_DIR";

/// Anthropic's costs page, which recommends these plugins for typed languages.
pub const COSTS: &str =
    "https://code.claude.com/docs/en/costs#install-code-intelligence-plugins-for-typed-languages";
/// The page that lists the official plugins, their binaries and how to install them.
pub const GUIDE: &str = "https://code.claude.com/docs/en/plugins/code-intelligence";

macro_rules! official {
    ($language:expr, $plugin:expr, $binary:expr, $typed:expr, $extensions:expr) => {
        Server {
            language: $language,
            plugin: $plugin,
            binary: $binary,
            readme: concat!(
                "https://github.com/anthropics/claude-plugins-official/tree/main/plugins/",
                $plugin
            ),
            extensions: $extensions,
            typed: $typed,
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
        true,
        &["c", "h", "cpp", "cc", "cxx", "hpp", "hxx", "C", "H"]
    ),
    official!("C#", "csharp-lsp", "csharp-ls", true, &["cs"]),
    official!("Go", "gopls-lsp", "gopls", true, &["go"]),
    official!("Java", "jdtls-lsp", "jdtls", true, &["java"]),
    official!("Kotlin", "kotlin-lsp", "kotlin-lsp", true, &["kt", "kts"]),
    Server {
        language: "Liquid",
        plugin: "liquid-lsp",
        binary: "shopify",
        readme: "https://github.com/Shopify/liquid-skills/tree/main/plugins/liquid-lsp",
        extensions: &["liquid"],
        typed: false,
    },
    official!("Lua", "lua-lsp", "lua-language-server", false, &["lua"]),
    official!("PHP", "php-lsp", "intelephense", false, &["php"]),
    official!(
        "Python",
        "pyright-lsp",
        "pyright-langserver",
        false,
        &["py", "pyi"]
    ),
    official!(
        "Ruby",
        "ruby-lsp",
        "ruby-lsp",
        false,
        &["rb", "rake", "gemspec", "ru", "erb"]
    ),
    official!("Rust", "rust-analyzer-lsp", "rust-analyzer", true, &["rs"]),
    official!("Swift", "swift-lsp", "sourcekit-lsp", true, &["swift"]),
    official!(
        "TypeScript and JavaScript",
        "typescript-lsp",
        "typescript-language-server",
        true,
        &["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"]
    ),
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

/// What a Claude tool event says for code intelligence, never a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum File {
    /// The language of the file a `Read`, `Edit`, `MultiEdit` or `Write` call named.
    Language(Lsp),
    /// The call's result was an error, such as a read of a file that doesn't exist.
    Failed,
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

/// Whether an environment variable's name is `PATH`, which Windows spells `Path` and reads in
/// any case.
pub(crate) fn is_path(name: &str) -> bool {
    name == "PATH" || (cfg!(windows) && name.eq_ignore_ascii_case("PATH"))
}

/// A path with a leading `~` read as `home`, as Claude Code reads its plugins directory.
pub(crate) fn expand(home: &str, path: &str) -> String {
    match path.strip_prefix('~') {
        Some("") => home.to_string(),
        Some(rest) if rest.starts_with(['/', '\\']) => join(&[home, &rest[1..]]),
        _ => path.to_string(),
    }
}

/// A Claude profile's plugin settings, as discovery found them, and what [`discover`] read
/// after the scan.
#[derive(Clone, Debug, PartialEq)]
pub struct Plugins {
    /// The plugins directory; None when a secret T3 environment value sets it.
    pub dir: Option<String>,
    /// `CLAUDE_CODE_PLUGIN_CACHE_DIR`, `~` expanded, when it is set.
    pub cache_dir: Option<String>,
    /// Whether `CLAUDE_CONFIG_DIR` names the profile, so a step must set it to reach it.
    pub explicit: bool,
    /// The `PATH` servers are looked for on; None when it isn't the one Claude Code starts
    /// with, as over `--ssh`.
    pub path: Option<String>,
    /// `enabledPlugins` in the user settings.
    pub user: Map<String, Value>,
    /// `enabledPlugins` and the marketplace policy in managed settings.
    pub managed: Map<String, Value>,
    /// What [`discover`] read, or why it couldn't; None before it runs, or when no session
    /// touched a language.
    pub read: Option<Result<State, Unread>>,
}

impl Plugins {
    /// The managed settings the card reads.
    pub fn policy(managed: &Map<String, Value>) -> Map<String, Value> {
        managed
            .iter()
            .filter(|(k, _)| {
                matches!(
                    k.as_str(),
                    "enabledPlugins"
                        | "strictKnownMarketplaces"
                        | "allowedMarketplaces"
                        | "blockedMarketplaces"
                )
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

/// Why a profile's plugins can't be read.
#[derive(Clone, Debug, PartialEq)]
pub enum Unread {
    /// A secret T3 environment value sets `CLAUDE_CODE_PLUGIN_CACHE_DIR`.
    Secret,
    /// A file Claude Code reads holds what it would not write, or can't be read.
    Invalid { file: String, problem: String },
}

/// The plugins of a profile whose sessions touched a language, as their files say now.
#[derive(Clone, Debug, PartialEq)]
pub struct State {
    /// Each language server plugin ID `installed_plugins.json` records, with each install.
    installs: IndexMap<String, Vec<Install>>,
    /// Each directory a session started in.
    places: HashMap<Text, Place>,
    /// Whether each touched language's server is on `PATH`; None when it wasn't checked.
    servers: HashMap<Lsp, Option<bool>>,
    /// Whether managed settings let the official marketplace install; None when an entry
    /// token-max doesn't evaluate could decide it.
    market: Option<bool>,
}

/// One install record.
#[derive(Clone, Debug, PartialEq)]
enum Install {
    /// User or managed scope: every project.
    Everywhere,
    /// Project or local scope, for the session that starts at `path` or in the same
    /// repository, as `checkout` names it.
    Project {
        path: String,
        checkout: Option<String>,
    },
}

/// What a session's starting directory says about its plugins.
#[derive(Clone, Debug, PartialEq)]
struct Place {
    /// The main checkout of the repository holding it, or its nearest remaining ancestor.
    checkout: Option<String>,
    /// The project the card names it by.
    project: String,
    /// Its project and local `enabledPlugins` entries, highest precedence first, with each
    /// file; None when the directory is gone.
    settings: Option<Vec<(String, Map<String, Value>)>>,
}

/// The file tool calls of a scan's events that named a language.
pub struct Touches<'a> {
    pub events: &'a [StructuralEvent],
    pub sessions: &'a Sessions,
    /// Each Claude subagent session's parent.
    pub parents: HashMap<&'a Text, &'a Text>,
    /// Calls before this are left out.
    pub since: Option<Millis>,
}

/// One file tool call that named a language and didn't fail.
struct Touch<'a> {
    event: &'a StructuralEvent,
    lsp: Lsp,
    /// The session it counts for: a subagent's is its parent's.
    session: &'a Text,
    /// The directory that session started in, where its plugins loaded.
    dir: Option<&'a Text>,
}

/// Each Claude subagent session's parent.
pub fn parents<'a>(
    subagents: impl IntoIterator<Item = &'a Subagent>,
) -> HashMap<&'a Text, &'a Text> {
    subagents
        .into_iter()
        .filter(|s| s.harness == Harness::Claude)
        .filter_map(|s| Some((&s.session, s.parent.as_ref()?)))
        .collect()
}

impl<'a> Touches<'a> {
    fn each(&self) -> impl Iterator<Item = Touch<'a>> + '_ {
        let failed: HashSet<&Id> = self
            .events
            .iter()
            .filter(|e| e.kind == EventKind::ToolEnd && e.file == Some(File::Failed))
            .filter_map(|e| e.tool_id.as_ref())
            .collect();
        self.events.iter().filter_map(move |e| {
            let Some(File::Language(lsp)) = e.file else {
                return None;
            };
            if e.kind != EventKind::ToolStart
                || e.harness != Harness::Claude
                || self.since.is_some_and(|s| e.time < s)
                || e.tool_id.as_ref().is_some_and(|id| failed.contains(id))
            {
                return None;
            }
            let session = self.parents.get(&e.session).copied().unwrap_or(&e.session);
            Some(Touch {
                event: e,
                lsp,
                session,
                dir: self.sessions.cwd(session).or(e.cwd.as_ref()),
            })
        })
    }
}

/// The languages and directories one profile's sessions touched.
#[derive(Default)]
struct Seen<'a> {
    languages: IndexSet<Lsp>,
    dirs: IndexSet<&'a Text>,
}

/// Read the plugins of each Claude profile whose sessions touched a language: the installs
/// its plugins directory records, the project settings where each session started, and
/// whether each language's server is on `PATH`. What can't be read stays with the card and
/// never fails the scan. `--defaults`, whose report has no cards, reads none.
pub fn discover(options: &DiscoveryOptions, profiles: &mut [Profile], touches: &Touches<'_>) {
    if options.defaults {
        for p in profiles {
            p.plugins = None;
        }
        return;
    }
    let mut seen: HashMap<&str, Seen<'_>> = HashMap::default();
    for t in touches.each() {
        for id in t.event.profiles.iter() {
            let s = seen.entry(id.as_str()).or_default();
            s.languages.insert(t.lsp);
            s.dirs.extend(t.dir);
        }
    }
    for p in profiles.iter_mut() {
        if let Some(s) = seen.get(p.id.as_str())
            && p.plugins.is_some()
        {
            let read = read(options, p, s);
            p.plugins.as_mut().expect("checked above").read = Some(read);
        }
    }
}

fn read(options: &DiscoveryOptions, profile: &Profile, seen: &Seen<'_>) -> Result<State, Unread> {
    let plugins = profile.plugins.as_ref().expect("read only with plugins");
    let Some(dir) = &plugins.dir else {
        return Err(Unread::Secret);
    };
    enabled_entries(&plugins.user, &profile.settings_file)?;
    enabled_entries(
        crate::controls::members(plugins.managed.get("enabledPlugins")),
        &options.managed_settings_file,
    )?;
    let installs = installs(&join(&[dir, "installed_plugins.json"]))?;
    let mut places = HashMap::default();
    for &dir in &seen.dirs {
        let invalid = |e: crate::safe::ScanError| Unread::Invalid {
            file: dir.to_string(),
            problem: e.message,
        };
        let exists = crate::discovery::existing_directory(dir).map_err(invalid)?;
        let settings = if exists {
            let mut found = vec![];
            // Claude Code reads them lowest precedence first.
            for file in crate::discovery::claude_project_files(dir, options).map_err(invalid)? {
                // A session in the home directory reads the user's own file as its project's.
                if crate::discovery::same_file(&file, &profile.settings_file) {
                    continue;
                }
                let config =
                    options
                        .settings(&file, Harness::Claude)
                        .map_err(|e| Unread::Invalid {
                            file: file.clone(),
                            problem: e.message,
                        })?;
                let entries = enabled_entries(
                    crate::controls::members(config.get("enabledPlugins")),
                    &file,
                )?;
                found.push((file, entries));
            }
            found.reverse();
            Some(found)
        } else {
            None
        };
        places.insert(
            dir.clone(),
            Place {
                // A directory removed since, such as a worktree, was most likely in the
                // repository of the nearest one that remains, as its project is named.
                checkout: nearest(dir).and_then(checkout),
                project: project(dir, Some(&options.home)),
                settings,
            },
        );
    }
    Ok(State {
        installs,
        places,
        servers: seen
            .languages
            .iter()
            .map(|&l| {
                let found = plugins
                    .path
                    .as_deref()
                    .map(|p| on_path(l.server().binary, p));
                (l, found)
            })
            .collect(),
        market: market(&plugins.managed),
    })
}

/// A language server plugin ID's name, before its `@marketplace`.
fn name(id: &str) -> &str {
    id.split_once('@').map_or(id, |(name, _)| name)
}

/// Whether an ID names one of [`SERVERS`]' plugins, of any marketplace.
fn served(id: &str) -> bool {
    SERVERS.iter().any(|s| s.plugin == name(id))
}

/// The `enabledPlugins` entries of `file` for language server plugins, each `true`, `false`
/// or a list of version constraints, which keeps the plugin on.
fn enabled_entries(entries: &Map<String, Value>, file: &str) -> Result<Map<String, Value>, Unread> {
    let mut found = Map::new();
    for (id, value) in entries.iter().filter(|(id, _)| served(id)) {
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

/// Each language server plugin `installed_plugins.json` records, with each install. A missing
/// file records none. The first version of the file holds one record per plugin, with no
/// scope, which is the user's.
fn installs(file: &str) -> Result<IndexMap<String, Vec<Install>>, Unread> {
    let invalid = |problem: &str| Unread::Invalid {
        file: file.into(),
        problem: problem.into(),
    };
    let bytes = match fs::read(file) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(IndexMap::default()),
        Err(_) => return Err(invalid("it can't be read")),
    };
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|_| invalid("it is not valid JSON"))?;
    let Some(Value::Object(plugins)) = value.get("plugins") else {
        return Err(invalid("it has no plugins object"));
    };
    let mut found = IndexMap::default();
    for (id, records) in plugins.iter().filter(|(id, _)| served(id)) {
        let records = match records {
            Value::Array(records) => records.as_slice(),
            Value::Object(_) => std::slice::from_ref(records),
            _ => return Err(invalid(&format!("{id}'s record is not a list"))),
        };
        let mut scopes = vec![];
        for record in records {
            let Value::Object(record) = record else {
                return Err(invalid(&format!("{id} has a record that is not an object")));
            };
            let project = record.get("projectPath").and_then(Value::as_str);
            scopes.push(match (record.get("scope"), project) {
                // The first version's records carry no scope.
                (None, _) => Install::Everywhere,
                (Some(Value::String(s)), _) if s == "user" || s == "managed" => Install::Everywhere,
                // An empty or relative path names no project.
                (Some(Value::String(s)), Some(path))
                    if (s == "project" || s == "local") && Path::new(path).is_absolute() =>
                {
                    Install::Project {
                        path: path.into(),
                        checkout: checkout(path),
                    }
                }
                _ => {
                    return Err(invalid(&format!(
                        "{id} has a record whose scope or projectPath token-max doesn't know"
                    )));
                }
            });
        }
        found.insert(id.clone(), scopes);
    }
    Ok(found)
}

/// Whether managed settings let a plugin be installed from the official marketplace:
/// `blockedMarketplaces` may name it, and `strictKnownMarketplaces` (or its alias
/// `allowedMarketplaces`) must. None when an entry token-max doesn't evaluate, such as a host
/// pattern, could decide it.
fn market(managed: &Map<String, Value>) -> Option<bool> {
    let entries = |key: &str| managed.get(key).map(|v| v.as_array().map(Vec::as_slice));
    let source = |e: &Value| e.get("source").and_then(Value::as_str).map(str::to_string);
    let field = |e: &Value, key: &str| e.get(key).and_then(Value::as_str).map(str::to_string);
    let mut unknown = false;
    if let Some(blocked) = entries("blockedMarketplaces") {
        for e in blocked? {
            match source(e).as_deref() {
                Some("github") => {
                    let repo = field(e, "repo").unwrap_or_default().to_ascii_lowercase();
                    if repo == MARKETPLACE_REPO || repo == "anthropics/*" {
                        // An entry for one ref or path blocks only those.
                        if e.get("ref").is_some() || e.get("path").is_some() {
                            unknown = true;
                        } else {
                            return Some(false);
                        }
                    }
                }
                // A git URL can spell the same repository.
                Some("git") => {
                    unknown |= field(e, "url")
                        .is_none_or(|u| u.to_ascii_lowercase().contains("github.com"))
                }
                Some("url" | "file" | "directory" | "pathPattern" | "skills-dir") => {}
                _ => unknown = true,
            }
        }
    }
    let strict = entries("strictKnownMarketplaces").or_else(|| entries("allowedMarketplaces"));
    if let Some(allowed) = strict {
        let allowed = allowed?;
        let names = allowed.iter().any(|e| {
            source(e).as_deref() == Some("github")
                && matches!(
                    field(e, "repo").as_deref(),
                    Some(MARKETPLACE_REPO | "anthropics/*")
                )
                && e.get("ref").is_none()
                && e.get("path").is_none()
        });
        if !names {
            if allowed
                .iter()
                .any(|e| source(e).as_deref() == Some("hostPattern"))
            {
                return None;
            }
            return Some(false);
        }
    }
    (!unknown).then_some(true)
}

/// Whether `binary` is a program in a directory of `path`, trying each of Windows' program
/// extensions there.
fn on_path(binary: &str, path: &str) -> bool {
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

/// The main checkout of the Git repository holding the existing directory `dir`: a linked
/// worktree's is its main repository's, as Claude Code matches a project install. None
/// outside a repository, or when its metadata can't be read.
fn checkout(dir: &str) -> Option<String> {
    let mut at = dir;
    loop {
        match git_dir(at).ok()? {
            // A linked worktree's common directory is its main checkout's `.git`.
            Some(git) if git.linked => {
                let common = Path::new(&git.common);
                return match (common.file_name(), common.parent()) {
                    (Some(name), Some(main)) if name == ".git" => {
                        Some(main.to_string_lossy().into_owned())
                    }
                    _ => Some(at.into()),
                };
            }
            Some(_) => return Some(at.into()),
            None => {}
        }
        let up = dirname(at);
        if up == at {
            return None;
        }
        at = up;
    }
}

/// `dir`, or its nearest ancestor that is still a directory.
fn nearest(dir: &str) -> Option<&str> {
    let mut at = dir;
    while !Path::new(at).is_dir() {
        let up = dirname(at);
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
    match checkout(dir) {
        Some(c) if c == dir || !home.is_some_and(|h| Path::new(h).starts_with(&c)) => c,
        _ => dir.into(),
    }
}

/// Whether `install` applies to a session that started at `dir`.
fn applies(install: &Install, dir: &str, place: Option<&Place>) -> bool {
    match install {
        Install::Everywhere => true,
        Install::Project { path, checkout } => {
            path == dir
                || checkout
                    .as_ref()
                    .is_some_and(|c| place.and_then(|p| p.checkout.as_ref()) == Some(c))
        }
    }
}

/// Where the `enabledPlugins` entry that decides a plugin at a directory is.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Decided {
    Managed,
    /// A project or local settings file.
    Project(String),
    User,
    /// No entry: the official plugins start on.
    Default,
}

/// Whether plugin `id` is on at a session's starting directory, and which settings decide it.
/// A directory removed since has its project settings unread, so the user's decide.
fn enabled(plugins: &Plugins, place: Option<&Place>, id: &str) -> (bool, Decided) {
    if let Some(v) = crate::controls::members(plugins.managed.get("enabledPlugins")).get(id) {
        return (on(v), Decided::Managed);
    }
    for (file, entries) in place
        .and_then(|p| p.settings.as_ref())
        .into_iter()
        .flatten()
    {
        if let Some(v) = entries.get(id) {
            return (on(v), Decided::Project(file.clone()));
        }
    }
    match plugins.user.get(id) {
        Some(v) => (on(v), Decided::User),
        None => (true, Decided::Default),
    }
}

/// What the sessions that ran without a language's plugin need for it to load.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Need {
    /// No record of the official plugin applies where they started.
    Install,
    /// Installed, or about to be, but turned off by the user settings.
    EnableUser,
    /// Installed, or about to be, but turned off by this project or local settings file.
    Enable(String),
    /// Managed settings turn the official plugin off.
    Blocked,
    /// Managed marketplace policy forbids installing it; `known` is false when it may.
    Forbidden { known: bool },
}

/// The languages one settings file's Claude profiles worked in over the window, for those
/// that share one plugins directory.
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
    /// Where plugins are installed; None when a secret T3 value sets it.
    pub plugins_dir: Option<String>,
    /// `CLAUDE_CODE_PLUGIN_CACHE_DIR`, when it moves the plugins directory.
    pub cache_dir: Option<String>,
    /// Whether `CLAUDE_CONFIG_DIR` names the profile.
    pub explicit: bool,
    /// A T3 instance whose settings can be replaced by what token-max doesn't read.
    pub unresolved: bool,
    /// Why the plugins can't be read, which leaves the card nothing else to say.
    pub unread: Option<Unread>,
}

/// The sessions that read or edited files of one language.
#[derive(Clone, Debug, PartialEq)]
pub struct LanguageUse {
    pub lsp: Lsp,
    pub sessions: u64,
    /// Sessions that may have been another profile's, left out of every count.
    pub shared: u64,
    /// Sessions that started where a copy of the plugin loads.
    pub loaded: u64,
    /// Each project the sessions ran in, with how many, most first.
    pub projects: Vec<(String, u64)>,
    /// The projects a copy is installed for only, when one is.
    pub installed_for: Vec<String>,
    /// What the other sessions need, with how many need it.
    pub needs: Vec<(Need, u64)>,
    /// Whether its server is on `PATH`; None when it wasn't checked.
    pub server: Option<bool>,
}

/// One line per Claude settings file and plugins directory: its sessions from `since`, else
/// the scan's start, to `now`, and the languages of the files their tool calls named. A T3
/// instance whose settings are unresolved keeps a line of its own, and a disabled one, which
/// starts nothing, has none.
pub fn lsp_use(scan: &Scan, since: Option<Millis>, now: Millis) -> Vec<LspUse> {
    let window = |time: Millis| since.is_none_or(|s| time >= s) && time <= now;
    let touches = Touches {
        events: &scan.events,
        sessions: &scan.sessions,
        parents: parents(&scan.subagents),
        since,
    };
    let touched: Vec<Touch<'_>> = touches.each().filter(|t| t.event.time <= now).collect();
    let claude = scan.profiles.iter().filter(|p| {
        p.harness == Harness::Claude
            && p.plugins.is_some()
            && p.t3.as_ref().is_none_or(|t| t.enabled)
    });
    let owner = |p: &Profile| {
        let dir = p.plugins.as_ref().and_then(|c| c.dir.as_deref());
        format!(
            "{}:enabledPlugins\n{}",
            p.settings_file,
            dir.unwrap_or("unread plugins")
        )
    };
    crate::rows::groups(claude, owner)
        .iter()
        .filter_map(|line| {
            // A record any other line may hold is left out, not counted twice.
            let own = |profiles: &[String]| profiles.iter().all(|p| line.ids.contains(p));
            let mut sessions: HashSet<&Text> = HashSet::default();
            for r in &scan.requests {
                if r.harness == Harness::Claude && window(r.time) && own(&r.profiles) {
                    sessions.insert(
                        touches
                            .parents
                            .get(&r.session)
                            .copied()
                            .unwrap_or(&r.session),
                    );
                }
            }
            let mut by_language: IndexMap<Lsp, IndexMap<&Text, (Option<&Text>, &Profile)>> =
                IndexMap::default();
            let mut shared: HashMap<Lsp, HashSet<&Text>> = HashMap::default();
            for t in &touched {
                if own(&t.event.profiles) {
                    let member = line.recorder(&t.event.profiles);
                    by_language
                        .entry(t.lsp)
                        .or_default()
                        .entry(t.session)
                        .or_insert((t.dir, member));
                } else if line.covers(&t.event.profiles) {
                    shared.entry(t.lsp).or_default().insert(t.session);
                }
            }
            if by_language.is_empty() {
                return None;
            }
            sessions.extend(by_language.values().flat_map(|s| s.keys().copied()));
            let lead = line.lead;
            let plugins = lead.plugins.as_ref().expect("filtered on its plugins");
            // Members share the plugins directory, but each holds its own sessions' places.
            let unread = line
                .members
                .iter()
                .find_map(|m| m.plugins.as_ref()?.read.as_ref()?.as_ref().err())
                .cloned();
            let languages = if unread.is_some() {
                vec![]
            } else {
                let mut languages: Vec<LanguageUse> = by_language
                    .iter()
                    .map(|(&lsp, by_session)| {
                        let shared = shared.get(&lsp).map_or(0, |s| {
                            s.iter().filter(|s| !by_session.contains_key(*s)).count()
                        });
                        language(lsp, by_session, shared as u64)
                    })
                    .collect();
                languages.sort_by(|a, b| b.sessions.cmp(&a.sessions).then(a.lsp.cmp(&b.lsp)));
                languages
            };
            Some(LspUse {
                profiles: line.ids.clone(),
                sessions: sessions.len() as u64,
                languages,
                settings_file: lead.settings_file.clone(),
                root: lead.root.clone(),
                plugins_dir: plugins.dir.clone(),
                cache_dir: plugins.cache_dir.clone(),
                explicit: plugins.explicit,
                unresolved: lead.t3.as_ref().is_some_and(|t| t.settings_unresolved),
                unread,
            })
        })
        .collect()
}

/// One language's sessions, each with the directory it started in and the profile that
/// recorded it, whose plugins were read.
fn language(
    lsp: Lsp,
    by_session: &IndexMap<&Text, (Option<&Text>, &Profile)>,
    shared: u64,
) -> LanguageUse {
    let official = lsp.official();
    let plugin = lsp.server().plugin;
    let mut loaded = 0;
    let mut needs: IndexMap<Need, u64> = IndexMap::default();
    let mut projects: IndexMap<String, u64> = IndexMap::default();
    let mut installed_for: IndexSet<String> = IndexSet::default();
    let mut server = None;
    for &(dir, member) in by_session.values() {
        let plugins = member.plugins.as_ref().expect("filtered on its plugins");
        let Some(Ok(state)) = &plugins.read else {
            unreachable!("lines with an unread profile have no languages")
        };
        server = state.servers.get(&lsp).copied().flatten().or(server);
        let dir = dir.map(|d| &**d).unwrap_or_default();
        let place = state.places.get(dir);
        if let Some(place) = place {
            *projects.entry(place.project.clone()).or_default() += 1;
        }
        let copies = state.installs.iter().filter(|(id, _)| name(id) == plugin);
        let mut loads = false;
        for (id, installs) in copies {
            installed_for.extend(installs.iter().filter_map(|i| match i {
                Install::Project { path, .. } => Some(path.clone()),
                Install::Everywhere => None,
            }));
            loads |=
                installs.iter().any(|i| applies(i, dir, place)) && enabled(plugins, place, id).0;
        }
        if loads {
            loaded += 1;
            continue;
        }
        let (on, decided) = enabled(plugins, place, &official);
        let installed = state
            .installs
            .get(&official)
            .is_some_and(|i| i.iter().any(|i| applies(i, dir, place)));
        let mut add = |need: Need| *needs.entry(need).or_default() += 1;
        match decided {
            Decided::Managed if !on => add(Need::Blocked),
            _ if !installed => {
                match state.market {
                    Some(true) => add(Need::Install),
                    known => add(Need::Forbidden {
                        known: known.is_some(),
                    }),
                }
                // A user-scope install turns it on in the user settings, but not where a
                // project's file turns it off.
                if let (false, Decided::Project(file)) = (on, decided) {
                    add(Need::Enable(file));
                }
            }
            Decided::Project(file) => add(Need::Enable(file)),
            // Installed and off, so the user settings turned it off.
            _ => add(Need::EnableUser),
        }
    }
    // The server matters for any copy that loads or that the advice would load; it was
    // checked once per language for every member sharing this PATH.
    let mut projects: Vec<(String, u64)> = projects.into_iter().collect();
    projects.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut installed_for: Vec<String> = installed_for.into_iter().collect();
    installed_for.sort();
    LanguageUse {
        lsp,
        sessions: by_session.len() as u64,
        shared,
        loaded,
        projects,
        installed_for,
        needs: needs.into_iter().collect(),
        server,
    }
}

/// How a card prints: paths from `~` under `home`, and the report's arrow, such as "->".
pub struct View<'a> {
    pub home: Option<&'a str>,
    pub arrow: &'a str,
}

/// How many projects a card names before it counts the rest.
const NAMED: usize = 3;

/// `vibes`' cards for one line: one for each language whose sessions ran without its
/// plugin, or whose plugin can't start its server, or one saying why the plugins can't be
/// read.
pub fn items(u: &LspUse, view: &View<'_>) -> Vec<Item> {
    let home = |path: &str| crate::report::home_path(view.home, path);
    if let Some(unread) = &u.unread {
        let (text, quiet) = match unread {
            Unread::Secret => (
                format!(
                    "A secret T3 environment value sets {CACHE_DIR}, which token-max doesn't read, so it can't tell which code intelligence plugins this instance has, and advises none."
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
            what: "A plugin that runs a language server for Claude: code navigation by symbol, and diagnostics after each edit.".into(),
            text,
            sources: vec![GUIDE.into()],
            how: None,
            quiet,
            after: vec![],
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
    let has = |f: &dyn Fn(&Need) -> bool| l.needs.iter().any(|(n, _)| f(n));
    // The server starts for a copy that loads, or one the advice would load.
    let served =
        l.loaded > 0 || has(&|n| matches!(n, Need::Install | Need::EnableUser | Need::Enable(_)));
    let missing = l.server == Some(false) && served;
    if l.needs.is_empty() && !missing {
        return None;
    }
    let mut text = format!(
        "{} files were read or edited in {} of {} ({}){}.",
        s.language,
        l.sessions,
        plural(u.sessions, "session"),
        share(l.sessions, u.sessions),
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
    let uncovered = l.sessions - l.loaded;
    if l.loaded == l.sessions {
        text += &format!(" Each of them ran with the {} plugin.", s.plugin);
    } else if l.loaded > 0 {
        text += &format!(
            " {} of them ran with a copy of the {} plugin, and {uncovered} without.",
            l.loaded, s.plugin
        );
    }
    // How many of the sessions without it a reason covers, when not all of them.
    let of = |n: u64| {
        if n == uncovered {
            String::new()
        } else {
            format!(" for {n} of them")
        }
    };
    let mut enable: Vec<&str> = vec![];
    for (need, n) in &l.needs {
        text += &match need {
            Need::Install if l.installed_for.is_empty() => {
                format!(" The {} plugin is not installed{}.", s.plugin, of(*n))
            }
            Need::Install => format!(
                " The {} plugin is installed only for {}, which leaves it out{}.",
                s.plugin,
                listed(&l.installed_for, &home),
                of(*n)
            ),
            Need::EnableUser => {
                enable.push(&u.settings_file);
                format!(
                    " The {} plugin is installed, and enabledPlugins in {} turns it off{}.",
                    s.plugin,
                    home(&u.settings_file),
                    of(*n)
                )
            }
            Need::Enable(file) => {
                enable.push(file);
                format!(
                    " enabledPlugins in {} turns the {} plugin off{}.",
                    home(file),
                    s.plugin,
                    of(*n)
                )
            }
            Need::Blocked => format!(
                " Managed settings turn {id} off{}, and nothing overrides them.",
                of(*n)
            ),
            Need::Forbidden { known: true } => format!(
                " The {} plugin is not installed{}, and managed settings forbid installing from {MARKETPLACE}.",
                s.plugin,
                of(*n)
            ),
            Need::Forbidden { known: false } => format!(
                " The {} plugin is not installed{}, and managed settings restrict plugin marketplaces in a way token-max doesn't evaluate, so it may not install from {MARKETPLACE}.",
                s.plugin,
                of(*n)
            ),
        };
    }
    text += &match l.server {
        Some(true) => format!(
            " Its language server, {}, is on the PATH token-max runs with.",
            s.binary
        ),
        Some(false) if served => format!(
            " Its language server, {}, is not on the PATH token-max runs with, and the plugin can't start without it on the PATH of the shell you start claude from.",
            s.binary
        ),
        Some(false) => format!(
            " Its language server, {}, is not on the PATH token-max runs with, but no session would load the plugin that starts it.",
            s.binary
        ),
        None => format!(
            " Whether its language server, {}, is installed was not checked, since token-max ran without the PATH of the shell claude starts from, as over --ssh.",
            s.binary
        ),
    };
    let advice = if s.typed {
        "Anthropic recommends it for typed languages".to_string()
    } else {
        format!(
            "Anthropic recommends these plugins for typed languages, and {} is dynamically typed",
            s.language
        )
    };
    text += &format!(
        " With the plugin, Claude looks up a symbol's definition and references instead of searching text and reading each candidate file, and sees the errors an edit introduced. {advice}; what it saves is not measured."
    );
    let setting = |file: &str| {
        let edit = Edit {
            path: vec!["enabledPlugins".into(), id.clone()],
            value: Value::Bool(true),
            op: Op::Set,
        };
        format!("set {} in {}", edit.text(Harness::Claude), home(file))
    };
    let install = has(&|n| *n == Need::Install);
    // The one change the walk can write: turning the plugin on in the profile's own file.
    let user = has(&|n| *n == Need::EnableUser);
    let edit = !install && !u.unresolved && user && enable.len() == 1;
    let how = if edit {
        Some(How::Edit(Patch {
            file: u.settings_file.clone(),
            harness: Harness::Claude,
            edits: vec![Edit {
                path: vec!["enabledPlugins".into(), id.clone()],
                value: Value::Bool(true),
                op: Op::Set,
            }],
        }))
    } else {
        let mut steps = vec![];
        if missing || (l.server.is_none() && install) {
            steps.push(format!(
                "install {}{}, as the plugin's README says",
                s.binary,
                if l.server.is_none() {
                    " if it isn't on the PATH of the shell you start claude from"
                } else {
                    ""
                }
            ));
        }
        if install {
            steps.push(format!(
                "{} run `claude plugin install {id}`",
                shell(u, &home)
            ));
        }
        steps.extend(enable.iter().map(|f| setting(f)));
        let reason = if u.unresolved && user {
            "the T3 instance's launch arguments, a secret environment value or machine policy can replace the settings it reads, and which apply is unresolved"
        } else if install {
            "no setting installs a plugin"
        } else if enable.is_empty() {
            "token-max installs no programs"
        } else {
            "a project's settings decide it there, which this card doesn't edit"
        };
        (!steps.is_empty()).then(|| How::Manual {
            step: if install || !enable.is_empty() {
                format!("{}, then start a new session", steps.join(", then "))
            } else {
                steps.join(", then ")
            },
            reason: reason.into(),
        })
    };
    Some(Item {
        name: format!("{} code intelligence", s.language),
        value: if edit {
            format!("off {} on", view.arrow)
        } else {
            String::new()
        },
        what: "A plugin that runs a language server for Claude: code navigation by symbol, and diagnostics after each edit.".into(),
        text,
        sources: vec![COSTS.into(), GUIDE.into(), s.readme.into()],
        quiet: how.is_none(),
        how,
        // No `doctor` line writes `enabledPlugins` or installs a server, so none moves this card.
        after: vec![],
    })
}

/// The variables a step sets before `claude plugin install`, since a bare install goes to
/// whichever Claude home and plugins directory the shell selects: "with CLAUDE_CONFIG_DIR set
/// to ~/.claude-work and CLAUDE_CODE_PLUGIN_CACHE_DIR unset,".
fn shell(u: &LspUse, home: &dyn Fn(&str) -> String) -> String {
    match (u.explicit, &u.cache_dir) {
        (false, None) => format!("with CLAUDE_CONFIG_DIR and {CACHE_DIR} unset,"),
        (true, None) => format!(
            "with CLAUDE_CONFIG_DIR set to {} and {CACHE_DIR} unset,",
            home(&u.root)
        ),
        (false, Some(dir)) => format!(
            "with CLAUDE_CONFIG_DIR unset and {CACHE_DIR} set to {},",
            home(dir)
        ),
        (true, Some(dir)) => format!(
            "with CLAUDE_CONFIG_DIR set to {} and {CACHE_DIR} set to {},",
            home(&u.root),
            home(dir)
        ),
    }
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

    /// The failed result of `request`'s first call.
    fn failed(session: &str, cwd: &str, second: i64) -> Value {
        json!({
            "type": "user",
            "sessionId": session,
            "uuid": format!("{session}-{second}-result"),
            "timestamp": time(second + 1),
            "cwd": cwd,
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": format!("{session}-{second}-0"), "is_error": true, "content": "synthetic"},
            ]},
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
        for other in [".rs", "Makefile", "notes.md", "/dir.rs/", ""] {
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
    fn install_records_are_read_only_for_language_servers_and_fail_on_what_claude_would_not_write()
    {
        let w = Workspace::new();
        let file = w.home.join("installed_plugins.json");
        let path = native_path(&file);
        assert_eq!(
            installs(&path),
            Ok(IndexMap::default()),
            "no file, no installs"
        );
        let repo = w.home.join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        let repo = native_path(&repo);
        json_file(
            file.clone(),
            json!({"version": 2, "plugins": {
                "rust-analyzer-lsp@claude-plugins-official": [{"scope": "user"}, {"scope": "local", "projectPath": repo}],
                "gopls-lsp@corp": [{"scope": "managed"}],
                // Records of other plugins are not this card's to judge.
                "formatter@corp": [{"scope": "someday"}],
            }}),
        );
        let found = installs(&path).unwrap();
        assert_eq!(
            found.keys().collect::<Vec<_>>(),
            [
                "rust-analyzer-lsp@claude-plugins-official",
                "gopls-lsp@corp"
            ]
        );
        assert_eq!(
            found["rust-analyzer-lsp@claude-plugins-official"],
            [
                Install::Everywhere,
                Install::Project {
                    path: repo.clone(),
                    checkout: Some(repo.clone()),
                }
            ]
        );
        // The first version of the file holds one record per plugin, which is the user's.
        json_file(
            file.clone(),
            json!({"version": 1, "plugins": {"gopls-lsp@claude-plugins-official": {"version": "1.0.0"}}}),
        );
        assert_eq!(
            installs(&path).unwrap()["gopls-lsp@claude-plugins-official"],
            [Install::Everywhere]
        );
        for invalid in [
            "[]",
            "{",
            r#"{"plugins": {"gopls-lsp@x": 1}}"#,
            r#"{"plugins": {"gopls-lsp@x": [null]}}"#,
            r#"{"plugins": {"gopls-lsp@x": [{"scope": 1}]}}"#,
            r#"{"plugins": {"gopls-lsp@x": [{"scope": "someday"}]}}"#,
            r#"{"plugins": {"gopls-lsp@x": [{"scope": "project"}]}}"#,
            // A project install must name an absolute directory, or it would hold every one.
            r#"{"plugins": {"gopls-lsp@x": [{"scope": "local", "projectPath": ""}]}}"#,
            r#"{"plugins": {"gopls-lsp@x": [{"scope": "project", "projectPath": "repo"}]}}"#,
        ] {
            fs::write(&file, invalid).unwrap();
            assert!(
                matches!(installs(&path), Err(Unread::Invalid { file, .. }) if file == path),
                "{invalid}"
            );
        }
    }

    #[test]
    fn marketplace_policy_allows_the_official_marketplace_only_where_it_can_tell() {
        let policy = |value: Value| market(value.as_object().unwrap());
        let official = json!({"source": "github", "repo": MARKETPLACE_REPO});
        assert_eq!(policy(json!({})), Some(true));
        // An empty allowlist locks every marketplace out.
        assert_eq!(policy(json!({"strictKnownMarketplaces": []})), Some(false));
        assert_eq!(
            policy(json!({"strictKnownMarketplaces": [official]})),
            Some(true)
        );
        assert_eq!(
            policy(json!({"allowedMarketplaces": [{"source": "github", "repo": "anthropics/*"}]})),
            Some(true)
        );
        // An entry pinned to a ref matches only that ref, and the official source names none.
        assert_eq!(
            policy(
                json!({"strictKnownMarketplaces": [{"source": "github", "repo": MARKETPLACE_REPO, "ref": "v1"}]})
            ),
            Some(false)
        );
        // A host pattern is a regular expression token-max doesn't evaluate.
        assert_eq!(
            policy(
                json!({"strictKnownMarketplaces": [{"source": "hostPattern", "hostPattern": "^github\\.com$"}]})
            ),
            None
        );
        assert_eq!(policy(json!({"strictKnownMarketplaces": "all"})), None);
        // The blocklist matches the owner in any case, and wins over the allowlist.
        assert_eq!(
            policy(
                json!({"blockedMarketplaces": [{"source": "github", "repo": "Anthropics/*"}], "strictKnownMarketplaces": [official]})
            ),
            Some(false)
        );
        assert_eq!(
            policy(
                json!({"blockedMarketplaces": [{"source": "github", "repo": "corp/plugins"}, {"source": "file", "path": "/opt/m.json"}]})
            ),
            Some(true)
        );
        // A git URL may spell the official repository.
        assert_eq!(
            policy(
                json!({"blockedMarketplaces": [{"source": "git", "url": "https://github.com/anthropics/claude-plugins-official.git"}]})
            ),
            None
        );
    }

    #[test]
    fn the_setting_that_wins_where_a_session_started_decides_its_plugin() {
        let id = "rust-analyzer-lsp@claude-plugins-official";
        let plugins = |user: Value, managed: Value| Plugins {
            dir: None,
            cache_dir: None,
            explicit: false,
            path: None,
            user: user.as_object().unwrap().clone(),
            managed: managed.as_object().unwrap().clone(),
            read: None,
        };
        let place = |settings: Option<Vec<(&str, Value)>>| Place {
            checkout: None,
            project: String::new(),
            settings: settings.map(|s| {
                s.into_iter()
                    .map(|(f, v)| (f.to_string(), v.as_object().unwrap().clone()))
                    .collect()
            }),
        };
        let none = plugins(json!({}), json!({}));
        // No entry anywhere: the official plugins start on.
        assert_eq!(enabled(&none, None, id), (true, Decided::Default));
        let off = plugins(json!({id: false}), json!({}));
        assert_eq!(enabled(&off, None, id), (false, Decided::User));
        // A list of version constraints keeps it on.
        let listed = plugins(json!({id: ["^1.0"]}), json!({}));
        assert!(enabled(&listed, None, id).0);
        // The local file outranks the project's, which outranks the user's.
        let local = place(Some(vec![
            ("local", json!({id: true})),
            ("project", json!({id: false})),
        ]));
        assert_eq!(
            enabled(&off, Some(&local), id),
            (true, Decided::Project("local".into()))
        );
        let project = place(Some(vec![
            ("local", json!({})),
            ("project", json!({id: false})),
        ]));
        assert_eq!(
            enabled(&none, Some(&project), id),
            (false, Decided::Project("project".into()))
        );
        // A directory removed since leaves the user's to decide.
        assert_eq!(
            enabled(&off, Some(&place(None)), id),
            (false, Decided::User)
        );
        // Managed settings outrank every file.
        let blocked = plugins(json!({}), json!({"enabledPlugins": {id: false}}));
        assert_eq!(
            enabled(&blocked, Some(&local), id),
            (false, Decided::Managed)
        );
    }

    #[test]
    fn a_repository_at_home_holds_no_project_but_home() {
        let w = Workspace::new();
        let home = native_path(&w.home);
        fs::create_dir_all(w.home.join(".git")).unwrap();
        let parent = w.home.join("worktrees/x");
        fs::create_dir_all(&parent).unwrap();
        let gone = native_path(parent.join("t3-gone"));
        assert_eq!(project(&gone, Some(&home)), native_path(&parent));
        assert_eq!(project(&home, Some(&home)), home);
        // Without a home to stop at, the dotfiles repository would claim it.
        assert_eq!(project(&gone, None), home);
        // A repository above home holds no project either, even for a directory outside it.
        let shared = w.home.join("shared/x");
        fs::create_dir_all(&shared).unwrap();
        let alex = native_path(w.home.join("alex"));
        assert_eq!(
            project(&native_path(&shared), Some(&alex)),
            native_path(&shared)
        );
        // Claude Code still matches installs by the repository, home or not.
        assert_eq!(checkout(&native_path(&shared)), Some(home));
    }

    #[test]
    fn sessions_count_where_they_started_and_cards_name_what_each_needs() {
        let w = Workspace::new();
        let repo = w.home.join("repo");
        fs::create_dir_all(repo.join(".git/worktrees/wt")).unwrap();
        // A linked worktree of the repository, and a removed one under it.
        let wt = w.home.join("wt");
        fs::create_dir_all(&wt).unwrap();
        fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", native_path(repo.join(".git/worktrees/wt"))),
        )
        .unwrap();
        fs::write(repo.join(".git/worktrees/wt/commondir"), "../..\n").unwrap();
        let gone = repo.join(".claude/worktrees/gone");
        let other = w.home.join("other");
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
                        ("Edit", file("tool.py")),
                        ("Grep", json!({"path": "/synthetic/x.go"})),
                    ],
                ),
                // A later directory doesn't change the plugins the session started with.
                request("a", &other_s, 6, &[("Read", file("z.rs"))]),
            ],
        );
        write(projects.join("b.jsonl"), &[request("b", &gone_s, 2, &[])]);
        let mut sidechain = request(
            "agent",
            &gone_s,
            3,
            &[("Write", file("new.rs")), ("Read", file("y.py"))],
        );
        sidechain["isSidechain"] = json!(true);
        sidechain["sessionId"] = json!("b");
        write(projects.join("b/subagents/agent-x.jsonl"), &[sidechain]);
        write(
            projects.join("c.jsonl"),
            &[request("c", &other_s, 4, &[("Read", file("lib.rs"))])],
        );
        write(
            projects.join("d.jsonl"),
            &[request("d", &wt_s, 5, &[("MultiEdit", file("x.py"))])],
        );
        // A read of a file that isn't there is no Kotlin worked in.
        write(
            projects.join("e.jsonl"),
            &[
                request("e", &other_s, 7, &[("Read", file("App.kt"))]),
                failed("e", &other_s, 7),
            ],
        );
        let registry = w.claude.join("plugins/installed_plugins.json");
        json_file(
            registry.clone(),
            json!({"version": 2, "plugins": {
                "rust-analyzer-lsp@claude-plugins-official": [{"scope": "user"}],
                // The worktree and the removed one share the repository's install.
                "pyright-lsp@claude-plugins-official": [{"scope": "local", "projectPath": repo_s}],
            }}),
        );
        let settings = w.claude.join("settings.json");
        let id = "rust-analyzer-lsp@claude-plugins-official";
        json_file(
            settings.clone(),
            json!({"model": "opus", "enabledPlugins": {id: false}}),
        );
        // The other directory's own file turns it back on there.
        json_file(
            other.join(".claude/settings.local.json"),
            json!({"enabledPlugins": {id: true}}),
        );
        let mut options = w.scan_options();
        let bin = program(&w, "rust-analyzer");
        options
            .discovery
            .env
            .insert("PATH".into(), native_path(&bin));
        let data = scan(&options).unwrap();
        assert!(!format!("{data:?}").contains("main.rs"), "no path is kept");
        let lines = lsp_use(&data, None, START + 3_600_000);
        assert_eq!(lines.len(), 1, "Codex gets no line");
        let u = &lines[0];
        assert_eq!((u.sessions, u.unread.as_ref()), (5, None));
        let got: Vec<_> = u
            .languages
            .iter()
            .map(|l| {
                (
                    l.lsp.server().language,
                    l.sessions,
                    l.loaded,
                    l.needs.clone(),
                    l.server,
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("Python", 3, 3, vec![], Some(false)),
                ("Rust", 3, 1, vec![(Need::EnableUser, 2)], Some(true)),
            ]
        );
        // The removed worktree is the repository's.
        assert_eq!(
            u.languages[1].projects,
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
        assert_eq!(cards[1].name, "Rust code intelligence");
        assert_eq!(
            cards[1].text,
            format!(
                "Rust files were read or edited in 3 of 5 sessions (60%), most in {} (2 sessions) and {} (1). 1 of them ran with a copy of the rust-analyzer-lsp plugin, and 2 without. The rust-analyzer-lsp plugin is installed, and enabledPlugins in {} turns it off. Its language server, rust-analyzer, is on the PATH token-max runs with. With the plugin, Claude looks up a symbol's definition and references instead of searching text and reading each candidate file, and sees the errors an edit introduced. Anthropic recommends it for typed languages; what it saves is not measured.",
                home(&repo_s),
                home(&other_s),
                home(&native_path(&settings)),
            )
        );
        assert_eq!(cards[1].value, "off -> on");
        assert!(!cards[1].quiet);
        // The walk writes the one edit, keeping the rest of the file.
        let owner = crate::vibes::owner(&cards[1..], None, 100.0);
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
            json!({"model": "opus", "enabledPlugins": {id: true}})
        );
        // Loaded everywhere, but without its server.
        assert!(
            cards[0].text.contains(
                " Each of them ran with the pyright-lsp plugin. Its language server, pyright-langserver, is not on the PATH"
            ),
            "{}",
            cards[0].text
        );
        assert!(cards[0].text.contains("and Python is dynamically typed;"));
        assert_eq!(
            cards[0].how,
            Some(How::Manual {
                step: "install pyright-langserver, as the plugin's README says".into(),
                reason: "token-max installs no programs".into(),
            })
        );

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
        // `--defaults`, whose report has no cards, reads no plugins.
        options.discovery.pretend_defaults();
        let data = scan(&options).unwrap();
        assert!(lsp_use(&data, None, START + 3_600_000).is_empty());
    }

    #[test]
    fn a_card_names_each_need_and_none_when_nothing_is_missing() {
        let rust = Lsp::of("a.rs").unwrap();
        let settings = native_path("/synthetic/claude/settings.json");
        let u = |l: LanguageUse| LspUse {
            profiles: vec!["p".into()],
            sessions: 300,
            languages: vec![l],
            settings_file: settings.clone(),
            root: native_path("/synthetic/claude-work"),
            plugins_dir: Some(native_path("/synthetic/claude-work/plugins")),
            cache_dir: None,
            explicit: false,
            unresolved: false,
            unread: None,
        };
        let usage = |needs: Vec<(Need, u64)>, loaded: u64, server: Option<bool>| LanguageUse {
            lsp: rust,
            sessions: 10,
            shared: 0,
            loaded,
            projects: vec![],
            installed_for: vec![],
            needs,
            server,
        };
        let view = View {
            home: None,
            arrow: "->",
        };
        let card = |l: LanguageUse| items(&u(l), &view);
        let how = |l: LanguageUse| card(l).remove(0).how;
        let step = |l: LanguageUse| match how(l) {
            Some(How::Manual { step, .. }) => step,
            other => panic!("{other:?}"),
        };
        // Loaded everywhere with its server on PATH, or unchecked: nothing to advise.
        assert!(card(usage(vec![], 10, Some(true))).is_empty());
        assert!(card(usage(vec![], 10, None)).is_empty());
        // Loaded, but its server is missing.
        assert_eq!(
            how(usage(vec![], 10, Some(false))),
            Some(How::Manual {
                step: "install rust-analyzer, as the plugin's README says".into(),
                reason: "token-max installs no programs".into(),
            })
        );
        // A bare install goes to whichever home and plugins directory the shell selects.
        let install = |server| usage(vec![(Need::Install, 10)], 0, server);
        let id = "rust-analyzer-lsp@claude-plugins-official";
        assert_eq!(
            how(install(Some(true))),
            Some(How::Manual {
                step: format!(
                    "with CLAUDE_CONFIG_DIR and CLAUDE_CODE_PLUGIN_CACHE_DIR unset, run `claude plugin install {id}`, then start a new session"
                ),
                reason: "no setting installs a plugin".into(),
            })
        );
        let named = items(
            &LspUse {
                explicit: true,
                cache_dir: Some(native_path("/synthetic/cache")),
                ..u(install(Some(true)))
            },
            &view,
        );
        let Some(How::Manual { step: named, .. }) = &named[0].how else {
            panic!("{named:?}")
        };
        assert!(
            named.starts_with(&format!(
                "with CLAUDE_CONFIG_DIR set to {} and CLAUDE_CODE_PLUGIN_CACHE_DIR set to {}, run",
                native_path("/synthetic/claude-work"),
                native_path("/synthetic/cache")
            )),
            "{named}"
        );
        assert!(step(install(Some(false))).starts_with(
            "install rust-analyzer, as the plugin's README says, then with CLAUDE_CONFIG_DIR"
        ));
        // Over SSH the server isn't checked, so the step says when it is needed.
        let unchecked = card(install(None)).remove(0);
        assert!(unchecked.text.contains(
            "was not checked, since token-max ran without the PATH of the shell claude starts from, as over --ssh."
        ));
        assert!(
            matches!(&unchecked.how, Some(How::Manual { step, .. }) if step.starts_with("install rust-analyzer if it isn't on the PATH of the shell you start claude from, as the plugin's README says, then with"))
        );
        // Five projects hold a copy: three are named.
        let partly = card(LanguageUse {
            installed_for: ["a", "b", "c", "d", "e"]
                .map(|p| format!("/synthetic/{p}"))
                .into(),
            ..usage(vec![(Need::Install, 4)], 6, Some(true))
        })
        .remove(0);
        assert!(
            partly.text.contains(
                " 6 of them ran with a copy of the rust-analyzer-lsp plugin, and 4 without. The rust-analyzer-lsp plugin is installed only for /synthetic/a, /synthetic/b, /synthetic/c and 2 more, which leaves it out."
            ),
            "{}",
            partly.text
        );
        // Turned off in the user settings: the one edit the walk writes.
        let off = card(usage(vec![(Need::EnableUser, 10)], 0, Some(false))).remove(0);
        assert_eq!(off.value, "off -> on");
        assert!(matches!(&off.how, Some(How::Edit(p)) if p.file == settings));
        // A T3 instance's other settings may decide it, so the change is by hand.
        let unsettled = items(
            &LspUse {
                unresolved: true,
                ..u(usage(vec![(Need::EnableUser, 10)], 0, Some(true)))
            },
            &view,
        )
        .remove(0);
        assert_eq!(
            unsettled.how,
            Some(How::Manual {
                step: format!(
                    r#"set "enabledPlugins": {{"{id}": true}} in {settings}, then start a new session"#
                ),
                reason: "the T3 instance's launch arguments, a secret environment value or machine policy can replace the settings it reads, and which apply is unresolved".into(),
            })
        );
        assert_eq!(unsettled.value, "");
        // A project's file decides it there; with sessions elsewhere needing an install, each
        // step is named, with the sessions it covers.
        let local = native_path("/synthetic/repo/.claude/settings.local.json");
        let mixed = card(usage(
            vec![(Need::Install, 3), (Need::Enable(local.clone()), 1)],
            0,
            Some(true),
        ))
        .remove(0);
        assert!(
            mixed.text.contains(&format!(
                " The rust-analyzer-lsp plugin is not installed for 3 of them. enabledPlugins in {local} turns the rust-analyzer-lsp plugin off for 1 of them."
            )),
            "{}",
            mixed.text
        );
        assert!(
            matches!(&mixed.how, Some(How::Manual { step, reason }) if step.ends_with(&format!(r#"run `claude plugin install {id}`, then set "enabledPlugins": {{"{id}": true}} in {local}, then start a new session"#)) && reason == "no setting installs a plugin"),
            "{:?}",
            mixed.how
        );
        // Blocked by policy: shown, with nothing to act on, unless a loaded copy lacks its server.
        let blocked = card(usage(vec![(Need::Blocked, 10)], 0, Some(false))).remove(0);
        assert!(blocked.quiet && blocked.how.is_none());
        assert!(blocked.text.contains(&format!(
            " Managed settings turn {id} off, and nothing overrides them."
        )));
        assert!(
            blocked
                .text
                .contains("but no session would load the plugin that starts it.")
        );
        let served = card(usage(vec![(Need::Blocked, 4)], 6, Some(false))).remove(0);
        assert!(!served.quiet);
        // Marketplace policy that forbids the install, or may, leaves nothing to act on.
        for known in [true, false] {
            let forbidden =
                card(usage(vec![(Need::Forbidden { known }, 10)], 0, Some(true))).remove(0);
            assert!(forbidden.quiet && forbidden.how.is_none(), "{known}");
        }
        // Sessions another profile may have run are named but not counted.
        let shared = card(LanguageUse {
            shared: 2,
            ..install(Some(true))
        })
        .remove(0);
        assert!(
            shared.text.starts_with(
                "Rust files were read or edited in 10 of 300 sessions (3%). 2 more sessions that may have been another profile's are left out."
            ),
            "{}",
            shared.text
        );
        let tiny = card(LanguageUse {
            sessions: 1,
            ..usage(vec![(Need::Install, 1)], 0, Some(true))
        })
        .remove(0);
        assert!(tiny.text.contains("(under 1%)"));
        // Plugins the card can't read: a secret is quiet, a broken file is not.
        let unread = |unread: Unread| {
            items(
                &LspUse {
                    unread: Some(unread),
                    languages: vec![],
                    ..u(install(Some(true)))
                },
                &view,
            )
            .remove(0)
        };
        assert!(unread(Unread::Secret).quiet);
        let invalid = unread(Unread::Invalid {
            file: settings.clone(),
            problem: "enabledPlugins.x@y is neither true, false nor a list".into(),
        });
        assert!(!invalid.quiet && invalid.how.is_none());
    }
}
