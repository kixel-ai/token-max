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
    DiscoveryOptions, directories, error, existing_directory, join, project_directories, realpath,
    resolve, settings,
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
        let value = value.trim().trim_matches(['"', '\'']);
        let value = label(&Value::from(value));
        match key.trim() {
            "name" => name = value,
            "model" => model = value,
            "effort" => effort = value,
            _ => {}
        }
    }
    None
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

/// A Codex role file's model and effort; `name` defaults to the file's stem.
fn codex_definition(file: &str, name: Option<&str>) -> Result<Option<AgentDefinition>, ScanError> {
    let role = settings(file, Harness::Codex)?;
    let stem = file
        .rsplit(['/', '\\'])
        .next()
        .and_then(|f| f.strip_suffix(".toml"));
    let Some(name) = name
        .map(str::to_string)
        .or_else(|| role.get("name").and_then(label))
        .or_else(|| stem.map(str::to_string))
    else {
        return Ok(None);
    };
    Ok(Some(AgentDefinition {
        name,
        file: file.to_string(),
        project: None,
        model: role.get("model").and_then(label),
        effort: role.get("model_reasoning_effort").and_then(label),
        modified: modified(file),
    }))
}

/// Whether an environment value turns a Claude Code flag on.
fn enabled(value: Option<&Value>) -> bool {
    matches!(
        js_string(value).to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Resolve every profile's current subagent model and effort controls, including agent
/// files in the projects its subagents ran in.
pub fn discover(
    options: &DiscoveryOptions,
    profiles: &mut [Profile],
    requests: &[Request],
) -> Result<(), ScanError> {
    let env = &options.env;
    let empty = Env::default();
    let default_root = join(&[&options.home, ".claude"]);
    let configured = env.get("CLAUDE_CONFIG_DIR").unwrap_or(&default_root);
    let active = realpath(&resolve(configured)).unwrap_or_default();
    for profile in profiles.iter_mut() {
        profile.agents = match profile.harness {
            Harness::Claude => {
                let user = settings(&profile.settings_file, Harness::Claude)?;
                let managed = settings(&options.managed_settings_file, Harness::Claude)?;
                let local_env = if profile.t3.is_none() && active == profile.root {
                    env
                } else {
                    &empty
                };
                let scopes = [
                    SettingsScope {
                        config: &user,
                        file: &profile.settings_file,
                    },
                    SettingsScope {
                        config: &managed,
                        file: &options.managed_settings_file,
                    },
                ];
                let environment = scoped_environment(&scopes, local_env);
                let owned = |key: &str| {
                    let value = js_string(environment.values.get(key));
                    (environment.values.contains_key(key)
                        && !value.is_empty()
                        && value != "inherit")
                        .then(|| Owned {
                            value,
                            owner: environment.owners[key].clone(),
                        })
                };
                let mut definitions = claude_definitions(&join(&[&profile.root, "agents"]), None)?;
                let mut seen: IndexSet<String> =
                    definitions.iter().map(|d| d.file.clone()).collect();
                let cwds =
                    directories(requests.iter().filter(|r| {
                        r.agent == Agent::Subagent && r.profiles.contains(&profile.id)
                    }));
                for cwd in cwds {
                    if !existing_directory(cwd)? {
                        continue;
                    }
                    for dir in project_directories(cwd, &[".git".into()])? {
                        for definition in
                            claude_definitions(&join(&[&dir, ".claude", "agents"]), Some(&dir))?
                        {
                            if seen.insert(definition.file.clone()) {
                                definitions.push(definition);
                            }
                        }
                    }
                }
                AgentControls {
                    model: owned("CLAUDE_CODE_SUBAGENT_MODEL"),
                    effort: owned("CLAUDE_CODE_SUBAGENT_EFFORT"),
                    force: enabled(environment.values.get("CLAUDE_CODE_SUBAGENT_MODEL_FORCE")),
                    explore_uncapped: enabled(
                        environment
                            .values
                            .get("CLAUDE_CODE_DISABLE_EXPLORE_INHERIT_CAP"),
                    ),
                    review: None,
                    definitions,
                }
            }
            Harness::Codex => {
                let system = settings(&options.codex_system_config_file, Harness::Codex)?;
                let user = settings(&profile.settings_file, Harness::Codex)?;
                let mut controls = AgentControls::default();
                let mut roles: IndexMap<String, AgentDefinition> = IndexMap::default();
                // The user file overrides the system file, key by key.
                for (config, file) in [
                    (&system, options.codex_system_config_file.as_str()),
                    (&user, profile.settings_file.as_str()),
                ] {
                    let agents = members(config.get("agents"));
                    let owned = |key: &str, owner: &str| {
                        config.get(key).and_then(label).map(|value| Owned {
                            value,
                            owner: format!("{file}:{owner}"),
                        })
                    };
                    if let Some(review) = owned("review_model", "review_model") {
                        controls.review = Some(review);
                    }
                    let setting = |key: &str| {
                        agents.get(key).and_then(label).map(|value| Owned {
                            value,
                            owner: format!("{file}:agents.{key}"),
                        })
                    };
                    if let Some(model) = setting("default_subagent_model") {
                        controls.model = Some(model);
                    }
                    if let Some(effort) = setting("default_subagent_reasoning_effort") {
                        controls.effort = Some(effort);
                    }
                    let base = crate::discovery::dirname(file);
                    for (name, role) in agents {
                        let Value::Object(role) = role else { continue };
                        let Some(path) = role.get("config_file").and_then(Value::as_str) else {
                            continue;
                        };
                        let path = if crate::safe::is_absolute(path) {
                            path.to_string()
                        } else {
                            join(&[base, path])
                        };
                        if let Some(definition) = codex_definition(&path, Some(name))? {
                            roles.insert(name.clone(), definition);
                        }
                    }
                    for path in files(&join(&[base, "agents"]), ".toml")? {
                        if roles.values().any(|r| r.file == path) {
                            continue;
                        }
                        if let Some(definition) = codex_definition(&path, None)? {
                            roles.entry(definition.name.clone()).or_insert(definition);
                        }
                    }
                }
                controls.definitions = roles.into_values().collect();
                controls
            }
        };
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
                model.contains(name)
            } else {
                model == name || model.starts_with(&format!("{name}-"))
            }
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
fn definition<'a>(
    controls: &'a AgentControls,
    kind: &str,
    cwd: Option<&str>,
) -> Option<&'a AgentDefinition> {
    let within = |dir: &str| {
        cwd.is_some_and(|cwd| {
            cwd == dir
                || cwd
                    .strip_prefix(dir)
                    .is_some_and(|rest| rest.starts_with(['/', '\\']))
        })
    };
    controls
        .definitions
        .iter()
        .filter(|d| d.name == kind)
        .filter(|d| d.project.as_deref().is_none_or(within))
        .max_by_key(|d| d.project.as_ref().map_or(0, String::len))
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
    let definition = definition(controls, kind, run.cwd)
        .filter(|_| subagent.is_none_or(|s| s.built_in != Some(true)));
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
fn codex_source(run: &Run<'_>, profile: &Profile, kind: &str) -> (ModelSource, String) {
    let controls = &profile.agents;
    // A review records no parent thread, so its session model is the configured one.
    let configured = match profile.settings.get("model") {
        Some(crate::model::Scalar::Text(model)) if run.parent.is_none() => Some(model.as_str()),
        _ => None,
    };
    let Some(recorded) = run.model else {
        return (ModelSource::Unknown, "no model was recorded".into());
    };
    let (source, detail, expect) = if kind == "review" {
        match &controls.review {
            Some(review) => (
                ModelSource::Setting,
                format!("{}={}", review.owner, review.value),
                Expect::Model(&review.value),
            ),
            None => match configured {
                Some(model) => (
                    ModelSource::Inherited,
                    format!(
                        "without review_model a review runs on the session model, {}:model={model}",
                        profile.settings_file
                    ),
                    Expect::Model(model),
                ),
                None => (
                    ModelSource::Inherited,
                    "without review_model a review runs on the session model".into(),
                    Expect::Parent,
                ),
            },
        }
    } else if let Some(file) = definition(controls, kind, None).filter(|d| d.model.is_some()) {
        if file.modified.is_none_or(|m| m > run.start) {
            return (
                ModelSource::Unknown,
                format!("{} changed after this run started", file.file),
            );
        }
        let model = file.model.as_deref().expect("filtered");
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
        (
            ModelSource::Unknown,
            format!(
                "ran on {recorded}, which current settings do not give ({detail}); the spawn_agent call may have asked for it"
            ),
        )
    }
}

/// The settings that change a type's model and effort.
fn settings_text(profile: &Profile, kind: &str, fork: bool) -> (String, String) {
    let controls = &profile.agents;
    match profile.harness {
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
            let defined = definition(controls, kind, None)
                .or_else(|| controls.definitions.iter().find(|d| d.name == kind));
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
        Harness::Codex if kind == "review" => (
            format!("review_model in {}", profile.settings_file),
            format!(
                "model_reasoning_effort in {}, shared with its session",
                profile.settings_file
            ),
        ),
        Harness::Codex => {
            let file = definition(controls, kind, None).map(|d| d.file.as_str());
            let config = &profile.settings_file;
            match file {
                Some(file) => (
                    format!("model in {file}, else [agents] default_subagent_model in {config}"),
                    format!(
                        "model_reasoning_effort in {file}, else [agents] default_subagent_reasoning_effort in {config}"
                    ),
                ),
                None => (
                    format!(
                        "model in a role file named {kind} under {}, else [agents] default_subagent_model in {config}",
                        join(&[&profile.root, "agents"])
                    ),
                    format!(
                        "model_reasoning_effort in that role file, else [agents] default_subagent_reasoning_effort in {config}"
                    ),
                ),
            }
        }
    }
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
    // Each conversation's model over time, to find the model a subagent inherited.
    let mut main: HashMap<&str, Vec<(Millis, &str)>> = HashMap::default();
    for r in scan.cache_context.requests.iter().chain(&scan.requests) {
        if r.agent != Agent::Subagent
            && let Some(model) = r.model.as_deref()
        {
            main.entry(&*r.session).or_default().push((r.time, model));
        }
    }
    let parent_model = |session: &str, time: Millis| {
        let models = main.get(session)?;
        let at = models.partition_point(|(t, _)| *t <= time);
        models[..at].last().or(models.first()).map(|(_, m)| *m)
    };
    let mut groups: IndexMap<(Harness, Vec<String>, &str), Vec<&Request>> = IndexMap::default();
    for r in scan.requests.iter().filter(|r| r.agent == Agent::Subagent) {
        let kind = spawns
            .get(&*r.session)
            .map_or(UNRECORDED, |s| s.kind.as_str());
        let mut profiles = r.profiles.to_vec();
        profiles.sort();
        groups
            .entry((r.harness, profiles, kind))
            .or_default()
            .push(r);
    }
    let mut lines = vec![];
    for ((harness, profiles, kind), requests) in groups {
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
        let mut sources: IndexMap<(ModelSource, String), u64> = IndexMap::default();
        for (session, rs) in &runs {
            let subagent = spawns.get(session).copied();
            let first = rs[0];
            let run = Run {
                subagent,
                model: rs.iter().find_map(|r| r.model.as_deref()),
                parent: subagent
                    .and_then(|s| s.parent.as_deref())
                    .and_then(|p| parent_model(p, first.time)),
                version: rs.iter().find_map(|r| r.version.as_deref()),
                cwd: rs.iter().find_map(|r| r.cwd.as_deref()),
                start: first.time,
            };
            let source = match shared {
                None => (
                    ModelSource::Unknown,
                    "profiles sharing this history have different agent settings".into(),
                ),
                Some(profile) => match harness {
                    Harness::Claude => claude_source(&run, &profile.agents, kind),
                    Harness::Codex => codex_source(&run, profile, kind),
                },
            };
            *sources.entry(source).or_default() += 1;
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
        let fork = runs.keys().all(|s| spawns.get(s).is_some_and(|s| s.fork)) && !runs.is_empty();
        let (model_setting, effort_setting) = match owners.first() {
            Some(profile) => settings_text(profile, kind, fork),
            None => ("unknown".into(), "unknown".into()),
        };
        lines.push(SubagentCost {
            harness,
            profiles,
            kind: kind.to_string(),
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
                defined("reviewer", Some("haiku"), Some("/work/project")),
                defined("reviewer", Some("sonnet"), Some("/elsewhere")),
            ],
            ..AgentControls::default()
        };
        let (found, detail) = claude_source(&r, &controls, "reviewer");
        assert_eq!(found, ModelSource::Definition);
        assert!(detail.contains("/work/project/reviewer.md"), "{detail}");
        controls.definitions[1].modified = Some(r.start + 1);
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
        let mut profile = make_profile("codex", "/home/.codex");
        profile.harness = Harness::Codex;
        let role = Subagent {
            harness: Harness::Codex,
            ..spawn("explorer")
        };
        let r = run(&role, "gpt-6-luna", "gpt-6.1-sol");
        assert_eq!(
            codex_source(&r, &profile, "explorer").0,
            ModelSource::Unknown
        );
        profile.agents.model = Some(Owned {
            value: "gpt-6-luna".into(),
            owner: "config.toml:agents.default_subagent_model".into(),
        });
        assert_eq!(
            codex_source(&r, &profile, "explorer").0,
            ModelSource::Setting
        );
        profile.agents.definitions = vec![defined("explorer", Some("gpt-6-luna"), None)];
        assert_eq!(
            codex_source(&r, &profile, "explorer").0,
            ModelSource::Definition
        );
        let parent = run(&role, "gpt-6.1-sol", "gpt-6.1-sol");
        profile.agents = AgentControls::default();
        assert_eq!(
            codex_source(&parent, &profile, "explorer").0,
            ModelSource::Inherited
        );
        // A review records no parent, so the configured session model stands in.
        let mut review = run(&role, "gpt-6.1-sol", "unused");
        review.parent = None;
        assert_eq!(
            codex_source(&review, &profile, "review").0,
            ModelSource::Unknown
        );
        profile.settings.insert(
            "model".into(),
            crate::model::Scalar::Text("gpt-6.1-sol".into()),
        );
        assert_eq!(
            codex_source(&review, &profile, "review").0,
            ModelSource::Inherited
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
            "name = \"builder\"\nmodel_reasoning_effort = \"high\"\n",
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
}
