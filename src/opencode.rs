//! OpenCode: where it keeps its settings and history, and the requests its SQLite history
//! records.
//!
//! OpenCode keeps every session in one database, `opencode.db`, rather than a transcript file
//! per session. The database is read in place through a read-only snapshot, and only counts,
//! identifiers, times and names leave SQLite: `json_extract` picks them out of each row, so
//! prompt, reasoning, reply and tool text are never read. Instruction files and skill
//! listings are the exception, read only to size them, as the other harnesses' recorded
//! context is.

use std::sync::Arc;

use jsonc_parser::cst::{CstInputValue, CstRootNode};
use rusqlite::{Connection, OpenFlags, Row, Transaction};
use serde_json::{Map, Value};

use crate::collections::{HashMap, HashSet, IndexMap};
use crate::controls::{Env, members};
use crate::discovery::{join, realpath, resolve};
use crate::doctor::{Edit, Op};
use crate::model::{
    Agent, AgentControls, AgentDefinition, Api, BillingPart, CompactionControls, Control,
    ControlValue, Few, Harness, Location, Millis, Owned, PartKind, PerBucket, Profile, Request,
    RequestEvidence, RequestMeta, Subagent, Text, TtlControl, Usage, WindowControl,
};
use crate::safe::{ScanError, identity, label};

/// The database every OpenCode session is kept in, in its data directory.
pub(crate) const DATABASE: &str = "opencode.db";

/// The `Profile::settings` key present when a global config orders its permissions as an
/// OpenCode 2 `permissions` list.
pub(crate) const RULES: &str = "permissions";

/// The `Profile::settings` key present when a rule OpenCode reads after the profile's
/// settings file could allow a skill that file denies, so no skill switch is offered.
pub(crate) const REALLOWED: &str = "permissions.reallowed";

/// OpenCode's own Code Mode namespaces; any other is an MCP server's.
pub(crate) const BUILT_IN: [&str; 2] = ["browser", "opencode"];

/// What an OpenCode request's pricing assumes, since OpenCode records neither.
const STANDARD_TIER: &str =
    "OpenCode records no service tier; priced at the provider's standard catalog rate";
const WRITE_LIFETIME: &str = "OpenCode records one cache-write count without its lifetime; priced at the catalog's cache-write rate, Anthropic's 5-minute writes";

/// The largest single value read from the database. An assistant row holds its whole reply
/// and tool output, which `json_extract` must load to pick out its counts; real ones reach a
/// few megabytes.
const VALUE_BOUND: i32 = 256 << 20;

/// `$XDG_DATA_HOME`, else `~/.local/share`, as OpenCode resolves it on every platform.
fn data_home(home: &str, env: &Env) -> String {
    match env.get("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        Some(dir) => resolve(dir),
        None => join(&[home, ".local", "share"]),
    }
}

/// OpenCode's data directory, which holds `opencode.db`.
pub(crate) fn data_dir(home: &str, env: &Env) -> String {
    join(&[&data_home(home, env), "opencode"])
}

/// OpenCode's global config directory: `$XDG_CONFIG_HOME/opencode`, else `~/.config/opencode`.
pub(crate) fn config_dir(home: &str, env: &Env) -> String {
    let base = match env.get("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
        Some(dir) => resolve(dir),
        None => join(&[home, ".config"]),
    };
    join(&[&base, "opencode"])
}

/// The global config files OpenCode merges, lowest precedence first.
const GLOBAL_FILES: [&str; 3] = ["config.json", "opencode.json", "opencode.jsonc"];

/// The project config files OpenCode reads in each directory from a session's worktree root
/// down to its working directory.
const PROJECT_FILES: [&str; 2] = ["opencode.json", "opencode.jsonc"];

/// The syntax OpenCode's own parser accepts: comments and trailing commas, nothing looser.
pub(crate) fn parse_options() -> jsonc_parser::ParseOptions {
    jsonc_parser::ParseOptions {
        allow_comments: true,
        allow_trailing_commas: true,
        allow_loose_object_property_names: false,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
        allow_bare_decimal_point_numbers: false,
        allow_extended_string_escapes: false,
        allow_non_finite_numbers: false,
    }
}

/// A config file's text as a value; None when OpenCode would not parse it either.
pub(crate) fn parse_config(text: &str) -> Option<Value> {
    if text.trim().is_empty() {
        return Some(Value::Object(Map::new()));
    }
    jsonc_parser::parse_to_serde_value::<Value>(text, &parse_options()).ok()
}

/// Merge `top` over `base` as OpenCode merges config layers: objects merge key by key,
/// anything else replaces.
fn merge(base: &mut Map<String, Value>, top: &Map<String, Value>) {
    for (key, value) in top {
        match (base.get_mut(key), value) {
            (Some(Value::Object(below)), Value::Object(above)) => merge(below, above),
            _ => {
                base.insert(key.clone(), value.clone());
            }
        }
    }
}

/// One config layer: its file and what it sets.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Layer {
    pub file: String,
    pub config: Map<String, Value>,
}

/// The config files of one profile, lowest precedence first: the global files, then the
/// file `OPENCODE_CONFIG` names when this shell runs this profile.
pub(crate) fn layers(
    options: &crate::discovery::DiscoveryOptions,
    env: &Env,
) -> Result<Vec<Layer>, ScanError> {
    let dir = config_dir(&options.home, env);
    let mut files: Vec<String> = GLOBAL_FILES.iter().map(|f| join(&[&dir, f])).collect();
    if let Some(custom) = env.get("OPENCODE_CONFIG").filter(|f| !f.is_empty()) {
        files.push(resolve(custom));
    }
    let mut result = vec![];
    for file in files {
        let config = options.settings(&file, Harness::OpenCode)?;
        if std::fs::metadata(&file).is_ok() {
            result.push(Layer { file, config });
        }
    }
    Ok(result)
}

/// The project config layers OpenCode reads for a session in `cwd`, lowest precedence first:
/// from the repository root, or `cwd` itself outside one, down to `cwd`.
pub(crate) fn project_layers(
    options: &crate::discovery::DiscoveryOptions,
    cwd: &str,
) -> Result<Vec<Layer>, ScanError> {
    let mut result = vec![];
    if !crate::discovery::existing_directory(cwd)? {
        return Ok(result);
    }
    let directories = crate::discovery::project_directories(cwd, &[".git".into()])?;
    for directory in directories.iter().rev() {
        for name in PROJECT_FILES {
            let file = join(&[directory, name]);
            if std::fs::metadata(&file).is_ok() {
                let config = options.settings(&file, Harness::OpenCode)?;
                result.push(Layer { file, config });
            }
        }
    }
    Ok(result)
}

/// Every layer merged, as OpenCode sees them.
pub(crate) fn merged(layers: &[Layer]) -> Map<String, Value> {
    let mut config = Map::new();
    for layer in layers {
        merge(&mut config, &layer.config);
    }
    config
}

/// The global file a confirmed change is written to: the highest-precedence global file
/// that exists, else `opencode.json`, which OpenCode creates.
pub(crate) fn settings_file(home: &str, env: &Env) -> String {
    let dir = config_dir(home, env);
    GLOBAL_FILES
        .iter()
        .rev()
        .map(|f| join(&[&dir, f]))
        .find(|f| std::fs::metadata(f).is_ok())
        .unwrap_or_else(|| join(&[&dir, "opencode.json"]))
}

/// One MCP server a config declares, and the path its switch lives under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Server {
    pub name: String,
    pub enabled: bool,
    /// `["mcp", "servers", name]` in OpenCode 2's shape, `["mcp", name]` in OpenCode 1's.
    pub path: Vec<String>,
}

/// The MCP servers a config declares, in either shape OpenCode accepts: `mcp.servers.<name>`,
/// or `mcp.<name>` beside `mcp.timeout`.
pub(crate) fn servers(config: &Map<String, Value>) -> Vec<Server> {
    let mcp = members(config.get("mcp"));
    let (entries, prefix): (&Map<String, Value>, &[&str]) = match mcp.get("servers") {
        Some(Value::Object(servers)) => (servers, &["mcp", "servers"]),
        _ => (mcp, &["mcp"]),
    };
    entries
        .iter()
        .filter(|(_, server)| server.is_object())
        .map(|(name, server)| Server {
            name: name.clone(),
            // OpenCode 2 switches `mcp.servers` entries with `disabled` alone, OpenCode 1's
            // `mcp` entries with `enabled` alone.
            enabled: if prefix.len() == 2 {
                server.get("disabled") != Some(&Value::Bool(true))
            } else {
                server.get("enabled") != Some(&Value::Bool(false))
            },
            path: prefix
                .iter()
                .map(|p| p.to_string())
                .chain([name.clone()])
                .collect(),
        })
        .collect()
}

