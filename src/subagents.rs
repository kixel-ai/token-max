//! What each subagent type or agent role cost, which model it ran on and why.
//!
//! The lines are evidence only. A cheaper model trades capability, and the parent's extra
//! work after a weaker subagent lands in requests no transcript attributes to it, so no
//! saving can be claimed and nothing is recommended.

use std::fs;
use std::time::UNIX_EPOCH;

use serde_json::Value;

use crate::collections::{HashMap, IndexMap, IndexSet};
use crate::controls::{Env, SettingsScope, js_string, members, scoped_environment};
use crate::discovery::{
    DiscoveryOptions, codex_markers, codex_trust, directories, error, existing_directory, join,
    named_profile_files, project_directories, realpath, resolve, settings,
};
use crate::model::{
    Agent, AgentControls, AgentDefinition, Harness, Millis, ModelSource, Owned, Profile, Request,
    Scan, Share, SourceShare, Subagent, SubagentCost,
};
use crate::prices::{Catalog, Sum, price_part, price_request, server_cost};
use crate::reader::UNRECORDED;
use crate::safe::{ScanError, label};

/// Claude Code 2.1.198 made the built-in Explore inherit the main model; it ran on Haiku before.
const EXPLORE_INHERITS: (u64, u64, u64) = (2, 1, 198);

/// Model families that rank above Opus, whose Explore runs are capped at Opus.
const ABOVE_OPUS: [&str; 2] = ["fable", "mythos"];

/// Codex 0.167's built-in agent roles, none of which sets a model.
const CODEX_ROLES: [&str; 3] = ["default", "explorer", "worker"];

/// Claude model family aliases a definition or call can name.
const FAMILIES: [&str; 5] = ["haiku", "sonnet", "opus", "fable", "mythos"];

/// What a built-in Claude agent's definition sets, as read from Claude Code 2.1.322.
struct BuiltIn {
    name: &'static str,
    /// The definition's model: an alias, `inherit`, or None to take the subagent setting.
    model: Option<&'static str>,
    /// What a same-named replacement must restate to keep behaving like it.
    replacement: &'static str,
}

const BUILT_INS: [BuiltIn; 6] = [
    BuiltIn {
        name: "Explore",
        model: Some("inherit"),
        replacement: "its read-only tool restriction and system prompt, and set omitClaudeMd: true",
    },
    BuiltIn {
        name: "Plan",
        model: Some("inherit"),
        replacement: "its read-only tool restriction and system prompt, and set omitClaudeMd: true",
    },
    BuiltIn {
        name: "general-purpose",
        model: None,
        replacement: "its system prompt",
    },
    BuiltIn {
        name: "statusline-setup",
        model: Some("sonnet"),
        replacement: "its tool restriction and system prompt",
    },
    BuiltIn {
        name: "claude-code-guide",
        model: Some("haiku"),
        replacement: "its tool restriction and system prompt",
    },
    // Haiku where Claude Code enables it for the main model, otherwise inherited.
    BuiltIn {
        name: "web-fetch",
        model: Some("haiku"),
        replacement: "its WebFetch-only tool restriction and system prompt, and set omitClaudeMd: true",
    },
];

fn built_in(kind: &str) -> Option<&'static BuiltIn> {
    BUILT_INS.iter().find(|b| b.name == kind)
}

/// Milliseconds since the epoch at which a file last changed.
fn modified(path: &str) -> Option<Millis> {
    let time = fs::metadata(path).ok()?.modified().ok()?;
    Some(time.duration_since(UNIX_EPOCH).ok()?.as_millis() as Millis)
}

/// Files under `dir` with `extension`, sorted; a missing directory has none.
fn files(dir: &str, extension: &str) -> Result<Vec<String>, ScanError> {
    let mut found = vec![];
    let mut pending = vec![dir.to_string()];
    while let Some(dir) = pending.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if crate::discovery::absent(e.kind()) => continue,
            Err(_) => return Err(error("cannot read agent directory", &dir, 0)),
        };
        for entry in entries {
            let entry = entry.map_err(|_| error("cannot read agent directory", &dir, 0))?;
            let path = join(&[&dir, &entry.file_name().to_string_lossy()]);
            let kind = entry
                .file_type()
                .map_err(|_| error("cannot read agent directory", &dir, 0))?;
            if kind.is_dir() {
                pending.push(path);
            } else if path.ends_with(extension) {
                found.push(path);
            }
        }
    }
    found.sort();
    Ok(found)
}

/// A Claude agent file's frontmatter `name`, `model` and `effort`, never its prompt.
fn frontmatter(text: &str) -> Option<(String, Option<String>, Option<String>)> {
    let mut lines = text.trim_start_matches('\u{feff}').lines();
    if lines.next()?.trim_end() != "---" {
        return None;
    }
    let (mut name, mut model, mut effort) = (None, None, None);
    for line in lines {
        let line = line.trim_end();
        if line == "---" {
            return name.map(|name| (name, model, effort));
        }
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = scalar(value);
        let labelled = label(&Value::from(value));
        match key.trim() {
            "name" => name = labelled,
            // Claude Code accepts a long-context model such as `opus[1m]`.
            "model" => {
                model = match value.strip_suffix("[1m]") {
                    Some(id) => label(&Value::from(id)).map(|id| format!("{id}[1m]")),
                    None => labelled,
                }
            }
            "effort" => effort = labelled,
            _ => {}
        }
    }
    None
}

/// A YAML plain or quoted scalar without its trailing comment.
fn scalar(value: &str) -> &str {
    let value = value.trim();
    for quote in ['"', '\''] {
        if let Some(rest) = value.strip_prefix(quote) {
            return rest.split(quote).next().unwrap_or_default();
        }
    }
    // A comment starts at a `#` that begins the value or follows whitespace.
    let end = value
        .char_indices()
        .find(|&(i, c)| c == '#' && (i == 0 || value[..i].ends_with(char::is_whitespace)))
        .map_or(value.len(), |(i, _)| i);
    value[..end].trim_end()
}

/// Claude agent files under `dir`; files without frontmatter define no agent.
fn claude_definitions(dir: &str, project: Option<&str>) -> Result<Vec<AgentDefinition>, ScanError> {
    let mut definitions = vec![];
    for file in files(dir, ".md")? {
        let text = fs::read(&file).map_err(|_| error("cannot read agent file", &file, 0))?;
        let Some((name, model, effort)) = frontmatter(&String::from_utf8_lossy(&text)) else {
            continue;
        };
        definitions.push(AgentDefinition {
            name,
            modified: modified(&file),
            file,
            project: project.map(str::to_string),
            model,
            effort,
        });
    }
    Ok(definitions)
}

/// Whether an optional role text field is set; None where Codex rejects it as blank or not a
/// string. The text itself is never kept.
fn present(value: Option<&Value>) -> Option<bool> {
    match value {
        None => Some(false),
        Some(Value::String(text)) => (!text.trim().is_empty()).then_some(true),
        Some(_) => None,
    }
}

/// One Codex agent role while its config layers merge.
#[derive(Clone)]
struct Role {
    /// Its role file's model and effort; None for a declaration without a file.
    definition: Option<AgentDefinition>,
    /// The config file that declares it, or its role file when discovered under `agents/`.
    owner: String,
    /// Whether it has the description Codex requires of every role.
    described: bool,
}

/// A Codex role file's name, whether it describes the role, and its model and effort. None
/// where Codex ignores it: it is missing, unreadable or malformed, has a blank field, or was
/// discovered without its own `name` and `developer_instructions`. The file's own `name`
/// overrides the name it was declared under.
fn codex_role_file(file: &str, declared: Option<&str>) -> Option<(String, bool, AgentDefinition)> {
    if !fs::metadata(file).is_ok_and(|m| m.is_file()) {
        return None;
    }
    let role = settings(file, Harness::Codex).ok()?;
    let described = present(role.get("description"))?;
    if !present(role.get("developer_instructions"))? && declared.is_none() {
        return None;
    }
    let name = match role.get("name") {
        Some(Value::String(name)) if !name.trim().is_empty() => label(&Value::from(name.trim()))?,
        None | Some(Value::String(_)) => declared?.to_string(),
        Some(_) => return None,
    };
    let definition = AgentDefinition {
        name: name.clone(),
        file: file.to_string(),
        project: None,
        model: role.get("model").and_then(label),
        effort: role.get("model_reasoning_effort").and_then(label),
        modified: modified(file),
    };
    Some((name, described, definition))
}

