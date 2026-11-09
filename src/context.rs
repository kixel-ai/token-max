//! What each piece of always-sent context cost, and the setting that turns it off.

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::exposure::{calibrate, codex_server, cohort};
use crate::model::{
    Category, Component, ContextCost, ContextKind, Control, ControlValue, ExposureScan, Harness,
    InvocationKind, InvocationStatus, Linkage, Millis, Profile, Request, ResultTotals, Text,
    Verdict, Via,
};
use crate::prices::{Catalog, prefix_spans};
use crate::safe::identity;

/// The settings fragment for one control: settings.json's `"enableArtifact": false`,
/// or config.toml's `features.apps = false`.
pub fn control_text(control: &Control, harness: Harness) -> String {
    if harness == Harness::Codex {
        let path: Vec<String> = control.path.iter().map(|key| toml_key(key)).collect();
        let value = match &control.value {
            ControlValue::Text(text) => toml::Value::String(text.clone()),
            ControlValue::Bool(b) => toml::Value::Boolean(*b),
        };
        let value = if control.append {
            toml::Value::Array(vec![value])
        } else {
            value
        };
        return format!("{} = {value}", path.join("."));
    }
    let mut value = json_value(&control.value);
    if control.append {
        value = Value::Array(vec![value]);
    }
    for key in control.path.iter().skip(1).rev() {
        let mut map = Map::new();
        map.insert(key.clone(), value);
        value = Value::Object(map);
    }
    let mut pretty = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
    value
        .serialize(&mut serde_json::Serializer::with_formatter(
            &mut pretty,
            formatter,
        ))
        .expect("JSON values serialize");
    let pretty = String::from_utf8(pretty).expect("JSON is UTF-8");
    // One line: each newline and its indentation become a single space, then
    // brackets close up. Like the TypeScript, this also applies inside strings.
    let mut flat = String::with_capacity(pretty.len());
    let mut chars = pretty.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\n' {
            while chars.next_if(|c| c.is_whitespace()).is_some() {}
            flat.push(' ');
        } else {
            flat.push(c);
        }
    }
    let flat = flat
        .replace("[ ", "[")
        .replace(" ]", "]")
        .replace("{ ", "{")
        .replace(" }", "}");
    format!(
        "{}: {flat}",
        serde_json::to_string(&control.path[0]).expect("strings serialize")
    )
}

/// Merge controls into one settings.json object, adding to lists rather than replacing them.
pub fn settings_patch<'a>(controls: impl IntoIterator<Item = &'a Control>) -> Map<String, Value> {
    let mut root = Map::new();
    for c in controls {
        let Some((last, parents)) = c.path.split_last() else {
            continue;
        };
        let mut node = &mut root;
        for key in parents {
            let child = node
                .entry(key.clone())
                .or_insert_with(|| Value::Object(Map::new()));
            // Controls never nest under a scalar; replacing one keeps the patch an object.
            if !child.is_object() {
                *child = Value::Object(Map::new());
            }
            node = child.as_object_mut().expect("just made an object");
        }
        let value = json_value(&c.value);
        if !c.append {
            node.insert(last.clone(), value);
            continue;
        }
        let list = node
            .entry(last.clone())
            .or_insert_with(|| Value::Array(vec![]));
        if !list.is_array() {
            *list = Value::Array(vec![]);
        }
        let list = list.as_array_mut().expect("just made an array");
        if !list.contains(&value) {
            list.push(value);
        }
    }
    root
}

/// A TOML key, quoted unless it is bare.
fn toml_key(key: &str) -> String {
    let bare = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if bare {
        key.into()
    } else {
        toml::Value::String(key.into()).to_string()
    }
}

/// Merge controls into one config.toml fragment, as `settings_patch` does for settings.json.
pub fn config_patch<'a>(controls: impl IntoIterator<Item = &'a Control>) -> String {
    toml::to_string(&Value::Object(settings_patch(controls))).expect("controls are TOML tables")
}

fn json_value(value: &ControlValue) -> Value {
    match value {
        ControlValue::Text(text) => Value::String(text.clone()),
        ControlValue::Bool(b) => Value::Bool(*b),
    }
}

/// Inputs for pricing every piece of recurring context.
pub struct ContextInput<'a> {
    /// The recorded request shapes and uses.
    pub scan: &'a ExposureScan,
    /// The selected requests whose usage prices those shapes.
    pub requests: &'a [Request],
    /// The selected profiles, whose settings files name the switches.
    pub profiles: &'a [Profile],
    /// Public prices.
    pub catalog: &'a Catalog,
    /// Recommend turning a piece off when it cost more than this per use.
    pub max_cost_per_use_usd: f64,
}

/// Context setting rows whose controls are not covered by a server recommendation.
pub fn setting_rows(costs: &[ContextCost]) -> impl Iterator<Item = &ContextCost> {
    costs.iter().flat_map(|c| {
        std::iter::once(c).chain(c.details.iter().filter(|_| c.verdict != Some(Verdict::Off)))
    })
}

/// OpenAI's rule of thumb for English text, used where no request records the
/// complete text to calibrate against, as with every Codex request.
/// <https://help.openai.com/en/articles/4936856-what-are-tokens-and-how-to-count-them>
pub const CODEX_TOKENS_PER_CHARACTER: f64 = 0.25;

/// What a piece is, shared by every revision that produces it.
struct Kind {
    kind: ContextKind,
    name: String,
    control: Option<Control>,
    parent: Option<usize>,
}

/// Where a piece's history says it sits in the request.
#[derive(Clone, Copy)]
enum History<'a> {
    /// A component revision, carried when the session's previous request had the same one.
    Revision(&'a str),
    /// Tool output the previous request already held.
    Carried,
    /// Tool output returned since the previous request.
    New,
}

/// A piece as it appears in one request.
struct Placed<'a> {
    /// Index into the interned piece keys.
    key: usize,
    characters: i64,
    /// Tools and the system prompt lead the request in recorded order; everything else rides in messages.
    prefix: bool,
    history: History<'a>,
    /// Images or other input whose tokens characters cannot measure.
    opaque: bool,
}

/// Pieces by key, interned so each request compares indices rather than strings.
#[derive(Default)]
struct Pieces<'a> {
    ids: HashMap<String, usize>,
    kinds: Vec<Kind>,
    /// By component and owning profile set, since a Codex server's switch
    /// depends on the profiles that declare it.
    of: HashMap<(&'a str, usize), Vec<Split>>,
    /// The row a skill invocation counts for, by hashed `skill:<name>` target. The
    /// scan already resolved a short name to the synced skill it reached.
    skills: HashMap<String, usize>,
}

/// One component's share of a piece: its characters, or None when only its
/// identity is recorded.
type Split = (usize, Option<i64>);

impl Pieces<'_> {
    fn intern(
        &mut self,
        key: String,
        kind: ContextKind,
        name: String,
        control: Option<Control>,
    ) -> usize {
        let next = self.kinds.len();
        *self.ids.entry(key).or_insert_with(|| {
            self.kinds.push(Kind {
                kind,
                name,
                control,
                parent: None,
            });
            next
        })
    }
}

// Claude Code cannot do its basic work without these, so they have no off switch here.
const CORE_TOOLS: [&str; 7] = [
    "Bash",
    "Read",
    "Edit",
    "Write",
    "Skill",
    "ToolSearch",
    "DeferredToolPlaceholder",
];

/// The MCP server whose tools Codex lists as ChatGPT apps, all switched by `features.apps`.
const CODEX_APPS: &str = "codex_apps";

fn deny(rule: String) -> Option<Control> {
    Some(Control {
        path: vec!["permissions".into(), "deny".into()],
        value: ControlValue::Text(rule),
        append: true,
    })
}

fn disable(path: &[&str]) -> Option<Control> {
    Some(Control {
        path: path.iter().map(|p| p.to_string()).collect(),
        value: ControlValue::Bool(false),
        append: false,
    })
}

/// The report name for skills switched together by `syncClaudeAiSkills`.
pub const SYNCED_SKILLS: &str = "claude.ai synced skills";

const SYNCED_NAMESPACE: &str = "anthropic-skills";
const SYNCED_SKILLS_KEY: &str = "setting:syncClaudeAiSkills";

/// The row and setting for one skill listing entry. `skillOverrides` hides a local or
/// bundled skill from the model and keeps `/name` working, but it does not apply to
/// plugin skills, and synced skills only turn off together through `syncClaudeAiSkills`.
fn skill(member: &str) -> (String, String, Option<Control>) {
    if synced_short(member).is_some() {
        return (
            SYNCED_SKILLS_KEY.into(),
            SYNCED_SKILLS.into(),
            disable(&["syncClaudeAiSkills"]),
        );
    }
    let key = format!("skill:{member}");
    // A plugin's skill, or an added directory's same-named one: no settings key turns
    // off just that skill.
    if member.contains(':') {
        return (key, member.into(), None);
    }
    let control = Control {
        path: vec!["skillOverrides".into(), member.into()],
        value: ControlValue::Text("user-invocable-only".into()),
        append: false,
    };
    (key, member.into(), Some(control))
}

/// The short name of a skill synced from claude.ai, such as `pdf` for `anthropic-skills:pdf`.
pub fn synced_short(name: &str) -> Option<&str> {
    name.strip_prefix(SYNCED_NAMESPACE)?.strip_prefix(':')
}