/// The MCP servers project layers declare, as OpenCode resolves them: a later layer's
/// declaration replaces an earlier one of the same name whole.
pub(crate) fn project_servers(layers: &[Layer]) -> Vec<Server> {
    let mut declared: IndexMap<String, Server> = IndexMap::default();
    for layer in layers {
        for server in servers(&layer.config) {
            declared.insert(server.name.clone(), server);
        }
    }
    declared.into_values().collect()
}

/// A server's `Profile::mcp_servers` entry: its key path as a JSON list, since a config
/// declares servers in either of two shapes and its switch must use the same one.
fn entry(server: &Server) -> String {
    serde_json::to_string(&server.path).expect("strings serialize")
}

/// The switched-on servers a profile's `mcp_servers` entries name.
pub(crate) fn declared(entries: &[String]) -> Vec<Server> {
    entries
        .iter()
        .filter_map(|e| serde_json::from_str::<Vec<String>>(e).ok())
        .filter_map(|path| {
            Some(Server {
                name: path.last()?.clone(),
                enabled: true,
                path,
            })
        })
        .collect()
}

/// The control that turns an MCP server off: `disabled: true` in OpenCode 2's
/// `mcp.servers`, `enabled: false` in OpenCode 1's `mcp`.
pub(crate) fn server_off(server: &Server) -> Control {
    let v2 = server.path.len() == 3 && server.path[1] == "servers";
    let (key, value) = if v2 {
        ("disabled", true)
    } else {
        ("enabled", false)
    };
    Control {
        path: server.path.iter().cloned().chain([key.into()]).collect(),
        value: ControlValue::Bool(value),
        append: false,
    }
}

/// The control that hides a skill from every agent, which OpenCode lists only while its
/// permission is not deny: a `deny` rule appended to OpenCode 2's ordered `permissions`,
/// where the last matching rule wins, when the config keeps `rules` there; else OpenCode 1's
/// `permission.skill.<id>`, which OpenCode 1 also reads.
pub(crate) fn skill_off(id: &str, rules: bool) -> Control {
    if rules {
        Control {
            path: vec![RULES.into()],
            value: ControlValue::Rule {
                action: "skill".into(),
                resource: id.into(),
                effect: "deny".into(),
            },
            append: true,
        }
    } else {
        Control {
            path: vec!["permission".into(), "skill".into(), id.into()],
            value: ControlValue::Text("deny".into()),
            append: false,
        }
    }
}

/// The MCP servers and skills a config turns off, as the report names their switches.
pub(crate) fn switched_off(config: &Map<String, Value>) -> Vec<Control> {
    let mut result: Vec<Control> = servers(config)
        .iter()
        .filter(|s| !s.enabled)
        .map(server_off)
        .collect();
    let skills = config.get("permission").and_then(|p| p.get("skill"));
    for (id, value) in members(skills) {
        if value.as_str() == Some("deny") && !id.contains('*') {
            result.push(skill_off(id, false));
        }
    }
    // A skill the last rule matching it denies, a wildcard's included.
    let rules = skill_rules(config.get(RULES));
    let named: IndexMap<&str, ()> = rules
        .iter()
        .filter(|(resource, _)| !resource.contains(['*', '?']))
        .map(|(resource, _)| (*resource, ()))
        .collect();
    for id in named.keys() {
        if effect(&rules, id) == Some("deny") {
            result.push(skill_off(id, true));
        }
    }
    result
}

/// Each rule of a `permissions` list that can match the `skill` action, a wildcard
/// action's included: its resource pattern and effect, in order.
fn skill_rules(rules: Option<&Value>) -> Vec<(&str, &str)> {
    let rules = rules.and_then(Value::as_array);
    rules
        .into_iter()
        .flatten()
        .filter(|rule| {
            rule.get("action")
                .and_then(Value::as_str)
                .is_some_and(|action| glob(action, "skill"))
        })
        .filter_map(|rule| {
            let field = |key: &str| rule.get(key).and_then(Value::as_str);
            Some((field("resource")?, field("effect")?))
        })
        .collect()
}

/// The effect of the last rule whose pattern matches `id`; OpenCode's last match wins.
fn effect<'a>(rules: &[(&str, &'a str)], id: &str) -> Option<&'a str> {
    rules
        .iter()
        .rev()
        .find(|(pattern, _)| glob(pattern, id))
        .map(|(_, effect)| *effect)
}

/// Whether `text` matches `pattern` as OpenCode's permission wildcards do: `*` matches any
/// run of characters and `?` any one.
fn glob(pattern: &str, text: &str) -> bool {
    let mut chars = pattern.chars();
    match chars.next() {
        None => text.is_empty(),
        Some('*') => {
            let rest = chars.as_str();
            text.char_indices()
                .map(|(i, _)| i)
                .chain([text.len()])
                .any(|i| glob(rest, &text[i..]))
        }
        Some(c) => {
            let mut text_chars = text.chars();
            match text_chars.next() {
                Some(t) if c == '?' || c == t => glob(chars.as_str(), text_chars.as_str()),
                _ => false,
            }
        }
    }
}

/// Whether a rule after the one a change appends to `file`'s `permissions` could allow a
/// skill again: an agent's own rules, which OpenCode appends after the global ones, or a
/// layer `layers` reads after `file`. A skill switch is then left to the person.
pub(crate) fn skills_reallowed(layers: &[Layer], file: &str) -> bool {
    let agents = layers.iter().any(|layer| agents_reallow(&layer.config));
    let later = layers
        .iter()
        .skip_while(|l| l.file != file)
        .skip(1)
        .any(|l| reallows(&l.config));
    agents || later
}

/// Whether a list of `permissions` holds a skill rule that is not a denial.
fn allows(rules: Option<&Value>) -> bool {
    skill_rules(rules)
        .iter()
        .any(|(_, effect)| *effect != "deny")
}

/// Whether an agent a config defines has its own rule that could allow a skill.
fn agents_reallow(config: &Map<String, Value>) -> bool {
    ["agent", "agents"].iter().any(|key| {
        members(config.get(*key))
            .values()
            .any(|agent| allows(agent.get(RULES)))
    })
}

/// Whether a config read after the global ones, such as a project's, has a rule that
/// could allow a skill the global file denies.
pub(crate) fn reallows(config: &Map<String, Value>) -> bool {
    allows(config.get(RULES)) || agents_reallow(config)
}

/// An OpenCode config with `edits` applied and the keys at `removals` deleted, keeping its
/// comments, order and formatting.
pub(crate) fn edit(text: &str, edits: &[Edit], removals: &[&[String]]) -> Result<String, String> {
    let source = if text.trim().is_empty() { "{}\n" } else { text };
    let root = CstRootNode::parse(source, &parse_options())
        .map_err(|_| "the file is not valid JSONC".to_string())?;
    let object = root
        .object_value()
        .ok_or_else(|| "the file does not hold a JSON object".to_string())?;
    let removed = removals.iter().map(|path| Edit {
        path: path.to_vec(),
        value: Value::Null,
        op: Op::Remove,
    });
    for edit in edits.iter().cloned().chain(removed) {
        let (last, parents) = edit.path.split_last().expect("edits name a setting");
        if edit.op == Op::Remove {
            let mut node = Some(object.clone());
            for key in parents {
                node = node.and_then(|n| n.object_value(key));
            }
            if let Some(prop) = node.and_then(|n| n.get(last)) {
                prop.remove();
            }
            continue;
        }
        let mut node = object.clone();
        for (i, key) in parents.iter().enumerate() {
            node = node
                .object_value_or_create(key)
                .ok_or_else(|| format!("{} is not an object", edit.path[..=i].join(".")))?;
        }
        match edit.op {
            Op::Set => match node.get(last) {
                Some(prop) => prop.set_value(input(&edit.value)),
                None => {
                    node.append(last, input(&edit.value));
                }
            },
            Op::Append => {
                let list = node
                    .array_value_or_create(last)
                    .ok_or_else(|| format!("{} is not a list", edit.path.join(".")))?;
                let elements: Vec<Value> = list
                    .elements()
                    .iter()
                    .filter_map(|e| parse_config(&e.to_string()))
                    .collect();
                fn text<'v>(v: &'v Value, key: &str) -> Option<&'v str> {
                    v.get(key).and_then(Value::as_str)
                }
                let held = match (text(&edit.value, "action"), text(&edit.value, "resource")) {
                    // A permission rule is in force when the last rule matching its action
                    // and resource, a wildcard's included, has its effect.
                    (Some(action), Some(resource)) => elements
                        .iter()
                        .rev()
                        .find(|v| {
                            text(v, "action").is_some_and(|p| glob(p, action))
                                && text(v, "resource").is_some_and(|p| glob(p, resource))
                        })
                        .is_some_and(|v| text(v, "effect") == text(&edit.value, "effect")),
                    _ => elements.contains(&edit.value),
                };
                if !held {
                    list.append(input(&edit.value));
                }
            }
            Op::Remove => unreachable!("removals are handled above"),
        }
    }
    Ok(root.to_string())
}