/// Apply one Codex config layer over the lower ones, as Codex 0.167 loads them: its settings
/// replace earlier values key by key, and each role it defines replaces an earlier role of
/// that name, keeping the earlier role file when it names none. Within a layer, a role
/// declared in `[agents]` wins over a file discovered under `agents/`.
fn codex_layer(
    controls: &mut AgentControls,
    roles: &mut IndexMap<String, Role>,
    config: &serde_json::Map<String, Value>,
    file: &str,
    folder: &str,
) -> Result<(), ScanError> {
    let owned = |value: Option<&Value>, key: &str| {
        value.and_then(label).map(|value| Owned {
            value,
            owner: format!("{file}:{key}"),
        })
    };
    let agents = members(config.get("agents"));
    for (slot, value, key) in [
        (&mut controls.session, config.get("model"), "model"),
        (
            &mut controls.review,
            config.get("review_model"),
            "review_model",
        ),
        (
            &mut controls.model,
            agents.get("default_subagent_model"),
            "agents.default_subagent_model",
        ),
        (
            &mut controls.effort,
            agents.get("default_subagent_reasoning_effort"),
            "agents.default_subagent_reasoning_effort",
        ),
    ] {
        if let Some(owned) = owned(value, key) {
            *slot = Some(owned);
        }
    }
    let mut layer: IndexMap<String, Role> = IndexMap::default();
    let mut declared = IndexSet::default();
    for (name, role) in agents {
        let Value::Object(role) = role else { continue };
        let Some(described) = present(role.get("description")) else {
            continue;
        };
        let (name, role) = match role.get("config_file") {
            None => (
                name.clone(),
                Role {
                    definition: None,
                    owner: file.to_string(),
                    described,
                },
            ),
            Some(Value::String(path)) => {
                let path = if crate::safe::is_absolute(path) {
                    path.clone()
                } else {
                    join(&[folder, path])
                };
                declared.insert(path.clone());
                let Some((name, own, definition)) = codex_role_file(&path, Some(name)) else {
                    continue;
                };
                (
                    name,
                    Role {
                        definition: Some(definition),
                        owner: file.to_string(),
                        described: described || own,
                    },
                )
            }
            Some(_) => continue,
        };
        layer.entry(name).or_insert(role);
    }
    for path in files(&join(&[folder, "agents"]), ".toml")? {
        if declared.contains(&path) {
            continue;
        }
        if let Some((name, described, definition)) = codex_role_file(&path, None) {
            layer.entry(name).or_insert(Role {
                definition: Some(definition),
                owner: path,
                described,
            });
        }
    }
    for (name, mut role) in layer {
        if let Some(earlier) = roles.get(&name) {
            role.described |= earlier.described;
            if role.definition.is_none() {
                role.definition.clone_from(&earlier.definition);
            }
        }
        // Codex ignores a role without a description, keeping any earlier one.
        if role.described {
            roles.insert(name, role);
        }
    }
    controls.definitions = roles
        .values()
        .filter_map(|r| r.definition.clone())
        .collect();
    controls.declared = roles
        .iter()
        .filter(|(_, r)| r.definition.is_none())
        .map(|(name, r)| (name.clone(), r.owner.clone()))
        .collect();
    Ok(())
}

/// Whether an environment value turns a Claude Code flag on.
fn enabled(value: Option<&Value>) -> bool {
    matches!(
        js_string(value).to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Claude's subagent controls from settings scopes, lowest precedence first, and the
/// launching environment.
fn claude_controls(
    scopes: &[SettingsScope],
    env: &Env,
    definitions: Vec<AgentDefinition>,
) -> AgentControls {
    let environment = scoped_environment(scopes, env);
    let owned = |key: &str| {
        let value = js_string(environment.values.get(key));
        (environment.values.contains_key(key) && !value.is_empty() && value != "inherit").then(
            || Owned {
                value,
                owner: environment.owners[key].clone(),
            },
        )
    };
    AgentControls {
        model: owned("CLAUDE_CODE_SUBAGENT_MODEL"),
        effort: owned("CLAUDE_CODE_SUBAGENT_EFFORT"),
        force: enabled(environment.values.get("CLAUDE_CODE_SUBAGENT_MODEL_FORCE")),
        explore_uncapped: enabled(
            environment
                .values
                .get("CLAUDE_CODE_DISABLE_EXPLORE_INHERIT_CAP"),
        ),
        definitions,
        ..AgentControls::default()
    }
}

/// Resolve every profile's current subagent model and effort controls, and how the project
/// settings and agent files at each recorded subagent directory change them. A directory
/// that no longer exists keeps the profile's controls; the recorded model still has to
/// match them for a source to be named.
pub fn discover(
    options: &DiscoveryOptions,
    profiles: &mut [Profile],
    requests: &[Request],
    earlier: &[Request],
) -> Result<(), ScanError> {
    let env = &options.env;
    let empty = Env::default();
    let default_root = join(&[&options.home, ".claude"]);
    let configured = env.get("CLAUDE_CONFIG_DIR").unwrap_or(&default_root);
    let active = realpath(&resolve(configured)).unwrap_or_default();
    let mut project_agents: HashMap<String, Vec<AgentDefinition>> = HashMap::default();
    for profile in profiles.iter_mut() {
        let cwds: Vec<String> = directories(
            requests
                .iter()
                .chain(earlier)
                .filter(|r| r.agent == Agent::Subagent && r.profiles.contains(&profile.id)),
        )
        .into_iter()
        .map(str::to_string)
        .collect();
        let mut existing = vec![];
        let mut scoped_dirs: IndexMap<String, AgentControls> = IndexMap::default();
        for cwd in cwds {
            if existing_directory(&cwd)? {
                existing.push(cwd);
            }
        }
        let mut agents = match profile.harness {
            Harness::Claude => {
                let user = settings(&profile.settings_file, Harness::Claude)?;
                let managed_file = options.managed_settings_file.as_str();
                let managed = settings(managed_file, Harness::Claude)?;
                let local_env = if profile.t3.is_none() && active == profile.root {
                    env
                } else {
                    &empty
                };
                let user_scope = SettingsScope {
                    config: &user,
                    file: &profile.settings_file,
                };
                let managed_scope = SettingsScope {
                    config: &managed,
                    file: managed_file,
                };
                let own = claude_definitions(&join(&[&profile.root, "agents"]), None)?;
                let agents = claude_controls(&[user_scope, managed_scope], local_env, own.clone());
                for cwd in existing {
                    let [project_file, legacy_file, local_file] =
                        crate::discovery::claude_project_files(&cwd, options)?;
                    let project = settings(&project_file, Harness::Claude)?;
                    let legacy = settings(&legacy_file, Harness::Claude)?;
                    let local = settings(&local_file, Harness::Claude)?;
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
                    let mut definitions = own.clone();
                    for dir in project_directories(&cwd, &[".git".into()])? {
                        if !project_agents.contains_key(&dir) {
                            let found = claude_definitions(
                                &join(&[&dir, ".claude", "agents"]),
                                Some(&dir),
                            )?;
                            project_agents.insert(dir.clone(), found);
                        }
                        definitions.extend(project_agents[&dir].iter().cloned());
                    }
                    let scoped = claude_controls(&scopes, local_env, definitions);
                    if scoped != agents {
                        scoped_dirs.insert(cwd, scoped);
                    }
                }
                agents
            }
            Harness::Codex => {
                let system_file = options.codex_system_config_file.as_str();
                let system = settings(system_file, Harness::Codex)?;
                let user = settings(&profile.settings_file, Harness::Codex)?;
                let mut agents = AgentControls::default();
                let mut roles = IndexMap::default();
                for (config, file) in [(&system, system_file), (&user, &*profile.settings_file)] {
                    let folder = crate::discovery::dirname(file);
                    codex_layer(&mut agents, &mut roles, config, file, folder)?;
                }
                // A profile chosen with --profile, which no record names, can replace them.
                let overrides = |config: &serde_json::Map<String, Value>| {
                    config.contains_key("agents") || config.contains_key("review_model")
                };
                let mut selectable: Vec<String> = members(user.get("profiles"))
                    .iter()
                    .filter(|(_, p)| p.as_object().is_some_and(overrides))
                    .map(|(name, _)| format!("{}:profiles.{name}", profile.settings_file))
                    .collect();
                if profile.t3.is_none() {
                    for name in named_profile_files(&profile.root)? {
                        let file = join(&[&profile.root, &name]);
                        if overrides(&settings(&file, Harness::Codex)?) {
                            selectable.push(file);
                        }
                    }
                }
                if !selectable.is_empty() {
                    agents.unresolved = Some(format!(
                        "{} can override subagent settings, but which profile a run selected is not recorded",
                        selectable.join(", ")
                    ));
                }
                let scopes = [
                    SettingsScope {
                        config: &system,
                        file: system_file,
                    },
                    SettingsScope {
                        config: &user,
                        file: &profile.settings_file,
                    },
                ];
                let markers = codex_markers(&scopes, &profile.settings_file)?;
                for cwd in existing {
                    let paths = project_directories(&cwd, &markers)?;
                    let root = paths.last().expect("cwd is included");
                    let trust = codex_trust(&scopes, &cwd, root);
                    if trust == Some("untrusted") {
                        continue;
                    }
                    let mut scoped = agents.clone();
                    let mut scoped_roles = roles.clone();
                    // The project root's layer first, the working directory's last.
                    for path in paths.iter().rev() {
                        let folder = join(&[path, ".codex"]);
                        let file = join(&[&folder, "config.toml"]);
                        let config = settings(&file, Harness::Codex)?;
                        codex_layer(&mut scoped, &mut scoped_roles, &config, &file, &folder)?;
                    }
                    if scoped == agents {
                        continue;
                    }
                    if trust != Some("trusted") {
                        scoped = AgentControls {
                            unresolved: Some(format!(
                                "project config under {root} changes subagent settings, but whether Codex trusts that project is not established"
                            )),
                            ..agents.clone()
                        };
                    }
                    scoped_dirs.insert(cwd, scoped);
                }
                agents
            }
        };
        scoped_dirs.sort_keys();
        agents.directories = scoped_dirs;
        profile.agents = agents;
    }
    Ok(())
}

/// A dotted version's first three numbers.
fn version(text: &str) -> Option<(u64, u64, u64)> {
    let mut parts = text.split('.').map(|p| {
        p.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .ok()
    });
    Some((parts.next()??, parts.next()??, parts.next()??))
}

/// The family alias a Claude model ID belongs to.
fn family(model: &str) -> Option<&'static str> {
    FAMILIES.into_iter().find(|f| model.contains(f))
}

/// What a run's model should be under one rule, checked against what it recorded.
enum Expect<'a> {
    /// A model ID or family alias.
    Model(&'a str),
    /// The parent's model.
    Parent,
    /// Explore's inheritance, capped at Opus above it.
    Capped,
    /// Either of two rules, such as web-fetch's conditional Haiku.
    Either(&'a str),
}

/// One run's recorded facts.
struct Run<'a> {
    subagent: Option<&'a Subagent>,
    model: Option<&'a str>,
    parent: Option<&'a str>,
    version: Option<&'a str>,
    cwd: Option<&'a str>,
    start: Millis,
}

impl Run<'_> {
    fn matches(&self, expect: &Expect<'_>) -> bool {
        let Some(model) = self.model else {
            return false;
        };
        let named = |name: &str| {
            let name = name.trim_end_matches("[1m]");
            if FAMILIES.contains(&name) {
                return model.contains(name);
            }
            // An ID also names its dated snapshots, never another model it prefixes.
            model.strip_prefix(name).is_some_and(|rest| {
                rest.is_empty() || {
                    let date = rest.strip_prefix('-').unwrap_or_default().as_bytes();
                    let digits =
                        |range: std::ops::Range<usize>| date[range].iter().all(u8::is_ascii_digit);
                    match date.len() {
                        8 => digits(0..8),
                        10 => {
                            digits(0..4)
                                && date[4] == b'-'
                                && digits(5..7)
                                && date[7] == b'-'
                                && digits(8..10)
                        }
                        _ => false,
                    }
                }
            })
        };
        match expect {
            Expect::Model(name) => named(name),
            Expect::Parent => self.parent == Some(model),
            Expect::Capped => {
                self.parent == Some(model)
                    || (self
                        .parent
                        .and_then(family)
                        .is_some_and(|f| ABOVE_OPUS.contains(&f))
                        && family(model) == Some("opus"))
            }
            Expect::Either(name) => named(name) || self.parent == Some(model),
        }
    }
}