/// Whether a control hides a skill from the model only, leaving typed uses working.
pub fn typed_only(control: Option<&Control>) -> bool {
    control.is_some_and(|c| c.path.first().is_some_and(|p| p == "skillOverrides"))
}

/// The MCP server a tool belongs to: the text between the first and second `__`.
fn mcp_server(name: &str) -> &str {
    name.split("__").nth(1).unwrap_or(name)
}

/// The setting that owns a Claude tool, grouping a tool family under one switch.
fn tool(name: &str) -> (String, ContextKind, String, Option<Control>) {
    if name.starts_with("mcp__") {
        let server = mcp_server(name);
        return (
            format!("mcp:{server}"),
            ContextKind::McpServer,
            server.to_string(),
            deny(format!("mcp__{server}")),
        );
    }
    // The Artifact tool's helpers (ArtifactComments, ArtifactData) go with it.
    if name.starts_with("Artifact") {
        return (
            "tool:Artifact".into(),
            ContextKind::Tool,
            "Artifact".into(),
            disable(&["enableArtifact"]),
        );
    }
    if name == "Workflow" {
        return (
            "tool:Workflow".into(),
            ContextKind::Tool,
            name.into(),
            disable(&["enableWorkflows"]),
        );
    }
    let control = if CORE_TOOLS.contains(&name) {
        None
    } else {
        deny(name.to_string())
    };
    (
        format!("tool:{name}"),
        ContextKind::Tool,
        name.into(),
        control,
    )
}

/// The config.toml switch that owns a Codex tool. Built-in tools have none; an MCP
/// server has one only where a selected config.toml declares it, since a server
/// a plugin or launcher adds is switched where it is added. Ids that differ only
/// in punctuation expose the same server name, so the piece is keyed by the
/// declaring id. A request copied into several profiles gets a switch only when
/// each of them declares the server under the same single id.
fn codex_tool(name: &str, servers: &[&[String]]) -> (String, ContextKind, String, Option<Control>) {
    let Some(rest) = name.strip_prefix("mcp__") else {
        return (
            format!("codex:tool:{name}"),
            ContextKind::Tool,
            name.into(),
            None,
        );
    };
    let server = rest.split("__").next().unwrap_or(rest);
    let mut key = format!("codex:mcp:{server}");
    let control = if server == CODEX_APPS {
        disable(&["features", "apps"])
    } else {
        let declared: Vec<Vec<&String>> = servers
            .iter()
            .map(|ids| ids.iter().filter(|id| codex_server(id) == server).collect())
            .collect();
        match declared.first().map(Vec::as_slice) {
            Some(&[id]) if declared.iter().all(|d| d[..] == [id]) => {
                key = format!("{key}@{id}");
                disable(&["mcp_servers", id, "enabled"])
            }
            _ => None,
        }
    };
    (key, ContextKind::McpServer, server.into(), control)
}

/// Split one Claude component into the pieces a user can switch separately.
fn pieces(
    component: &Component,
    components: &HashMap<&str, &Component>,
    interned: &mut Pieces<'_>,
) -> Vec<(usize, i64)> {
    let Some(characters) = component.size.characters else {
        return vec![];
    };
    let name = component.name.as_str();
    let mut one = |key: String, kind: ContextKind, name: String, control: Option<Control>| {
        vec![(interned.intern(key, kind, name, control), characters)]
    };
    match component.category {
        Category::ToolSchema | Category::McpSchema | Category::McpDiscovery => {
            let (key, kind, name, control) = tool(name);
            one(key, kind, name, control)
        }
        Category::SkillListing => {
            let members: Vec<(&str, i64)> = component
                .members
                .iter()
                .filter_map(|id| {
                    let m = components
                        .get(id.as_str())
                        .expect("listing members are recorded components");
                    Some((m.name.as_str(), m.size.characters?))
                })
                .collect();
            let listed: i64 = members.iter().map(|(_, n)| n).sum();
            let mut result: Vec<(usize, i64)> = members
                .into_iter()
                .map(|(member, n)| {
                    let (key, name, control) = skill(member);
                    let id = interned.intern(key, ContextKind::Skill, name, control);
                    interned
                        .skills
                        .insert(identity(&format!("skill:{member}")), id);
                    (id, n)
                })
                .collect();
            let framing = interned.intern(
                "other:skill-list".into(),
                ContextKind::Other,
                "Skill list framing".into(),
                None,
            );
            result.push((framing, (characters - listed).max(0)));
            result
        }
        Category::Instructions => one(
            format!("instructions:{name}"),
            ContextKind::Instructions,
            name.into(),
            None,
        ),
        Category::PluginInstructions => {
            // Each UTF-16 unit outside the allowed set becomes one underscore, as in JavaScript.
            let mut server = String::with_capacity(name.len());
            for c in name.chars() {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    server.push(c);
                } else {
                    server.extend(std::iter::repeat_n('_', c.len_utf16()));
                }
            }
            one(
                format!("mcp:{server}"),
                ContextKind::McpServer,
                server.clone(),
                deny(format!("mcp__{server}")),
            )
        }
        Category::Agents => one(
            "other:agents".into(),
            ContextKind::Other,
            "Subagent list".into(),
            None,
        ),
        // Other attachments are one-off conversation content, not recurring context.
        // A prompt snapshot repeats the system prompt the request shape already gives.
        Category::Other if name == "wire-system" => one(
            "system".into(),
            ContextKind::System,
            "Claude Code system prompt".into(),
            None,
        ),
        Category::Other => vec![],
        // Loaded on use, so they are the cost of using a feature, not of having it on.
        Category::SkillBody | Category::McpResult => vec![],
        category => one(
            format!("other:{}", category.as_str()),
            ContextKind::Other,
            category.as_str().into(),
            None,
        ),
    }
}

/// Split one Codex world-state or provided-tool component into switchable pieces.
fn codex_pieces(
    component: &Component,
    servers: &[&[String]],
    interned: &mut Pieces<'_>,
) -> Vec<Split> {
    let characters = component.size.characters;
    let (key, kind, name, control) = match component.category {
        Category::ToolSchema | Category::McpSchema | Category::McpDiscovery => {
            codex_tool(&component.name, servers)
        }
        // The skills check owns per-skill findings, so the catalog is one piece here.
        Category::SkillListing => (
            "codex:other:skill-list".into(),
            ContextKind::Other,
            "Skill list".into(),
            None,
        ),
        Category::Instructions => (
            "codex:instructions".into(),
            ContextKind::Instructions,
            "AGENTS.md".into(),
            None,
        ),
        Category::ManagedInstructions => (
            "codex:other:managed".into(),
            ContextKind::Other,
            "Managed developer instructions".into(),
            None,
        ),
        _ => return vec![],
    };
    vec![(interned.intern(key, kind, name, control), characters)]
}

/// The piece for one tool's output, filed under the tool, or its MCP server.
fn output_piece(owner: &str, harness: Harness, interned: &mut Pieces<'_>) -> usize {
    let prefix = if harness == Harness::Codex {
        "codex:"
    } else {
        ""
    };
    interned.intern(
        format!("{prefix}output:{owner}"),
        ContextKind::ToolOutput,
        owner.into(),
        None,
    )
}

/// Price a schema once at its actual position, crediting its server total too.
fn mcp_detail(component: &Component, parent: usize, interned: &mut Pieces<'_>) -> usize {
    let server = &interned.kinds[parent];
    if server.kind != ContextKind::McpServer {
        return parent;
    }
    let (kind, name, control) = if component.category == Category::McpDiscovery {
        (ContextKind::McpDiscovery, component.name.clone(), None)
    } else if matches!(
        component.category,
        Category::ToolSchema | Category::McpSchema
    ) && let Some((_, tool)) = component
        .name
        .strip_prefix("mcp__")
        .and_then(|n| n.split_once("__"))
        && !tool.is_empty()
    {
        let control = match server.control.as_ref() {
            Some(c) if c.path.first().is_some_and(|p| p == "mcp_servers") => Some(Control {
                path: vec![
                    "mcp_servers".into(),
                    c.path[1].clone(),
                    "disabled_tools".into(),
                ],
                value: ControlValue::Text(tool.into()),
                append: true,
            }),
            Some(c) if c.path.first().is_some_and(|p| p == "permissions") => {
                deny(component.name.clone())
            }
            _ => None,
        };
        (ContextKind::McpTool, component.name.clone(), control)
    } else {
        return parent;
    };
    let key = interned.intern(
        format!("detail:{parent}:{kind:?}:{name}"),
        kind,
        name,
        control,
    );
    interned.kinds[key].parent = Some(parent);
    key
}