/// A JSON value as the CST inserts it.
fn input(value: &Value) -> CstInputValue {
    match value {
        Value::Null => CstInputValue::Null,
        Value::Bool(b) => CstInputValue::Bool(*b),
        Value::Number(n) => CstInputValue::Number(n.to_string()),
        Value::String(s) => CstInputValue::String(s.clone()),
        Value::Array(items) => CstInputValue::Array(items.iter().map(input).collect()),
        Value::Object(map) => {
            CstInputValue::Object(map.iter().map(|(k, v)| (k.clone(), input(v))).collect())
        }
    }
}

/// A model selection, in either form OpenCode 2 accepts.
struct Selection {
    provider: Option<String>,
    model: String,
    variant: Option<String>,
}

/// Read a model selection: `provider/model#variant`, where the model ID may hold more
/// slashes and the variant is optional, or `{"providerID", "model", "variant"}`.
fn model_reference(value: &Value) -> Option<Selection> {
    if let Some(selection) = value.as_object() {
        let field = |key: &str| selection.get(key).and_then(Value::as_str).map(String::from);
        return Some(Selection {
            provider: field("providerID"),
            model: field("model")?,
            variant: field("variant"),
        });
    }
    let reference = value.as_str()?;
    let (model, variant) = match reference.split_once('#') {
        Some((model, variant)) => (model, Some(variant.to_string())),
        None => (reference, None),
    };
    let (provider, model) = match model.split_once('/') {
        Some((provider, model)) => (Some(provider.to_string()), model),
        None => (None, model),
    };
    Some(Selection {
        provider,
        model: model.to_string(),
        variant,
    })
}

/// Subagent model and effort controls: each `agents.<name>` (OpenCode 2) or `agent.<name>`
/// (OpenCode 1) entry's `model` and variant, and the top-level `model` a subagent without
/// one runs on when its parent's is unrecorded.
fn agent_controls(layers: &[Layer]) -> AgentControls {
    let mut definitions: IndexMap<String, AgentDefinition> = IndexMap::default();
    let mut session = None;
    for layer in layers {
        if let Some(selection) = layer.config.get("model").and_then(model_reference) {
            session = Some(Owned {
                value: selection.model,
                owner: format!("{}:model", layer.file),
            });
        }
        for key in ["agent", "agents"] {
            for (name, agent) in members(layer.config.get(key)) {
                let entry = definitions
                    .entry(name.clone())
                    .or_insert_with(|| AgentDefinition {
                        name: name.clone(),
                        file: layer.file.clone(),
                        project: None,
                        model: None,
                        effort: None,
                        modified: None,
                        key: Some(key.into()),
                        provider: None,
                    });
                if let Some(selection) = agent.get("model").and_then(model_reference) {
                    entry.model = Some(selection.model);
                    entry.provider = selection.provider;
                    entry.file = layer.file.clone();
                    entry.modified = modified(&layer.file);
                    entry.key = Some(key.into());
                    if selection.variant.is_some() {
                        entry.effort = selection.variant;
                    }
                }
                if let Some(variant) = agent.get("variant").and_then(Value::as_str) {
                    entry.effort = Some(variant.into());
                }
            }
        }
    }
    AgentControls {
        session,
        definitions: definitions.into_values().collect(),
        ..AgentControls::default()
    }
}

/// When a file last changed.
fn modified(file: &str) -> Option<Millis> {
    let time = std::fs::metadata(file).ok()?.modified().ok()?;
    let since = time.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(since.as_millis() as Millis)
}

/// One OpenCode data directory's current settings, without reading its history.
pub(crate) fn profile(
    options: &crate::discovery::DiscoveryOptions,
    canonical: String,
    alias: String,
    key: &str,
) -> Result<Profile, ScanError> {
    // Shell overrides belong only to the data directory this shell's OpenCode uses.
    let active = data_dir(&options.home, &options.env);
    let active = realpath(&active).unwrap_or(active);
    let empty = Env::default();
    let env = if active == canonical {
        &options.env
    } else {
        &empty
    };
    let layers = layers(options, env)?;
    let config = merged(&layers);
    let settings_file = settings_file(&options.home, env);
    // `OPENCODE_DB` names another database, relative to the data directory; `:memory:` keeps
    // none.
    let database = match env.get("OPENCODE_DB").filter(|d| !d.is_empty()) {
        Some(d) if d == ":memory:" => None,
        Some(d) if std::path::Path::new(d).is_absolute() => Some(d.clone()),
        Some(d) => Some(join(&[&canonical, d])),
        None => Some(join(&[&canonical, DATABASE])),
    };
    let histories = match database.as_deref().map(realpath) {
        None => vec![],
        Some(Ok(database)) => vec![database],
        Some(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => vec![],
        Some(Err(_)) => {
            return Err(crate::discovery::error(
                "cannot inspect OpenCode database",
                database.as_deref().unwrap_or_default(),
                0,
            ));
        }
    };
    let unset = |reason: &str| TtlControl {
        value: None,
        owner: String::new(),
        reason: reason.into(),
        varies: false,
    };
    let ttl = PerBucket {
        main: unset("OpenCode's cache lifetime is not replayed"),
        subagent: unset("OpenCode's cache lifetime is not replayed"),
    };
    let user = layers
        .iter()
        .find(|l| l.file == settings_file)
        .map_or_else(Map::new, |l| l.config.clone());
    // `OPENCODE_CONFIG`'s file, which overrides the global one changes are written to.
    let outranking_files = layers
        .iter()
        .skip_while(|l| l.file != settings_file)
        .skip(1)
        .filter(|l| !l.config.is_empty())
        .map(|l| l.file.clone())
        .collect();
    let mut pretended_on = vec![];
    if options.defaults {
        let real = crate::discovery::settings(&settings_file, Harness::OpenCode)?;
        pretended_on = switched_off(&real)
            .into_iter()
            .map(|control| (settings_file.clone(), control))
            .collect();
    }
    let reallowed = skills_reallowed(&layers, &settings_file);
    Ok(Profile {
        id: identity(key),
        harness: Harness::OpenCode,
        root: canonical,
        aliases: vec![alias],
        histories,
        settings_file,
        ttl,
        provider: None,
        billing: None,
        settings: [
            (
                RULES,
                layers
                    .iter()
                    .any(|l| l.config.get(RULES).is_some_and(Value::is_array)),
            ),
            (REALLOWED, reallowed),
        ]
        .into_iter()
        .filter(|(_, set)| *set)
        .map(|(key, _)| (key.to_string(), crate::model::Scalar::Bool(true)))
        .collect(),
        mcp_servers: servers(&config)
            .iter()
            .filter(|s| s.enabled)
            .map(entry)
            .collect(),
        compaction: CompactionControls {
            default: WindowControl {
                value: None,
                owner: String::new(),
                reason: "OpenCode's compaction is not replayed".into(),
                applicable: false,
                minimum_claude_patch: None,
                comparison_unknown: None,
                unset: true,
                setting: None,
            },
            models: IndexMap::default(),
            capped: false,
            declared: None,
            tuned: IndexMap::default(),
            contexts: IndexMap::default(),
            percent: None,
            layers: None,
        },
        idle: None,
        speed: None,
        bash: None,
        retention: None,
        turn_end: None,
        effort: None,
        advisor: None,
        allowlist: None,
        project_files: vec![],
        speed_directories: None,
        idle_directories: None,
        background: vec![],
        background_directories: IndexMap::default(),
        background_requests: IndexMap::default(),
        memory: None,
        inbound: None,
        t3: None,
        plugins: None,
        skill: None,
        agents: agent_controls(&layers),
        instruction_budget_file: None,
        instruction_budget_unresolved: None,
        outranking_files,
        ttl_directories: IndexMap::default(),
        claude_config: None,
        updater: None,
        codex_system_config: None,
        switched_off: switched_off(&user),
        pretended_on,
        policy_off: vec![],
    })
}

/// A native OpenCode session: its own ID, the session that spawned it, where it ran and the
/// OpenCode version that created it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Session {
    /// The reader's key, an identity of `id`.
    pub key: Text,
    pub id: String,
    pub parent: Option<String>,
    pub cwd: Option<Text>,
    pub version: Option<Text>,
    pub created: Millis,
}

/// What one tool call in a request was, by name only, never its input or output.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Use {
    /// The `skill` tool loading a skill, by its ID.
    Skill(String),
    /// A Code Mode `execute` script calling `server.tool`.
    Mcp { server: String, tool: String },
    /// Any other tool, which an MCP server configured as `server` owns when named `server_tool`.
    Tool(String),
    /// A person picking a skill for a turn, with `@` or as a skill message, by its ID.
    Selected(String),
}

/// One tool call, by name, and whether it failed.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Call {
    pub session: Text,
    /// Unique within the database: its message's ID and the call's own.
    pub id: String,
    pub time: Millis,
    pub what: Use,
    pub failed: bool,
}

/// One OpenCode database's requests and sessions, ready for the reader.
#[derive(Debug, Default)]
pub(crate) struct History {
    pub requests: Vec<Request>,
    pub sessions: Vec<Session>,
    pub subagents: Vec<Subagent>,
    pub calls: Vec<Call>,
    /// What each session last sent besides the conversation, by session key.
    pub contexts: IndexMap<Text, Context>,
}