/// The agent definition a run of `kind` used: the closest project file, else the profile's.
/// Directory controls hold only the project files that apply there.
fn definition<'a>(controls: &'a AgentControls, kind: &str) -> Option<&'a AgentDefinition> {
    controls
        .definitions
        .iter()
        .filter(|d| d.name == kind)
        .max_by_key(|d| d.project.as_ref().map_or(0, String::len))
}

/// The controls a run in `cwd` used: its directory's, else the profile's.
fn scoped<'a>(controls: &'a AgentControls, cwd: Option<&str>) -> &'a AgentControls {
    cwd.and_then(|cwd| controls.directories.get(cwd))
        .unwrap_or(controls)
}

/// Where a Claude run's model came from, or why that is unknown.
fn claude_source(run: &Run<'_>, controls: &AgentControls, kind: &str) -> (ModelSource, String) {
    let unknown = |reason: String| (ModelSource::Unknown, reason);
    let Some(recorded) = run.model else {
        return unknown("no model was recorded".into());
    };
    let decided = |(source, detail, expect): (ModelSource, String, Expect<'_>)| {
        if run.matches(&expect) {
            (source, detail)
        } else if matches!(expect, Expect::Parent | Expect::Capped) && run.parent.is_none() {
            unknown(format!(
                "{detail}, but the main model at spawn is not recorded"
            ))
        } else {
            unknown(format!(
                "ran on {recorded}, which current settings do not give ({detail})"
            ))
        }
    };
    let subagent = run.subagent;
    if subagent.is_some_and(|s| s.fork) {
        return decided((
            ModelSource::Inherited,
            "a fork always runs on the main model".into(),
            Expect::Parent,
        ));
    }
    let setting = controls.model.as_ref().map(|m| {
        (
            ModelSource::Setting,
            format!("{}={}", m.owner, m.value),
            Expect::Model(&m.value),
        )
    });
    let inherited = || {
        (
            ModelSource::Inherited,
            "inherits the main model".to_string(),
            Expect::Parent,
        )
    };
    if controls.force {
        // The forced setting replaces call and definition models; Explore keeps its cap
        // only while no subagent model is set.
        if let Some(setting) = setting {
            return decided(setting);
        }
    } else if let Some(call) = subagent.and_then(|s| s.model.as_deref()) {
        return decided(if call == "inherit" {
            (
                ModelSource::Inherited,
                "the Agent call asked to inherit the main model".into(),
                Expect::Parent,
            )
        } else {
            (
                ModelSource::Call,
                format!("the Agent call asked for {call}"),
                Expect::Model(call),
            )
        });
    }
    let definition =
        definition(controls, kind).filter(|_| subagent.is_none_or(|s| s.built_in != Some(true)));
    if let Some(file) = definition {
        if file.modified.is_none_or(|m| m > run.start) {
            return unknown(format!("{} changed after this run started", file.file));
        }
        if !controls.force {
            match file.model.as_deref() {
                Some("inherit") => {
                    return decided((
                        ModelSource::Inherited,
                        format!("model: inherit in {}", file.file),
                        Expect::Parent,
                    ));
                }
                Some(model) => {
                    return decided((
                        ModelSource::Definition,
                        format!("model: {model} in {}", file.file),
                        Expect::Model(model),
                    ));
                }
                None => {}
            }
        }
        return decided(setting.unwrap_or_else(inherited));
    }
    let Some(builtin) =
        built_in(kind).filter(|_| subagent.is_none_or(|s| s.built_in != Some(false)))
    else {
        return match subagent {
            None => unknown("no transcript metadata names its type".into()),
            Some(_) => unknown("no current agent file defines it".into()),
        };
    };
    if builtin.name == "Explore" {
        return match run.version.and_then(version) {
            None => unknown("the Claude Code version is not recorded".into()),
            Some(v) if v < EXPLORE_INHERITS => decided((
                ModelSource::Definition,
                "built-in Explore ran on Haiku before Claude Code 2.1.198".into(),
                Expect::Model("haiku"),
            )),
            Some(_) if controls.explore_uncapped => decided((
                ModelSource::Inherited,
                "built-in Explore inherits the main model; CLAUDE_CODE_DISABLE_EXPLORE_INHERIT_CAP lifts its Opus cap".into(),
                Expect::Parent,
            )),
            Some(_) if controls.force => decided(inherited()),
            Some(_) => decided((
                ModelSource::Inherited,
                "built-in Explore inherits the main model, capped at Opus".into(),
                Expect::Capped,
            )),
        };
    }
    match builtin.model {
        _ if controls.force => decided(setting.unwrap_or_else(inherited)),
        Some("inherit") => decided((
            ModelSource::Inherited,
            format!("built-in {kind} inherits the main model"),
            Expect::Parent,
        )),
        Some(alias) if builtin.name == "web-fetch" => {
            let source = if run.matches(&Expect::Model(alias)) {
                ModelSource::Definition
            } else {
                ModelSource::Inherited
            };
            decided((
                source,
                format!(
                    "built-in {kind} runs on {alias} where Claude Code enables it, otherwise inherits the main model"
                ),
                Expect::Either(alias),
            ))
        }
        Some(alias) => decided((
            ModelSource::Definition,
            format!("built-in {kind} runs on {alias}"),
            Expect::Model(alias),
        )),
        None => decided(setting.unwrap_or_else(inherited)),
    }
}