/// The previous request of a session, which bounds where its attachments sit in the next one.
struct Prior<'a> {
    input: Option<i64>,
    /// Where the content it carried ends.
    content_end: Option<i64>,
    /// Reply tokens that stay in the conversation: output less reasoning, which a
    /// harness may drop, and zero where reasoning is not counted apart.
    reply: i64,
    keys: HashSet<(&'a str, usize)>,
    /// The tokenizer and the leading tools and system prompt by revision.
    prefix: (String, Vec<&'a Text>),
    results: Option<&'a ResultTotals>,
}

struct Row {
    cost: ContextCost,
    owner: usize,
    key: usize,
    time: Millis,
}

/// Price each piece of context in every selected request that recorded its
/// shape. A piece is charged at the cache state of its position in the prompt:
/// Claude's tools and system prompt lead in recorded order. Message attachments,
/// Codex's context and tool output have no recorded offset, so each takes the
/// latest position its history allows: the end of the session's previous request
/// when that request carried it too, else the end of this request, where newly
/// added content goes.
pub fn context_costs(input: &ContextInput<'_>) -> Vec<ContextCost> {
    let ContextInput { scan, catalog, .. } = *input;
    let calibration: HashMap<String, f64> = calibrate(scan, input.requests)
        .into_iter()
        .map(|c| (c.cohort, c.tokens_per_character))
        .collect();
    let requests: HashMap<&str, &Request> =
        input.requests.iter().map(|r| (r.id.as_str(), r)).collect();
    let components: HashMap<&str, &Component> =
        scan.components.iter().map(|c| (c.id.as_str(), c)).collect();
    let mut interned = Pieces::default();
    let mut owners: HashMap<Vec<String>, usize> = HashMap::new();
    let mut owner_profiles: Vec<Vec<String>> = vec![];
    let mut rows: IndexMap<(usize, usize), Row> = IndexMap::new();
    let mut latest: HashMap<usize, (Millis, Vec<usize>)> = HashMap::new();
    let mut previous: HashMap<&str, Prior<'_>> = HashMap::new();
    let mut ordered: Vec<_> = scan
        .requests
        .iter()
        .filter_map(|exposure| Some((exposure, *requests.get(exposure.request.as_str())?)))
        .collect();
    ordered.sort_by_key(|(_, r)| r.time);
    for (exposure, r) in ordered {
        let mut profiles = r.profiles.clone();
        profiles.sort();
        let owner = match owners.get(&profiles) {
            Some(&id) => id,
            None => {
                let id = owner_profiles.len();
                owners.insert(profiles.clone(), id);
                owner_profiles.push(profiles);
                id
            }
        };
        let view = &exposure.view;
        let codex = r.harness == Harness::Codex;
        // Claude's recorded request shapes are exact. Codex records each change to
        // the context it sends, so its chronological state is what the model saw.
        // Any other request still prices its tool output, but cannot say what else it sent.
        let (shown, shaped): (Vec<&Text>, bool) = match (r.harness, view.linkage) {
            (Harness::Claude, Linkage::Wire) => (view.components.iter().collect(), true),
            (Harness::Codex, Linkage::Chronological) => (view.candidates.iter().collect(), true),
            _ => (vec![], false),
        };
        // The MCP servers each of this request's own Codex profiles declares.
        let servers: Vec<&[String]> = input
            .profiles
            .iter()
            .filter(|p| p.harness == Harness::Codex && r.profiles.contains(&p.id))
            .map(|p| p.mcp_servers.as_slice())
            .collect();
        let component = |id: &str| {
            *components
                .get(id)
                .expect("exposures reference recorded components")
        };
        // Codex names its top-level tools by an opaque identity; a provided
        // schema of the same tool already measures it.
        let measured: HashSet<&str> = shown
            .iter()
            .map(|id| component(id))
            .filter(|c| {
                c.size.characters.is_some()
                    && matches!(c.category, Category::ToolSchema | Category::McpSchema)
            })
            .map(|c| c.name.as_str())
            .collect();
        let mut parts: Vec<Placed<'_>> = vec![];
        for (index, id) in shown.iter().enumerate() {
            let id: &str = id;
            let component = component(id);
            if codex
                && component.size.characters.is_none()
                && measured.contains(component.name.as_str())
            {
                continue;
            }
            if !interned.of.contains_key(&(id, owner)) {
                let mut split = if codex {
                    codex_pieces(component, &servers, &mut interned)
                } else {
                    pieces(component, &components, &mut interned)
                        .into_iter()
                        .map(|(key, characters)| (key, Some(characters)))
                        .collect()
                };
                for (key, _) in &mut split {
                    *key = mcp_detail(component, *key, &mut interned);
                }
                interned.of.insert((id, owner), split);
            }
            let prefix = !codex && index < view.prefix;
            parts.extend(
                interned.of[&(id, owner)]
                    .iter()
                    .map(|&(key, characters)| Placed {
                        key,
                        characters: characters.unwrap_or(0),
                        prefix,
                        history: History::Revision(id),
                        opaque: characters.is_none(),
                    }),
            );
        }
        let prior_results = previous
            .get(r.session.as_str())
            .and_then(|p| p.results)
            .filter(|p| {
                exposure
                    .results
                    .as_ref()
                    .is_some_and(|r| r.epoch == p.epoch)
            });
        for (owner_name, characters, opaque) in exposure.results.iter().flat_map(|r| &r.tools) {
            let key = output_piece(owner_name, r.harness, &mut interned);
            let carried = prior_results
                .and_then(|p| p.tools.iter().find(|(t, _, _)| t == owner_name))
                .map_or(0, |(_, n, _)| (*n).min(*characters));
            for (history, characters) in [
                (History::Carried, carried),
                (History::New, characters - carried),
            ] {
                parts.push(Placed {
                    key,
                    characters,
                    prefix: false,
                    history,
                    opaque: *opaque,
                });
            }
        }
        let p = if r.parts.len() == 1 {
            Some(&r.parts[0])
        } else {
            None
        };
        let u = p.map(|p| &p.usage);
        let tokenizer = cohort(r);
        let ratio = calibration
            .get(&tokenizer)
            .copied()
            .or(codex.then_some(CODEX_TOKENS_PER_CHARACTER));
        let spans = p
            .filter(|_| ratio.is_some())
            .and_then(|p| prefix_spans(catalog, r, p));
        let size = |piece: &Placed<'_>| piece.characters as f64 * ratio.unwrap_or(0.0);
        let total: f64 = parts.iter().map(size).sum();
        // A calibration that overshoots the recorded input cannot place these pieces.
        let end = u.and_then(|u| u.input);
        let placed = spans.filter(|_| end.is_some_and(|end| total <= end as f64));
        let mut unique: Vec<usize> = vec![];
        {
            let mut seen = HashSet::new();
            for piece in &parts {
                for key in std::iter::once(piece.key).chain(interned.kinds[piece.key].parent) {
                    if seen.insert(key) {
                        unique.push(key);
                    }
                }
            }
        }
        for &key in &unique {
            let row = rows.entry((owner, key)).or_insert_with(|| {
                let kind = &interned.kinds[key];
                Row {
                    cost: ContextCost {
                        harness: r.harness,
                        profiles: owner_profiles[owner].clone(),
                        kind: kind.kind,
                        name: kind.name.clone(),
                        control: kind.control.clone(),
                        requests: 0,
                        priced: 0,
                        tokens: None,
                        cost_usd: 0.0,
                        estimated_usd: None,
                        calls: None,
                        last_used: None,
                        current: false,
                        cost_per_use_usd: None,
                        verdict: None,
                        largest_characters: None,
                        details: vec![],
                        instruction_size: None,
                    },
                    owner,
                    key,
                    time: Millis::MIN,
                }
            });
            row.cost.requests += 1;
        }
        for piece in &parts {
            if let History::Revision(revision) = piece.history
                && interned.kinds[piece.key].kind == ContextKind::Instructions
            {
                rows.get_mut(&(owner, piece.key))
                    .expect("piece has a row")
                    .cost
                    .instruction_size = component(revision).instruction_size;
            }
        }
        // The leading tools and system prompt by revision, and the tokenizer that
        // counted them: when either changes, the previous input no longer bounds
        // where messages start in this request's tokens.
        let prefix = (
            tokenizer,
            if codex {
                vec![]
            } else {
                shown.iter().take(view.prefix).copied().collect()
            },
        );
        // Claude Code marks the last message for caching, so only a few framing
        // tokens follow a request's cached prefix, unless the pieces measured in
        // it do not fit there. Elsewhere content can run to the end of the input.
        let content_end = u
            .filter(|_| !codex)
            .and_then(|u| Some(u.read? + u.write?))
            .filter(|&cached| cached > 0 && total <= cached as f64)
            .or(end);
        let prior = previous.insert(
            r.session.as_str(),
            Prior {
                input: end,
                content_end,
                reply: u
                    .and_then(|u| Some(u.output? - u.reasoning?))
                    .unwrap_or(0)
                    .max(0),
                keys: parts
                    .iter()
                    .filter_map(|x| match x.history {
                        History::Revision(revision) if !x.prefix => Some((revision, x.key)),
                        _ => None,
                    })
                    .collect(),
                prefix: prefix.clone(),
                results: exposure.results.as_deref(),
            },
        );
        if let (Some(slots), Some(end)) = (placed, end) {
            let charge = |start: f64, tokens: f64| {
                let mut cost = 0.0;
                let mut floor = 0.0;
                for &(length, price) in &slots {
                    cost +=
                        0f64.max((floor + length).min(start + tokens) - floor.max(start)) * price;
                    floor += length;
                }
                cost
            };
            // Tools and the system prompt sit at known offsets. An attachment's offset
            // is unknown, but its history bounds it: one the previous request carried
            // lies inside that request's input, and a new one was appended after it.
            // It is priced only when every offset in those bounds pays the same rate.
            let slot = |position: f64| {
                let mut floor = 0.0;
                for (index, (length, _)) in slots.iter().enumerate() {
                    if position < floor + length {
                        return index;
                    }
                    floor += length;
                }
                slots.len() - 1
            };
            let prefix_end: f64 = parts.iter().filter(|x| x.prefix).map(size).sum();
            let known = prior
                .as_ref()
                .filter(|prior| prior.prefix == prefix)
                .and_then(|prior| {
                    prior
                        .input
                        .filter(|&input| input <= end)
                        .map(|i| (i, prior))
                });
            let mut charged: IndexMap<usize, (f64, f64, bool)> = IndexMap::new();
            let mut offset = 0.0;
            for piece in &parts {
                let tokens = size(piece);
                let entry = charged.entry(piece.key).or_insert((0.0, 0.0, false));
                entry.0 += tokens;
                if piece.opaque {
                    entry.2 = true;
                    continue;
                }
                if piece.prefix {
                    entry.1 += charge(offset, tokens);
                    offset += tokens;
                    continue;
                }
                if tokens == 0.0 {
                    continue;
                }
                let carried = known.filter(|(_, prior)| match piece.history {
                    History::Revision(revision) => prior.keys.contains(&(revision, piece.key)),
                    History::Carried => true,
                    History::New => false,
                });
                let from = match (known, carried) {
                    // New content follows the previous request and its reply.
                    (Some((input, prior)), None) => (input + prior.reply) as f64,
                    _ => prefix_end,
                };
                let to = match carried {
                    Some((input, prior)) => prior.content_end.unwrap_or(input) as f64,
                    None => content_end.unwrap_or(end) as f64,
                } - tokens;
                let first = slot(from);
                if to >= from && first == slot(to + tokens - 1e-9) {
                    entry.1 += tokens * slots[first].1;
                } else {
                    entry.2 = true;
                }
            }
            let parents: Vec<_> = charged
                .iter()
                .filter_map(|(&key, &value)| Some((interned.kinds[key].parent?, value)))
                .collect();
            for (parent, (tokens, cost, missing)) in parents {
                let entry = charged.entry(parent).or_insert((0.0, 0.0, false));
                entry.0 += tokens;
                entry.1 += cost;
                entry.2 |= missing;
            }
            for (key, (tokens, cost, missing)) in charged {
                // A row with any piece of unknown cost in this request stays unpriced
                // for it, so a subtotal is never shown as the whole.
                if missing {
                    continue;
                }
                let row = &mut rows[&(owner, key)];
                row.cost.priced += 1;
                row.cost.cost_usd += cost;
                if r.time >= row.time {
                    row.time = r.time;
                    row.cost.tokens = Some(tokens.round() as i64);
                }
            }
        }
        match latest.get(&owner) {
            _ if !shaped => {}
            Some((time, _)) if r.time < *time => {}
            _ => {
                latest.insert(owner, (r.time, unique));
            }
        }
    }
    // Copied histories can leave a call and the requests around it with different
    // profile sets, and a copy cannot say which profile made the call, so it
    // counts for every row of its tool that shares a profile with it.
    let mut families: HashMap<(ContextKind, &str), Vec<usize>> = HashMap::new();
    let mut skill_families: HashMap<usize, Vec<usize>> = HashMap::new();
    for (index, row) in rows.values_mut().enumerate() {
        let kind = row.cost.kind;
        if !matches!(
            kind,
            ContextKind::Tool
                | ContextKind::McpServer
                | ContextKind::McpTool
                | ContextKind::Skill
                | ContextKind::ToolOutput
        ) {
            continue;
        }
        row.cost.calls = Some(0);
        if kind == ContextKind::Skill {
            skill_families.entry(row.key).or_default().push(index);
        } else {
            let name = interned.kinds[row.key].name.as_str();
            families.entry((kind, name)).or_default().push(index);
        }
    }
    let mut last: HashMap<usize, Millis> = HashMap::new();
    let mut credit =
        |indices: Option<&Vec<usize>>, profiles: &[String], time: Millis, calls: u64| {
            for &index in indices.into_iter().flatten() {
                let row = &mut rows[index].cost;
                if !profiles.iter().any(|p| row.profiles.contains(p)) {
                    continue;
                }
                *row.calls.as_mut().expect("family rows count calls") += calls;
                let entry = last.entry(index).or_insert(Millis::MIN);
                *entry = (*entry).max(time);
            }
        };
    for c in &scan.tool_calls {
        let family = if c.name.starts_with("mcp__") {
            (ContextKind::McpServer, mcp_server(&c.name))
        } else if c.name.starts_with("Artifact") {
            (ContextKind::Tool, "Artifact")
        } else {
            (ContextKind::Tool, c.name.as_str())
        };
        credit(families.get(&family), &c.profiles, c.last_used, c.calls);
        if c.name.starts_with("mcp__") {
            credit(
                families.get(&(ContextKind::McpTool, c.name.as_str())),
                &c.profiles,
                c.last_used,
                c.calls,
            );
        }
    }
    let synced_group = identity(&format!("plugin:{SYNCED_NAMESPACE}"));
    for i in &scan.invocations {
        // A namespace-wide switch also removes recorded uses of members whose
        // listing was not measured. Other skill controls apply only to their own target.
        let key = if i.harness == Harness::Claude && i.group.as_ref() == Some(&synced_group) {
            interned.ids.get(SYNCED_SKILLS_KEY)
        } else {
            interned.skills.get(&i.target)
        };
        let Some(&key) = key else {
            continue;
        };
        // Typed requests keep working when a skill is hidden from the model, so only
        // the model's own calls show what its listing is for. A setting that removes
        // the skill entirely takes typed requests away too, so those count as uses.
        let counts = match i.kind {
            InvocationKind::SkillCall => i.status != InvocationStatus::Reference,
            InvocationKind::SkillRequest => !typed_only(interned.kinds[key].control.as_ref()),
            _ => false,
        };
        if counts {
            credit(skill_families.get(&key), &i.profiles, i.time, 1);
        }
    }
    // Results count as their tool's uses, and the largest one sizes its output cap.
    let mut largest: HashMap<usize, i64> = HashMap::new();
    for t in &scan.tool_results {
        for &index in families
            .get(&(ContextKind::ToolOutput, t.name.as_str()))
            .into_iter()
            .flatten()
        {
            let row = &mut rows[index].cost;
            if !t.profiles.iter().any(|p| row.profiles.contains(p)) {
                continue;
            }
            *row.calls.as_mut().expect("family rows count calls") += t.results;
            let entry = largest.entry(index).or_insert(0);
            *entry = (*entry).max(t.largest_characters);
        }
    }
    let latest: HashMap<usize, HashSet<usize>> = latest
        .into_iter()
        .map(|(owner, (_, keys))| (owner, keys.into_iter().collect()))
        .collect();
    let mut details: HashMap<(usize, usize), Vec<ContextCost>> = HashMap::new();
    // Native MCP completions name the raw server and tool. Model-facing names
    // can be sanitized or shortened and cannot safely supply a deny-list entry.
    let raw_tools: HashSet<(&str, &str)> = scan
        .invocations
        .iter()
        .filter(|i| {
            i.harness == Harness::Codex
                && i.kind == InvocationKind::McpCall
                && i.via == Via::Completion
                && i.status == InvocationStatus::Completed
        })
        .flat_map(|i| i.profiles.iter().map(|p| (i.target.as_str(), p.as_str())))
        .collect();
    let mut result: Vec<(usize, usize, ContextCost)> = rows
        .into_values()
        .enumerate()
        .filter_map(|(index, row)| {
            let mut c = row.cost;
            c.current = latest
                .get(&row.owner)
                .is_some_and(|keys| keys.contains(&row.key));
            c.estimated_usd =
                (c.priced > 0).then(|| (c.cost_usd * c.requests as f64) / c.priced as f64);
            c.last_used = last.get(&index).copied();
            c.largest_characters = largest.get(&index).copied();
            if c.harness == Harness::Codex
                && c.kind == ContextKind::McpTool
                && let Some(control) = &c.control
                && let ControlValue::Text(tool) = &control.value
            {
                let raw = identity(&format!("mcp:mcp__{}__{tool}", control.path[1]));
                if !c
                    .profiles
                    .iter()
                    .all(|p| raw_tools.contains(&(raw.as_str(), p.as_str())))
                {
                    c.control = None;
                }
            }
            if let Some(calls) = c.calls {
                c.cost_per_use_usd = c.estimated_usd.map(|usd| usd / calls.max(1) as f64);
            }
            if let (Some(_), true, Some(per_use)) = (&c.control, c.current, c.cost_per_use_usd) {
                c.verdict = Some(if per_use > input.max_cost_per_use_usd {
                    Verdict::Off
                } else {
                    Verdict::Keep
                });
            }
            if let Some(parent) = interned.kinds[row.key].parent {
                details.entry((row.owner, parent)).or_default().push(c);
                None
            } else {
                Some((row.owner, row.key, c))
            }
        })
        .collect();
    for (owner, key, c) in &mut result {
        c.details = details.remove(&(*owner, *key)).unwrap_or_default();
        c.details.sort_by(|a, b| a.name.cmp(&b.name));
    }
    let mut result: Vec<_> = result.into_iter().map(|(_, _, c)| c).collect();
    let order = |c: &ContextCost| c.estimated_usd.unwrap_or(-1.0);
    result.sort_by(|a, b| {
        order(b)
            .partial_cmp(&order(a))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    result
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::model::{
        Exposure, ExposureView, IdentityBasis, Ids, Invocation, Owner, Scope, Size, TextCoverage,
        ToolCalls, ToolResults, Usage, Via,
    };
    use crate::test_support::{make_request, prices};

    fn ids(values: &[&str]) -> Ids {
        values.iter().map(|v| Text::from(*v)).collect()
    }

    fn component(
        id: &str,
        category: Category,
        name: &str,
        characters: i64,
        members: &[&str],
    ) -> Component {
        Component {
            instruction_size: None,
            size: Size {
                characters: Some(characters),
                recorded_tokens: None,
                fingerprint: id.into(),
            },
            id: id.into(),
            key: format!("{}:{name}", category.as_str()),
            name: name.into(),
            category,
            owner: Owner::Context,
            group: None,
            provenance: "synthetic".into(),
            locations: vec![],
            first_seen: 0,
            current: crate::model::Current::Unresolved,
            scope: if members.is_empty() {
                Scope::Component
            } else {
                Scope::Aggregate
            },
            members: members.iter().map(|m| m.to_string()).collect(),
            targets: vec![],
        }
    }

    /// A wire exposure whose complete text has `characters` characters.
    fn exposure(request: &str, components: &[&str], prefix: usize, characters: i64) -> Exposure {
        Exposure {
            request: request.into(),
            view: Arc::new(ExposureView {
                components: ids(components),
                candidates: ids(&[]),
                removed: ids(&[]),
                inherited: ids(&[]),
                deferred: ids(&[]),
                linkage: Linkage::Wire,
                coverage: TextCoverage::CompleteText,
                references: None,
                complete_characters: Some(characters),
                prefix,
                wire_model: Some("claude-opus-5-5".into()),
                reasons: vec![],
            }),
            results: None,
        }
    }

    /// A Claude Opus request with 1h writes and `ordinary` uncached input tokens.
    fn request(id: &str, time: Millis, read: i64, write1h: i64, ordinary: i64) -> Request {
        let mut r = make_request();
        r.id = id.into();
        r.time = time;
        r.parts[0].usage = Usage {
            input: Some(read + write1h + ordinary),
            read: Some(read),
            write: Some(write1h),
            write1h: Some(write1h),
            write5m: Some(0),
            ..r.parts[0].usage
        };
        r
    }

    fn costs(scan: &ExposureScan, requests: &[Request], limit: f64) -> Vec<ContextCost> {
        context_costs(&ContextInput {
            scan,
            requests,
            profiles: &[],
            catalog: &prices(),
            max_cost_per_use_usd: limit,
        })
    }

    fn row<'a>(rows: &'a [ContextCost], name: &str) -> &'a ContextCost {
        rows.iter().find(|c| c.name == name).expect(name)
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-12 * b.abs().max(1e-6)
    }

    const READ: f64 = 0.2e-6;
    const WRITE1H: f64 = 8e-6;
    const ORDINARY: f64 = 4e-6;

    /// Tools and the system prompt lead the request, then a skill listing in messages.
    fn shaped() -> (ExposureScan, Vec<Request>) {
        let scan = ExposureScan {
            components: vec![
                component("artifact", Category::ToolSchema, "Artifact", 4000, &[]),
                component("bash", Category::ToolSchema, "Bash", 1000, &[]),
                component("docs", Category::McpSchema, "mcp__docs__read", 500, &[]),
                component("system", Category::Other, "wire-system", 2000, &[]),
                component("review", Category::SkillListing, "review", 300, &[]),
                component(
                    "listing",
                    Category::SkillListing,
                    "skills",
                    350,
                    &["review"],
                ),
            ],
            // The pooled calibration is 3000 tokens over 10,000 characters.
            requests: vec![exposure(
                "r1",
                &["artifact", "bash", "docs", "system", "listing"],
                4,
                10_000,
            )],
            tool_calls: vec![
                ToolCalls {
                    name: "Bash".into(),
                    profiles: vec!["profile".into()],
                    calls: 1,
                    last_used: 5,
                },
                ToolCalls {
                    name: "mcp__docs__read".into(),
                    profiles: vec!["profile".into()],
                    calls: 2,
                    last_used: 7,
                },
            ],
            ..ExposureScan::default()
        };
        (scan, vec![request("r1", 1, 2000, 200, 800)])
    }

    #[test]
    fn prefix_pieces_take_their_recorded_cache_position() {
        let (scan, requests) = shaped();
        let rows = costs(&scan, &requests, 1.0);
        // Tools at 0.3 tokens per character: Artifact 1200, Bash 300, docs 150, then
        // the system prompt's 600 span the last 350 cached reads, all 200 1h writes
        // and 50 ordinary input tokens.
        let artifact = row(&rows, "Artifact");
        assert!(close(artifact.cost_usd, 1200.0 * READ));
        assert_eq!(artifact.tokens, Some(1200));
        assert_eq!(artifact.calls, Some(0));
        assert_eq!(artifact.verdict, Some(Verdict::Keep));
        let system = row(&rows, "Claude Code system prompt");
        assert!(close(
            system.cost_usd,
            350.0 * READ + 200.0 * WRITE1H + 50.0 * ORDINARY
        ));
        assert_eq!(
            (system.control.as_ref(), system.calls, system.verdict),
            (None, None, None)
        );
        // The listing has no earlier request; past the 2,250-token prefix every
        // possible offset is ordinary input, so it is priced.
        let review = row(&rows, "review");
        assert_eq!(review.kind, ContextKind::Skill);
        assert!(close(review.cost_usd, 90.0 * ORDINARY));
        assert!(close(
            row(&rows, "Skill list framing").cost_usd,
            15.0 * ORDINARY
        ));
        let docs = row(&rows, "docs");
        assert_eq!(
            (docs.kind, docs.calls, docs.last_used),
            (ContextKind::McpServer, Some(2), Some(7))
        );
        let bash = row(&rows, "Bash");
        assert_eq!(
            (bash.control.as_ref(), bash.calls, bash.verdict),
            (None, Some(1), None)
        );
        // Most expensive first.
        assert!(
            rows.windows(2)
                .all(|w| w[0].estimated_usd >= w[1].estimated_usd)
        );
    }

    #[test]
    fn verdicts_compare_cost_per_use_with_the_limit() {
        let (scan, requests) = shaped();
        let artifact = 1200.0 * READ;
        let rows = costs(&scan, &requests, artifact / 2.0);
        // No use counts as one use.
        assert_eq!(row(&rows, "Artifact").verdict, Some(Verdict::Off));
        assert!(close(
            row(&rows, "Artifact").cost_per_use_usd.unwrap(),
            artifact
        ));
        // Two calls halve the docs server's 150-token read cost.
        assert!(close(
            row(&rows, "docs").cost_per_use_usd.unwrap(),
            150.0 * READ / 2.0
        ));
        assert_eq!(row(&rows, "docs").verdict, Some(Verdict::Keep));
        assert_eq!(row(&rows, "Bash").verdict, None);
    }

    /// One token per character: each request's complete text equals its input.
    fn attachments() -> (ExposureScan, Vec<Request>) {
        let scan = ExposureScan {
            components: vec![
                component("tool", Category::ToolSchema, "Search", 1000, &[]),
                component("file", Category::Instructions, "/repo/CLAUDE.md", 500, &[]),
                component(
                    "new",
                    Category::Instructions,
                    "/repo/sub/CLAUDE.md",
                    300,
                    &[],
                ),
            ],
            requests: vec![
                exposure("r1", &["tool", "file"], 1, 1500),
                exposure("r2", &["tool", "file", "new"], 1, 1800),
                exposure("r3", &["tool", "file", "new"], 1, 1800),
            ],
            ..ExposureScan::default()
        };
        let requests = vec![
            // New in the first request: appended after the prefix, inside the 1h writes.
            request("r1", 1, 0, 1500, 0),
            // Carried, so it lies within the previous 1,500 input tokens, all cached reads;
            // the new file follows them, inside the 1h writes.
            request("r2", 2, 1500, 300, 0),
            // Fewer cached reads than the previous input: the carried file could sit
            // in the reads or the writes, so this request leaves it unpriced.
            request("r3", 3, 1200, 600, 0),
        ];
        (scan, requests)
    }

    #[test]
    fn attachments_take_the_latest_position_their_history_allows() {
        let (scan, requests) = attachments();
        let rows = costs(&scan, &requests, 1.0);
        let file = row(&rows, "/repo/CLAUDE.md");
        assert_eq!((file.requests, file.priced), (3, 2));
        assert!(close(file.cost_usd, 500.0 * WRITE1H + 500.0 * READ));
        assert!(close(
            file.estimated_usd.unwrap(),
            file.cost_usd * 3.0 / 2.0
        ));
        let new = row(&rows, "/repo/sub/CLAUDE.md");
        assert_eq!((new.requests, new.priced), (2, 1));
        assert!(close(new.cost_usd, 300.0 * WRITE1H));
        assert!(file.current && new.current);
    }

    fn overshoot() -> (ExposureScan, Vec<Request>) {
        let scan = ExposureScan {
            components: vec![
                component("system", Category::Other, "wire-system", 1800, &[]),
                component("file", Category::Instructions, "/repo/CLAUDE.md", 200, &[]),
            ],
            requests: vec![
                exposure("low", &["system"], 1, 1800),
                exposure("high", &["system", "file"], 1, 1800),
            ],
            ..ExposureScan::default()
        };
        // The pooled ratio puts the low request's pieces above its recorded input.
        let requests = vec![
            request("low", 1, 0, 0, 10),
            request("high", 2, 0, 0, 10_000),
        ];
        (scan, requests)
    }

    #[test]
    fn an_overshooting_calibration_leaves_that_request_unpriced() {
        let (scan, requests) = overshoot();
        let rows = costs(&scan, &requests, 1.0);
        let system = row(&rows, "Claude Code system prompt");
        assert_eq!((system.requests, system.priced), (2, 1));
        // The unpriced request is scaled from the priced one, never counted as free.
        assert_eq!(system.estimated_usd, Some(system.cost_usd * 2.0));
        // The file is new in the high request, after a prior whose input fits.
        assert_eq!(row(&rows, "/repo/CLAUDE.md").priced, 1);
    }

    fn copied() -> (ExposureScan, Vec<Request>) {
        let scan = ExposureScan {
            components: vec![
                component(
                    "artifact",
                    Category::ToolSchema,
                    "ArtifactComments",
                    100,
                    &[],
                ),
                component("review", Category::SkillListing, "review", 100, &[]),
                component(
                    "listing",
                    Category::SkillListing,
                    "skills",
                    100,
                    &["review"],
                ),
                component(
                    "plugin",
                    Category::PluginInstructions,
                    "my plugin/é",
                    100,
                    &[],
                ),
            ],
            requests: vec![
                exposure("r1", &["artifact", "listing", "plugin"], 1, 300),
                exposure("r2", &["listing"], 0, 100),
            ],
            tool_calls: vec![ToolCalls {
                name: "Artifact".into(),
                profiles: vec!["a".into()],
                calls: 3,
                last_used: 9,
            }],
            invocations: [
                (InvocationKind::SkillCall, InvocationStatus::Called, 4),
                (InvocationKind::SkillCall, InvocationStatus::Reference, 5),
                (InvocationKind::SkillRequest, InvocationStatus::Requested, 6),
            ]
            .into_iter()
            .map(|(kind, status, time)| Invocation {
                id: format!("i{time}"),
                target: identity("skill:review"),
                group: None,
                kind,
                status,
                via: Via::Tool,
                session: "session".into(),
                sessions: vec!["session".into()],
                identity_basis: IdentityBasis::Recorded,
                harness: Harness::Claude,
                client: None,
                version: None,
                time,
                profiles: vec!["b".into()],
                locations: vec![],
            })
            .collect(),
            ..ExposureScan::default()
        };
        let mut r1 = request("r1", 1, 0, 0, 300);
        r1.profiles = vec!["b".into(), "a".into()];
        let mut r2 = request("r2", 2, 0, 0, 100);
        r2.profiles = vec!["a".into(), "b".into()];
        (scan, vec![r1, r2])
    }

    #[test]
    fn mcp_details_share_placement_without_double_counting_input() {
        let scan = ExposureScan {
            components: vec![
                component("read", Category::McpSchema, "mcp__docs__read", 400, &[]),
                component("write", Category::McpSchema, "mcp__docs__write", 100, &[]),
                component("discovery", Category::McpDiscovery, "mcp__docs", 100, &[]),
            ],
            requests: vec![exposure("r1", &["read", "write", "discovery"], 3, 600)],
            tool_calls: vec![ToolCalls {
                name: "mcp__docs__read".into(),
                profiles: vec!["profile".into()],
                calls: 3,
                last_used: 5,
            }],
            ..ExposureScan::default()
        };
        let rows = costs(&scan, &[request("r1", 1, 0, 600, 0)], 1.0);
        let server = row(&rows, "docs");
        assert_eq!(
            (server.tokens, server.priced, server.calls),
            (Some(600), 1, Some(3))
        );
        assert_eq!(server.details.len(), 3);
        assert!(close(
            server.cost_usd,
            server.details.iter().map(|c| c.cost_usd).sum()
        ));
        let read = row(&server.details, "mcp__docs__read");
        assert_eq!((read.tokens, read.calls), (Some(400), Some(3)));
        assert_eq!(
            control_text(read.control.as_ref().unwrap(), Harness::Claude),
            r#""permissions": {"deny": ["mcp__docs__read"]}"#
        );
        let discovery = row(&server.details, "mcp__docs");
        assert_eq!(
            (discovery.kind, discovery.tokens, discovery.calls),
            (ContextKind::McpDiscovery, Some(100), None)
        );
    }

    #[test]
    fn codex_tool_controls_require_an_unambiguous_server_id() {
        let schema = component(
            "schema",
            Category::McpSchema,
            "mcp__docs_server__read",
            400,
            &[],
        );
        for (declared, expected) in [
            (vec![vec!["docs-server".to_string()]], Some("docs-server")),
            (
                vec![vec!["docs-server".to_string(), "docs_server".to_string()]],
                None,
            ),
            (
                vec![
                    vec!["docs-server".to_string()],
                    vec!["docs_server".to_string()],
                ],
                None,
            ),
        ] {
            let servers: Vec<&[String]> = declared.iter().map(Vec::as_slice).collect();
            let mut interned = Pieces::default();
            let parts = codex_pieces(&schema, &servers, &mut interned);
            let key = mcp_detail(&schema, parts[0].0, &mut interned);
            let tool = &interned.kinds[key];
            assert_eq!(tool.kind, ContextKind::McpTool);
            assert_eq!(
                tool.control
                    .as_ref()
                    .map(|c| control_text(c, Harness::Codex)),
                expected.map(|id| format!("mcp_servers.{id}.disabled_tools = [\"read\"]"))
            );
        }
        // Discovery with the same exposed name must not consume the schema's identity.
        let mut interned = Pieces::default();
        let (key, kind, name, control) = tool(&schema.name);
        let parent = interned.intern(key, kind, name, control);
        let mut discovery = schema.clone();
        discovery.category = Category::McpDiscovery;
        let found = mcp_detail(&discovery, parent, &mut interned);
        let loaded = mcp_detail(&schema, parent, &mut interned);
        assert_ne!(found, loaded);
        assert_eq!(interned.kinds[found].kind, ContextKind::McpDiscovery);
        assert_eq!(interned.kinds[loaded].kind, ContextKind::McpTool);
    }

    #[test]
    fn codex_blocks_require_native_raw_tool_identity() {
        for (exposed, raw) in [
            ("read", Some("read")),
            ("read", None),
            ("read_file", Some("read-file")),
            ("read_file", Some("read.file")),
        ] {
            let mut scan = ExposureScan {
                components: vec![component(
                    "schema",
                    Category::McpSchema,
                    &format!("mcp__docs_server__{exposed}"),
                    400,
                    &[],
                )],
                requests: vec![Exposure {
                    request: "r1".into(),
                    view: Arc::new(ExposureView {
                        components: ids(&[]),
                        candidates: ids(&["schema"]),
                        linkage: Linkage::Chronological,
                        ..(*exposure("r1", &[], 0, 0).view).clone()
                    }),
                    results: None,
                }],
                ..ExposureScan::default()
            };
            if let Some(raw) = raw {
                let mut invocation = skill_use(InvocationKind::McpCall, "unused", 2);
                invocation.harness = Harness::Codex;
                invocation.via = Via::Completion;
                invocation.status = InvocationStatus::Completed;
                invocation.target = identity(&format!("mcp:mcp__docs-server__{raw}"));
                scan.invocations.push(invocation);
            }
            let mut r = make_request();
            r.id = "r1".into();
            r.harness = Harness::Codex;
            r.provider = Some("openai".into());
            r.model = Some("gpt-5.4".into());
            r.api = crate::model::Api::Responses;
            r.parts[0].model = r.model.clone();
            r.parts[0].tier = Some("standard".into());
            r.parts[0].usage = Usage {
                input: Some(4000),
                read: Some(0),
                write: None,
                write5m: None,
                write1h: None,
                ..r.parts[0].usage
            };
            let mut profile = crate::test_support::make_profile("profile", "/codex");
            profile.harness = Harness::Codex;
            profile.mcp_servers = vec!["docs-server".into()];
            let costs = context_costs(&ContextInput {
                scan: &scan,
                requests: &[r],
                profiles: &[profile],
                catalog: &prices(),
                max_cost_per_use_usd: 1.0,
            });
            let tool = &costs[0].details[0];
            assert_eq!(tool.control.is_some(), raw == Some(exposed));
            assert_eq!(tool.verdict.is_some(), raw == Some(exposed));
        }
    }

    #[test]
    fn instruction_sizes_follow_the_recorded_revision() {
        let mut claude = component(
            "claude",
            Category::Instructions,
            "/repo/CLAUDE.md",
            200,
            &[],
        );
        claude.instruction_size = Some(crate::model::InstructionSize {
            bytes: 401,
            lines: 200,
        });
        let scan = ExposureScan {
            components: vec![claude],
            requests: vec![exposure("r1", &["claude"], 0, 300)],
            ..ExposureScan::default()
        };
        let rows = costs(&scan, &[request("r1", 1, 0, 300, 0)], 1.0);
        let c = row(&rows, "/repo/CLAUDE.md");
        assert_eq!(
            c.instruction_size,
            Some(crate::model::InstructionSize {
                bytes: 401,
                lines: 200
            })
        );
    }

    #[test]
    fn copied_calls_and_model_skill_calls_count_as_uses() {
        let (scan, requests) = copied();
        let rows = costs(&scan, &requests, 1.0);
        // The copied call names only profile a, and still counts for the shared row.
        let artifact = row(&rows, "Artifact");
        assert_eq!(artifact.profiles, ["a", "b"]);
        assert_eq!((artifact.calls, artifact.last_used), (Some(3), Some(9)));
        assert_eq!(
            control_text(artifact.control.as_ref().unwrap(), Harness::Claude),
            r#""enableArtifact": false"#
        );
        // Only the model's own call counts; references and typed requests do not.
        let review = row(&rows, "review");
        assert_eq!((review.calls, review.last_used), (Some(1), Some(4)));
        assert!(review.current);
        // Each UTF-16 unit outside the server-name set becomes one underscore.
        let plugin = row(&rows, "my_plugin__");
        assert_eq!(plugin.kind, ContextKind::McpServer);
        // The latest request dropped the tool and plugin, so neither gets a verdict.
        assert!(!artifact.current && artifact.verdict.is_none());
        assert!(!plugin.current);
    }

    /// Tool output a conversation holds, as `(tool, characters, opaque)`.
    fn held(epoch: u64, tools: &[(&str, i64, bool)]) -> Option<Arc<ResultTotals>> {
        Some(Arc::new(ResultTotals {
            epoch,
            tools: tools.iter().map(|&(t, n, o)| (t.into(), n, o)).collect(),
        }))
    }

    #[test]
    fn tool_output_is_carried_until_compaction() {
        // One token per character: each request's complete text equals its input.
        let mut exposures = vec![
            exposure("r1", &[], 0, 1500),
            exposure("r2", &[], 0, 1800),
            exposure("r3", &[], 0, 500),
            exposure("r4", &[], 0, 600),
        ];
        let results = [
            held(1, &[("Bash", 1000, false)]),
            // 1,000 carried characters, then 300 new ones.
            held(1, &[("Bash", 1300, false)]),
            // A compaction started a new conversation.
            held(2, &[("Bash", 200, false)]),
            held(2, &[("Bash", 200, false), ("Read", 100, true)]),
        ];
        for (e, r) in exposures.iter_mut().zip(results) {
            e.results = r;
        }
        let scan = ExposureScan {
            requests: exposures,
            tool_results: vec![ToolResults {
                name: "Bash".into(),
                profiles: vec!["profile".into()],
                results: 3,
                largest_characters: 1000,
            }],
            ..ExposureScan::default()
        };
        let requests = vec![
            // New output follows the empty prefix, all inside the 1h writes.
            request("r1", 1, 0, 1500, 0),
            // Carried output lies within the previous 1,500 tokens, all cached reads;
            // the new output follows them, inside the 1h writes.
            request("r2", 2, 1500, 300, 0),
            // After a compaction the previous input no longer bounds anything, but
            // every position is ordinary input.
            request("r3", 3, 0, 0, 500),
            // An image has no measurable size, so Read stays unpriced.
            request("r4", 4, 500, 0, 100),
        ];
        let rows = costs(&scan, &requests, 1.0);
        let bash = row(&rows, "Bash");
        assert_eq!(bash.kind, ContextKind::ToolOutput);
        assert_eq!((bash.requests, bash.priced), (4, 4));
        assert!(close(
            bash.cost_usd,
            1000.0 * WRITE1H + (1000.0 * READ + 300.0 * WRITE1H) + 200.0 * ORDINARY + 200.0 * READ
        ));
        assert_eq!(
            (bash.calls, bash.largest_characters, bash.verdict),
            (Some(3), Some(1000), None)
        );
        let read = row(&rows, "Read");
        assert_eq!(
            (read.requests, read.priced, read.estimated_usd),
            (1, 0, None)
        );
    }

    #[test]
    fn codex_context_uses_config_switches() {
        let codex = |id: &str, time: Millis, input: i64, read: i64| {
            let mut r = make_request();
            r.id = id.into();
            r.time = time;
            r.harness = Harness::Codex;
            r.provider = Some("openai".into());
            r.model = Some("gpt-6.1-sol".into());
            r.api = crate::model::Api::Responses;
            r.parts[0].model = r.model.clone();
            r.parts[0].tier = Some("standard".into());
            r.parts[0].usage = Usage {
                input: Some(input),
                output: Some(100),
                read: Some(read),
                write: Some(0),
                write5m: None,
                write1h: None,
                reasoning: Some(60),
                ..r.parts[0].usage
            };
            r
        };
        let state = ["apps", "docs", "agents", "skills"];
        let chronological = |id: &str| Exposure {
            view: Arc::new(ExposureView {
                components: ids(&[]),
                candidates: ids(&state),
                linkage: Linkage::Chronological,
                coverage: TextCoverage::Partial,
                complete_characters: None,
                prefix: 0,
                wire_model: None,
                ..(*exposure(id, &[], 0, 0).view).clone()
            }),
            ..exposure(id, &[], 0, 0)
        };
        let scan = ExposureScan {
            components: vec![
                component(
                    "apps",
                    Category::McpDiscovery,
                    "mcp__codex_apps__github",
                    400,
                    &[],
                ),
                component("docs", Category::McpDiscovery, "mcp__docs_server", 800, &[]),
                component("agents", Category::Instructions, "agents_md", 2000, &[]),
                component("skills", Category::SkillListing, "host_skills", 4000, &[]),
            ],
            requests: vec![chronological("r1"), chronological("r2")],
            tool_calls: vec![ToolCalls {
                name: "mcp__codex_apps__github.fetch".into(),
                profiles: vec!["profile".into()],
                calls: 2,
                last_used: 5,
            }],
            ..ExposureScan::default()
        };
        let mut profile = crate::test_support::make_profile("profile", "/codex");
        profile.harness = Harness::Codex;
        profile.mcp_servers = vec!["docs-server".into(), "unused".into()];
        let rows = context_costs(&ContextInput {
            scan: &scan,
            requests: &[codex("r1", 1, 4000, 0), codex("r2", 2, 5000, 4100)],
            profiles: &[profile],
            catalog: &prices(),
            max_cost_per_use_usd: 1.0,
        });
        // A quarter token per character. The first request read nothing from cache;
        // the second read past everything the first one sent.
        let (input, read) = (2e-6, 0.1e-6);
        let apps = row(&rows, "codex_apps");
        assert_eq!(
            (apps.harness, apps.kind),
            (Harness::Codex, ContextKind::McpServer)
        );
        assert!(close(apps.cost_usd, 100.0 * input + 100.0 * read));
        assert_eq!(
            control_text(apps.control.as_ref().unwrap(), Harness::Codex),
            "features.apps = false"
        );
        assert_eq!((apps.calls, apps.verdict), (Some(2), Some(Verdict::Keep)));
        // The model sees `docs-server` as `docs_server`.
        let docs = row(&rows, "docs_server");
        assert_eq!(
            control_text(docs.control.as_ref().unwrap(), Harness::Codex),
            "mcp_servers.docs-server.enabled = false"
        );
        assert_eq!(docs.verdict, Some(Verdict::Keep));
        let agents = row(&rows, "AGENTS.md");
        assert_eq!(
            (agents.kind, agents.control.as_ref()),
            (ContextKind::Instructions, None)
        );
        assert!(close(agents.cost_usd, 500.0 * input + 500.0 * read));
        // The skills check owns per-skill findings; the catalog is one piece here.
        assert_eq!(row(&rows, "Skill list").control, None);
        assert!(rows.iter().all(|c| c.kind != ContextKind::Skill));
    }

    #[test]
    fn codex_opaque_pieces_and_switches_per_profile() {
        // gpt-5.4 bills no cache writes, so its usage may omit them.
        let codex = |id: &str, profiles: &[&str]| {
            let mut r = make_request();
            r.id = id.into();
            r.session = id.into();
            r.profiles = profiles.iter().map(|p| p.to_string()).collect();
            r.harness = Harness::Codex;
            r.provider = Some("openai".into());
            r.model = Some("gpt-5.4".into());
            r.api = crate::model::Api::Responses;
            r.parts[0].model = r.model.clone();
            r.parts[0].tier = Some("standard".into());
            r.parts[0].usage = Usage {
                input: Some(4000),
                output: Some(100),
                read: Some(0),
                write: None,
                write5m: None,
                write1h: None,
                reasoning: Some(60),
                ..r.parts[0].usage
            };
            r
        };
        let chronological = |id: &str, state: &[&str]| Exposure {
            view: Arc::new(ExposureView {
                components: ids(&[]),
                candidates: ids(state),
                linkage: Linkage::Chronological,
                coverage: TextCoverage::Partial,
                complete_characters: None,
                prefix: 0,
                wire_model: None,
                ..(*exposure(id, &[], 0, 0).view).clone()
            }),
            ..exposure(id, &[], 0, 0)
        };
        let opaque = |id: &str, category: Category, name: &str| {
            let mut c = component(id, category, name, 0, &[]);
            c.size.characters = None;
            c
        };
        let scan = ExposureScan {
            components: vec![
                component("docs", Category::McpDiscovery, "mcp__docs_server", 800, &[]),
                opaque("top", Category::McpSchema, "functions"),
                component("provided", Category::ToolSchema, "functions", 400, &[]),
                opaque("agents", Category::Instructions, "agents_md"),
                opaque("search", Category::McpSchema, "mcp__other__search"),
                component("read", Category::ToolSchema, "mcp__other__read", 400, &[]),
            ],
            requests: vec![
                chronological(
                    "r1",
                    &["docs", "top", "provided", "agents", "search", "read"],
                ),
                chronological("r2", &["docs", "top"]),
                chronological("r3", &["docs"]),
            ],
            ..ExposureScan::default()
        };
        // Both ids expose the server as `docs_server`.
        let profile = |id: &str, server: &str| {
            let mut p = crate::test_support::make_profile(id, &format!("/{id}"));
            p.harness = Harness::Codex;
            p.mcp_servers = vec![server.into()];
            p
        };
        let rows = context_costs(&ContextInput {
            scan: &scan,
            requests: &[
                codex("r1", &["a"]),
                codex("r2", &["b"]),
                codex("r3", &["a", "b"]),
            ],
            profiles: &[profile("a", "docs-server"), profile("b", "docs_server")],
            catalog: &prices(),
            max_cost_per_use_usd: 1.0,
        });
        let of = |name: &str, profile: &str| {
            rows.iter()
                .find(|c| c.name == name && c.profiles.join("+") == profile)
                .expect(name)
        };
        for (profile, id) in [("a", "docs-server"), ("b", "docs_server")] {
            let docs = of("docs_server", profile);
            assert_eq!(
                control_text(docs.control.as_ref().unwrap(), Harness::Codex),
                format!("mcp_servers.{id}.enabled = false")
            );
            // Unrecorded writes on a model that bills none leave the input ordinary.
            assert_eq!(docs.priced, 1);
        }
        // A provided schema measures the opaque top-level identity beside it;
        // without one, the tools' size is unknown.
        assert_eq!(of("functions", "a").priced, 1);
        assert_eq!(of("functions", "b").priced, 0);
        // Another tool's schema of the same server does not measure it.
        assert_eq!(of("other", "a").priced, 0);
        let other = of("other", "a");
        assert_eq!(row(&other.details, "mcp__other__read").priced, 1);
        assert_eq!(
            row(&other.details, "mcp__other__search").estimated_usd,
            None
        );
        assert_eq!(other.estimated_usd, None);
        // A request copied into profiles that declare the server differently
        // gets no switch.
        assert_eq!(of("docs_server", "a+b").control, None);
        // Metadata-only instructions stay visible, unpriced.
        let agents = of("AGENTS.md", "a");
        assert_eq!((agents.requests, agents.priced), (1, 0));
    }

    fn skill_use(kind: InvocationKind, target: &str, time: Millis) -> Invocation {
        Invocation {
            id: format!("i{time}"),
            target: identity(&format!("skill:{target}")),
            group: target
                .split_once(':')
                .map(|(namespace, _)| identity(&format!("plugin:{namespace}"))),
            kind,
            status: if kind == InvocationKind::SkillCall {
                InvocationStatus::Called
            } else {
                InvocationStatus::Requested
            },
            via: Via::Tool,
            session: "session".into(),
            sessions: vec!["session".into()],
            identity_basis: IdentityBasis::Recorded,
            harness: Harness::Claude,
            client: None,
            version: None,
            time,
            profiles: vec!["profile".into()],
            locations: vec![],
        }
    }

    #[test]
    fn skills_take_the_setting_their_source_supports() {
        let members = [
            "review",
            "pdf",
            "anthropic-skills:pdf",
            "anthropic-skills:xlsx",
            "tools:deploy",
        ];
        let mut components: Vec<Component> = members
            .iter()
            .map(|m| component(m, Category::SkillListing, m, 100, &[]))
            .collect();
        components.push(component(
            "listing",
            Category::SkillListing,
            "skills",
            500,
            &members,
        ));
        let scan = ExposureScan {
            components,
            requests: vec![exposure("r1", &["listing"], 0, 500)],
            invocations: vec![
                // The scan resolved this short name to the local skill that owns it.
                skill_use(InvocationKind::SkillRequest, "pdf", 1),
                skill_use(InvocationKind::SkillRequest, "anthropic-skills:xlsx", 2),
                skill_use(InvocationKind::SkillCall, "anthropic-skills:pdf", 3),
                skill_use(InvocationKind::SkillRequest, "tools:deploy", 4),
                // Uses of the shared switch count even when that member's listing is absent.
                skill_use(InvocationKind::SkillCall, "anthropic-skills:slides", 5),
                skill_use(InvocationKind::SkillRequest, "anthropic-skills:slides", 6),
                skill_use(InvocationKind::SkillReference, "anthropic-skills:other", 7),
            ],
            ..ExposureScan::default()
        };
        let rows = costs(&scan, &[request("r1", 1, 0, 0, 500)], 1.0);
        // Typing /pdf still works when the model cannot see it, so it is no model use.
        let pdf = row(&rows, "pdf");
        assert_eq!(
            control_text(pdf.control.as_ref().unwrap(), Harness::Claude),
            r#""skillOverrides": {"pdf": "user-invocable-only"}"#
        );
        assert_eq!(pdf.calls, Some(0));
        // Synced skills share one switch that removes typed uses too.
        let synced = row(&rows, SYNCED_SKILLS);
        assert_eq!(
            control_text(synced.control.as_ref().unwrap(), Harness::Claude),
            r#""syncClaudeAiSkills": false"#
        );
        assert_eq!((synced.calls, synced.last_used), (Some(4), Some(6)));
        assert_eq!(synced.tokens, Some(200));
        // skillOverrides does not reach a plugin's skill, so no verdict names it.
        let plugin = row(&rows, "tools:deploy");
        assert_eq!(plugin.control, None);
        assert_eq!((plugin.calls, plugin.verdict), (Some(1), None));
    }

    #[test]
    fn local_and_synced_skills_keep_separate_controls() {
        for members in [
            ["anthropic-skills", "anthropic-skills:pdf"],
            ["anthropic-skills:pdf", "anthropic-skills"],
        ] {
            let mut components: Vec<_> = members
                .iter()
                .map(|m| component(m, Category::SkillListing, m, 100, &[]))
                .collect();
            components.push(component(
                "listing",
                Category::SkillListing,
                "skills",
                200,
                &members,
            ));
            let scan = ExposureScan {
                components,
                requests: vec![exposure("r1", &["listing"], 0, 200)],
                invocations: vec![
                    skill_use(InvocationKind::SkillCall, "anthropic-skills", 1),
                    skill_use(InvocationKind::SkillCall, "anthropic-skills:pdf", 2),
                ],
                ..ExposureScan::default()
            };
            let rows = costs(&scan, &[request("r1", 1, 0, 0, 200)], 1.0);
            assert_eq!(
                rows.iter().filter(|r| r.kind == ContextKind::Skill).count(),
                2
            );
            let local = row(&rows, "anthropic-skills");
            let synced = row(&rows, "claude.ai synced skills");
            assert_eq!(
                control_text(local.control.as_ref().unwrap(), Harness::Claude),
                r#""skillOverrides": {"anthropic-skills": "user-invocable-only"}"#
            );
            assert_eq!(
                control_text(synced.control.as_ref().unwrap(), Harness::Claude),
                r#""syncClaudeAiSkills": false"#
            );
            for cost in [local, synced] {
                assert_eq!(cost.tokens, Some(100));
                assert_eq!(cost.calls, Some(1));
            }
        }
    }

    #[test]
    fn control_text_renders_one_line() {
        let control = |path: &[&str], value: ControlValue, append: bool| Control {
            path: path.iter().map(|p| p.to_string()).collect(),
            value,
            append,
        };
        let claude = |path: &[&str], value: ControlValue, append: bool| {
            control_text(&control(path, value, append), Harness::Claude)
        };
        assert_eq!(
            claude(&["enableArtifact"], ControlValue::Bool(false), false),
            r#""enableArtifact": false"#
        );
        assert_eq!(
            claude(
                &["skillOverrides", "review"],
                ControlValue::Text("user-invocable-only".into()),
                false
            ),
            r#""skillOverrides": {"review": "user-invocable-only"}"#
        );
        assert_eq!(
            claude(
                &["permissions", "deny"],
                ControlValue::Text("mcp__docs".into()),
                true
            ),
            r#""permissions": {"deny": ["mcp__docs"]}"#
        );
        // config.toml takes dotted keys, quoting any that are not bare.
        assert_eq!(
            control_text(
                &control(
                    &["mcp_servers", "my.server", "enabled"],
                    ControlValue::Bool(false),
                    false
                ),
                Harness::Codex
            ),
            r#"mcp_servers."my.server".enabled = false"#
        );
    }

    #[test]
    fn config_patch_renders_toml_tables() {
        let off = |path: &[&str]| Control {
            path: path.iter().map(|p| p.to_string()).collect(),
            value: ControlValue::Bool(false),
            append: false,
        };
        let patch = config_patch([
            &off(&["features", "apps"]),
            &off(&["mcp_servers", "t3-code", "enabled"]),
        ]);
        let parsed: toml::Table = toml::from_str(&patch).expect("valid TOML");
        assert_eq!(parsed["features"]["apps"], toml::Value::Boolean(false));
        assert_eq!(
            parsed["mcp_servers"]["t3-code"]["enabled"],
            toml::Value::Boolean(false)
        );
    }

    #[test]
    fn settings_patch_merges_and_appends_without_duplicates() {
        let deny = |rule: &str| Control {
            path: vec!["permissions".into(), "deny".into()],
            value: ControlValue::Text(rule.into()),
            append: true,
        };
        let artifact = Control {
            path: vec!["enableArtifact".into()],
            value: ControlValue::Bool(false),
            append: false,
        };
        let patch = settings_patch([
            &deny("Agent"),
            &artifact,
            &deny("mcp__docs"),
            &deny("Agent"),
        ]);
        assert_eq!(
            Value::Object(patch),
            serde_json::json!({
                "permissions": {"deny": ["Agent", "mcp__docs"]},
                "enableArtifact": false
            })
        );
    }
}