/// One session's recorded instructions, skill listing and Code Mode catalog, rendered as
/// OpenCode 2 renders them into its prompt.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Context {
    /// Each instruction file's path, and its text as `Instructions from: <path>` and content.
    pub instructions: Vec<(String, String)>,
    /// The skill listing's whole text, and each skill's ID and entry within it.
    pub skills: Option<(String, Vec<(String, String)>)>,
    /// Each Code Mode namespace's name and its catalog lines.
    pub namespaces: Vec<(String, String)>,
    /// Text a host app adds under its own key, such as T3 Code's `api/t3-code`.
    pub apps: Vec<(String, String)>,
}

/// Why the database could not be read, without its contents.
fn unreadable(file: &str, error: rusqlite::Error) -> ScanError {
    let reason = match error {
        rusqlite::Error::SqliteFailure(e, _) => match e.code {
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked => {
                "OpenCode database is locked"
            }
            rusqlite::ErrorCode::TooBig => "OpenCode database holds a value over 256 MiB",
            rusqlite::ErrorCode::NotADatabase => "OpenCode database is not a SQLite database",
            _ => "cannot read OpenCode database",
        },
        _ => "unexpected OpenCode database shape",
    };
    crate::discovery::error(reason, file, 0)
}

/// A token count or time: a non-negative integer no larger than 2^53 - 1; None when absent.
fn number(row: &Row, index: usize) -> rusqlite::Result<Option<i64>> {
    use rusqlite::types::Value as Sql;
    match row.get::<_, Sql>(index)? {
        Sql::Null => Ok(None),
        Sql::Integer(n) if (0..=crate::safe::MAX_SAFE_INTEGER).contains(&n) => Ok(Some(n)),
        Sql::Real(f)
            if f >= 0.0 && f.fract() == 0.0 && f <= crate::safe::MAX_SAFE_INTEGER as f64 =>
        {
            Ok(Some(f as i64))
        }
        _ => Err(rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            "invalid token count or time".into(),
        )),
    }
}

/// A short label, such as a model or provider ID; None when absent or not one.
fn name(row: &Row, index: usize) -> rusqlite::Result<Option<String>> {
    use rusqlite::types::Value as Sql;
    Ok(match row.get::<_, Sql>(index)? {
        Sql::Text(text) => label(&Value::String(text)),
        _ => None,
    })
}

/// The tables the database has.
fn tables(tx: &Transaction) -> rusqlite::Result<HashSet<String>> {
    let mut statement = tx.prepare("SELECT name FROM sqlite_master WHERE type = 'table'")?;
    let names = statement.query_map([], |row| row.get::<_, String>(0))?;
    names.collect()
}

/// One assistant step's recorded usage and model, before it becomes a request.
struct Step {
    line: u64,
    id: String,
    session: String,
    time: Millis,
    end: Millis,
    provider: Option<String>,
    model: Option<String>,
    variant: Option<String>,
    agent: Option<String>,
    summary: bool,
    /// Input, output, reasoning, cache reads and cache writes, as OpenCode counts them.
    tokens: [Option<i64>; 5],
}

/// The usage OpenCode records, normalized: its input excludes cache reads and writes, and its
/// output excludes reasoning, so both are added back.
fn usage(tokens: [Option<i64>; 5]) -> Usage {
    let [input, output, reasoning, read, write] = tokens;
    let sum = |a: Option<i64>, b: Option<i64>| Some(a? + b?);
    Usage {
        input: sum(sum(input, read), write),
        output: sum(output, reasoning),
        read,
        write,
        reasoning,
        ..Usage::default()
    }
}

/// Assistant steps in OpenCode 2's `session_message`, one request each.
const V2_STEPS: &str = "SELECT m.rowid, m.id, m.session_id, m.time_created,
    json_extract(m.data, '$.model.providerID'), json_extract(m.data, '$.model.id'),
    json_extract(m.data, '$.model.variant'), json_extract(m.data, '$.agent'),
    json_extract(m.data, '$.time.created'), json_extract(m.data, '$.time.completed'),
    json_type(m.data, '$.tokens'),
    json_extract(m.data, '$.tokens.input'), json_extract(m.data, '$.tokens.output'),
    json_extract(m.data, '$.tokens.reasoning'), json_extract(m.data, '$.tokens.cache.read'),
    json_extract(m.data, '$.tokens.cache.write'), json_extract(m.data, '$.summary') IS 1
  FROM session_message m WHERE m.type = 'assistant' ORDER BY m.session_id, m.seq";

/// Steps in OpenCode 1's `part` table: each `step-finish` part is one request of the
/// assistant message it belongs to, and records only when it finished.
const V1_STEPS: &str = "SELECT p.rowid, p.id, p.session_id, p.time_created,
    json_extract(m.data, '$.providerID'), json_extract(m.data, '$.modelID'),
    json_extract(m.data, '$.variant'),
    coalesce(json_extract(m.data, '$.agent'), json_extract(m.data, '$.mode')),
    p.time_created, p.time_created,
    json_type(p.data, '$.tokens'),
    json_extract(p.data, '$.tokens.input'), json_extract(p.data, '$.tokens.output'),
    json_extract(p.data, '$.tokens.reasoning'), json_extract(p.data, '$.tokens.cache.read'),
    json_extract(p.data, '$.tokens.cache.write'), json_extract(m.data, '$.summary') IS 1
  FROM part p JOIN message m ON m.id = p.message_id
  WHERE json_extract(p.data, '$.type') = 'step-finish'
    AND json_extract(m.data, '$.role') = 'assistant'
  ORDER BY p.session_id, p.id";

/// Where OpenCode 1's rows are numbered, apart from OpenCode 2's in the same database.
const V1_LINES: u64 = 1 << 40;

/// One step row; None for a step OpenCode recorded no usage for, which failed before the
/// provider answered or is still running, so is no billed request yet.
fn step(row: &Row, offset: u64) -> rusqlite::Result<Option<Step>> {
    if row.get::<_, Option<String>>(10)?.as_deref() != Some("object") {
        return Ok(None);
    }
    let rowid: i64 = row.get(0)?;
    let time = match number(row, 8)? {
        Some(time) => time,
        None => number(row, 3)?.unwrap_or(0),
    };
    Ok(Some(Step {
        line: offset + rowid as u64,
        id: row.get(1)?,
        session: row.get(2)?,
        time,
        end: number(row, 9)?.unwrap_or(time).max(time),
        provider: name(row, 4)?,
        model: name(row, 5)?,
        variant: name(row, 6)?,
        agent: name(row, 7)?,
        summary: row.get(16)?,
        tokens: [
            number(row, 11)?,
            number(row, 12)?,
            number(row, 13)?,
            number(row, 14)?,
            number(row, 15)?,
        ],
    }))
}

/// Tool calls in OpenCode 2's assistant content, by name and skill ID only.
const V2_CALLS: &str = "SELECT m.session_id,
    coalesce(json_extract(m.data, '$.time.created'), m.time_created),
    m.id || ':' || coalesce(json_extract(c.value, '$.id'), c.key),
    json_extract(c.value, '$.name'), json_extract(c.value, '$.state.status'),
    json_extract(c.value, '$.state.input.id')
  FROM session_message m, json_each(m.data, '$.content') c
  WHERE m.type = 'assistant' AND json_extract(c.value, '$.type') = 'tool'";

/// The tools Code Mode scripts called, each named `server.tool`.
const V2_SCRIPT_CALLS: &str = "SELECT m.session_id,
    coalesce(json_extract(m.data, '$.time.created'), m.time_created),
    m.id || ':' || coalesce(json_extract(c.value, '$.id'), c.key) || ':' || t.key,
    json_extract(t.value, '$.tool'), json_extract(t.value, '$.status')
  FROM session_message m, json_each(m.data, '$.content') c,
    json_each(c.value, '$.state.metadata.toolCalls') t
  WHERE m.type = 'assistant' AND json_extract(c.value, '$.type') = 'tool'
    AND json_extract(c.value, '$.name') = 'execute'";

/// Skills a person picked: each `@` skill of a user message, and each skill message.
const V2_SELECTED: &str = "SELECT m.session_id,
    coalesce(json_extract(m.data, '$.time.created'), m.time_created),
    m.id || ':' || s.key, json_extract(s.value, '$.id')
  FROM session_message m, json_each(m.data, '$.skills') s
  WHERE m.type = 'user'
  UNION ALL
  SELECT m.session_id, coalesce(json_extract(m.data, '$.time.created'), m.time_created),
    m.id, json_extract(m.data, '$.skill')
  FROM session_message m WHERE m.type = 'skill'";