/// Where a Codex run's model came from, or why that is unknown.
fn codex_source(run: &Run<'_>, controls: &AgentControls, kind: &str) -> (ModelSource, String) {
    let Some(recorded) = run.model else {
        return (ModelSource::Unknown, "no model was recorded".into());
    };
    let native = run.subagent.is_some_and(|s| s.native);
    let unknown = |reason: String| (ModelSource::Unknown, reason);
    if native && kind != "review" {
        return unknown(format!(
            "Codex chooses the {kind} subagent's model itself; token-max does not model it"
        ));
    }
    let role = definition(controls, kind).filter(|_| !native);
    if let Some(file) = role
        && file.modified.is_none_or(|m| m > run.start)
    {
        return unknown(format!("{} changed after this run started", file.file));
    }
    // A custom role no layer defines any more may have set a model that is gone now.
    if !native
        && role.is_none()
        && !CODEX_ROLES.contains(&kind)
        && !controls.declared.contains_key(kind)
    {
        return unknown(match kind {
            UNRECORDED => "no record names its agent role".into(),
            _ => format!("no current role file defines {kind}"),
        });
    }
    let (source, detail, expect) = if native {
        // A review that records no parent thread runs on the configured session model.
        match (&controls.review, &controls.session) {
            (Some(review), _) => (
                ModelSource::Setting,
                format!("{}={}", review.owner, review.value),
                Expect::Model(&review.value),
            ),
            (None, Some(session)) if run.parent.is_none() => (
                ModelSource::Inherited,
                format!(
                    "without review_model a review runs on the session model, {}={}",
                    session.owner, session.value
                ),
                Expect::Model(&session.value),
            ),
            (None, _) => (
                ModelSource::Inherited,
                "without review_model a review runs on the session model".into(),
                Expect::Parent,
            ),
        }
    } else if let Some((file, model)) = role.and_then(|d| Some((d, d.model.as_deref()?))) {
        (
            ModelSource::Definition,
            format!("model = {model} in {}", file.file),
            Expect::Model(model),
        )
    } else if let Some(model) = &controls.model {
        (
            ModelSource::Setting,
            format!("{}={}", model.owner, model.value),
            Expect::Model(&model.value),
        )
    } else {
        (
            ModelSource::Inherited,
            "inherits the parent thread's model".into(),
            Expect::Parent,
        )
    };
    if run.matches(&expect) {
        (source, detail)
    } else if matches!(expect, Expect::Parent) && run.parent.is_none() {
        (
            ModelSource::Unknown,
            format!("{detail}, but the parent's model is not recorded"),
        )
    } else {
        // Only a spawned role has a spawn_agent call that could have named another model.
        let call = if native {
            ""
        } else {
            "; the spawn_agent call may have asked for it"
        };
        (
            ModelSource::Unknown,
            format!("ran on {recorded}, which current settings do not give ({detail}){call}"),
        )
    }
}

/// The file a Codex setting's owner names, else `fallback`.
fn owner_file<'a>(owned: Option<&'a Owned>, fallback: &'a str) -> &'a str {
    owned
        .and_then(|o| o.owner.rsplit_once(':'))
        .map_or(fallback, |(file, _)| file)
}

/// The settings that change a type's model and effort under one directory's controls.
fn settings_text(
    profile: &Profile,
    controls: &AgentControls,
    kind: &str,
    fork: bool,
    native: bool,
) -> (String, String) {
    let mut texts = match profile.harness {
        Harness::Claude if fork => (
            "none; a fork always runs on the main model".into(),
            "none; a fork runs at the main conversation's effort".into(),
        ),
        Harness::Claude if kind == UNRECORDED => (
            "the Agent call's model, the agent file's model:, or CLAUDE_CODE_SUBAGENT_MODEL".into(),
            "the Agent call's effort, the agent file's effort:, or CLAUDE_CODE_SUBAGENT_EFFORT"
                .into(),
        ),
        Harness::Claude => {
            let defined = definition(controls, kind);
            let file = defined.map_or_else(
                || join(&[&profile.root, "agents", &format!("{kind}.md")]),
                |d| d.file.clone(),
            );
            let builtin = built_in(kind).filter(|_| defined.is_none());
            let mut model = format!("model: in {file}");
            // The variable reaches only agents whose definition names no model.
            let unset = match builtin {
                Some(b) => b.model.is_none(),
                None => defined.is_none_or(|d| d.model.is_none()),
            };
            if unset {
                model += ", or CLAUDE_CODE_SUBAGENT_MODEL";
            }
            if let Some(b) = builtin {
                model += &format!(
                    "; that file replaces the built-in {kind}, so it must restate {}",
                    b.replacement
                );
            }
            let mut effort = format!("effort: in {file}");
            if defined.is_none_or(|d| d.effort.is_none()) {
                effort += ", or CLAUDE_CODE_SUBAGENT_EFFORT";
            }
            (model, effort)
        }
        Harness::Codex if native && kind != "review" => (
            format!("none that token-max models for Codex {kind} subagents"),
            format!("none that token-max models for Codex {kind} subagents"),
        ),
        Harness::Codex if native => (
            format!(
                "review_model in {}",
                owner_file(controls.review.as_ref(), &profile.settings_file)
            ),
            format!(
                "model_reasoning_effort in {}, shared with its session",
                profile.settings_file
            ),
        ),
        Harness::Codex => {
            let model_file = owner_file(controls.model.as_ref(), &profile.settings_file);
            let effort_file = owner_file(controls.effort.as_ref(), &profile.settings_file);
            match definition(controls, kind) {
                Some(role) => (
                    format!(
                        "model in {}, else [agents] default_subagent_model in {model_file}",
                        role.file
                    ),
                    format!(
                        "model_reasoning_effort in {}, else [agents] default_subagent_reasoning_effort in {effort_file}",
                        role.file
                    ),
                ),
                None => (
                    match controls.declared.get(kind) {
                        // A declaration hides a discovered file of its name in the same layer.
                        Some(file) => format!(
                            "model in a role file named by config_file under [agents.{kind}] in {file}, else [agents] default_subagent_model in {model_file}"
                        ),
                        None => format!(
                            "model in a role file named {kind} under {}, else [agents] default_subagent_model in {model_file}",
                            join(&[&profile.root, "agents"])
                        ),
                    },
                    format!(
                        "model_reasoning_effort in that role file, else [agents] default_subagent_reasoning_effort in {effort_file}"
                    ),
                ),
            }
        }
    };
    // The forced variable replaces the Agent call's and the agent file's model.
    if profile.harness == Harness::Claude && controls.force && !fork {
        texts.0 = match &controls.model {
            Some(setting) => format!(
                "{}, which CLAUDE_CODE_SUBAGENT_MODEL_FORCE applies over the Agent call and agent files",
                setting.owner
            ),
            None => "CLAUDE_CODE_SUBAGENT_MODEL; CLAUDE_CODE_SUBAGENT_MODEL_FORCE ignores the Agent call's and agent files' model".into(),
        };
    }
    // An untrusted project or a selectable profile may override the files named above.
    if let Some(reason) = &controls.unresolved {
        for text in [&mut texts.0, &mut texts.1] {
            *text += &format!("; unresolved: {reason}");
        }
    }
    texts
}

