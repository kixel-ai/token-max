//! What each piece of always-sent context cost, and the setting that turns it off.

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::exposure::{calibrate, cohort};
use crate::model::{
    Category, Component, ContextCost, ContextKind, Control, ControlValue, ExposureScan, Harness,
    InvocationKind, InvocationStatus, Linkage, Millis, Request, Text, Verdict,
};
use crate::prices::{Catalog, unit_rates};
use crate::safe::identity;

/// The settings.json fragment for one control, such as `"enableArtifact": false`.
pub fn control_text(control: &Control) -> String {
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
    /// Public prices.
    pub catalog: &'a Catalog,
    /// Recommend turning a piece off when it cost more than this per use.
    pub max_cost_per_use_usd: f64,
}

/// What a piece is, shared by every revision that produces it.
struct Kind {
    kind: ContextKind,
    name: String,
    control: Option<Control>,
}

/// A piece as it appears in one request.
struct Placed<'a> {
    /// Index into the interned piece keys.
    key: usize,
    characters: i64,
    /// Tools and the system prompt lead the request in recorded order; everything else rides in messages.
    prefix: bool,
    /// The component revision it came from, so changed content is not mistaken for carried content.
    revision: &'a str,
}

/// Pieces by key, interned so each request compares indices rather than strings.
#[derive(Default)]
struct Pieces<'a> {
    ids: HashMap<String, usize>,
    kinds: Vec<Kind>,
    of: HashMap<&'a str, Vec<(usize, i64)>>,
}

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

fn deny(rule: String) -> Option<Control> {
    Some(Control {
        path: vec!["permissions".into(), "deny".into()],
        value: ControlValue::Text(rule),
        append: true,
    })
}

fn disable(setting: &str) -> Option<Control> {
    Some(Control {
        path: vec![setting.into()],
        value: ControlValue::Bool(false),
        append: false,
    })
}

/// The MCP server a tool belongs to: the text between the first and second `__`.
fn mcp_server(name: &str) -> &str {
    name.split("__").nth(1).unwrap_or(name)
}