/// Tool parts in OpenCode 1's `part` table.
const V1_CALLS: &str = "SELECT p.session_id, p.time_created,
    p.message_id || ':' || coalesce(json_extract(p.data, '$.callID'), p.id),
    json_extract(p.data, '$.tool'), json_extract(p.data, '$.state.status'),
    json_extract(p.data, '$.state.input.name')
  FROM part p WHERE json_extract(p.data, '$.type') = 'tool'";

/// Each session's latest recorded instructions, skills, Code Mode catalog and app text.
const CONTEXTS: &str = "SELECT s.session_id, j.key, b.value
  FROM instruction_state s, json_each(s.current_values) j
  JOIN instruction_blob b ON b.hash = j.value
  WHERE j.key IN ('core/instructions', 'core/skill-guidance', 'core/codemode')
    OR j.key LIKE 'api/%'
  ORDER BY s.session_id, j.key";

/// The skill listing as OpenCode 2 renders it, and each skill's entry in it.
fn skill_listing(skills: &[Value]) -> Option<(String, Vec<(String, String)>)> {
    let mut entries = vec![];
    for skill in skills {
        let field = |key: &str| skill.get(key).and_then(Value::as_str);
        let (id, name, description) = (field("id")?, field("name")?, field("description")?);
        entries.push((
            id.to_string(),
            format!(
                "  <skill>\n    <id>{id}</id>\n    <name>{name}</name>\n    <description>{description}</description>\n  </skill>"
            ),
        ));
    }
    let mut lines = vec![
        "Skills provide specialized instructions and workflows for specific tasks.",
        "Use the skill tool to load a skill when a task matches its description.",
        "The user may also invoke a skill directly. When that happens, its instructions appear in the conversation as a <skill_content> block, the same shape the skill tool returns. A skill that is already present this way does not need to be invoked again.",
    ];
    if entries.is_empty() {
        lines.push("No skills are currently available.");
    } else {
        lines.push("<available_skills>");
        lines.extend(entries.iter().map(|(_, entry)| entry.as_str()));
        lines.push("</available_skills>");
    }
    Some((lines.join("\n"), entries))
}

/// One Code Mode namespace's catalog lines as OpenCode 2 renders them: a heading naming its
/// tool count, then each listed tool's line.
fn namespace(value: &Value) -> Option<(String, String)> {
    let name = value.get("name")?.as_str()?;
    let entries = value.get("entries")?.as_array()?;
    let shown = entries.len() as u64;
    let total = value.get("count").and_then(Value::as_u64).unwrap_or(shown);
    let tools = if total == 1 {
        "1 tool".to_string()
    } else {
        format!("{total} tools")
    };
    let count = if shown == total {
        tools
    } else if shown == 0 {
        format!("{tools}, none shown")
    } else {
        format!("{tools}, {shown} shown")
    };
    let description = value
        .get("description")
        .and_then(Value::as_str)
        .map_or(String::new(), |d| format!(" // {d}"));
    let mut lines = vec![format!("- {name} ({count}){description}")];
    for entry in entries {
        lines.push(entry.get("line")?.as_str()?.to_string());
    }
    Some((server_name(name), lines.join("\n")))
}

/// A Code Mode namespace's server, without the suffix T3 adds to the MCP server it gives
/// each thread, `t3-code-<16 hex digits>`, so that server is one row.
fn server_name(name: &str) -> String {
    match name.strip_prefix("t3-code-") {
        Some(rest) if rest.len() == 16 && rest.bytes().all(|b| b.is_ascii_hexdigit()) => {
            "t3-code".into()
        }
        _ => name.into(),
    }
}

/// Read one OpenCode database through a read-only snapshot, as the profiles `profiles` share it.
pub(crate) fn read(file: &str, profiles: &[String]) -> Result<History, ScanError> {
    let fail = |e| unreadable(file, e);
    let mut db = Connection::open_with_flags(
        file,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(fail)?;
    // OpenCode writes while it runs; a reader waits out a write briefly, then fails loud.
    db.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(fail)?;
    db.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH, VALUE_BOUND)
        .map_err(fail)?;
    db.execute_batch("PRAGMA query_only=ON; PRAGMA trusted_schema=OFF;")
        .map_err(fail)?;
    let tx = db.transaction().map_err(fail)?;
    let history = read_snapshot(&tx, file, profiles).map_err(fail)?;
    tx.rollback().map_err(fail)?;
    Ok(history)
}