/// Count values, most common first, ties in first-seen order.
fn shares<'a>(values: impl Iterator<Item = &'a str>) -> Vec<Share> {
    let mut counts: IndexMap<&str, u64> = IndexMap::default();
    for value in values {
        *counts.entry(value).or_default() += 1;
    }
    let mut shares: Vec<Share> = counts
        .into_iter()
        .map(|(name, count)| Share {
            name: name.to_string(),
            count,
        })
        .collect();
    shares.sort_by_key(|s| std::cmp::Reverse(s.count));
    shares
}

/// Price a request's recorded tokens as if every part ran on `model`.
fn reprice(catalog: &Catalog, r: &Request, model: &str) -> crate::model::Cost {
    let mut sum = Sum::new();
    for p in &r.parts {
        if p.model.as_deref() == Some(model) {
            sum.add(&price_part(catalog, r, p));
        } else {
            let mut part = p.clone();
            part.model = Some(model.into());
            sum.add(&price_part(catalog, r, &part));
        }
    }
    sum.add(&server_cost(r));
    sum.cost()
}

/// One line per subagent type or agent role and profile set, most expensive first.
pub fn subagent_costs(scan: &Scan, catalog: &Catalog) -> Vec<SubagentCost> {
    let spawns: HashMap<&str, &Subagent> =
        scan.subagents.iter().map(|s| (&*s.session, s)).collect();
    // Each conversation's requests over time, including those before the report window: a
    // subagent's spawn and a parent's model at that time can precede the window.
    let mut history: HashMap<&str, Vec<&Request>> = HashMap::default();
    let context = &scan.cache_context;
    for r in context
        .requests
        .iter()
        .chain(&context.codex)
        .chain(&scan.requests)
    {
        history.entry(&*r.session).or_default().push(r);
    }
    for requests in history.values_mut() {
        requests.sort_by_key(|r| r.time);
    }
    let parent_model = |session: &str, time: Millis| {
        let models: Vec<(Millis, &str)> = history
            .get(session)?
            .iter()
            .filter_map(|r| Some((r.time, r.model.as_deref()?)))
            .collect();
        let at = models.partition_point(|(t, _)| *t <= time);
        models[..at].last().or(models.first()).map(|(_, m)| *m)
    };
    // A Codex source such as `review` never shares a line with a role of the same name.
    let mut groups: IndexMap<(Harness, Vec<String>, &str, bool), Vec<&Request>> =
        IndexMap::default();
    for r in scan.requests.iter().filter(|r| r.agent == Agent::Subagent) {
        let spawn = spawns.get(&*r.session);
        let kind = spawn.map_or(UNRECORDED, |s| s.kind.as_str());
        let native = spawn.is_some_and(|s| s.native);
        let mut profiles = r.profiles.to_vec();
        profiles.sort();
        groups
            .entry((r.harness, profiles, kind, native))
            .or_default()
            .push(r);
    }
    let mut lines = vec![];
    for ((harness, profiles, kind, native), requests) in groups {
        let owners: Vec<&Profile> = scan
            .profiles
            .iter()
            .filter(|p| profiles.contains(&p.id))
            .collect();
        let shared = owners
            .first()
            .filter(|first| owners.iter().all(|p| p.agents == first.agents));
        let mut runs: IndexMap<&str, Vec<&Request>> = IndexMap::default();
        for r in &requests {
            runs.entry(&*r.session).or_default().push(r);
        }
        let fork = runs.keys().all(|s| spawns.get(s).is_some_and(|s| s.fork)) && !runs.is_empty();
        let mut sources: IndexMap<(ModelSource, String), u64> = IndexMap::default();
        let mut model_settings: IndexSet<String> = IndexSet::default();
        let mut effort_settings: IndexSet<String> = IndexSet::default();
        for session in runs.keys() {
            let subagent = spawns.get(session).copied();
            let all = &history[session];
            let start = all[0].time;
            let run = Run {
                subagent,
                model: all.iter().find_map(|r| r.model.as_deref()),
                parent: subagent
                    .and_then(|s| s.parent.as_deref())
                    .and_then(|p| parent_model(p, start)),
                version: all.iter().find_map(|r| r.version.as_deref()),
                cwd: all.iter().find_map(|r| r.cwd.as_deref()),
                start,
            };
            let source = match shared {
                None => (
                    ModelSource::Unknown,
                    "profiles sharing this history have different agent settings".into(),
                ),
                Some(profile) => {
                    let controls = scoped(&profile.agents, run.cwd);
                    match (&controls.unresolved, harness) {
                        (Some(reason), _) => (ModelSource::Unknown, reason.clone()),
                        (None, Harness::Claude) => claude_source(&run, controls, kind),
                        (None, Harness::Codex) => codex_source(&run, controls, kind),
                    }
                }
            };
            *sources.entry(source).or_default() += 1;
            if let Some(profile) = owners.first() {
                let (model, effort) = settings_text(
                    profile,
                    scoped(&profile.agents, run.cwd),
                    kind,
                    fork,
                    native,
                );
                model_settings.insert(model);
                effort_settings.insert(effort);
            }
        }
        let mut sources: Vec<SourceShare> = sources
            .into_iter()
            .map(|((source, detail), runs)| SourceShare {
                source,
                detail,
                runs,
            })
            .collect();
        sources.sort_by_key(|s| std::cmp::Reverse(s.runs));
        // Runs in different projects can be set by different files; name each.
        let joined = |settings: IndexSet<String>| {
            let settings: Vec<String> = settings.into_iter().collect();
            if settings.is_empty() {
                "unknown".to_string()
            } else {
                settings.join("; for other runs, ")
            }
        };
        let (model_setting, effort_setting) = (joined(model_settings), joined(effort_settings));
        let mut cost = Sum::new();
        let mut priced = 0;
        for r in &requests {
            let c = price_request(catalog, r);
            priced += u64::from(c.total_usd.is_some());
            cost.add(&c);
        }
        let provider = requests.iter().find_map(|r| r.provider.as_deref());
        let cheapest = provider.and_then(|p| catalog.cheapest(p));
        let on_cheapest = |r: &&Request| {
            cheapest.is_some_and(|c| r.parts.iter().all(|p| p.model.as_deref() == Some(c)))
        };
        let reprice = cheapest
            .filter(|_| !requests.iter().all(on_cheapest))
            .map(|model| {
                let mut sum = Sum::new();
                for r in &requests {
                    if r.provider.as_deref() == provider {
                        sum.add(&reprice(catalog, r, model));
                    } else {
                        sum.add(&crate::prices::unknown_cost(
                            "a request from another provider has no cheapest model to compare",
                        ));
                    }
                }
                sum.cost()
            });
        lines.push(SubagentCost {
            harness,
            profiles,
            kind: kind.to_string(),
            native,
            runs: runs.len() as u64,
            requests: requests.len() as u64,
            priced,
            cost: cost.cost(),
            models: shares(
                requests
                    .iter()
                    .map(|r| r.model.as_deref().unwrap_or(UNRECORDED)),
            ),
            efforts: shares(
                requests
                    .iter()
                    .map(|r| r.effort.as_deref().unwrap_or(UNRECORDED)),
            ),
            sources,
            cheapest: cheapest.map(str::to_string),
            reprice,
            model_setting,
            effort_setting,
        });
    }
    lines.sort_by(|a, b| {
        b.cost
            .known_usd
            .partial_cmp(&a.cost.known_usd)
            .expect("finite costs")
    });
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CacheContext, ExposureScan, ScanCoverage};
    use crate::test_support::{Workspace, make_profile, make_request, prices};

    fn spawn(kind: &str) -> Subagent {
        Subagent {
            harness: Harness::Claude,
            session: "child".into(),
            parent: Some("parent".into()),
            kind: kind.into(),
            built_in: None,
            fork: false,
            native: false,
            model: None,
            effort: None,
        }
    }

    fn run<'a>(subagent: &'a Subagent, model: &'a str, parent: &'a str) -> Run<'a> {
        Run {
            subagent: Some(subagent),
            model: Some(model),
            parent: Some(parent),
            version: Some("2.1.322"),
            cwd: Some("/work/project/src"),
            start: 1_000,
        }
    }

    fn defined(name: &str, model: Option<&str>, project: Option<&str>) -> AgentDefinition {
        AgentDefinition {
            name: name.into(),
            file: format!("{}/{name}.md", project.unwrap_or("/home/.claude/agents")),
            project: project.map(str::to_string),
            model: model.map(str::to_string),
            effort: None,
            modified: Some(0),
        }
    }

    fn source(run: &Run<'_>, controls: &AgentControls, kind: &str) -> ModelSource {
        claude_source(run, controls, kind).0
    }

    #[test]
    fn frontmatter_keeps_only_labels_and_never_the_prompt() {
        let text = "\u{feff}---\nname: reviewer\ndescription: SYNTHETIC_PRIVATE text\nmodel: \"haiku\"\neffort: low\ntools:\n  - Read\n---\nmodel: opus\n";
        assert_eq!(
            frontmatter(text),
            Some(("reviewer".into(), Some("haiku".into()), Some("low".into())))
        );
        assert_eq!(
            frontmatter(
                "---\nname: scout # quick\nmodel: haiku # cheaper\neffort: 'low' # x\n---\n"
            ),
            Some(("scout".into(), Some("haiku".into()), Some("low".into())))
        );
        assert_eq!(
            frontmatter("---\nname: \"a\" # quoted\nmodel: # none\n---\n"),
            Some(("a".into(), None, None))
        );
        // A long-context model keeps its suffix.
        assert_eq!(
            frontmatter("---\nname: deep\nmodel: opus[1m]\n---\n"),
            Some(("deep".into(), Some("opus[1m]".into()), None))
        );
        assert_eq!(frontmatter("name: reviewer\n"), None);
        assert_eq!(frontmatter("---\nmodel: haiku\n---\n"), None);
        assert_eq!(frontmatter("---\nname: unterminated\n"), None);
    }

    #[test]
    fn explore_inherits_capped_at_opus_after_2_1_198_and_ran_on_haiku_before() {
        let none = AgentControls::default();
        let explore = spawn("Explore");
        let capped = run(&explore, "claude-opus-5-5", "claude-opus-5-5");
        assert_eq!(source(&capped, &none, "Explore"), ModelSource::Inherited);
        let above = run(&explore, "claude-opus-5-5", "claude-fable-5-2");
        assert_eq!(source(&above, &none, "Explore"), ModelSource::Inherited);
        let uncapped = AgentControls {
            explore_uncapped: true,
            ..AgentControls::default()
        };
        assert_eq!(source(&above, &uncapped, "Explore"), ModelSource::Unknown);
        let mut older = run(&explore, "claude-haiku-4-5", "claude-opus-4-8");
        older.version = Some("2.1.197");
        assert_eq!(source(&older, &none, "Explore"), ModelSource::Definition);
        older.version = None;
        assert_eq!(source(&older, &none, "Explore"), ModelSource::Unknown);
        // A model no current rule gives is never guessed.
        let odd = run(&explore, "claude-sonnet-5-5", "claude-opus-5-5");
        assert_eq!(source(&odd, &none, "Explore"), ModelSource::Unknown);
    }

    #[test]
    fn the_call_then_the_definition_then_the_setting_then_the_parent_choose_the_model() {
        let mut call = spawn("general-purpose");
        call.model = Some("haiku".into());
        let none = AgentControls::default();
        let on =
            |s: &Subagent, model: &str| source(&run(s, model, "claude-opus-5-5"), &none, &s.kind);
        assert_eq!(on(&call, "claude-haiku-5-5"), ModelSource::Call);
        assert_eq!(on(&call, "claude-opus-5-5"), ModelSource::Unknown);
        let mut fork = spawn("fork");
        fork.fork = true;
        assert_eq!(on(&fork, "claude-opus-5-5"), ModelSource::Inherited);
        let general = spawn("general-purpose");
        assert_eq!(on(&general, "claude-opus-5-5"), ModelSource::Inherited);
        let setting = AgentControls {
            model: Some(Owned {
                value: "sonnet".into(),
                owner: "settings.json:env.CLAUDE_CODE_SUBAGENT_MODEL".into(),
            }),
            ..AgentControls::default()
        };
        let sonnet = run(&general, "claude-sonnet-5-5", "claude-opus-5-5");
        assert_eq!(
            source(&sonnet, &setting, "general-purpose"),
            ModelSource::Setting
        );
        // The setting never reaches Explore, whose definition says inherit.
        let explore = spawn("Explore");
        let inherited = run(&explore, "claude-opus-5-5", "claude-opus-5-5");
        assert_eq!(
            source(&inherited, &setting, "Explore"),
            ModelSource::Inherited
        );
        // Forcing it replaces the call's model.
        let forced = AgentControls {
            force: true,
            ..setting.clone()
        };
        let sonnet_call = run(&call, "claude-sonnet-5-5", "claude-opus-5-5");
        assert_eq!(
            source(&sonnet_call, &forced, "general-purpose"),
            ModelSource::Setting
        );
        // So the guidance names the forced setting, not the agent file it overrides.
        let profile = make_profile("profile", "/home/.claude");
        let (model, _) = settings_text(&profile, &forced, "Explore", false, false);
        assert!(
            model.starts_with("settings.json:env.CLAUDE_CODE_SUBAGENT_MODEL,")
                && !model.contains("model: in"),
            "{model}"
        );
    }

    #[test]
    fn custom_agents_use_the_closest_unchanged_definition() {
        let custom = spawn("reviewer");
        let r = run(&custom, "claude-haiku-5-5", "claude-opus-5-5");
        let none = AgentControls::default();
        assert_eq!(source(&r, &none, "reviewer"), ModelSource::Unknown);
        let mut controls = AgentControls {
            definitions: vec![
                defined("reviewer", Some("opus"), None),
                defined("reviewer", Some("sonnet"), Some("/work")),
                defined("reviewer", Some("haiku"), Some("/work/project")),
            ],
            ..AgentControls::default()
        };
        let (found, detail) = claude_source(&r, &controls, "reviewer");
        assert_eq!(found, ModelSource::Definition);
        assert!(detail.contains("/work/project/reviewer.md"), "{detail}");
        controls.definitions[2].modified = Some(r.start + 1);
        let (changed, detail) = claude_source(&r, &controls, "reviewer");
        assert_eq!(changed, ModelSource::Unknown);
        assert!(
            detail.contains("changed after this run started"),
            "{detail}"
        );
        // A user file of a built-in's name replaces it.
        let mut replaced = spawn("Explore");
        replaced.built_in = Some(false);
        let controls = AgentControls {
            definitions: vec![defined("Explore", Some("haiku"), None)],
            ..AgentControls::default()
        };
        let r = run(&replaced, "claude-haiku-5-5", "claude-opus-5-5");
        assert_eq!(source(&r, &controls, "Explore"), ModelSource::Definition);
    }

    #[test]
    fn codex_roles_use_their_file_then_the_default_setting_then_the_parent() {
        let role = Subagent {
            harness: Harness::Codex,
            ..spawn("explorer")
        };
        let r = run(&role, "gpt-6-luna", "gpt-6.1-sol");
        let mut controls = AgentControls::default();
        assert_eq!(
            codex_source(&r, &controls, "explorer").0,
            ModelSource::Unknown
        );
        controls.model = Some(Owned {
            value: "gpt-6-luna".into(),
            owner: "config.toml:agents.default_subagent_model".into(),
        });
        assert_eq!(
            codex_source(&r, &controls, "explorer").0,
            ModelSource::Setting
        );
        // A setting names its dated snapshots, but not another model its ID prefixes.
        let dated = run(&role, "gpt-6-luna-2026-10-01", "gpt-6.1-sol");
        assert_eq!(
            codex_source(&dated, &controls, "explorer").0,
            ModelSource::Setting
        );
        let mini = run(&role, "gpt-6-luna-mini", "gpt-6.1-sol");
        assert_eq!(
            codex_source(&mini, &controls, "explorer").0,
            ModelSource::Unknown
        );
        controls.definitions = vec![defined("explorer", Some("gpt-6-luna"), None)];
        assert_eq!(
            codex_source(&r, &controls, "explorer").0,
            ModelSource::Definition
        );
        // A role file changed since the run, even to drop its model, explains nothing.
        controls.definitions = vec![defined("explorer", None, None)];
        controls.definitions[0].modified = Some(r.start + 1);
        assert_eq!(
            codex_source(&r, &controls, "explorer").0,
            ModelSource::Unknown
        );
        let parent = run(&role, "gpt-6.1-sol", "gpt-6.1-sol");
        let mut controls = AgentControls::default();
        assert_eq!(
            codex_source(&parent, &controls, "explorer").0,
            ModelSource::Inherited
        );
        // A custom role no current file defines may have had a model; it stays unknown.
        assert_eq!(
            codex_source(&parent, &controls, "checker").0,
            ModelSource::Unknown
        );
        // One a config layer still declares without a role file runs on the fallbacks.
        controls
            .declared
            .insert("checker".into(), "config.toml".into());
        assert_eq!(
            codex_source(&parent, &controls, "checker").0,
            ModelSource::Inherited
        );
        controls.declared.clear();
        // A review records no parent, so the configured session model stands in.
        let review_mode = Subagent {
            native: true,
            ..role.clone()
        };
        let mut review = run(&review_mode, "gpt-6.1-sol", "unused");
        review.parent = None;
        assert_eq!(
            codex_source(&review, &controls, "review").0,
            ModelSource::Unknown
        );
        controls.session = Some(Owned {
            value: "gpt-6.1-sol".into(),
            owner: "config.toml:model".into(),
        });
        assert_eq!(
            codex_source(&review, &controls, "review").0,
            ModelSource::Inherited
        );
        // A role named review is a role, not Codex's review mode.
        let named = run(&role, "gpt-6-luna", "gpt-6.1-sol");
        let reviewer = AgentControls {
            definitions: vec![defined("review", Some("gpt-6-luna"), None)],
            ..controls.clone()
        };
        assert_eq!(
            codex_source(&named, &reviewer, "review").0,
            ModelSource::Definition
        );
        let mut compact = run(&review_mode, "gpt-6.1-sol", "gpt-6.1-sol");
        compact.parent = None;
        assert_eq!(
            codex_source(&compact, &controls, "compact").0,
            ModelSource::Unknown
        );
    }

    fn scan(requests: Vec<Request>, subagents: Vec<Subagent>) -> Scan {
        Scan {
            subagents,
            snapshot_started_at: String::new(),
            snapshot_finished_at: String::new(),
            since: None,
            roots: vec![],
            profiles: vec![make_profile("profile", "/home/.claude")],
            snapshots: vec![],
            requests,
            events: vec![],
            exposure: ExposureScan::default(),
            cache_context: CacheContext::default(),
            coverage: ScanCoverage::default(),
            t3: vec![],
            sessions: Default::default(),
        }
    }

    #[test]
    fn each_type_reports_its_cost_models_efforts_and_a_cheapest_model_reprice() {
        let main = Request {
            id: "main".into(),
            session: "parent".into(),
            ..make_request()
        };
        let child = |id: &str, session: &str, model: &str, effort: Option<&str>| {
            let mut r = Request {
                id: id.into(),
                session: session.into(),
                agent: Agent::Subagent,
                time: 10,
                effort: effort.map(Into::into),
                ..make_request()
            };
            r.model = Some(model.into());
            r.parts[0].model = Some(model.into());
            r
        };
        let mut haiku = spawn("web-fetch");
        haiku.session = "fetch".into();
        let lines = subagent_costs(
            &scan(
                vec![
                    main,
                    child("a", "child", "claude-opus-5-5", Some("medium")),
                    child("b", "child", "claude-opus-5-5", None),
                    child("c", "fetch", "claude-haiku-5-5", Some("low")),
                    child("d", "orphan", "claude-opus-5-5", None),
                ],
                vec![spawn("Explore"), haiku],
            ),
            &prices(),
        );
        let kinds: Vec<&str> = lines.iter().map(|l| l.kind.as_str()).collect();
        assert_eq!(kinds, ["Explore", UNRECORDED, "web-fetch"]);
        let explore = &lines[0];
        assert_eq!((explore.runs, explore.requests, explore.priced), (1, 2, 2));
        assert_eq!(explore.models.len(), 1);
        let efforts: Vec<(&str, u64)> = explore
            .efforts
            .iter()
            .map(|e| (e.name.as_str(), e.count))
            .collect();
        assert_eq!(efforts, [("medium", 1), (UNRECORDED, 1)]);
        assert_eq!(explore.sources[0].source, ModelSource::Inherited);
        assert_eq!(explore.cheapest.as_deref(), Some("claude-haiku-5-5"));
        let recorded = explore.cost.total_usd.unwrap();
        let cheaper = explore.reprice.as_ref().unwrap().total_usd.unwrap();
        assert!(
            cheaper > 0.0 && cheaper < recorded,
            "{cheaper} vs {recorded}"
        );
        assert!(explore.model_setting.contains("omitClaudeMd: true"));
        assert_eq!(lines[1].sources[0].source, ModelSource::Unknown);
        // Already on the cheapest model: nothing to reprice.
        assert_eq!(lines[2].reprice, None);
    }

    #[test]
    fn discovery_reads_agent_files_and_settings_without_their_prompts() {
        let w = Workspace::new();
        let write = |path: std::path::PathBuf, text: &str| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write(
            w.claude.join("agents/nested/reviewer.md"),
            "---\nname: reviewer\nmodel: haiku\neffort: low\n---\nSYNTHETIC_PRIVATE prompt\n",
        );
        write(w.claude.join("agents/notes.md"), "no frontmatter\n");
        write(
            w.claude.join("settings.json"),
            r#"{"env":{"CLAUDE_CODE_SUBAGENT_MODEL":"sonnet","CLAUDE_CODE_SUBAGENT_MODEL_FORCE":"1"}}"#,
        );
        write(
            w.codex.join("config.toml"),
            "review_model = \"gpt-6-luna\"\n[agents]\ndefault_subagent_model = \"gpt-6-luna\"\n[agents.checker]\ndescription = \"x\"\nconfig_file = \"roles/checker.toml\"\n",
        );
        write(
            w.codex.join("roles/checker.toml"),
            "model = \"gpt-6-luna\"\nmodel_reasoning_effort = \"low\"\ndeveloper_instructions = \"SYNTHETIC_PRIVATE\"\n",
        );
        write(
            w.codex.join("agents/worker.toml"),
            "name = \"builder\"\ndescription = \"SYNTHETIC_PRIVATE\"\ndeveloper_instructions = \"SYNTHETIC_PRIVATE\"\nmodel_reasoning_effort = \"high\"\n",
        );
        // Codex ignores a discovered role file without its own name and instructions.
        write(
            w.codex.join("agents/unnamed.toml"),
            "model = \"gpt-6-luna\"\n",
        );
        write(w.claude.join("projects/project/main.jsonl"), "");
        let data = crate::reader::scan(&w.scan_options()).unwrap();
        let claude = data
            .profiles
            .iter()
            .find(|p| p.harness == Harness::Claude)
            .unwrap();
        let agents = &claude.agents;
        assert_eq!(agents.model.as_ref().unwrap().value, "sonnet");
        assert!(agents.force);
        let names: Vec<(&str, Option<&str>)> = agents
            .definitions
            .iter()
            .map(|d| (d.name.as_str(), d.model.as_deref()))
            .collect();
        assert_eq!(names, [("reviewer", Some("haiku"))]);
        let codex = data
            .profiles
            .iter()
            .find(|p| p.harness == Harness::Codex)
            .unwrap();
        let agents = &codex.agents;
        assert_eq!(agents.model.as_ref().unwrap().value, "gpt-6-luna");
        assert_eq!(agents.review.as_ref().unwrap().value, "gpt-6-luna");
        let roles: Vec<(&str, Option<&str>, Option<&str>)> = agents
            .definitions
            .iter()
            .map(|d| (d.name.as_str(), d.model.as_deref(), d.effort.as_deref()))
            .collect();
        assert_eq!(
            roles,
            [
                ("checker", Some("gpt-6-luna"), Some("low")),
                ("builder", None, Some("high")),
            ]
        );
        assert!(
            !serde_json::to_string(&data)
                .unwrap()
                .contains("SYNTHETIC_PRIVATE")
        );
    }

    fn put(path: std::path::PathBuf, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn path(p: &std::path::Path) -> String {
        p.to_string_lossy().into_owned()
    }

    /// Re-discover one harness's profile with subagent requests in `cwds`.
    fn discover_in(w: &Workspace, harness: Harness, cwds: &[&std::path::Path]) -> Profile {
        let options = w.scan_options();
        let mut data = crate::reader::scan(&options).unwrap();
        let id = data
            .profiles
            .iter()
            .find(|p| p.harness == harness)
            .unwrap()
            .id
            .clone();
        let requests: Vec<Request> = cwds
            .iter()
            .map(|cwd| Request {
                harness,
                agent: Agent::Subagent,
                cwd: Some(path(cwd).into()),
                profiles: vec![id.clone()].into(),
                ..make_request()
            })
            .collect();
        discover(&options.discovery, &mut data.profiles, &requests, &[]).unwrap();
        data.profiles.into_iter().find(|p| p.id == id).unwrap()
    }

    #[test]
    fn project_settings_and_agent_files_apply_in_their_own_directories() {
        let w = Workspace::new();
        let project = w.home.join("project");
        let other = w.home.join("other");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        std::fs::create_dir_all(other.join(".git")).unwrap();
        put(
            project.join(".claude/settings.json"),
            r#"{"env":{"CLAUDE_CODE_SUBAGENT_MODEL":"haiku"}}"#,
        );
        put(
            project.join(".claude/agents/reviewer.md"),
            "---\nname: reviewer\nmodel: sonnet\n---\n",
        );
        put(
            w.claude.join("agents/reviewer.md"),
            "---\nname: reviewer\nmodel: opus\n---\n",
        );
        put(w.claude.join("projects/project/main.jsonl"), "");
        let profile = discover_in(&w, Harness::Claude, &[&project, &other]);
        let agents = &profile.agents;
        assert_eq!(agents.model, None);
        // A directory without project settings or agents keeps the profile's controls.
        assert_eq!(agents.directories.len(), 1);
        let inside = scoped(agents, Some(&path(&project)));
        let setting = inside.model.as_ref().unwrap();
        assert_eq!(setting.value, "haiku");
        let file = path(&project.join(".claude").join("settings.json"));
        assert!(setting.owner.starts_with(&file), "{setting:?}");
        assert_eq!(
            definition(inside, "reviewer").unwrap().model.as_deref(),
            Some("sonnet")
        );
        let outside = scoped(agents, Some(&path(&other)));
        assert_eq!(
            definition(outside, "reviewer").unwrap().model.as_deref(),
            Some("opus")
        );
        // The guidance names the file that applies where the run happened.
        let (model, _) = settings_text(&profile, inside, "reviewer", false, false);
        let file = path(&project.join(".claude").join("agents").join("reviewer.md"));
        assert!(model.contains(&file), "{model}");
    }

    #[test]
    fn a_codex_profile_that_can_replace_subagent_settings_leaves_them_unresolved() {
        let w = Workspace::new();
        let project = w.home.join("project");
        std::fs::create_dir_all(&project).unwrap();
        put(w.codex.join("fast.config.toml"), "model = \"gpt-6-luna\"\n");
        let profile = discover_in(&w, Harness::Codex, &[&project]);
        assert_eq!(profile.agents.unresolved, None);
        put(
            w.codex.join("config.toml"),
            "[profiles.legacy]\nreview_model = \"gpt-6-luna\"\n",
        );
        put(
            w.codex.join("cheap.config.toml"),
            "[agents]\ndefault_subagent_model = \"gpt-6-luna\"\n",
        );
        let profile = discover_in(&w, Harness::Codex, &[&project]);
        let reason = profile.agents.unresolved.clone().unwrap();
        let named = path(&w.codex.join("cheap.config.toml"));
        assert!(
            reason.contains(&named) && reason.contains("profiles.legacy"),
            "{reason}"
        );
        assert!(!reason.contains("fast.config.toml"), "{reason}");
        let (model, effort) = settings_text(&profile, &profile.agents, "review", false, true);
        assert!(
            model.contains(&named) && effort.contains(&named),
            "{model} {effort}"
        );
    }

    #[test]
    fn codex_role_layers_follow_codex_precedence_and_project_trust() {
        let w = Workspace::new();
        let trusted = w.home.join("trusted");
        let unknown = w.home.join("unknown");
        std::fs::create_dir_all(trusted.join(".git")).unwrap();
        std::fs::create_dir_all(unknown.join(".git")).unwrap();
        let role = |name: &str, model: &str| {
            format!(
                "name = \"{name}\"\ndescription = \"SYNTHETIC_PRIVATE\"\ndeveloper_instructions = \"SYNTHETIC_PRIVATE\"\nmodel = \"{model}\"\n"
            )
        };
        // The system and user layers both discover `dup`; the user's layer wins.
        put(w.home.join("agents/dup.toml"), &role("dup", "gpt-system"));
        put(w.codex.join("agents/dup.toml"), &role("dup", "gpt-user"));
        // A role file's own name overrides the name it is declared under.
        put(
            w.codex.join("config.toml"),
            &format!(
                "[agents.legacy]\ndescription = \"x\"\nconfig_file = \"roles/legacy.toml\"\n[agents.checker]\ndescription = \"x\"\n[agents.ghost]\nnickname_candidates = []\n[projects.{}]\ntrust_level = \"trusted\"\n",
                serde_json::to_string(&path(&trusted)).unwrap()
            ),
        );
        put(
            w.codex.join("roles/legacy.toml"),
            "name = \"reviewer\"\nmodel = \"gpt-legacy\"\n",
        );
        put(
            trusted.join(".codex/config.toml"),
            "review_model = \"gpt-6-luna\"\n",
        );
        put(
            unknown.join(".codex/config.toml"),
            "review_model = \"gpt-6-luna\"\n",
        );
        let profile = discover_in(&w, Harness::Codex, &[&trusted, &unknown]);
        let agents = &profile.agents;
        let model = |name: &str| definition(agents, name).and_then(|d| d.model.as_deref());
        assert_eq!(model("dup"), Some("gpt-user"));
        assert_eq!(model("reviewer"), Some("gpt-legacy"));
        assert_eq!(model("legacy"), None);
        // A declaration without a role file is a role; one without a description is not.
        let declared: Vec<&str> = agents.declared.keys().map(String::as_str).collect();
        assert_eq!(declared, ["checker"]);
        let (setting, _) = settings_text(&profile, agents, "checker", false, false);
        assert!(setting.contains("[agents.checker]"), "{setting}");
        assert_eq!(agents.review, None);
        let inside = scoped(agents, Some(&path(&trusted)));
        assert_eq!(inside.review.as_ref().unwrap().value, "gpt-6-luna");
        assert_eq!(inside.unresolved, None);
        let untrusted = scoped(agents, Some(&path(&unknown)));
        assert!(
            untrusted
                .unresolved
                .as_deref()
                .is_some_and(|r| r.contains("trust")),
            "{untrusted:?}"
        );
        assert!(
            !serde_json::to_string(&profile)
                .unwrap()
                .contains("SYNTHETIC_PRIVATE")
        );
    }

    #[test]
    fn a_run_keeps_its_start_and_its_parent_from_before_the_window() {
        let request = |id: &str, session: &str, time: Millis, model: &str| {
            let mut r = Request {
                id: id.into(),
                session: session.into(),
                agent: Agent::Subagent,
                time,
                ..make_request()
            };
            r.model = Some(model.into());
            r.parts[0].model = Some(model.into());
            r
        };
        // A reviewer run started before its agent file changed and the window began.
        let mut data = scan(
            vec![
                request("late", "child", 100, "claude-haiku-5-5"),
                request("nested", "grandchild", 100, "claude-opus-5-5"),
            ],
            vec![
                spawn("reviewer"),
                Subagent {
                    session: "grandchild".into(),
                    parent: Some("middle".into()),
                    ..spawn("general-purpose")
                },
            ],
        );
        data.cache_context.requests = vec![request("early", "child", 10, "claude-haiku-5-5")];
        // The grandchild's parent is itself a subagent, known only from its earlier Codex-style
        // start record.
        data.cache_context.codex = vec![request("middle", "middle", 20, "claude-opus-5-5")];
        let mut file = defined("reviewer", Some("haiku"), None);
        file.modified = Some(50);
        data.profiles[0].agents.definitions = vec![file];
        let lines = subagent_costs(&data, &prices());
        let line = |kind: &str| lines.iter().find(|l| l.kind == kind).unwrap();
        let reviewer = line("reviewer");
        assert_eq!(reviewer.requests, 1);
        assert_eq!(reviewer.sources[0].source, ModelSource::Unknown);
        assert!(
            reviewer.sources[0].detail.contains("changed after"),
            "{:?}",
            reviewer.sources
        );
        assert_eq!(
            line("general-purpose").sources[0].source,
            ModelSource::Inherited
        );
    }
}