/// The setting that owns a tool, grouping a tool family under one switch.
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
            disable("enableArtifact"),
        );
    }
    if name == "Workflow" {
        return (
            "tool:Workflow".into(),
            ContextKind::Tool,
            name.into(),
            disable("enableWorkflows"),
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

/// Split one component into the pieces a user can switch separately.
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
                    let control = Control {
                        path: vec!["skillOverrides".into(), member.into()],
                        value: ControlValue::Text("user-invocable-only".into()),
                        append: false,
                    };
                    let key = format!("skill:{member}");
                    let id = interned.intern(key, ContextKind::Skill, member.into(), Some(control));
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

/// The previous request of a session, which bounds where its attachments sit in the next one.
struct Prior<'a> {
    input: Option<i64>,
    keys: HashSet<(&'a str, usize)>,
    /// The tokenizer and the leading tools and system prompt by revision.
    prefix: (String, &'a [Text]),
}

struct Row {
    cost: ContextCost,
    owner: usize,
    key: usize,
    time: Millis,
}

/// Price each piece of context in every selected request that recorded its exact
/// shape. A piece is charged at the cache state of its position in the prompt:
/// tools and the system prompt lead in recorded order. Message attachments have
/// no recorded offset, so each takes the latest position its history allows:
/// the end of the session's previous request when that request carried it too,
/// else the end of this request, where newly added content goes.
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
        .filter_map(|exposure| {
            let r = *requests.get(exposure.request.as_str())?;
            (exposure.view.linkage == Linkage::Wire && r.harness == Harness::Claude)
                .then_some((exposure, r))
        })
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
        let mut parts: Vec<Placed<'_>> = vec![];
        for (index, id) in view.components.iter().enumerate() {
            let id: &str = id;
            if !interned.of.contains_key(id) {
                let component = components
                    .get(id)
                    .expect("exposures reference recorded components");
                let split = pieces(component, &components, &mut interned);
                interned.of.insert(id, split);
            }
            let prefix = index < view.prefix;
            parts.extend(interned.of[id].iter().map(|&(key, characters)| Placed {
                key,
                characters,
                prefix,
                revision: id,
            }));
        }
        let p = if r.parts.len() == 1 {
            Some(&r.parts[0])
        } else {
            None
        };
        let u = p.map(|p| &p.usage);
        let tokenizer = cohort(r);
        let ratio = calibration.get(&tokenizer).copied();
        let rates = p.and_then(|p| unit_rates(catalog, r, p));
        let priced = match (u, ratio, rates) {
            (Some(u), Some(_), Some(rates))
                if u.input.is_some()
                    && u.read.is_some()
                    && u.write.is_some()
                    && (u.write == Some(0) || (u.write1h.is_some() && u.write5m.is_some())) =>
            {
                Some((u, rates))
            }
            _ => None,
        };
        let size = |piece: &Placed<'_>| piece.characters as f64 * ratio.unwrap_or(0.0);
        let total: f64 = parts.iter().map(size).sum();
        // A calibration that overshoots the recorded input cannot place these pieces.
        let placed = priced.filter(|(u, _)| total <= u.input.expect("priced") as f64);
        let mut unique: Vec<usize> = vec![];
        {
            let mut seen = HashSet::new();
            for piece in &parts {
                if seen.insert(piece.key) {
                    unique.push(piece.key);
                }
            }
        }
        for &key in &unique {
            let row = rows.entry((owner, key)).or_insert_with(|| {
                let kind = &interned.kinds[key];
                Row {
                    cost: ContextCost {
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
                    },
                    owner,
                    key,
                    time: Millis::MIN,
                }
            });
            row.cost.requests += 1;
        }
        // The leading tools and system prompt by revision, and the tokenizer that
        // counted them: when either changes, the previous input no longer bounds
        // where messages start in this request's tokens.
        let prefix = (
            tokenizer,
            &view.components[..view.prefix.min(view.components.len())],
        );
        let prior = previous.insert(
            r.session.as_str(),
            Prior {
                input: u.and_then(|u| u.input),
                keys: parts
                    .iter()
                    .filter(|x| !x.prefix)
                    .map(|x| (x.revision, x.key))
                    .collect(),
                prefix: prefix.clone(),
            },
        );
        if let Some((u, rates)) = placed {
            let end = u.input.expect("priced");
            let write = |tokens: Option<i64>| {
                if u.write == Some(0) {
                    0.0
                } else {
                    tokens.expect("priced") as f64
                }
            };
            // Cache slots in prefix order: reads, then 1h writes, then 5m writes, then ordinary input.
            let slots: [(f64, f64); 4] = [
                (u.read.expect("priced") as f64, rates.read),
                (write(u.write1h), rates.write1h),
                (write(u.write5m), rates.write5m),
                (f64::INFINITY, rates.ordinary),
            ];
            let charge = |start: f64, tokens: f64| {
                let mut cost = 0.0;
                let mut floor = 0.0;
                for (length, price) in slots {
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
                if piece.prefix {
                    entry.1 += charge(offset, tokens);
                    offset += tokens;
                    continue;
                }
                let carried =
                    known.filter(|(_, prior)| prior.keys.contains(&(piece.revision, piece.key)));
                let from = match (known, carried) {
                    (Some((input, _)), None) => input as f64,
                    _ => prefix_end,
                };
                let to = match carried {
                    Some((input, _)) => input as f64,
                    None => end as f64,
                } - tokens;
                let first = slot(from);
                if to >= from && first == slot(to + tokens - 1e-9) {
                    entry.1 += tokens * slots[first].1;
                } else {
                    entry.2 = true;
                }
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
    for (index, row) in rows.values_mut().enumerate() {
        let kind = row.cost.kind;
        if !matches!(
            kind,
            ContextKind::Tool | ContextKind::McpServer | ContextKind::Skill
        ) {
            continue;
        }
        row.cost.calls = Some(0);
        let name = interned.kinds[row.key].name.as_str();
        families.entry((kind, name)).or_default().push(index);
    }
    let mut last: HashMap<usize, Millis> = HashMap::new();
    let mut credit =
        |family: (ContextKind, &str), profiles: &[String], time: Millis, calls: u64| {
            for &index in families.get(&family).into_iter().flatten() {
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
        credit(family, &c.profiles, c.last_used, c.calls);
    }
    let skills: HashMap<String, &str> = interned
        .kinds
        .iter()
        .filter(|k| k.kind == ContextKind::Skill)
        .map(|k| (identity(&format!("skill:{}", k.name)), k.name.as_str()))
        .collect();
    for i in &scan.invocations {
        // Typed requests keep working when a skill is hidden from the model, so only
        // the model's own calls show what its listing is for.
        if i.kind != InvocationKind::SkillCall || i.status == InvocationStatus::Reference {
            continue;
        }
        if let Some(&name) = skills.get(&i.target) {
            credit((ContextKind::Skill, name), &i.profiles, i.time, 1);
        }
    }
    let latest: HashMap<usize, HashSet<usize>> = latest
        .into_iter()
        .map(|(owner, (_, keys))| (owner, keys.into_iter().collect()))
        .collect();
    let mut result: Vec<ContextCost> = rows
        .into_values()
        .enumerate()
        .map(|(index, row)| {
            let mut c = row.cost;
            c.current = latest
                .get(&row.owner)
                .is_some_and(|keys| keys.contains(&row.key));
            c.estimated_usd =
                (c.priced > 0).then(|| (c.cost_usd * c.requests as f64) / c.priced as f64);
            c.last_used = last.get(&index).copied();
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
            c
        })
        .collect();
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
        ToolCalls, Usage, Via,
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
    fn copied_calls_and_model_skill_calls_count_as_uses() {
        let (scan, requests) = copied();
        let rows = costs(&scan, &requests, 1.0);
        // The copied call names only profile a, and still counts for the shared row.
        let artifact = row(&rows, "Artifact");
        assert_eq!(artifact.profiles, ["a", "b"]);
        assert_eq!((artifact.calls, artifact.last_used), (Some(3), Some(9)));
        assert_eq!(
            control_text(artifact.control.as_ref().unwrap()),
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

    #[test]
    fn control_text_renders_one_line() {
        let control = |path: &[&str], value: ControlValue, append: bool| Control {
            path: path.iter().map(|p| p.to_string()).collect(),
            value,
            append,
        };
        assert_eq!(
            control_text(&control(
                &["enableArtifact"],
                ControlValue::Bool(false),
                false
            )),
            r#""enableArtifact": false"#
        );
        assert_eq!(
            control_text(&control(
                &["skillOverrides", "review"],
                ControlValue::Text("user-invocable-only".into()),
                false
            )),
            r#""skillOverrides": {"review": "user-invocable-only"}"#
        );
        assert_eq!(
            control_text(&control(
                &["permissions", "deny"],
                ControlValue::Text("mcp__docs".into()),
                true
            )),
            r#""permissions": {"deny": ["mcp__docs"]}"#
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