/// Every session, request, call and context the snapshot holds.
fn read_snapshot(tx: &Transaction, file: &str, profiles: &[String]) -> rusqlite::Result<History> {
    let tables = tables(tx)?;
    let mut history = History::default();
    // A database OpenCode 2 is still migrating holds sessions in both tables; OpenCode 2's
    // row wins for one in both.
    let session_tables: Vec<&str> = ["session", "session_v2"]
        .into_iter()
        .filter(|t| tables.contains(*t))
        .collect();
    if session_tables.is_empty() {
        return Ok(history);
    }
    let mut sessions: IndexMap<String, Session> = IndexMap::default();
    for session_table in session_tables {
        let mut statement = tx.prepare(&format!(
            "SELECT id, parent_id, directory, version, time_created FROM {session_table}"
        ))?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let cwd: Option<String> = row.get(2)?;
            sessions.insert(
                id.clone(),
                Session {
                    key: identity(&id).into(),
                    id,
                    parent: row.get(1)?,
                    cwd: cwd.filter(|c| crate::safe::is_absolute(c)).map(Text::from),
                    version: name(row, 3)?.map(Text::from),
                    created: number(row, 4)?.unwrap_or(0),
                },
            );
        }
    }
    sessions.sort_by(|_, a, _, b| (a.created, &a.id).cmp(&(b.created, &b.id)));
    let v2 = tables.contains("session_message");
    let v1 = tables.contains("message") && tables.contains("part");
    let mut steps: Vec<Step> = vec![];
    if v2 {
        let mut statement = tx.prepare(V2_STEPS)?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            steps.extend(step(row, 0)?);
        }
    }
    // OpenCode 2 imports OpenCode 1's sessions into its own table, so a session holding
    // OpenCode 2 messages is read from those alone.
    let imported: HashSet<String> = if v2 {
        let mut statement = tx.prepare("SELECT DISTINCT session_id FROM session_message")?;
        let ids = statement.query_map([], |row| row.get::<_, String>(0))?;
        ids.collect::<rusqlite::Result<_>>()?
    } else {
        HashSet::default()
    };
    if v1 {
        let mut statement = tx.prepare(V1_STEPS)?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            if let Some(step) = step(row, V1_LINES)?
                && !imported.contains(&step.session)
            {
                steps.push(step);
            }
        }
    }
    let location_file: Text = file.into();
    let profiles: crate::model::Profiles = profiles.into();
    // A subagent session's first recorded agent names its kind.
    let mut kinds: HashMap<String, Option<String>> = HashMap::default();
    for step in steps {
        // A message whose session is gone has nowhere to belong.
        let Some(session) = sessions.get(&step.session) else {
            continue;
        };
        if session.parent.is_some() {
            kinds
                .entry(step.session.clone())
                .or_insert_with(|| step.agent.clone());
        }
        let compaction = step.summary || step.agent.as_deref() == Some("compaction");
        let model: Option<Text> = step.model.map(Text::from);
        history.requests.push(Request {
            evidence: RequestEvidence::Measured,
            id: format!("opencode:{}", identity(&step.id)).into(),
            time: step.time,
            end_time: step.end,
            reference: None,
            turn: None,
            parts: vec![BillingPart {
                kind: if compaction {
                    PartKind::Compaction
                } else {
                    PartKind::Message
                },
                model: model.clone(),
                usage: usage(step.tokens),
                tier: Some("standard".into()),
                speed: None,
                geo: None,
            }],
            locations: Few::One(Location {
                file: location_file.clone(),
                line: step.line,
            }),
            t3: None,
            meta: Arc::new(RequestMeta {
                harness: Harness::OpenCode,
                session: session.key.clone(),
                agent: if session.parent.is_some() {
                    Agent::Subagent
                } else {
                    Agent::Main
                },
                mode: step.agent.as_deref().map(Text::from),
                model,
                provider: step.provider.map(Text::from),
                api: Api::Unknown,
                source: compaction.then(|| "compaction".into()),
                trigger: None,
                version: session.version.clone(),
                client: None,
                cwd: session.cwd.clone(),
                // `default` is the model's own default effort, which OpenCode leaves unnamed.
                effort: step.variant.filter(|v| v != "default").map(Text::from),
                server_tools: None,
                profiles: profiles.clone(),
                attempt: None,
                requested_speed: None,
                issues: vec![STANDARD_TIER.into(), WRITE_LIFETIME.into()],
            }),
        });
    }
    for session in sessions.values() {
        let Some(parent) = &session.parent else {
            continue;
        };
        history.subagents.push(Subagent {
            harness: Harness::OpenCode,
            session: session.key.clone(),
            parent: Some(identity(parent).into()),
            kind: kinds
                .get(&session.id)
                .cloned()
                .flatten()
                .unwrap_or_else(|| crate::reader::UNRECORDED.into()),
            built_in: None,
            fork: false,
            native: false,
            model: None,
            effort: None,
        });
    }
    let key = |session: &str| -> Text {
        sessions
            .get(session)
            .map_or_else(|| identity(session).into(), |s| s.key.clone())
    };
    for (query, present, v1_rows) in [(V2_CALLS, v2, false), (V1_CALLS, v1, true)] {
        if !present {
            continue;
        }
        let mut statement = tx.prepare(query)?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let session: String = row.get(0)?;
            if v1_rows && imported.contains(&session) {
                continue;
            }
            let Some(tool) = name(row, 3)? else {
                continue;
            };
            let what = match (tool.as_str(), name(row, 5)?) {
                ("skill", Some(skill)) => Use::Skill(skill),
                // A script's calls are read on their own, by the tools they name.
                ("execute", _) if !v1_rows => continue,
                _ => Use::Tool(tool),
            };
            history.calls.push(Call {
                session: key(&session),
                id: row.get(2)?,
                time: number(row, 1)?.unwrap_or(0),
                what,
                failed: row.get::<_, Option<String>>(4)?.as_deref() == Some("error"),
            });
        }
    }
    if v2 {
        let mut statement = tx.prepare(V2_SCRIPT_CALLS)?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let session: String = row.get(0)?;
            // A script calls `server.tool`; a bare name is one of OpenCode's own tools.
            let Some((server, tool)) = name(row, 3)?.and_then(|t| {
                let (server, tool) = t.split_once('.')?;
                Some((server_name(server), tool.to_string()))
            }) else {
                continue;
            };
            history.calls.push(Call {
                session: key(&session),
                id: row.get(2)?,
                time: number(row, 1)?.unwrap_or(0),
                what: Use::Mcp { server, tool },
                failed: row.get::<_, Option<String>>(4)?.as_deref() == Some("error"),
            });
        }
        let mut statement = tx.prepare(V2_SELECTED)?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let session: String = row.get(0)?;
            let Some(skill) = name(row, 3)? else {
                continue;
            };
            history.calls.push(Call {
                session: key(&session),
                id: row.get(2)?,
                time: number(row, 1)?.unwrap_or(0),
                what: Use::Selected(skill),
                failed: false,
            });
        }
    }
    if tables.contains("instruction_state") && tables.contains("instruction_blob") {
        let mut statement = tx.prepare(CONTEXTS)?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let session: String = row.get(0)?;
            let kind: String = row.get(1)?;
            let Some(text) = row.get::<_, Option<String>>(2)? else {
                continue;
            };
            let value: Value = serde_json::from_str(&text).map_err(|_| {
                rusqlite::Error::FromSqlConversionFailure(
                    2,
                    rusqlite::types::Type::Text,
                    "invalid recorded instructions".into(),
                )
            })?;
            let context = history.contexts.entry(key(&session)).or_default();
            match kind.as_str() {
                "core/instructions" => {
                    for file in value.as_array().into_iter().flatten() {
                        let path = file.get("path").and_then(Value::as_str);
                        let content = file.get("content").and_then(Value::as_str);
                        if let (Some(path), Some(content)) = (path, content) {
                            context.instructions.push((
                                path.to_string(),
                                format!("Instructions from: {path}\n{content}"),
                            ));
                        }
                    }
                }
                "core/skill-guidance" => {
                    context.skills = value.as_array().and_then(|s| skill_listing(s));
                }
                "core/codemode" => {
                    let namespaces = value.get("namespaces").and_then(Value::as_array);
                    context.namespaces = namespaces
                        .into_iter()
                        .flatten()
                        .filter_map(namespace)
                        .collect();
                }
                app => {
                    if let Some(text) = value.as_str() {
                        context.apps.push((app.to_string(), text.to_string()));
                    }
                }
            }
        }
    }
    history.sessions = sessions.into_values().collect();
    Ok(history)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ContextKind, Verdict};
    use crate::test_support::{OPENCODE_START, Workspace, opencode_database, prices};
    use serde_json::json;

    fn text(path: &std::path::Path) -> String {
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn a_database_yields_steps_calls_and_the_context_each_session_last_sent() {
        let w = Workspace::new();
        let file = text(&opencode_database(&w.home));
        let history = read(&file, &["profile".into()]).unwrap();
        // Two priced main steps and the subagent's; the running step and the user message
        // are no requests.
        assert_eq!(history.requests.len(), 3);
        let first = &history.requests[0];
        assert_eq!(first.time, OPENCODE_START + 1_000);
        assert_eq!(first.end_time, OPENCODE_START + 1_500);
        // OpenCode's input leaves out cache reads and writes, and its output reasoning.
        let u = &first.parts[0].usage;
        assert_eq!(
            (u.input, u.output, u.read, u.write),
            (Some(3000), Some(150), Some(0), Some(2000))
        );
        assert_eq!(first.effort.as_deref(), Some("high"));
        assert_eq!(first.provider.as_deref(), Some("opencode"));
        assert_eq!(first.agent, Agent::Main);
        let sub = &history.requests[2];
        assert_eq!(sub.agent, Agent::Subagent);
        assert_eq!(history.subagents.len(), 1);
        assert_eq!(history.subagents[0].kind, "explore");
        assert_eq!(
            history.subagents[0].parent.as_deref(),
            Some(&*identity("ses_main"))
        );
        let calls: Vec<&Use> = history.calls.iter().map(|c| &c.what).collect();
        assert_eq!(
            calls,
            [
                &Use::Skill("used-skill".into()),
                &Use::Mcp {
                    server: "t3-code".into(),
                    tool: "thread_list".into()
                }
            ]
        );
        let context = &history.contexts[&*identity("ses_main")];
        assert_eq!(
            context.instructions,
            [(
                "/work/repo/AGENTS.md".to_string(),
                "Instructions from: /work/repo/AGENTS.md\nBe brief.\n".to_string()
            )]
        );
        let (listing, skills) = context.skills.as_ref().unwrap();
        let ids: Vec<&str> = skills.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["used-skill", "idle-skill"]);
        assert!(skills.iter().all(|(_, entry)| listing.contains(entry)));
        assert_eq!(
            context.namespaces,
            [(
                "t3-code".to_string(),
                "- t3-code-0123456789abcdef (1 tool) // T3\n- thread_list".to_string()
            )]
        );
        // Nothing a person typed survives the read.
        assert!(!format!("{history:?}").contains("SYNTHETIC_PROMPT"));
    }

    #[test]
    fn opencode_1_sessions_are_read_once_beside_opencode_2s_imports() {
        let w = Workspace::new();
        let file = w.home.join("opencode.db");
        let db = Connection::open(&file).unwrap();
        db.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT NOT NULL,
               version TEXT NOT NULL, time_created INTEGER NOT NULL);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
               time_created INTEGER, time_updated INTEGER, data TEXT NOT NULL);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL,
               session_id TEXT NOT NULL, time_created INTEGER, time_updated INTEGER,
               data TEXT NOT NULL);
             CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
               type TEXT NOT NULL, seq INTEGER NOT NULL, time_created INTEGER NOT NULL,
               data TEXT NOT NULL);
             CREATE TABLE session_v2 (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT NOT NULL,
               version TEXT NOT NULL, time_created INTEGER NOT NULL);
             INSERT INTO session VALUES ('ses_a', NULL, '/work', '1.14.0', 1),
               ('ses_b', NULL, '/work', '1.14.0', 2);
             -- Mid-migration: OpenCode 2 has copied ses_a's session row, not ses_b's.
             INSERT INTO session_v2 VALUES ('ses_a', NULL, '/work', '2.2.0', 1);",
        )
        .unwrap();
        let tokens =
            json!({"input": 10, "output": 1, "reasoning": 0, "cache": {"read": 0, "write": 0}});
        let assistant = json!({"role": "assistant", "providerID": "opencode",
            "modelID": "zen-flash", "mode": "build"});
        let finish = json!({"type": "step-finish", "tokens": tokens}).to_string();
        for (message, session) in [("msg_a", "ses_a"), ("msg_b", "ses_b")] {
            db.execute(
                "INSERT INTO message VALUES (?1, ?2, 1, 1, ?3)",
                rusqlite::params![message, session, assistant.to_string()],
            )
            .unwrap();
        }
        for (part, message, session) in [
            ("prt_a1", "msg_a", "ses_a"),
            ("prt_b1", "msg_b", "ses_b"),
            ("prt_b2", "msg_b", "ses_b"),
        ] {
            db.execute(
                "INSERT INTO part VALUES (?1, ?2, ?3, 5, 5, ?4)",
                rusqlite::params![part, message, session, finish],
            )
            .unwrap();
        }
        // OpenCode 2 imported ses_a into its own table, so its OpenCode 1 step is read no more.
        let step =
            json!({"model": {"id": "zen-flash", "providerID": "opencode"}, "tokens": tokens});
        db.execute(
            "INSERT INTO session_message VALUES ('msg_a2', 'ses_a', 'assistant', 1, 3, ?1)",
            [step.to_string()],
        )
        .unwrap();
        drop(db);
        let history = read(&text(&file), &["profile".into()]).unwrap();
        let versions: Vec<Option<&str>> = history
            .sessions
            .iter()
            .map(|s| s.version.as_deref())
            .collect();
        assert_eq!(versions, [Some("2.2.0"), Some("1.14.0")]);
        let sessions: Vec<&str> = history.requests.iter().map(|r| &*r.session).collect();
        assert_eq!(
            sessions,
            [
                &*identity("ses_a"),
                &*identity("ses_b"),
                &*identity("ses_b")
            ]
        );
        // The two numbering schemes never collide.
        assert!(
            history.requests[1]
                .locations
                .iter()
                .all(|l| l.line > V1_LINES)
        );
    }

    #[test]
    fn an_edit_keeps_the_files_comments_and_layout() {
        let text =
            "{\n  // Mine.\n  \"permission\": {\n    \"skill\": { \"a\": \"deny\", },\n  },\n}\n";
        let edits = [
            Edit {
                path: vec!["permission".into(), "skill".into(), "b".into()],
                value: json!("deny"),
                op: Op::Set,
            },
            Edit {
                path: vec!["mcp".into(), "docs".into(), "enabled".into()],
                value: json!(false),
                op: Op::Set,
            },
            Edit {
                path: vec!["plugin".into()],
                value: json!("x"),
                op: Op::Append,
            },
        ];
        let removal = ["gone".to_string()];
        let edited = edit(text, &edits, &[&removal[..]]).unwrap();
        assert!(edited.contains("// Mine."), "{edited}");
        assert_eq!(
            parse_config(&edited).unwrap(),
            json!({
                "permission": {"skill": {"a": "deny", "b": "deny"}},
                "mcp": {"docs": {"enabled": false}},
                "plugin": ["x"],
            })
        );
        // Adding a member twice adds it once.
        assert_eq!(edit(&edited, &edits, &[]).unwrap(), edited);
        assert!(edit("{ \"mcp\": 1 }", &edits[1..2], &[]).is_err());
        assert!(edit("{ 'single': 1 }", &edits, &[]).is_err());
    }

    #[test]
    fn a_server_is_switched_off_in_the_shape_its_config_declares_it() {
        let v1 =
            json!({"mcp": {"timeout": 5, "docs": {"type": "local"}, "off": {"enabled": false}}});
        let v2 =
            json!({"mcp": {"servers": {"docs": {"type": "remote"}, "off": {"disabled": true}}}});
        let path = |config: &Value| -> Vec<Vec<String>> {
            let entries: Vec<String> = servers(config.as_object().unwrap())
                .iter()
                .filter(|s| s.enabled)
                .map(entry)
                .collect();
            declared(&entries)
                .iter()
                .map(|s| server_off(s).path)
                .collect()
        };
        assert_eq!(path(&v1), [["mcp", "docs", "enabled"]]);
        // OpenCode 2 ignores `enabled` under `mcp.servers`.
        assert_eq!(path(&v2), [["mcp", "servers", "docs", "disabled"]]);
        assert_eq!(
            switched_off(v2.as_object().unwrap()),
            [Control {
                path: vec![
                    "mcp".into(),
                    "servers".into(),
                    "off".into(),
                    "disabled".into()
                ],
                value: ControlValue::Bool(true),
                append: false,
            }]
        );
        assert_eq!(
            switched_off(v1.as_object().unwrap()),
            [server_off(&Server {
                name: "off".into(),
                enabled: false,
                path: vec!["mcp".into(), "off".into()],
            })]
        );
    }

    #[test]
    fn a_skill_picked_with_at_or_a_skill_message_is_a_use() {
        let w = Workspace::new();
        let file = opencode_database(&w.home);
        let db = Connection::open(&file).unwrap();
        let picked = json!({"text": "SYNTHETIC_PROMPT", "skills": [{"id": "picked"}]});
        let typed = json!({"skill": "typed", "name": "typed", "text": "SYNTHETIC_SKILL"});
        for (id, kind, data) in [("msg_u2", "user", picked), ("msg_s", "skill", typed)] {
            db.execute(
                "INSERT INTO session_message VALUES (?1, 'ses_main', ?2, 9, 1, ?3)",
                rusqlite::params![id, kind, data.to_string()],
            )
            .unwrap();
        }
        drop(db);
        let history = read(&text(&file), &["profile".into()]).unwrap();
        let selected: Vec<&Use> = history
            .calls
            .iter()
            .map(|c| &c.what)
            .filter(|u| matches!(u, Use::Selected(_)))
            .collect();
        assert_eq!(
            selected,
            [
                &Use::Selected("picked".into()),
                &Use::Selected("typed".into())
            ]
        );
        assert!(!format!("{history:?}").contains("SYNTHETIC_"));
    }

    #[test]
    fn opencode_2_rules_deny_a_skill_by_the_last_rule_naming_it() {
        let config = json!({"permissions": [
            {"action": "skill", "resource": "*", "effect": "allow"},
            {"action": "skill", "resource": "a", "effect": "deny"},
            {"action": "skill", "resource": "b", "effect": "deny"},
            {"action": "skill", "resource": "b", "effect": "allow"},
            {"action": "skill", "resource": "d", "effect": "deny"},
            {"action": "skill", "resource": "d*", "effect": "ask"},
        ]});
        // The last matching rule decides, a later wildcard's included.
        assert_eq!(
            switched_off(config.as_object().unwrap()),
            [skill_off("a", true)]
        );
        // The rule is appended after the file's own, where it wins, once.
        let off = skill_off("c", true);
        let edits = [Edit {
            path: off.path.clone(),
            value: crate::context::json_value(&off.value),
            op: Op::Append,
        }];
        let text = "{\n  // Mine.\n  \"permissions\": [\n    { \"action\": \"skill\", \"resource\": \"*\", \"effect\": \"allow\" },\n  ],\n}\n";
        let edited = edit(text, &edits, &[]).unwrap();
        assert!(edited.contains("// Mine."), "{edited}");
        let rules = parse_config(&edited).unwrap()["permissions"].clone();
        assert_eq!(
            rules[1],
            json!({"action": "skill", "resource": "c", "effect": "deny"})
        );
        assert_eq!(edit(&edited, &edits, &[]).unwrap(), edited);
        // A wildcard action's allow outranks the denial too.
        let any_action = "{ \"permissions\": [{ \"action\": \"skill\", \"resource\": \"c\", \"effect\": \"deny\" }, { \"action\": \"*\", \"resource\": \"?\", \"effect\": \"allow\" }] }";
        let rules = parse_config(&edit(any_action, &edits, &[]).unwrap()).unwrap();
        assert_eq!(rules["permissions"].as_array().unwrap().len(), 3);
        assert!(glob("do?s", "docs") && glob("*", "") && !glob("do?s", "dos"));
        // A later wildcard allow outranks the denial, so it is appended again.
        let wildcard = "{ \"permissions\": [{ \"action\": \"skill\", \"resource\": \"c\", \"effect\": \"deny\" }, { \"action\": \"skill\", \"resource\": \"c*\", \"effect\": \"allow\" }] }";
        let rules = parse_config(&edit(wildcard, &edits, &[]).unwrap()).unwrap();
        assert_eq!(rules["permissions"].as_array().unwrap().len(), 3);
        // A denial an allow follows is no longer in force, so it is appended again.
        let allowed = "{ \"permissions\": [{ \"action\": \"skill\", \"resource\": \"c\", \"effect\": \"deny\" }, { \"action\": \"skill\", \"resource\": \"c\", \"effect\": \"allow\" }] }";
        let rules = parse_config(&edit(allowed, &edits, &[]).unwrap()).unwrap();
        assert_eq!(rules["permissions"].as_array().unwrap().len(), 3);
        assert_eq!(skill_off("c", false).path, ["permission", "skill", "c"]);
    }

    #[test]
    fn a_rule_read_after_the_settings_file_leaves_skills_to_the_person() {
        let layer = |file: &str, config: Value| Layer {
            file: file.into(),
            config: config.as_object().unwrap().clone(),
        };
        let allow = json!([{"action": "skill", "resource": "*", "effect": "allow"}]);
        let global = layer("/g/opencode.jsonc", json!({"permissions": allow.clone()}));
        assert!(!skills_reallowed(
            std::slice::from_ref(&global),
            &global.file
        ));
        let custom = layer("/c.json", json!({"permissions": allow.clone()}));
        assert!(skills_reallowed(&[global.clone(), custom], &global.file));
        let agent = layer(
            "/g/opencode.jsonc",
            json!({"agents": {"build": {"permissions": allow}}}),
        );
        assert!(skills_reallowed(&[agent], &global.file));
    }

    #[test]
    fn opencode_2_agents_name_their_variant_after_the_model() {
        let layers = [Layer {
            file: "/g/opencode.json".into(),
            config: json!({"model": "openrouter/vendor/zen-flash#low",
                    "agent": {"plan": {"model": "openai/m1", "variant": "high"}},
                    "agents": {"explore": {"model": "opencode/zen-flash#max"},
                        "review": {"model": {"providerID": "openai", "model": "m2",
                            "variant": "low"}}}})
            .as_object()
            .unwrap()
            .clone(),
        }];
        let controls = agent_controls(&layers);
        assert_eq!(controls.session.unwrap().value, "vendor/zen-flash");
        let found: Vec<[Option<&str>; 4]> = controls
            .definitions
            .iter()
            .map(|d| {
                [
                    Some(d.name.as_str()),
                    d.model.as_deref(),
                    d.effort.as_deref(),
                    d.key.as_deref(),
                ]
            })
            .collect();
        assert_eq!(
            found,
            [
                [Some("plan"), Some("m1"), Some("high"), Some("agent")],
                [
                    Some("explore"),
                    Some("zen-flash"),
                    Some("max"),
                    Some("agents")
                ],
                [Some("review"), Some("m2"), Some("low"), Some("agents")],
            ]
        );
    }

    #[test]
    fn opencode_db_names_the_database_a_profile_reads() {
        let w = Workspace::new();
        let data = w.home.join(".local/share/opencode");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("other.db"), "").unwrap();
        let canonical = realpath(&text(&data)).unwrap();
        let histories = |database: &str| {
            let mut options = w.scan_options().discovery;
            options.env.insert("OPENCODE_DB".into(), database.into());
            profile(&options, canonical.clone(), canonical.clone(), "k")
                .unwrap()
                .histories
        };
        assert_eq!(
            histories("other.db"),
            [realpath(&text(&data.join("other.db"))).unwrap()]
        );
        assert!(histories(":memory:").is_empty());
    }

    #[test]
    fn a_later_project_layer_replaces_a_server_whole() {
        let layer = |file: &str, config: Value| Layer {
            file: file.into(),
            config: config.as_object().unwrap().clone(),
        };
        let layers = [
            layer(
                "/r/opencode.json",
                json!({"mcp": {"servers": {"docs": {"type": "remote", "disabled": true}}}}),
            ),
            layer(
                "/r/opencode.jsonc",
                json!({"mcp": {"servers": {"docs": {"type": "remote"}}}}),
            ),
        ];
        let found: Vec<(String, bool)> = project_servers(&layers)
            .into_iter()
            .map(|s| (s.name, s.enabled))
            .collect();
        assert_eq!(found, [("docs".to_string(), true)]);
    }

    #[test]
    fn a_server_a_project_redeclares_keeps_no_global_switch() {
        let w = Workspace::new();
        let file = opencode_database(&w.home);
        let repo = w.home.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(
            repo.join("opencode.json"),
            r#"{"mcp": {"servers": {"docs": {"type": "remote", "url": "https://p"}, "local": {"type": "remote", "url": "https://l"}}}}"#,
        )
        .unwrap();
        let db = Connection::open(&file).unwrap();
        db.execute(
            "INSERT INTO session_v2 VALUES ('ses_repo', NULL, ?1, '2.2.0', 1)",
            [text(&repo)],
        )
        .unwrap();
        // `docs_x_find` is `docs_x`'s outside the repository, and could be `docs`'s too in it.
        let step = |id: &str| {
            json!({"model": {"id": "zen-flash", "providerID": "opencode"},
                "time": {"created": OPENCODE_START + 9_000},
                "tokens": {"input": 1, "output": 1, "reasoning": 0, "cache": {"read": 0, "write": 0}},
                "content": [{"type": "tool", "id": id, "name": "docs_x_find",
                    "state": {"status": "completed"}}]})
            .to_string()
        };
        // A project that switches `docs` off leaves `docs_x` the only owner.
        let off = w.home.join("off");
        std::fs::create_dir_all(off.join(".git")).unwrap();
        std::fs::write(
            off.join("opencode.json"),
            r#"{"mcp": {"servers": {"docs": {"type": "remote", "url": "https://p", "disabled": true}}}}"#,
        )
        .unwrap();
        db.execute(
            "INSERT INTO session_v2 VALUES ('ses_off', NULL, ?1, '2.2.0', 1)",
            [text(&off)],
        )
        .unwrap();
        for (id, session) in [
            ("msg_outside", "ses_main"),
            ("msg_inside", "ses_repo"),
            ("msg_off", "ses_off"),
        ] {
            db.execute(
                "INSERT INTO session_message VALUES (?1, ?2, 'assistant', 9, ?3, ?4)",
                rusqlite::params![id, session, OPENCODE_START + 9_000, step(id)],
            )
            .unwrap();
        }
        drop(db);
        let config = w.home.join(".config/opencode/opencode.json");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            &config,
            r#"{"mcp": {"servers": {"local": {"type": "remote", "url": "https://g"}, "docs_x": {"type": "remote", "url": "https://x"}, "kept": {"type": "remote", "url": "https://k"}}}}"#,
        )
        .unwrap();
        let mut options = w.scan_options();
        // OpenCode's data directory under an XDG variable that names no OpenCode directory
        // is a default one, skipped.
        options
            .discovery
            .env
            .insert("XDG_DATA_HOME".into(), text(&w.home.join("missing")));
        let scan = crate::reader::scan(&options).unwrap();
        let profile = scan
            .profiles
            .iter()
            .find(|p| p.harness == Harness::OpenCode)
            .unwrap();
        let names: Vec<String> = declared(&profile.mcp_servers)
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, ["docs_x", "kept"]);
        let calls: Vec<(&str, u64)> = scan
            .exposure
            .tool_calls
            .iter()
            .map(|c| (c.name.as_str(), c.calls))
            .filter(|(name, _)| name.contains("docs"))
            .collect();
        assert_eq!(calls, [("mcp__docs_x__find", 2)]);
        // A project whose settings cannot be read leaves OpenCode's switches to the person
        // and says why, without failing the scan.
        let broken = w.home.join("broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("opencode.json"), "{ nope").unwrap();
        let db = Connection::open(&file).unwrap();
        db.execute(
            "INSERT INTO session_v2 VALUES ('ses_broken', NULL, ?1, '2.2.0', 1)",
            [text(&broken)],
        )
        .unwrap();
        drop(db);
        let scan = crate::reader::scan(&options).unwrap();
        let profile = scan
            .profiles
            .iter()
            .find(|p| p.harness == Harness::OpenCode)
            .unwrap();
        assert!(profile.mcp_servers.is_empty());
        assert!(profile.settings.contains_key(REALLOWED));
        assert!(
            scan.coverage
                .issues
                .iter()
                .any(|i| i.contains("could not be read"))
        );
    }

    #[test]
    fn a_scan_prices_opencode_and_judges_its_skills_by_their_calls() {
        let w = Workspace::new();
        opencode_database(&w.home);
        let config = w.home.join(".config/opencode/opencode.jsonc");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            &config,
            "{\n  // Synthetic.\n  \"model\": \"opencode/zen-flash\",\n}\n",
        )
        .unwrap();
        let scan = crate::reader::scan(&w.scan_options()).unwrap();
        let profile = scan
            .profiles
            .iter()
            .find(|p| p.harness == Harness::OpenCode)
            .unwrap();
        assert_eq!(profile.settings_file, text(&config));
        assert_eq!(scan.requests.len(), 3);
        let catalog = prices();
        let first = crate::prices::price_request(&catalog, &scan.requests[0]);
        // 1,000 ordinary input, 2,000 written at the catalog's write rate, and 150 output.
        let want = (1000.0 * 1.0 + 2000.0 * 1.25 + 150.0 * 4.0) / 1e6;
        assert!((first.total_usd.unwrap().to_f64() - want).abs() < 1e-9);
        let report = crate::replay::analyze(crate::replay::AnalyzeOptions {
            scan,
            catalog: &catalog,
            since: None,
            now: OPENCODE_START + 60_000,
            max_cost_per_use_usd: 1.0,
            reading: None,
        })
        .unwrap();
        let row = |name: &str| {
            report
                .context
                .iter()
                .flat_map(|c| std::iter::once(c).chain(&c.details))
                .find(|c| c.name == name)
                .unwrap_or_else(|| panic!("no {name} row"))
        };
        let idle = row("idle-skill");
        assert_eq!(idle.kind, ContextKind::Skill);
        assert_eq!(idle.calls, Some(0));
        assert_eq!(idle.verdict, Some(Verdict::Off));
        assert_eq!(
            idle.control.as_ref().unwrap().path,
            ["permission", "skill", "idle-skill"]
        );
        let used = row("used-skill");
        assert_eq!(used.calls, Some(1));
        assert_eq!(used.verdict, Some(Verdict::Keep));
        assert_eq!(row("t3-code").calls, Some(1));
        assert_eq!(row("/work/repo/AGENTS.md").kind, ContextKind::Instructions);
        assert!(report.context.iter().all(|c| c.priced > 0));
    }
}
