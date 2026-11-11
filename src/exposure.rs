//! Historical context components and how they were used, derived in the reader's single pass.
//!
//! Raw records are observed transiently; only counts, fingerprints, opaque identities and
//! readable names (tools, skills, servers, file paths) are retained.

use crate::collections::{HashMap, HashSet, IndexMap, IndexSet};
use std::sync::Arc;

use serde_json::{Map, Number, Value};
use sha2::{Digest, Sha256};

use crate::skills::skill_file;

use crate::context::synced_short;
use crate::model::{
    Calibration, CalibrationSample, Category, Component, Exposure, ExposureScan, ExposureView,
    Harness, IdentityBasis, Ids, Invocation, InvocationKind, InvocationStatus, Linkage, Location,
    Millis, Observed, Owner, References, Request, RequestEvidence, ResultTotals, Scope, Size,
    Snapshot, Text, TextCoverage, ToolCalls, ToolResults, Via, Visibility, Visible,
};
use crate::safe::{self, ScanError, identity, label, object};

/// Prefer recorded counts; a missing calibration leaves token and dollar estimates unknown.
pub fn measure_size(text: Option<&str>) -> Size {
    Size {
        characters: text.map(|t| t.chars().count() as i64),
        recorded_tokens: None,
        fingerprint: identity(text.unwrap_or("opaque")),
    }
}

/// Tokenization belongs to the model; client versions only change what is sent.
pub fn cohort(r: &Request) -> String {
    let key = (harness_name(r.harness), &r.model, &r.provider);
    identity(&serde_json::to_string(&key).expect("strings serialize"))
}

/// Calibrate complete text-only wire inputs against observed, single-request input totals.
pub fn calibrate(scan: &ExposureScan, requests: &[Request]) -> Vec<Calibration> {
    let by_id: HashMap<&str, &Request> = requests.iter().map(|r| (&*r.id, r)).collect();
    let mut result: IndexMap<String, (Calibration, i64, i64)> = IndexMap::default();
    for exposure in &scan.requests {
        let Some(r) = by_id.get(&*exposure.request) else {
            continue;
        };
        let Some(model) = r.model.as_deref().filter(|m| !m.is_empty()) else {
            continue;
        };
        let characters = match exposure.view.complete_characters {
            Some(c) if c != 0 => c,
            _ => continue,
        };
        if r.evidence != RequestEvidence::Measured
            || r.parts.len() != 1
            || exposure.view.coverage != TextCoverage::CompleteText
            || exposure.view.wire_model.as_deref() != Some(model)
        {
            continue;
        }
        let tokens = match r.parts[0].usage.input {
            Some(t) if t != 0 => t,
            _ => continue,
        };
        let key = cohort(r);
        let (c, total_tokens, total_characters) =
            result.entry(key.clone()).or_insert_with(|| {
                (
                    Calibration {
                        cohort: key,
                        samples: vec![],
                        tokens_per_character: 0.0,
                        range: (f64::INFINITY, f64::NEG_INFINITY),
                        assumptions: vec![
                            "empirical text-only wire serialization ratio includes framing; transfer to components is estimated".into(),
                            "observed sample extrema are sensitivity scenarios, not guaranteed bounds".into(),
                        ],
                    },
                    0,
                    0,
                )
            });
        let ratio = tokens as f64 / characters as f64;
        c.samples.push(CalibrationSample {
            request: r.id.clone(),
            characters,
            tokens,
        });
        c.range = (c.range.0.min(ratio), c.range.1.max(ratio));
        // Integer sums are exact, as the floating sums they replace are below 2^53.
        *total_tokens += tokens;
        *total_characters += characters;
        c.tokens_per_character = *total_tokens as f64 / *total_characters as f64;
    }
    result.into_values().map(|(c, _, _)| c).collect()
}

/// Collect derived metadata in the reader's frozen, single parsing pass.
pub struct ExposureCollector {
    since: Option<Millis>,
    // Tool calls by call id, so copies in other transcript files count once.
    calls: IndexMap<String, ToolCall>,
    // Tool results by call id, for the same reason.
    results: IndexMap<String, ToolResult>,
    names: HashSet<Text>,
    // The tool each call id in the current file belongs to.
    call_tools: HashMap<String, Text>,
    // Tool output characters the current conversation holds, by tool, and whether any was opaque.
    held: IndexMap<Text, (i64, bool)>,
    held_epoch: u64,
    epochs: u64,
    // The last snapshot of `held`, shared until it changes.
    held_snapshot: Option<Arc<ResultTotals>>,
    // Tools the harness provides for the whole session, which a full world state does not replace.
    provided: HashSet<Text>,
    components: IndexMap<Text, Component>,
    invocations: IndexMap<String, Invocation>,
    observations: HashMap<Text, FileObservations>,
    file: Option<File>,
    // Active revision and category by component key, in JavaScript `Map` insertion order.
    active: IndexMap<Text, (Text, Category)>,
    pending_removed: IndexSet<Text>,
    session: Text,
    // The last raw Claude session id and the session derived from it.
    session_cache: Option<(String, Text)>,
    turn: Option<String>,
    turn_from_user: bool,
    turn_available: bool,
    turn_aliases: HashMap<String, String>,
    client: Option<String>,
    version: Option<String>,
    uncertain: bool,
    deferred: IndexSet<Text>,
    digests: HashMap<String, Ids>,
    // Catalog names keyed without ASCII case, retaining their spelling for use attribution.
    known_skills: HashMap<String, String>,
    // A name resolves only when every current rendered copy has the same absolute path.
    codex_skill_paths: HashMap<String, (Option<String>, usize)>,
    // Explicit typed skill blocks establish use evidence without claiming catalog membership.
    typed_skills: HashSet<String>,
    // Short names of listed synced skills: `/pdf` reaches `anthropic-skills:pdf` while no
    // other known skill is named `pdf`.
    synced_skills: HashMap<String, String>,
    user_aliases: HashMap<String, HashMap<String, u64>>,
    // Surfaced tool definitions by attachment uuid and name, for later `sameAs` references.
    surfaced: HashMap<(String, String), Text>,
    // Request shapes (system prompt and tool definitions) by hash, as sizes only.
    shapes: HashMap<String, Arc<Shape>>,
    // Hash-addressed message sizes in the current file, for complete request sizes.
    // Text characters per message blob, or None when it holds images or other opaque input.
    blobs: HashMap<String, Option<i64>>,
    // Counts blob sizes overwritten with a different value, which invalidates `prefixes`.
    blob_epoch: u64,
    // Each digest list's longest all-text prefix, so a request extending its base reads only
    // its own tail instead of every message so far.
    prefixes: HashMap<usize, TextPrefix>,
    invalidated: bool,
    pending_tools: HashMap<String, PendingTool>,
    lists: HashSet<Ids>,
    views: HashSet<Arc<ExposureView>>,
    // The last view created, whose lists the next one usually extends.
    previous: Option<Arc<ExposureView>>,
}

/// The transcript file being observed.
struct File {
    name: String,
    harness: Harness,
    profiles: Vec<Text>,
    subagent: bool,
    basename: String,
}

#[derive(Default)]
struct FileObservations {
    lines: HashMap<u64, Arc<ExposureView>>,
    refs: HashMap<String, Arc<ExposureView>>,
    results: HashMap<u64, Arc<ResultTotals>>,
}

struct ToolResult {
    tool: Text,
    harness: Harness,
    sessions: Vec<Text>,
    profiles: Vec<Text>,
    characters: i64,
}

/// The leading digests of one list whose blobs are all text, as of a `blob_epoch`.
struct TextPrefix {
    // Keeps the list, and so its address, alive.
    _items: Ids,
    epoch: u64,
    length: usize,
    characters: i64,
    /// Whether the digest after the prefix is a blob holding opaque input.
    opaque: bool,
}

struct ToolCall {
    name: Text,
    harness: Harness,
    sessions: Vec<Text>,
    profiles: Vec<Text>,
    time: Millis,
}

#[derive(Clone)]
struct PendingTool {
    target: String,
    group: Option<String>,
    kind: InvocationKind,
}

/// A recorded request shape, measured once and placed on every request that uses it.
struct Shape {
    system: Placed,
    tools: Vec<Placed>,
}

/// A component whose identity and size are computed once.
struct Placed {
    category: Category,
    name: String,
    key: Text,
    revision: String,
    size: Size,
    group: Option<String>,
}

impl Placed {
    fn new(category: Category, name: &str, content: Content<'_>, group: Option<String>) -> Self {
        let key = component_key(category, name);
        let size = content.measure();
        Placed {
            category,
            name: name.to_string(),
            revision: identity(&format!("{key}:{}", size.fingerprint)),
            key: key.into(),
            size,
            group,
        }
    }
}

/// What a component's size and fingerprint are measured from.
enum Content<'a> {
    Text(&'a str),
    /// The `JSON.stringify` text of a value.
    Json(&'a Value),
    /// `JSON.stringify(undefined)`, which is not text.
    Missing,
    /// A value whose rendered size is unknown; only its serialized identity is kept.
    Opaque(&'a Value),
}

impl Content<'_> {
    fn measure(&self) -> Size {
        match self {
            Content::Text(text) => measure_size(Some(text)),
            Content::Missing => measure_size(None),
            Content::Json(value) => {
                let mut sink = Measure::default();
                json(value, &mut sink);
                Size {
                    characters: Some(sink.characters as i64),
                    recorded_tokens: None,
                    fingerprint: hex(sink.hash.finalize()),
                }
            }
            Content::Opaque(value) => Size {
                characters: None,
                recorded_tokens: None,
                fingerprint: Content::Json(value).measure().fingerprint,
            },
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Active,
    /// Exists locally but is not necessarily in the model input.
    Deferred,
    /// An entry span owned by its aggregate, never additive exposure.
    Member,
}

enum Grouping<'a> {
    /// Derived from the component name.
    Name,
    Fixed(Option<&'a str>),
}

/// How to create a component revision that has not been seen before.
struct New<'a> {
    category: Category,
    name: &'a str,
    provenance: &'static str,
    role: Role,
    grouping: Grouping<'a>,
    targets: Option<Vec<String>>,
}

impl<'a> New<'a> {
    fn active(category: Category, name: &'a str, provenance: &'static str) -> Self {
        New {
            category,
            name,
            provenance,
            role: Role::Active,
            grouping: Grouping::Name,
            targets: None,
        }
    }
}

/// One raw record and its surroundings.
struct Line<'a> {
    raw: &'a Map<String, Value>,
    payload: &'a Map<String, Value>,
    kind: Option<&'a str>,
    snapshot: &'a Snapshot,
    location: &'a Location,
    time: Option<Millis>,
}

impl Line<'_> {
    /// When a component in this record was seen.
    fn at(&self) -> Millis {
        self.time.unwrap_or(0)
    }
}

/// One invocation observation.
struct Invoke {
    kind: InvocationKind,
    via: Via,
    status: InvocationStatus,
    call: String,
    group: Option<String>,
    basis: IdentityBasis,
}

const CHRONOLOGICAL: &str = "chronological state is not proof of exact request exposure";
const INVALIDATED: &str =
    "invalid pre-window component metadata; exposure cannot be reconstructed completely";
const UNCERTAIN: &str = "initial/inherited or post-compaction exposure is incomplete";
const SHAPE_REASON: &str = "recorded request shape gives the system prompt and tools; message attachments are placed by chronology";
const FULL_REASON: &str =
    "serialized text includes wire framing; API tokenization is calibrated, not exact";
const DIGEST_REASON: &str = "request digest/base/keep/tail hashes have no verified content mapping; attachment state is candidate exposure only";
const VISIBILITY_REASONS: [&str; 3] = [
    "call visibility describes the client/version window, not proof of use by each component",
    "no supported client/version establishes complete nested-call or typed-skill recording",
    "historical exposure and current effective enablement are independent",
];

impl ExposureCollector {
    /// A collector whose tool-call counts start at `since`.
    pub fn new(since: Option<Millis>) -> Self {
        Self {
            since,
            calls: IndexMap::default(),
            results: IndexMap::default(),
            names: HashSet::default(),
            call_tools: HashMap::default(),
            held: IndexMap::default(),
            held_epoch: 0,
            epochs: 0,
            held_snapshot: None,
            provided: HashSet::default(),
            components: IndexMap::default(),
            invocations: IndexMap::default(),
            observations: HashMap::default(),
            file: None,
            active: IndexMap::default(),
            pending_removed: IndexSet::default(),
            session: Text::from(""),
            session_cache: None,
            turn: None,
            turn_from_user: false,
            turn_available: false,
            turn_aliases: HashMap::default(),
            client: None,
            version: None,
            uncertain: true,
            deferred: IndexSet::default(),
            digests: HashMap::default(),
            known_skills: HashMap::default(),
            codex_skill_paths: HashMap::default(),
            typed_skills: HashSet::default(),
            synced_skills: HashMap::default(),
            user_aliases: HashMap::default(),
            surfaced: HashMap::default(),
            shapes: HashMap::default(),
            blobs: HashMap::default(),
            blob_epoch: 0,
            prefixes: HashMap::default(),
            invalidated: false,
            pending_tools: HashMap::default(),
            lists: HashSet::default(),
            previous: None,
            views: HashSet::default(),
        }
    }

    /// Invalidate context after tolerated corrupt pre-window metadata, without inventing removals.
    pub fn invalidate(&mut self) {
        self.active.clear();
        self.pending_removed.clear();
        self.deferred.clear();
        self.digests.clear();
        self.pending_tools.clear();
        self.known_skills.clear();
        self.codex_skill_paths.clear();
        self.typed_skills.clear();
        self.synced_skills.clear();
        self.uncertain = true;
        self.turn = None;
        self.turn_from_user = false;
        self.turn_available = false;
        self.invalidated = true;
    }

    /// Observe one raw record transiently, retaining no raw object or content strings.
    ///
    /// `location.file` names `snapshot.file`; it is reused for every retained location.
    /// On error the state is left as far as the record got, as the reader may continue.
    pub fn observe(
        &mut self,
        raw: &Map<String, Value>,
        snapshot: &Snapshot,
        location: &Location,
    ) -> Result<(), ScanError> {
        debug_assert_eq!(&*location.file, snapshot.file);
        if self.file.as_ref().is_none_or(|f| f.name != snapshot.file) {
            self.switch(snapshot);
        }
        let payload = object(raw.get("payload").unwrap_or(&Value::Null));
        let claude = snapshot.harness == Harness::Claude;
        if claude && let Some(Value::String(session)) = raw.get("sessionId") {
            self.session = self.claude_session(session)?;
        }
        if claude {
            self.client = label(field(raw, "entrypoint")).or(self.client.take());
            self.version = label(field(raw, "version")).or(self.version.take());
        }
        let kind = raw.get("type").and_then(Value::as_str);
        if kind == Some("session_meta") {
            self.session = id(present(payload.get("id")).or(payload.get("session_id")))?.into();
            self.client = safe::originator(field(payload, "originator"));
            self.version = label(field(payload, "cli_version"));
        }
        if !claude
            && (kind == Some("turn_context")
                || (kind == Some("event_msg") && is(payload, "type", "task_started")))
        {
            let recorded = payload
                .get("turn_id")
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
                .map(identity);
            if let Some(recorded) = recorded {
                if self.turn_from_user
                    && let Some(inferred) = &self.turn
                {
                    self.turn_aliases.insert(inferred.clone(), recorded.clone());
                    self.turn_available = false;
                } else if self.turn.as_ref() != Some(&recorded) {
                    self.turn_available = true;
                }
                self.turn = Some(recorded);
                self.turn_from_user = false;
            }
        }
        let line = Line {
            raw,
            payload,
            kind,
            snapshot,
            location,
            time: safe::timestamp(field(raw, "timestamp")),
        };
        if kind == Some("compacted")
            || (kind == Some("system") && is(raw, "subtype", "compact_boundary"))
        {
            self.clear(None);
            self.uncertain = true;
            self.deferred.clear();
            self.forget_results();
        }
        if kind == Some("world_state") {
            self.world_state(&line)?;
        }
        if kind == Some("attachment") {
            self.attachment(&line)?;
        }
        if kind == Some("response_item") && is(payload, "type", "additional_tools") {
            for v in array(payload.get("tools"))? {
                let tool = record(Some(v))?;
                let name = text(tool.get("name"))?;
                let category = tool_category(name);
                self.component(
                    New::active(category, name, "provided-additional-tools"),
                    Content::Json(v),
                    line.at(),
                );
                self.provided.insert(component_key(category, name).into());
            }
        }
        if kind == Some("api-request-shape") {
            let shape = record(raw.get("shape"))?;
            let hash = id(raw.get("shapeHash"))?;
            let system = Placed::new(
                Category::Other,
                "wire-system",
                shape.get("system").map_or(Content::Missing, Content::Json),
                None,
            );
            let mut tools = vec![];
            for v in array(shape.get("tools"))? {
                let name = text(record(Some(v))?.get("name"))?;
                tools.push(Placed::new(
                    tool_category(name),
                    name,
                    Content::Json(v),
                    group(name),
                ));
            }
            self.shapes.insert(hash, Arc::new(Shape { system, tools }));
        }
        if kind == Some("api-request-blob") {
            let hash = id(raw.get("hash"))?;
            let characters = blob_characters(raw.get("message"))?;
            if let Some(previous) = self.blobs.insert(hash, characters)
                && previous != characters
            {
                self.blob_epoch += 1;
            }
        }
        if kind == Some("api-request") {
            self.api_request(&line)?;
        }
        self.contents(&line)?;
        self.completions(&line)?;

        let token = kind == Some("token_usage_record")
            || (kind == Some("event_msg") && is(payload, "type", "token_count"));
        if kind == Some("assistant") || token {
            let wire = match raw.get("requestRef") {
                Some(Value::String(_)) if kind == Some("assistant") => {
                    let reference = id(raw.get("requestRef"))?;
                    self.observations
                        .get(&*location.file)
                        .and_then(|o| o.refs.get(&reference))
                        .cloned()
                }
                _ => None,
            };
            let view = match wire {
                Some(view) => view,
                None => self.chronological(),
            };
            let held = self.held_results();
            let observed = self.observations.entry(location.file.clone()).or_default();
            observed.lines.insert(location.line, view);
            observed.results.insert(location.line, held);
            self.pending_removed.clear();
        }
        if !self.active.is_empty() {
            self.uncertain = false;
        }
        Ok(())
    }

    /// Link derived views to deduplicated requests, retaining conflicts instead of choosing a copy.
    pub fn finish(
        self,
        requests: &[Request],
        context: &[Request],
        since: Option<Millis>,
    ) -> ExposureScan {
        let ExposureCollector {
            calls,
            results,
            components,
            invocations,
            observations,
            mut lists,
            mut views,
            turn_aliases,
            ..
        } = self;
        let mut previous: Option<Ids> = None;
        let mut inherited = |lists: &mut HashSet<Ids>, view: &ExposureView| {
            let ids = view
                .components
                .iter()
                .chain(view.candidates.iter())
                .filter(|id| since.is_some_and(|s| components[&***id].first_seen < s))
                .cloned()
                .collect();
            let ids = intern_list(lists, previous.as_ref(), ids);
            previous = Some(ids.clone());
            ids
        };
        // Final views by the one observed view they derive from.
        let mut settled: HashMap<usize, Arc<ExposureView>> = HashMap::default();
        let mut exposures = Vec::with_capacity(requests.len());
        for r in requests {
            let found: Vec<&Arc<ExposureView>> = r
                .locations
                .iter()
                .filter_map(|l| {
                    let file = observations.get(&*l.file)?;
                    let observed = r.reference.as_ref().and_then(|x| file.refs.get(&**x));
                    observed.or_else(|| file.lines.get(&l.line))
                })
                .collect();
            let first = found.first().copied();
            let view = match first {
                Some(first) if found.iter().all(|v| Arc::ptr_eq(v, first)) => settled
                    .entry(Arc::as_ptr(first) as usize)
                    .or_insert_with(|| {
                        let view = ExposureView {
                            inherited: inherited(&mut lists, first),
                            ..(**first).clone()
                        };
                        intern_view(&mut views, view)
                    })
                    .clone(),
                Some(first) if found.iter().all(|v| equivalent(v, first)) => {
                    let removed = unique(found.iter().flat_map(|v| v.removed.iter()));
                    let view = ExposureView {
                        removed: intern_list(&mut lists, None, removed),
                        inherited: inherited(&mut lists, first),
                        ..(**first).clone()
                    };
                    intern_view(&mut views, view)
                }
                _ => {
                    let candidates = unique(
                        found
                            .iter()
                            .flat_map(|v| v.components.iter().chain(v.candidates.iter())),
                    );
                    let removed = unique(found.iter().flat_map(|v| v.removed.iter()));
                    let deferred = unique(found.iter().flat_map(|v| v.deferred.iter()));
                    let mut view = ExposureView {
                        components: intern_list(&mut lists, None, vec![]),
                        candidates: intern_list(&mut lists, None, candidates),
                        removed: intern_list(&mut lists, None, removed),
                        inherited: intern_list(&mut lists, None, vec![]),
                        deferred: intern_list(&mut lists, None, deferred),
                        linkage: Linkage::Unknown,
                        coverage: TextCoverage::Unknown,
                        references: None,
                        complete_characters: None,
                        prefix: 0,
                        wire_model: None,
                        reasons: vec![if found.is_empty() {
                            "no observed request context".into()
                        } else {
                            "copied/inherited histories disagree on exposure".into()
                        }],
                    };
                    view.inherited = inherited(&mut lists, &view);
                    intern_view(&mut views, view)
                }
            };
            // Copies hold the same conversation, so the first location's output stands for all.
            let results = r.locations.iter().find_map(|l| {
                observations
                    .get(&*l.file)
                    .and_then(|file| file.results.get(&l.line))
                    .cloned()
            });
            exposures.push(Exposure {
                request: r.id.clone(),
                view,
                results,
            });
        }
        drop(views);
        drop(lists);

        // Uses belong to their sessions' resolved owners, which T3 ownership can narrow
        // below the profiles sharing the transcript's home. Earlier requests count, since a
        // session's use can fall inside the window after its last request.
        let mut owners: HashMap<(Harness, &str), Vec<Text>> = HashMap::default();
        for r in context.iter().chain(requests) {
            let owner = owners.entry((r.harness, &r.session)).or_default();
            for p in r.profiles.iter() {
                if !owner.iter().any(|o| **o == **p) {
                    owner.push(p.as_str().into());
                }
            }
        }
        // A session without requests keeps the profiles its transcripts gave every copy.
        let owned = |harness: Harness, sessions: &[Text], profiles: &mut Vec<Text>| {
            let found: Vec<_> = sessions
                .iter()
                .map(|s| owners.get(&(harness, &**s)))
                .collect();
            if found.iter().all(Option::is_some) {
                profiles.clear();
            }
            for p in found.into_iter().flatten().flatten() {
                if !profiles.contains(p) {
                    profiles.push(p.clone());
                }
            }
        };
        let calls = calls.into_values().map(|mut c| {
            owned(c.harness, &c.sessions, &mut c.profiles);
            c
        });
        let results = results.into_values().map(|mut r| {
            owned(r.harness, &r.sessions, &mut r.profiles);
            r
        });

        let visibility = visibility(requests, &exposures, &invocations, since);

        let mut used = vec![false; components.len()];
        let mut queue = vec![];
        // Lists are interned, so each distinct one is visited once.
        let mut seen = HashSet::default();
        for e in &exposures {
            let v = &e.view;
            let lists = [
                &v.components,
                &v.candidates,
                &v.removed,
                &v.inherited,
                &v.deferred,
            ];
            for id in lists
                .into_iter()
                .filter(|list| seen.insert(list.address()))
                .flat_map(|list| list.iter())
            {
                let index = components.get_index_of(&**id).expect("observed component");
                if !std::mem::replace(&mut used[index], true) {
                    queue.push(index);
                }
            }
        }
        while let Some(index) = queue.pop() {
            for member in &components[index].members {
                let index = components.get_index_of(member.as_str()).expect("member");
                if !std::mem::replace(&mut used[index], true) {
                    queue.push(index);
                }
            }
        }
        ExposureScan {
            components: components
                .into_values()
                .zip(used)
                .filter(|&(_, used)| used)
                .map(|(c, _)| c)
                .collect(),
            requests: exposures,
            invocations: invocations
                .into_values()
                .filter(|i| since.is_none_or(|s| i.time >= s))
                .map(|mut i| {
                    if let Some(recorded) = i.turn.as_ref().and_then(|turn| turn_aliases.get(turn))
                    {
                        i.turn = Some(recorded.clone());
                    }
                    let found: Vec<_> = i
                        .sessions
                        .iter()
                        .map(|s| owners.get(&(i.harness, s.as_str())))
                        .collect();
                    if found.iter().all(Option::is_some) {
                        i.profiles.clear();
                    }
                    for p in found.into_iter().flatten().flatten() {
                        if !i.profiles.iter().any(|o| **o == **p) {
                            i.profiles.push(p.to_string());
                        }
                    }
                    i
                })
                .collect(),
            visibility,
            tool_calls: tool_calls(calls),
            tool_results: tool_results(results),
        }
    }

    /// Start a new conversation with no tool output, as after a compaction or in another file.
    fn forget_results(&mut self) {
        self.held.clear();
        self.epochs += 1;
        self.held_epoch = self.epochs;
        self.held_snapshot = None;
    }

    /// The tool output the conversation holds now, shared while it is unchanged.
    fn held_results(&mut self) -> Arc<ResultTotals> {
        let epoch = self.held_epoch;
        let held = &self.held;
        self.held_snapshot
            .get_or_insert_with(|| {
                Arc::new(ResultTotals {
                    epoch,
                    tools: held
                        .iter()
                        .map(|(tool, &(characters, opaque))| (tool.clone(), characters, opaque))
                        .collect(),
                })
            })
            .clone()
    }

    /// Count a call to `name`, once per call id across copied files.
    fn call(&mut self, id: &str, name: &str, time: Option<Millis>) {
        self.call_tools
            .insert(id.to_string(), intern(&mut self.names, &output_owner(name)));
        let Some(time) = time.filter(|&t| self.since.is_none_or(|s| t >= s)) else {
            return;
        };
        let file = self.file.as_ref().expect("observing a file");
        let entry = match self.calls.get_mut(id) {
            Some(entry) => entry,
            None => {
                let name = intern(&mut self.names, name);
                let entry = ToolCall {
                    name,
                    harness: file.harness,
                    sessions: vec![],
                    profiles: vec![],
                    time,
                };
                self.calls.entry(id.to_string()).or_insert(entry)
            }
        };
        if !entry.sessions.contains(&self.session) {
            entry.sessions.push(self.session.clone());
        }
        for profile in &file.profiles {
            if !entry.profiles.contains(profile) {
                entry.profiles.push(profile.clone());
            }
        }
    }

    /// Add one tool result to the conversation, and to its tool's window totals.
    fn result(&mut self, id: &str, output: Option<&Value>, time: Option<Millis>) {
        let (characters, opaque) = output_characters(output);
        let tool = match self.call_tools.get(id) {
            Some(tool) => tool.clone(),
            None => intern(&mut self.names, UNKNOWN_TOOL),
        };
        let held = self.held.entry(tool.clone()).or_insert((0, false));
        held.0 += characters;
        held.1 |= opaque;
        self.held_snapshot = None;
        if time.is_none_or(|t| self.since.is_some_and(|s| t < s)) {
            return;
        }
        let file = self.file.as_ref().expect("observing a file");
        let entry = self
            .results
            .entry(id.to_string())
            .or_insert_with(|| ToolResult {
                tool,
                harness: file.harness,
                sessions: vec![],
                profiles: vec![],
                characters,
            });
        if !entry.sessions.contains(&self.session) {
            entry.sessions.push(self.session.clone());
        }
        for profile in &file.profiles {
            if !entry.profiles.contains(profile) {
                entry.profiles.push(profile.clone());
            }
        }
    }

    fn switch(&mut self, snapshot: &Snapshot) {
        self.file = Some(File {
            name: snapshot.file.clone(),
            harness: snapshot.harness,
            profiles: snapshot
                .profiles
                .iter()
                .map(|p| Text::from(p.as_str()))
                .collect(),
            subagent: is_subagent(&snapshot.file),
            basename: basename(&snapshot.file).to_string(),
        });
        self.active.clear();
        self.pending_removed.clear();
        self.session = identity(&snapshot.file).into();
        self.session_cache = None;
        self.turn = None;
        self.turn_from_user = false;
        self.turn_available = false;
        self.client = None;
        self.version = None;
        self.uncertain = true;
        self.deferred.clear();
        self.digests.clear();
        self.pending_tools.clear();
        self.known_skills.clear();
        self.codex_skill_paths.clear();
        self.typed_skills.clear();
        self.synced_skills.clear();
        self.user_aliases.clear();
        self.surfaced.clear();
        self.blobs.clear();
        self.prefixes.clear();
        self.invalidated = false;
        self.call_tools.clear();
        self.provided.clear();
        self.forget_results();
    }

    fn file(&self) -> &File {
        self.file.as_ref().expect("observing a file")
    }

    fn claude_session(&mut self, session: &str) -> Result<Text, ScanError> {
        if let Some((raw, derived)) = &self.session_cache
            && raw == session
        {
            return Ok(derived.clone());
        }
        let base = safe::id(&Value::from(session))?;
        let file = self.file();
        let derived: Text = if file.subagent {
            identity(&format!("{base}:{}", file.basename)).into()
        } else {
            base.into()
        };
        self.session_cache = Some((session.to_string(), derived.clone()));
        Ok(derived)
    }

    /// Total characters of a request's message blobs, or None unless every one is known text.
    fn message_characters(&mut self, items: &Ids) -> Option<i64> {
        let (mut length, mut characters, mut opaque) = (0, 0, false);
        if let Some((base, keep)) = items.base()
            && let Some(prefix) = self.prefixes.get(&base.address())
            && prefix.epoch == self.blob_epoch
        {
            if prefix.length < keep {
                (length, characters, opaque) = (prefix.length, prefix.characters, prefix.opaque);
            } else if prefix.length == keep {
                (length, characters) = (keep, prefix.characters);
            }
        }
        if !opaque {
            for digest in items.iter_from(length) {
                match self.blobs.get(&**digest) {
                    Some(Some(c)) => {
                        characters += c;
                        length += 1;
                    }
                    Some(None) => {
                        opaque = true;
                        break;
                    }
                    None => break,
                }
            }
        }
        let prefix = TextPrefix {
            _items: items.clone(),
            epoch: self.blob_epoch,
            length,
            characters,
            opaque,
        };
        self.prefixes.insert(items.address(), prefix);
        (length == items.len()).then_some(characters)
    }

    /// One shared copy of `ids`, extending the same list of the previous view.
    fn list(&mut self, field: fn(&ExposureView) -> &Ids, ids: Vec<Text>) -> Ids {
        intern_list(&mut self.lists, self.previous.as_deref().map(field), ids)
    }

    /// Record one sighting of a component revision and return its id.
    fn component(&mut self, new: New<'_>, content: Content<'_>, at: Millis) -> Text {
        let key: Text = component_key(new.category, new.name).into();
        let size = content.measure();
        let revision = identity(&format!("{key}:{}", size.fingerprint));
        let instruction_size = match (&content, new.category) {
            (Content::Text(text), Category::Instructions) => Some(crate::model::InstructionSize {
                bytes: text.len(),
                lines: text.lines().count(),
            }),
            _ => None,
        };
        let id = self.place(new, &key, &revision, &size, at);
        self.components[&*id].instruction_size = instruction_size;
        id
    }

    fn place(&mut self, new: New<'_>, key: &Text, revision: &str, size: &Size, at: Millis) -> Text {
        let revision = match self.components.get_full_mut(revision) {
            Some((_, id, existing)) => {
                existing.first_seen = existing.first_seen.min(at);
                id.clone()
            }
            None => {
                let revision = Text::from(revision);
                let category = new.category;
                let skill = matches!(category, Category::SkillListing | Category::SkillBody);
                let mcp = matches!(
                    category,
                    Category::McpDiscovery | Category::McpSchema | Category::McpResult
                );
                let member = new.role == Role::Member;
                let component = Component {
                    instruction_size: None,
                    size: size.clone(),
                    id: revision.to_string(),
                    key: key.to_string(),
                    name: new.name.to_string(),
                    category,
                    owner: if skill { Owner::Skills } else { Owner::Context },
                    group: match new.grouping {
                        Grouping::Name => group(new.name),
                        Grouping::Fixed(g) => g.map(str::to_string),
                    },
                    provenance: new.provenance.to_string(),
                    first_seen: at,
                    current: crate::model::Current::Unresolved,
                    scope: if !member
                        && matches!(category, Category::SkillListing | Category::Other)
                    {
                        Scope::Aggregate
                    } else {
                        Scope::Component
                    },
                    members: vec![],
                    skill_path: None,
                    targets: new.targets.unwrap_or_else(|| {
                        if member || category == Category::SkillBody {
                            vec![target("skill", new.name)]
                        } else if mcp {
                            vec![target("mcp", new.name)]
                        } else {
                            vec![]
                        }
                    }),
                };
                self.components.insert(revision.clone(), component);
                revision
            }
        };
        match new.role {
            Role::Member => {}
            Role::Deferred => {
                self.deferred.insert(revision.clone());
            }
            Role::Active => {
                if let Some((prior, _)) = self.active.get(key)
                    && *prior != revision
                {
                    self.pending_removed.insert(prior.clone());
                }
                self.active
                    .insert(key.clone(), (revision.clone(), new.category));
            }
        }
        revision
    }

    fn place_shaped(&mut self, placed: &Placed, provenance: &'static str, at: Millis) -> Text {
        let new = New {
            category: placed.category,
            name: &placed.name,
            provenance,
            role: Role::Active,
            grouping: Grouping::Fixed(placed.group.as_deref()),
            targets: None,
        };
        self.place(new, &placed.key, &placed.revision, &placed.size, at)
    }

    /// A listed skill name, and the short name `/pdf` that also reaches `anthropic-skills:pdf`.
    fn know_skill(&mut self, name: &str) {
        let key = name.to_ascii_lowercase();
        if self.file.as_ref().expect("observing a file").harness == Harness::Claude
            && let Some(short) = synced_short(&key)
            && !claude_command(short)
        {
            self.synced_skills
                .insert(short.to_string(), name.to_string());
        }
        self.known_skills.insert(key, name.to_string());
    }

    fn knows_skill(&self, name: &str) -> bool {
        let key = name.to_ascii_lowercase();
        self.known_skills.contains_key(&key)
            || self.synced_skills.contains_key(&key)
            || self.typed_skills.contains(&key)
    }

    /// The skill a name reaches in the current catalog: another skill keeps its short name.
    fn skill_name<'a>(&'a self, name: &'a str) -> &'a str {
        let key = name.to_ascii_lowercase();
        self.known_skills
            .get(&key)
            .or_else(|| self.synced_skills.get(&key))
            .map_or(name, String::as_str)
    }

    /// A rendered skill listing and one member span per `- name: description` line.
    fn listing(&mut self, key: &str, body: &str, provenance: &'static str, at: Millis) -> Text {
        let aggregate = self.component(
            New::active(Category::SkillListing, key, provenance),
            Content::Text(body),
            at,
        );
        let mut members = vec![];
        let entries = listing_entries(body);
        let codex = self.file.as_ref().expect("observing a file").harness == Harness::Codex;
        let paths = if codex {
            crate::skills::paths(body, &entries)
        } else {
            vec![None; entries.len()]
        };
        for ((name, span), path) in entries.into_iter().zip(paths) {
            self.know_skill(name);
            let new = New {
                role: Role::Member,
                ..New::active(Category::SkillListing, name, "rendered-listing-entry-span")
            };
            let member = if codex {
                self.codex_skill_paths
                    .entry(name.to_string())
                    .and_modify(|prior| {
                        if prior.0 != path {
                            prior.0 = None;
                        }
                        prior.1 += 1;
                    })
                    .or_insert_with(|| (path.clone(), 1));
                let key: Text =
                    component_key(Category::SkillListing, &format!("{name}@{path:?}")).into();
                let size = Content::Text(span).measure();
                let revision = identity(&format!("{key}:{}", size.fingerprint));
                let member = self.place(new, &key, &revision, &size, at);
                self.components[&*member].skill_path = path;
                member
            } else {
                self.component(new, Content::Text(span), at)
            };
            members.push(member.to_string());
        }
        self.components[&*aggregate].members = members;
        aggregate
    }

    fn clear(&mut self, category: Option<Category>) {
        if category.is_none_or(|category| category == Category::SkillListing) {
            self.known_skills.clear();
            self.codex_skill_paths.clear();
            self.synced_skills.clear();
        }
        let pending = &mut self.pending_removed;
        let provided = &self.provided;
        self.active.retain(|key, (revision, c)| {
            if category.is_none_or(|category| category == *c) && !provided.contains(key) {
                pending.insert(revision.clone());
                false
            } else {
                true
            }
        });
    }

    fn remove(&mut self, category: Category, name: &str) {
        if let Some((previous, _)) = self
            .active
            .shift_remove(component_key(category, name).as_str())
        {
            self.pending_removed.insert(previous);
        }
    }

    fn invoke(&mut self, name: &str, mut input: Invoke, line: &Line<'_>) -> Result<(), ScanError> {
        let time = line
            .time
            .ok_or_else(|| ScanError::new("missing invocation timestamp"))?;
        let target = if is_skill(input.kind) {
            let name = self.skill_name(name);
            let selected = (input.kind == InvocationKind::SkillRequest)
                .then(|| self.codex_skill_paths.get(name))
                .flatten();
            let ambiguous = selected.is_some_and(|(path, count)| path.is_none() && *count > 1);
            let name = selected
                .and_then(|(path, _)| path.as_deref())
                .unwrap_or(name);
            input.group = input.group.or_else(|| group(name));
            target(
                if ambiguous {
                    "skill-ambiguous"
                } else if selected.is_some_and(|(path, _)| path.is_none()) {
                    "skill-unresolved"
                } else {
                    "skill"
                },
                &crate::skills::normalize(name),
            )
        } else {
            target("mcp", name)
        };

        self.invocation(target, input, time);
        Ok(())
    }

    fn invocation(&mut self, target: String, input: Invoke, time: Millis) {
        let file = self.file.as_ref().expect("observing a file");
        let id = identity(&format!(
            "{}:{}:{}",
            harness_name(file.harness),
            input.call,
            kind_name(input.kind)
        ));
        if let Some(prior) = self.invocations.get_mut(&id) {
            if input.via == Via::Typed {
                prior.target = target;
                prior.via = input.via;
            }
            if !prior.sessions.iter().any(|s| **s == *self.session) {
                prior.sessions.push(self.session.to_string());
            }
            for profile in &file.profiles {
                if !prior.profiles.iter().any(|p| **p == **profile) {
                    prior.profiles.push(profile.to_string());
                }
            }
            if matches!(
                input.status,
                InvocationStatus::Completed | InvocationStatus::Failed
            ) {
                prior.status = input.status;
            }
            return;
        }
        let invocation = Invocation {
            id: id.clone(),
            target,
            group: input.group,
            kind: input.kind,
            status: input.status,
            via: input.via,
            session: self.session.to_string(),
            sessions: vec![self.session.to_string()],
            identity_basis: input.basis,
            turn: (file.harness == Harness::Codex)
                .then(|| self.turn.clone())
                .flatten(),
            harness: file.harness,
            client: self.client.clone(),
            version: self.version.clone(),
            time,
            profiles: file.profiles.iter().map(|p| p.to_string()).collect(),
        };
        self.invocations.insert(id, invocation);
    }

    fn view(&mut self, view: ExposureView) -> Arc<ExposureView> {
        let view = intern_view(&mut self.views, view);
        self.previous = Some(view.clone());
        view
    }

    fn chronological(&mut self) -> Arc<ExposureView> {
        let current: Vec<Text> = self.active.values().map(|(r, _)| r.clone()).collect();
        let any = !current.is_empty();
        let mut reasons = vec![CHRONOLOGICAL.to_string()];
        if self.invalidated {
            reasons.push(INVALIDATED.into());
        }
        if self.uncertain {
            reasons.push(UNCERTAIN.into());
        }
        let removed = self.pending_removed.iter().cloned().collect();
        let deferred = self.deferred.iter().cloned().collect();
        let view = ExposureView {
            components: self.list(|v| &v.components, vec![]),
            candidates: self.list(|v| &v.candidates, current),
            removed: self.list(|v| &v.removed, removed),
            inherited: self.list(|v| &v.inherited, vec![]),
            deferred: self.list(|v| &v.deferred, deferred),
            linkage: if any {
                Linkage::Chronological
            } else {
                Linkage::Unknown
            },
            coverage: if any {
                TextCoverage::Partial
            } else {
                TextCoverage::Unknown
            },
            references: None,
            complete_characters: None,
            prefix: 0,
            wire_model: None,
            reasons,
        };
        self.view(view)
    }

    fn world_state(&mut self, line: &Line<'_>) -> Result<(), ScanError> {
        let p = line.payload;
        let at = line.at();
        let state = record(p.get("state"))?;
        match p.get("full") {
            None | Some(Value::Bool(_)) => {}
            Some(_) => return Err(ScanError::new("invalid world state mode")),
        }
        if p.get("full") == Some(&Value::Bool(true)) {
            self.clear(None);
        }
        for (field, category) in [
            ("host_skills", Category::SkillListing),
            ("agents_md", Category::Instructions),
            (
                "managed_developer_instructions",
                Category::ManagedInstructions,
            ),
        ] {
            let Some(value) = state.get(field) else {
                continue;
            };
            if value.is_null() {
                self.clear(Some(category));
                continue;
            }
            let v = record(Some(value))?;
            let body = v.get(if field == "host_skills" {
                "body"
            } else {
                "text"
            });
            // Structured entries describe availability/fingerprints, not rendered sizes.
            if let Some(body) = present(body) {
                self.clear(Some(category));
                let body = text(Some(body))?;
                if category == Category::SkillListing {
                    self.listing(field, body, "world-state-rendered", at);
                } else {
                    self.component(
                        New::active(category, field, "world-state-rendered"),
                        Content::Text(body),
                        at,
                    );
                }
            } else if !v.is_empty() {
                self.clear(Some(category));
                self.component(
                    New::active(category, field, "world-state-metadata-only"),
                    Content::Opaque(value),
                    at,
                );
            }
        }
        for (field, category) in [
            ("tools", Category::McpDiscovery),
            ("top_level_tools", Category::McpSchema),
        ] {
            if !state.contains_key(field) {
                continue;
            }
            for (name, value) in js_entries(record(state.get(field))?) {
                if value.is_null() {
                    self.remove(category, name);
                    continue;
                }
                let body = text(Some(value))?;
                let (content, provenance) = if field == "tools" {
                    (Content::Text(body), "deferred-namespace-description")
                } else {
                    (Content::Opaque(value), "opaque-schema-identity")
                };
                self.component(New::active(category, name, provenance), content, at);
            }
        }
        // Boolean policy flags cannot establish the rendered plugin/app instruction body.
        for (field, category) in [
            ("plugins_instructions", Category::PluginInstructions),
            ("apps_instructions", Category::AppInstructions),
        ] {
            if let Some(value) = state.get(field) {
                self.component(
                    New::active(category, field, "world-state-policy-only"),
                    Content::Opaque(value),
                    at,
                );
            }
        }
        Ok(())
    }

    fn attachment(&mut self, line: &Line<'_>) -> Result<(), ScanError> {
        let raw = line.raw;
        let at = line.at();
        let a = record(raw.get("attachment"))?;
        let kind = a.get("type");
        let kind_is = |expected: &str| kind.and_then(Value::as_str) == Some(expected);
        if kind_is("skill_listing") {
            self.clear(Some(Category::SkillListing));
            for name in array(a.get("names"))? {
                self.know_skill(text(Some(name))?);
            }
            self.listing(
                "listing",
                text(a.get("content"))?,
                "rendered-skill-listing",
                at,
            );
        } else if kind_is("instructions") {
            for v in array(a.get("files"))? {
                let f = record(Some(v))?;
                let path = text(f.get("path"))?;
                let content = text(f.get("content"))?;
                self.component(
                    New::active(Category::Instructions, path, "instruction-attachment"),
                    Content::Text(content),
                    at,
                );
            }
        } else if kind_is("prompt_snapshot") {
            self.clear(Some(Category::Other));
            self.clear(Some(Category::McpSchema));
            let prompt = texts(array(a.get("systemPrompt"))?)?.join("\n");
            self.component(
                New::active(Category::Other, "system-prompt", "prompt-snapshot-system"),
                Content::Text(&prompt),
                at,
            );
            for v in array(a.get("tools"))? {
                let name = text(record(Some(v))?.get("name"))?;
                self.component(
                    New::active(Category::McpSchema, name, "prompt-snapshot-tool"),
                    Content::Json(v),
                    at,
                );
            }
        } else if matches!(
            js_string(kind).as_str(),
            "deferred_tools_delta" | "mcp_instructions_delta" | "agent_listing_delta"
        ) {
            let agents = kind_is("agent_listing_delta");
            let category = if kind_is("deferred_tools_delta") {
                Category::McpDiscovery
            } else if agents {
                Category::Agents
            } else {
                Category::PluginInstructions
            };
            let names = array(a.get(if agents { "addedTypes" } else { "addedNames" }))?;
            let lines = array(a.get(if kind_is("mcp_instructions_delta") {
                "addedBlocks"
            } else {
                "addedLines"
            }))?;
            let removed = array(a.get(if agents {
                "removedTypes"
            } else {
                "removedNames"
            }))?;
            for name in removed {
                self.remove(category, text(Some(name))?);
            }
            if a.get("isInitial") == Some(&Value::Bool(true)) {
                self.clear(Some(category));
            }
            // Preserve aggregate expansion when there is no verified one-to-one mapping.
            if names.len() == lines.len() {
                for (name, body) in names.iter().zip(lines) {
                    let name = text(Some(name))?;
                    let body = text(Some(body))?;
                    self.component(
                        New::active(category, name, "attachment-delta"),
                        Content::Text(body),
                        at,
                    );
                }
            } else if !lines.is_empty() {
                let name = format!("{}:{}", js_string(kind), line.location.line);
                let body = texts(lines)?.join("\n");
                self.component(
                    New::active(category, &name, "unmapped-rendered-delta"),
                    Content::Text(&body),
                    at,
                );
            }
            for v in array(a.get("surfacedDefinitions"))? {
                let definition = record(Some(v))?;
                let name = text(definition.get("name"))?;
                self.remove(Category::McpDiscovery, name);
                let revision = if definition.contains_key("sameAs") {
                    let reference = (id(definition.get("sameAs"))?, name.to_string());
                    let same = self.surfaced.get(&reference).cloned().ok_or_else(|| {
                        ScanError::new("unresolved surfaced definition reference")
                    })?;
                    let component = &self.components[&*same];
                    let key = Text::from(component.key.as_str());
                    let category = component.category;
                    if let Some((prior, _)) = self.active.get(&key)
                        && *prior != same
                    {
                        self.pending_removed.insert(prior.clone());
                    }
                    self.active.insert(key, (same.clone(), category));
                    same
                } else {
                    let body = definition.get("definition");
                    record(body)?;
                    let body = body.expect("checked definition");
                    self.component(
                        New::active(Category::McpSchema, name, "surfaced-tool-definition"),
                        Content::Json(body),
                        at,
                    )
                };
                let uuid = id(raw.get("uuid"))?;
                self.surfaced.insert((uuid, name.to_string()), revision);
            }
        } else if kind_is("deferred_tools_record") {
            // Deferred definitions exist locally but are not necessarily in the model input.
            for v in array(a.get("entries"))? {
                let tool = record(Some(v))?;
                let deferred = tool.get("defer_loading") == Some(&Value::Bool(true));
                let new = New {
                    category: Category::McpSchema,
                    name: text(tool.get("name"))?,
                    provenance: if deferred {
                        "deferred-definition-not-exposed"
                    } else {
                        "eager-tool-record"
                    },
                    role: if deferred {
                        Role::Deferred
                    } else {
                        Role::Active
                    },
                    grouping: Grouping::Fixed(None),
                    targets: None,
                };
                self.component(new, Content::Json(v), at);
            }
        } else if let Some(rendered) = raw.get("rendered") {
            // Unknown attachment categories still contribute visible derived evidence.
            let mut parts = vec![];
            for v in array(Some(rendered))? {
                let content = record(Some(v))?.get("content");
                parts.push(match content {
                    Some(Value::String(s)) => s.clone(),
                    _ => {
                        let mut blocks = vec![];
                        for block in array(content)? {
                            blocks.push(text(record(Some(block))?.get("text"))?);
                        }
                        blocks.join("\n")
                    }
                });
            }
            let body = parts.join("\n");
            let name = match present(raw.get("uuid")) {
                Some(uuid) => js_string(Some(uuid)),
                None => line.location.line.to_string(),
            };
            self.component(
                New::active(Category::Other, &name, "unclassified-rendered-attachment"),
                Content::Text(&body),
                at,
            );
        }
        Ok(())
    }

    /// Full Messages API parameters or a recorded request shape prove wire exposure.
    fn api_request(&mut self, line: &Line<'_>) -> Result<(), ScanError> {
        let raw = line.raw;
        let at = line.at();
        let params = object(field(raw, "params"));
        let full = matches!(params.get("messages"), Some(Value::Array(_)))
            && params.contains_key("system")
            && matches!(params.get("tools"), Some(Value::Array(_)));
        let base = match raw.get("base") {
            Some(Value::String(_)) => Some(id(raw.get("base"))?),
            _ => None,
        };
        let keep = safe::count(raw.get("keep"))?;
        let mut tail = vec![];
        for v in array(raw.get("tail"))? {
            tail.push(Text::from(safe::id(v)?));
        }
        let tail: Arc<[Text]> = tail.into();
        let prior = base.as_ref().and_then(|b| self.digests.get(b));
        let items = match (&base, prior) {
            (Some(_), Some(prior)) => {
                let keep = keep
                    .map(|k| k as u64)
                    .filter(|&k| k <= prior.len() as u64)
                    .ok_or_else(|| ScanError::new("invalid request digest prefix length"))?;
                Some(Ids::new(
                    Some((prior, keep as usize)),
                    tail.iter().cloned().collect(),
                ))
            }
            (Some(_), None) => None,
            (None, _) => Some(Ids::new(None, tail.iter().cloned().collect())),
        };
        if let Some(Value::String(_)) = raw.get("requestDigest")
            && let Some(items) = &items
        {
            self.digests
                .insert(id(raw.get("requestDigest"))?, items.clone());
        }
        let shape = match raw.get("shapeHash") {
            Some(Value::String(_)) if !full => self.shapes.get(&id(raw.get("shapeHash"))?).cloned(),
            _ => None,
        };
        let mut components = vec![];
        let mut complete = None;
        let mut prefix = 0;
        if let Some(shape) = &shape {
            // The prompt cache prefix runs tools, then system, then messages.
            self.clear(Some(Category::ToolSchema));
            self.clear(Some(Category::McpSchema));
            for tool in &shape.tools {
                components.push(self.place_shaped(tool, "shape-tool", at));
            }
            components.push(self.place_shaped(&shape.system, "shape-system", at));
            let leading: HashSet<&str> = components.iter().map(|c| &**c).collect();
            let rest: Vec<Text> = self
                .active
                .values()
                .map(|(r, _)| r)
                .filter(|r| !leading.contains(&***r))
                .cloned()
                .collect();
            components.extend(rest);
            let messages = items
                .as_ref()
                .and_then(|items| self.message_characters(items));
            if let Some(messages) = messages {
                // A shape without a system prompt counts it as zero, as JavaScript adds null.
                complete = Some(
                    messages
                        + shape.system.size.characters.unwrap_or(0)
                        + shape
                            .tools
                            .iter()
                            .map(|t| t.size.characters.unwrap_or(0))
                            .sum::<i64>(),
                );
            }
            prefix = shape.tools.len() + 1;
        }
        if full {
            // Listed in cache-prefix order: tools, then system.
            let tools = array(params.get("tools"))?;
            for v in tools {
                let name = text(record(Some(v))?.get("name"))?;
                components.push(self.component(
                    New::active(tool_category(name), name, "wire-tool"),
                    Content::Json(v),
                    at,
                ));
            }
            components.push(self.component(
                New::active(Category::Other, "wire-system", "wire-system"),
                Content::Json(&params["system"]),
                at,
            ));
            if complete_text(params)? {
                let mut sink = Chars(0);
                sink.put("{\"system\":");
                json(&params["system"], &mut sink);
                sink.put(",\"messages\":");
                json(&params["messages"], &mut sink);
                sink.put(",\"tools\":");
                json(&params["tools"], &mut sink);
                sink.put("}");
                complete = Some(sink.0 as i64);
            }
            prefix = tools.len() + 1;
        }
        let wired = full || shape.is_some();
        let candidates = if wired {
            vec![]
        } else {
            self.active.values().map(|(r, _)| r.clone()).collect()
        };
        let removed = self.pending_removed.iter().cloned().collect();
        let deferred = self.deferred.iter().cloned().collect();
        let reason = if shape.is_some() {
            SHAPE_REASON
        } else if full {
            FULL_REASON
        } else {
            DIGEST_REASON
        };
        let view = ExposureView {
            components: self.list(|v| &v.components, components),
            candidates: self.list(|v| &v.candidates, candidates),
            removed: self.list(|v| &v.removed, removed),
            inherited: self.list(|v| &v.inherited, vec![]),
            deferred: self.list(|v| &v.deferred, deferred),
            linkage: if wired {
                Linkage::Wire
            } else {
                Linkage::Unknown
            },
            coverage: if complete.is_none() {
                TextCoverage::Partial
            } else {
                TextCoverage::CompleteText
            },
            references: Some(References {
                base,
                keep,
                tail: Ids::new(None, tail),
                items,
            }),
            complete_characters: complete,
            prefix,
            wire_model: label(field(params, "model")),
            reasons: vec![reason.to_string()],
        };
        let reference = id(raw.get("id"))?;
        let view = self.view(view);
        self.observations
            .entry(line.location.file.clone())
            .or_default()
            .refs
            .insert(reference, view);
        Ok(())
    }

    /// Message content blocks: requested skills, tool calls and their results.
    fn contents(&mut self, line: &Line<'_>) -> Result<(), ScanError> {
        let (raw, p, kind) = (line.raw, line.payload, line.kind);
        let message = object(field(raw, "message"));
        let item = if kind == Some("event_msg") && is(p, "type", "item_completed") {
            record(p.get("item"))?
        } else {
            p
        };
        let codex = line.snapshot.harness == Harness::Codex;
        // api-request-blob lines are hash-addressed copies of turns already recorded as user/assistant lines.
        let contents = if !codex {
            if kind == Some("user") || kind == Some("assistant") {
                message.get("content")
            } else {
                None
            }
        } else {
            present(item.get("content")).or_else(|| {
                if is(p, "type", "user_message") {
                    p.get("message")
                } else {
                    None
                }
            })
        };
        let user = kind == Some("user")
            || is(item, "role", "user")
            || is(item, "type", "UserMessage")
            || (kind == Some("event_msg") && is(p, "type", "user_message"));
        let synthetic;
        let blocks: &[Value] = match contents {
            Some(Value::String(s)) => {
                synthetic = [serde_json::json!({ "type": "text", "text": s })];
                &synthetic
            }
            _ => array(contents)?,
        };
        let native_user = is(p, "type", "user_message") || is(item, "type", "UserMessage");
        let user = user
            && raw.get("isMeta") != Some(&Value::Bool(true))
            && (!codex
                || native_user
                || blocks.iter().any(|v| {
                    let b = object(v);
                    is(b, "type", "skill")
                        || b.get("text").and_then(Value::as_str).is_some_and(|t| {
                            let t = t.trim_start();
                            !t.starts_with("<skill>") && !t.starts_with("<user_shell_command>")
                        })
                }));
        let mut block_ids = vec![];
        let mut typed_aliases = HashMap::default();
        for (index, v) in blocks.iter().enumerate() {
            let b = object(v);
            let source = match present(raw.get("uuid")).or(present(item.get("id"))) {
                Some(value) => js_string(Some(value)),
                None => format!("{}:{}", line.snapshot.file, line.location.line),
            };
            let mut call = format!("{source}:{index}");
            let mut basis = IdentityBasis::Recorded;
            let block_text = b.get("text").and_then(Value::as_str);
            if user
                && codex
                && let Some(t) = block_text
            {
                // Native aliases lack a shared message id. Match their nth equal text occurrence,
                // keeping repeated messages in each source separate and disclosing the inference.
                let signature = identity(&format!("{}:{t}", self.session));
                let source = format!(
                    "{}:{}",
                    js_string(raw.get("type")),
                    js_string(p.get("type"))
                );
                let sources = self.user_aliases.entry(signature.clone()).or_default();
                let occurrence = sources.entry(source).or_insert(0);
                *occurrence += 1;
                call = format!("user:{signature}:{occurrence}");
                basis = IdentityBasis::ContentOccurrence;
            }
            if codex && let Some(t) = block_text {
                for (offset, name) in mentions(t) {
                    typed_aliases
                        .entry(name.to_ascii_lowercase())
                        .or_insert_with(|| (format!("{call}:{offset}"), basis));
                }
            }
            if user && is(b, "type", "skill") {
                self.typed_skills
                    .insert(text(b.get("name"))?.to_ascii_lowercase());
            }
            block_ids.push((call, basis));
        }
        if user && codex {
            // Text aliases share their inferred turn, so a request and a later load
            // count as one use even when native records lack a turn id.
            let boundary = blocks
                .iter()
                .zip(&block_ids)
                .find(|(v, _)| object(v).get("text").is_some_and(Value::is_string))
                .map(|(_, id)| id)
                .or_else(|| block_ids.first());
            let inferred = boundary.map(|(call, _)| identity(call));
            let recorded = inferred
                .as_ref()
                .and_then(|turn| self.turn_aliases.get(turn))
                .cloned()
                .or_else(|| self.turn_available.then(|| self.turn.clone()).flatten());
            if let (Some(inferred), Some(recorded)) = (&inferred, &recorded) {
                self.turn_aliases.insert(inferred.clone(), recorded.clone());
            }
            self.turn_from_user = recorded.is_none();
            self.turn_available = false;
            self.turn = recorded.or(inferred);
        }
        for (v, (call, basis)) in blocks.iter().zip(block_ids) {
            let b = object(v);
            let block_text = b.get("text").and_then(Value::as_str);
            if user && is(b, "type", "skill") {
                let name = text(b.get("name"))?;
                let alias = typed_aliases.get(&name.to_ascii_lowercase());
                let invoke = Invoke {
                    kind: InvocationKind::SkillRequest,
                    via: Via::Typed,
                    status: InvocationStatus::Requested,
                    call: alias.map_or_else(|| call.clone(), |(call, _)| call.clone()),
                    group: group(name),
                    basis: alias.map_or(IdentityBasis::Recorded, |(_, basis)| *basis),
                };
                let selected = if codex {
                    b.get("path")
                        .and_then(Value::as_str)
                        .filter(|p| crate::skills::absolute(p) && skill_file(p))
                        .filter(|_| {
                            !self
                                .codex_skill_paths
                                .get(self.skill_name(name))
                                .is_some_and(|(path, count)| path.is_none() && *count == 1)
                        })
                        .unwrap_or(name)
                } else {
                    name
                };
                self.invoke(selected, invoke, line)?;
            }
            if user
                && raw.get("isMeta") != Some(&Value::Bool(true))
                && let Some(t) = block_text
            {
                let requested = |known: bool| {
                    if known {
                        (InvocationKind::SkillRequest, InvocationStatus::Requested)
                    } else {
                        (InvocationKind::SkillReference, InvocationStatus::Reference)
                    }
                };
                for (offset, name) in mentions(t) {
                    let (kind, status) = requested(self.knows_skill(name));
                    let invoke = Invoke {
                        kind,
                        via: Via::Mention,
                        status,
                        call: format!("{call}:{offset}"),
                        group: group(name),
                        basis,
                    };
                    self.invoke(name, invoke, line)?;
                }
                if let Some(name) = command(t) {
                    let (kind, status) = requested(self.knows_skill(name));
                    let invoke = Invoke {
                        kind,
                        via: Via::Wrapper,
                        status,
                        call: call.clone(),
                        group: group(name),
                        basis,
                    };
                    self.invoke(name, invoke, line)?;
                }
            }
            if is(b, "type", "tool_use") {
                self.tool_use(b, line)?;
            }
            if is(b, "type", "tool_result")
                && let Some(Value::String(use_id)) = b.get("tool_use_id")
            {
                self.result(use_id, b.get("content"), line.time);
                let key = id(b.get("tool_use_id"))?;
                if let Some(tool) = self.pending_tools.get(&key).cloned() {
                    let time = line
                        .time
                        .ok_or_else(|| ScanError::new("missing invocation timestamp"))?;
                    let failed = b.get("is_error") == Some(&Value::Bool(true));
                    let invoke = Invoke {
                        kind: tool.kind,
                        via: Via::Completion,
                        status: if failed {
                            InvocationStatus::Failed
                        } else {
                            InvocationStatus::Completed
                        },
                        call: use_id.clone(),
                        group: tool.group.clone(),
                        basis: IdentityBasis::Recorded,
                    };
                    self.invocation(tool.target.clone(), invoke, time);
                    if !failed {
                        let content = match b.get("content") {
                            Some(Value::String(s)) => Content::Text(s),
                            Some(v) => Content::Json(v),
                            None => Content::Missing,
                        };
                        let new = New {
                            category: if is_skill(tool.kind) {
                                Category::SkillBody
                            } else {
                                Category::McpResult
                            },
                            name: &key,
                            provenance: "observed-tool-result-not-wire-proof",
                            role: Role::Active,
                            grouping: Grouping::Fixed(tool.group.as_deref()),
                            targets: Some(vec![tool.target]),
                        };
                        self.component(new, content, time);
                    }
                }
            }
        }
        Ok(())
    }

    fn tool_use(&mut self, b: &Map<String, Value>, line: &Line<'_>) -> Result<(), ScanError> {
        let name = text(b.get("name"))?;
        let call = text(b.get("id"))?;
        self.call(call, name, line.time);
        let args = object(field(b, "input"));
        let (scope, kind, target_name) = if name == "Skill" {
            ("skill", InvocationKind::SkillCall, text(args.get("skill"))?)
        } else if name.starts_with("mcp__") {
            ("mcp", InvocationKind::McpCall, name)
        } else if name == "Read"
            && let Some(Value::String(path)) = args.get("file_path")
            && skill_file(path)
        {
            ("skill", InvocationKind::SkillLoad, path.as_str())
        } else {
            return Ok(());
        };
        let invoke = Invoke {
            kind,
            via: Via::Tool,
            status: InvocationStatus::Called,
            call: call.to_string(),
            group: group(target_name),
            basis: IdentityBasis::Recorded,
        };
        self.invoke(target_name, invoke, line)?;
        let target_name = if scope == "skill" {
            self.skill_name(target_name)
        } else {
            target_name
        };
        let pending = PendingTool {
            target: target(scope, target_name),
            group: group(target_name),
            kind,
        };
        self.pending_tools.insert(id(b.get("id"))?, pending);
        Ok(())
    }

    /// Codex completions and tool calls outside message content.
    fn completions(&mut self, line: &Line<'_>) -> Result<(), ScanError> {
        let (p, kind) = (line.payload, line.kind);
        let item = if kind == Some("event_msg") && is(p, "type", "item_completed") {
            record(p.get("item"))?
        } else {
            p
        };
        let completed = kind == Some("event_msg") && is(p, "type", "item_completed");
        if completed && is(item, "type", "McpToolCall") {
            let name = format!(
                "mcp__{}__{}",
                text(item.get("server"))?,
                text(item.get("tool"))?
            );
            let failed = object(field(item, "result")).get("isError") == Some(&Value::Bool(true))
                || is(item, "status", "failed");
            let call = text(item.get("id"))?;
            let group = match item.get("pluginId") {
                Some(Value::String(plugin)) => Some(identity(&format!("plugin:{plugin}"))),
                _ => group(&name),
            };
            let invoke = Invoke {
                kind: InvocationKind::McpCall,
                via: Via::Completion,
                status: if failed {
                    InvocationStatus::Failed
                } else {
                    InvocationStatus::Completed
                },
                call: call.to_string(),
                group,
                basis: IdentityBasis::Recorded,
            };
            self.invoke(&name, invoke, line)?;
            // The model sees each server under its sanitized name, as in `mcp__t3_code`.
            let exposed = format!(
                "mcp__{}__{}",
                codex_server(text(item.get("server"))?),
                text(item.get("tool"))?
            );
            self.call(call, &exposed, line.time);
            if let Some(result) = item.get("result") {
                self.component(
                    New::active(
                        Category::McpResult,
                        call,
                        "observed-mcp-result-not-wire-proof",
                    ),
                    Content::Json(result),
                    line.at(),
                );
            }
        }
        if completed && is(item, "type", "CommandExecution") {
            let parsed = array(item.get("parsed_cmd"))?;
            let reads: Vec<&Map<String, Value>> = parsed
                .iter()
                .map(object)
                .filter(|c| {
                    is(c, "type", "read")
                        && matches!(c.get("path"), Some(Value::String(path)) if skill_file(path))
                })
                .collect();
            let succeeded = item.get("exit_code").and_then(Value::as_f64) == Some(0.0);
            for read in &reads {
                let path = text(read.get("path"))?;
                let invoke = Invoke {
                    kind: InvocationKind::SkillLoad,
                    via: Via::Completion,
                    status: if succeeded && parsed.len() == 1 {
                        InvocationStatus::Completed
                    } else if succeeded {
                        // A compound command's exit status cannot confirm each file read.
                        InvocationStatus::Called
                    } else {
                        InvocationStatus::Failed
                    },
                    call: format!("{}:{}", text(item.get("id"))?, id(read.get("path"))?),
                    group: group(path),
                    basis: IdentityBasis::Recorded,
                };
                self.invoke(path, invoke, line)?;
                if reads.len() == 1
                    && parsed.len() == 1
                    && succeeded
                    && let Some(Value::String(output)) = item.get("aggregated_output")
                {
                    self.component(
                        New::active(Category::SkillBody, path, "identified-file-read-output"),
                        Content::Text(output),
                        line.at(),
                    );
                }
            }
        }
        if kind == Some("response_item")
            && matches!(
                js_string(p.get("type")).as_str(),
                "function_call_output" | "custom_tool_call_output"
            )
        {
            // Some outputs carry their own id and tool name instead of a call id.
            let call = text(present(p.get("call_id")).or(p.get("id")))?;
            if !self.call_tools.contains_key(call)
                && let Some(Value::String(bare)) = p.get("name")
            {
                let name = codex_tool_name(bare, p.get("namespace").and_then(Value::as_str));
                let owner = intern(&mut self.names, &output_owner(&name));
                self.call_tools.insert(call.to_string(), owner);
            }
            self.result(call, p.get("output"), line.time);
        }
        if kind == Some("response_item")
            && matches!(
                js_string(p.get("type")).as_str(),
                "function_call" | "custom_tool_call"
            )
        {
            let bare = text(p.get("name"))?;
            let name = codex_tool_name(bare, p.get("namespace").and_then(Value::as_str));
            let call = text(present(p.get("id")).or(p.get("call_id")))?;
            // Uses count by the id a nested completion shares; outputs name the call by `call_id`.
            self.call(call, &name, line.time);
            let output_id = text(present(p.get("call_id")).or(p.get("id")))?;
            let owner = self.call_tools[call].clone();
            self.call_tools.insert(output_id.to_string(), owner);
            let kind = if name.starts_with("mcp__") {
                Some(InvocationKind::McpCall)
            } else if resource_tool(&name) {
                Some(if name.ends_with("get_prompt") {
                    InvocationKind::McpPrompt
                } else {
                    InvocationKind::McpResource
                })
            } else {
                None
            };
            if let Some(kind) = kind {
                let invoke = Invoke {
                    kind,
                    via: Via::Tool,
                    status: InvocationStatus::Called,
                    call: call.to_string(),
                    group: group(&name),
                    basis: IdentityBasis::Recorded,
                };
                self.invoke(&name, invoke, line)?;
            }
            // Code mentions do not establish that any nested call executed.
            if let Some(Value::String(code)) = present(p.get("arguments")).or(p.get("input")) {
                for (offset, reference) in script_references(code) {
                    let invoke = Invoke {
                        kind: InvocationKind::ScriptReference,
                        via: Via::Script,
                        status: InvocationStatus::Reference,
                        call: format!("{call}:{offset}"),
                        group: group(reference),
                        basis: IdentityBasis::Recorded,
                    };
                    self.invoke(reference, invoke, line)?;
                }
            }
        }
        Ok(())
    }
}

/// Visibility windows per client/version cohort and component, in first-request order.
fn visibility(
    requests: &[Request],
    exposures: &[Exposure],
    invocations: &IndexMap<String, Invocation>,
    since: Option<Millis>,
) -> Vec<Visibility> {
    type Cohort<'a> = (Harness, Option<&'a str>, Option<&'a str>, Vec<&'a str>);
    type SessionKey<'a> = (Harness, Option<&'a str>, Option<&'a str>, &'a str);
    struct Group<'a> {
        r: &'a Request,
        view: Arc<ExposureView>,
        sessions: HashSet<&'a str>,
        since: Millis,
        until: Millis,
    }
    struct Window<'a> {
        r: &'a Request,
        sessions: HashSet<&'a str>,
        since: Millis,
        until: Millis,
    }
    // Requests with the same cohort and listed components open the same windows. Grouping
    // them first preserves the order in which each window first appears.
    let mut cohorts: HashMap<Cohort<'_>, usize> = HashMap::default();
    let mut groups: IndexMap<(usize, usize, usize), Group<'_>> = IndexMap::default();
    for (r, e) in requests.iter().zip(exposures) {
        let mut profiles: Vec<&str> = r.profiles.iter().map(String::as_str).collect();
        profiles.sort_unstable();
        let next = cohorts.len();
        let cohort = *cohorts
            .entry((
                r.harness,
                r.client.as_deref(),
                r.version.as_deref(),
                profiles,
            ))
            .or_insert(next);
        let key = (
            cohort,
            e.view.components.address(),
            e.view.candidates.address(),
        );
        let g = groups.entry(key).or_insert_with(|| Group {
            r,
            view: e.view.clone(),
            sessions: HashSet::default(),
            since: r.time,
            until: r.end_time,
        });
        g.sessions.insert(&*r.session);
        g.since = g.since.min(r.time);
        g.until = g.until.max(r.end_time);
    }
    let mut windows: IndexMap<(usize, Option<Text>), Window<'_>> = IndexMap::default();
    for (&(cohort, _, _), g) in &groups {
        let listed: Vec<Option<Text>> =
            if g.view.components.is_empty() && g.view.candidates.is_empty() {
                vec![None]
            } else {
                g.view
                    .components
                    .iter()
                    .chain(g.view.candidates.iter())
                    .map(|c| Some(c.clone()))
                    .collect()
            };
        for component in listed {
            let w = windows
                .entry((cohort, component))
                .or_insert_with(|| Window {
                    r: g.r,
                    sessions: HashSet::default(),
                    since: since.unwrap_or(g.r.time),
                    until: g.r.end_time,
                });
            w.sessions.extend(&g.sessions);
            w.since = w.since.min(g.since);
            w.until = w.until.max(g.until);
        }
    }

    #[derive(Clone, Copy)]
    struct Calls {
        since: Millis,
        until: Millis,
        direct: bool,
        nested: bool,
        typed: bool,
    }
    impl Calls {
        fn merge(self, other: Calls) -> Calls {
            Calls {
                since: self.since.min(other.since),
                until: self.until.max(other.until),
                direct: self.direct || other.direct,
                nested: self.nested || other.nested,
                typed: self.typed || other.typed,
            }
        }
    }
    // A window counts a call recorded in any of its sessions, so per-session summaries combine.
    let mut by_session: HashMap<SessionKey<'_>, Calls> = HashMap::default();
    for i in invocations
        .values()
        .filter(|i| since.is_none_or(|s| i.time >= s))
    {
        let call = Calls {
            since: i.time,
            until: i.time,
            direct: i.kind == InvocationKind::McpCall,
            nested: i.via == Via::Completion && i.kind == InvocationKind::McpCall,
            typed: i.via == Via::Typed,
        };
        for session in &i.sessions {
            let key = (
                i.harness,
                i.client.as_deref(),
                i.version.as_deref(),
                session.as_str(),
            );
            by_session
                .entry(key)
                .and_modify(|c| *c = c.merge(call))
                .or_insert(call);
        }
    }
    let reasons: Arc<[String]> = VISIBILITY_REASONS.iter().map(|r| r.to_string()).collect();
    windows
        .into_iter()
        .map(|((_, component), w)| {
            let r = w.r;
            let calls = w
                .sessions
                .iter()
                .filter_map(|s| {
                    by_session
                        .get(&(r.harness, r.client.as_deref(), r.version.as_deref(), *s))
                        .copied()
                })
                .reduce(Calls::merge);
            Visibility {
                harness: r.harness,
                client: r.client.clone(),
                version: r.version.clone(),
                profiles: r.profiles.clone(),
                since: since.unwrap_or(calls.map_or(w.since, |c| w.since.min(c.since))),
                until: calls.map_or(w.until, |c| w.until.max(c.until)),
                exposure: if component.is_none() {
                    Visible::Unknown
                } else {
                    Visible::Partial
                },
                component,
                direct_calls: if calls.is_some_and(|c| c.direct) {
                    Observed::Observed
                } else {
                    Observed::Unknown
                },
                nested_calls: if calls.is_some_and(|c| c.nested) {
                    Visible::Partial
                } else {
                    Visible::Unknown
                },
                typed_skills: if calls.is_some_and(|c| c.typed) {
                    Observed::Observed
                } else {
                    Observed::Unknown
                },
                automatic_unused: false,
                reasons: reasons.clone(),
            }
        })
        .collect()
}

/// A Codex tool's full name from its bare name and namespace.
fn codex_tool_name(bare: &str, namespace: Option<&str>) -> String {
    if bare.starts_with("mcp__") {
        bare.to_string()
    } else if let Some(namespace) = namespace.filter(|n| n.starts_with("mcp__")) {
        format!("{namespace}__{bare}")
    } else {
        match namespace {
            Some(namespace) => format!("{namespace}.{bare}"),
            None => bare.to_string(),
        }
    }
}

/// The owner of output from a tool whose call was not recorded.
const UNKNOWN_TOOL: &str = "unknown";

/// The tool a result's output is filed under: an MCP server's tools share one owner.
fn output_owner(name: &str) -> String {
    match name.strip_prefix("mcp__") {
        Some(rest) => format!("mcp__{}", rest.split("__").next().unwrap_or(rest)),
        None => name.to_string(),
    }
}

/// A Codex MCP server id as its tool names spell it: anything outside `[A-Za-z0-9_]` becomes `_`.
pub(crate) fn codex_server(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Text characters of a tool's output, and whether it also held images or other non-text input.
fn output_characters(output: Option<&Value>) -> (i64, bool) {
    match output {
        None | Some(Value::Null) => (0, false),
        Some(Value::String(s)) => (s.chars().count() as i64, false),
        Some(Value::Array(items)) => items.iter().fold((0, false), |(n, opaque), item| match item
            .get("text")
        {
            Some(Value::String(s)) => (n + s.chars().count() as i64, opaque),
            _ => (n, true),
        }),
        Some(other) => (
            Content::Json(other).measure().characters.unwrap_or(0),
            false,
        ),
    }
}

/// Count each tool's results and its largest one per profile set, after merging copies.
fn tool_results(results: impl Iterator<Item = ToolResult>) -> Vec<ToolResults> {
    let mut result: IndexMap<(Text, Vec<Text>), ToolResults> = IndexMap::default();
    for r in results {
        let mut profiles = r.profiles;
        profiles.sort_by(|a, b| js_cmp(a, b));
        result
            .entry((r.tool.clone(), profiles.clone()))
            .and_modify(|prior| {
                prior.results += 1;
                prior.largest_characters = prior.largest_characters.max(r.characters);
            })
            .or_insert_with(|| ToolResults {
                name: r.tool.to_string(),
                profiles: profiles.iter().map(|p| p.to_string()).collect(),
                results: 1,
                largest_characters: r.characters,
            });
    }
    result.into_values().collect()
}

/// Count each tool's calls per profile set, after merging copies of one call.
fn tool_calls(calls: impl Iterator<Item = ToolCall>) -> Vec<ToolCalls> {
    let mut result: IndexMap<(Text, Vec<Text>), ToolCalls> = IndexMap::default();
    for call in calls {
        let mut profiles = call.profiles;
        profiles.sort_by(|a, b| js_cmp(a, b));
        result
            .entry((call.name.clone(), profiles.clone()))
            .and_modify(|prior| {
                prior.calls += 1;
                prior.last_used = prior.last_used.max(call.time);
            })
            .or_insert_with(|| ToolCalls {
                name: call.name.to_string(),
                profiles: profiles.iter().map(|p| p.to_string()).collect(),
                calls: 1,
                last_used: call.time,
            });
    }
    result.into_values().collect()
}

/// Views that agree on everything a request's exposure is judged by.
fn equivalent(a: &ExposureView, b: &ExposureView) -> bool {
    a.components == b.components
        && a.candidates == b.candidates
        && a.deferred == b.deferred
        && a.linkage == b.linkage
        && a.coverage == b.coverage
        && a.references == b.references
        && a.complete_characters == b.complete_characters
        && a.wire_model == b.wire_model
}

fn unique<'a>(ids: impl Iterator<Item = &'a Text>) -> Vec<Text> {
    ids.collect::<IndexSet<_>>().into_iter().cloned().collect()
}

/// One shared copy of `ids`, keeping the prefix it has in common with `like`.
fn intern_list(lists: &mut HashSet<Ids>, like: Option<&Ids>, ids: Vec<Text>) -> Ids {
    let ids = Ids::sharing(like, ids);
    if let Some(found) = lists.get(&ids) {
        return found.clone();
    }
    lists.insert(ids.clone());
    ids
}

fn intern_view(views: &mut HashSet<Arc<ExposureView>>, view: ExposureView) -> Arc<ExposureView> {
    if let Some(found) = views.get(&view) {
        return found.clone();
    }
    let view = Arc::new(view);
    views.insert(view.clone());
    view
}

fn intern(pool: &mut HashSet<Text>, value: &str) -> Text {
    if let Some(found) = pool.get(value) {
        return found.clone();
    }
    let value = Text::from(value);
    pool.insert(value.clone());
    value
}

/// Characters of one recorded message, or None when its content holds images,
/// documents or redacted thinking, whose billed tokens text cannot measure. Only
/// a content-level thinking block's opaque signature is dropped; every other
/// field, tool input included, counts.
fn blob_characters(message: Option<&Value>) -> Result<Option<i64>, ScanError> {
    // JSON.stringify(undefined) is not text; the TypeScript failed here too.
    let message = message.ok_or_else(|| ScanError::new("invalid context object"))?;
    let mut sink = Chars(0);
    match message {
        Value::Object(m) if m.contains_key("content") => {
            if opaque_blocks(&m["content"]) {
                return Ok(None);
            }
            write_object(m, &mut sink, None, |key, value, sink| {
                if key == "content" {
                    blocks(value, sink)
                } else {
                    json(value, sink)
                }
            });
        }
        _ => json(message, &mut sink),
    }
    Ok(Some(sink.0 as i64))
}

fn opaque_blocks(content: &Value) -> bool {
    let Value::Array(items) = content else {
        return false;
    };
    items
        .iter()
        .any(|v| match v.get("type").and_then(Value::as_str) {
            Some("image" | "document" | "redacted_thinking") => true,
            Some("tool_result") => v.get("content").is_some_and(opaque_blocks),
            _ => false,
        })
}

/// Serialize content blocks without thinking signatures.
fn blocks<S: Sink>(content: &Value, sink: &mut S) {
    let Value::Array(items) = content else {
        return json(content, sink);
    };
    sink.put("[");
    for (i, v) in items.iter().enumerate() {
        if i > 0 {
            sink.put(",");
        }
        let Value::Object(block) = v else {
            json(v, sink);
            continue;
        };
        match block.get("type").and_then(Value::as_str) {
            Some("thinking") => write_object(block, sink, Some("signature"), |_, v, s| json(v, s)),
            Some("tool_result") => write_object(block, sink, None, |key, value, sink| {
                if key == "content" {
                    blocks(value, sink)
                } else {
                    json(value, sink)
                }
            }),
            _ => json(v, sink),
        }
    }
    sink.put("]");
}

fn complete_text(params: &Map<String, Value>) -> Result<bool, ScanError> {
    fn content(value: Option<&Value>) -> Result<bool, ScanError> {
        if let Some(Value::String(_)) = value {
            return Ok(true);
        }
        let mut complete = true;
        for v in array(value)? {
            let block = record(Some(v))?;
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    text(block.get("text"))?;
                }
                Some("tool_use") => {
                    text(block.get("id"))?;
                    text(block.get("name"))?;
                    record(block.get("input"))?;
                }
                Some("tool_result") => {
                    text(block.get("tool_use_id"))?;
                    if block.contains_key("content") && !content(block.get("content"))? {
                        complete = false;
                    }
                }
                _ => complete = false,
            }
        }
        Ok(complete)
    }
    let mut complete = content(params.get("system"))?;
    for v in array(params.get("messages"))? {
        let message = record(Some(v))?;
        let role = message.get("role").and_then(Value::as_str);
        if role != Some("user") && role != Some("assistant") {
            return Err(ScanError::new("invalid wire message role"));
        }
        if !message.contains_key("content") {
            return Err(ScanError::new("missing wire message content"));
        }
        if !content(message.get("content"))? {
            complete = false;
        }
    }
    for v in array(params.get("tools"))? {
        let tool = record(Some(v))?;
        match tool.get("input_schema") {
            None => complete = false,
            schema => {
                record(schema)?;
            }
        }
    }
    Ok(complete)
}

// ---------------------------------------------------------------------------
// Raw record access, mirroring JavaScript's view of parsed JSON.

fn field<'a>(map: &'a Map<String, Value>, key: &str) -> &'a Value {
    map.get(key).unwrap_or(&Value::Null)
}

/// JavaScript's `??` treats null like a missing key.
fn present(value: Option<&Value>) -> Option<&Value> {
    value.filter(|v| !v.is_null())
}

fn is(map: &Map<String, Value>, key: &str, expected: &str) -> bool {
    map.get(key).and_then(Value::as_str) == Some(expected)
}

fn array(value: Option<&Value>) -> Result<&[Value], ScanError> {
    match value {
        None => Ok(&[]),
        Some(Value::Array(items)) => Ok(items),
        Some(_) => Err(ScanError::new("invalid context list")),
    }
}

fn record(value: Option<&Value>) -> Result<&Map<String, Value>, ScanError> {
    match value {
        Some(Value::Object(map)) => Ok(map),
        _ => Err(ScanError::new("invalid context object")),
    }
}

fn text(value: Option<&Value>) -> Result<&str, ScanError> {
    match value {
        Some(Value::String(s)) => Ok(s),
        _ => Err(ScanError::new("invalid context text")),
    }
}

fn texts(values: &[Value]) -> Result<Vec<&str>, ScanError> {
    values.iter().map(|v| text(Some(v))).collect()
}

fn id(value: Option<&Value>) -> Result<String, ScanError> {
    safe::id(value.unwrap_or(&Value::Null))
}

/// JavaScript's `String(value)`, with a missing value as "undefined".
fn js_string(value: Option<&Value>) -> String {
    match value {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => number(n),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| match v {
                Value::Null => String::new(),
                v => js_string(Some(v)),
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

/// JavaScript's default string sort compares UTF-16 code units.
fn js_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

fn component_key(category: Category, name: &str) -> String {
    identity(&format!("{}:{name}", category.as_str()))
}

fn target(scope: &str, value: &str) -> String {
    identity(&format!("{scope}:{value}"))
}

/// Built-in tools and MCP tools have different owners and controls.
fn tool_category(name: &str) -> Category {
    if name.starts_with("mcp__") {
        Category::McpSchema
    } else {
        Category::ToolSchema
    }
}

fn is_skill(kind: InvocationKind) -> bool {
    matches!(
        kind,
        InvocationKind::SkillRequest
            | InvocationKind::SkillReference
            | InvocationKind::SkillCall
            | InvocationKind::SkillLoad
    )
}

fn harness_name(harness: Harness) -> &'static str {
    match harness {
        Harness::Claude => "claude",
        Harness::Codex => "codex",
    }
}

fn kind_name(kind: InvocationKind) -> &'static str {
    match kind {
        InvocationKind::SkillRequest => "skill-request",
        InvocationKind::SkillReference => "skill-reference",
        InvocationKind::SkillCall => "skill-call",
        InvocationKind::SkillLoad => "skill-load",
        InvocationKind::McpCall => "mcp-call",
        InvocationKind::McpResource => "mcp-resource",
        InvocationKind::McpPrompt => "mcp-prompt",
        InvocationKind::ScriptReference => "script-reference",
    }
}

// ---------------------------------------------------------------------------
// Patterns, matched exactly as the JavaScript regular expressions they replace.

/// JavaScript's `\s`.
fn js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

/// Where JavaScript's multiline `^` may match, and what `.` excludes.
fn line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// `/[/\\]subagents[/\\]/`
fn is_subagent(path: &str) -> bool {
    let b = path.as_bytes();
    path.match_indices("subagents").any(|(i, m)| {
        i > 0
            && matches!(b[i - 1], b'/' | b'\\')
            && matches!(b.get(i + m.len()), Some(b'/' | b'\\'))
    })
}

fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or("")
}

/// `/^(?:(?:mcp|functions)\.)?(?:read_resource|...|get_prompt)$/`
fn resource_tool(name: &str) -> bool {
    let bare = name
        .strip_prefix("mcp.")
        .or_else(|| name.strip_prefix("functions."))
        .unwrap_or(name);
    matches!(
        bare,
        "read_resource"
            | "read_mcp_resource"
            | "list_resources"
            | "list_mcp_resources"
            | "list_mcp_resource_templates"
            | "get_prompt"
    )
}

/// A plugin (`name:`) or MCP server (`mcp__server__`) owner identity.
fn group(value: &str) -> Option<String> {
    if crate::skills::absolute(value) || skill_file(value) {
        return None;
    }
    // /^(?:mcp__)?([^:]+):/
    let plugin = value.find(':').and_then(|colon| {
        if value.starts_with("mcp__") && colon > 5 {
            Some(&value[5..colon])
        } else if colon > 0 {
            Some(&value[..colon])
        } else {
            None
        }
    });
    if let Some(plugin) = plugin {
        return Some(identity(&format!("plugin:{plugin}")));
    }
    // /^mcp__(.+?)(?:__|$)/: the shortest single-line name followed by "__" or the end.
    let rest = value.strip_prefix("mcp__")?;
    for (i, c) in rest.char_indices() {
        if line_terminator(c) {
            return None;
        }
        let end = i + c.len_utf8();
        if end == rest.len() || rest[end..].starts_with("__") {
            return Some(identity(&format!("mcp-server:{}", &rest[..end])));
        }
    }
    None
}

/// `/^- ([a-zA-Z0-9_.:-]+):[^\n]*$/gm`: each entry's name and whole line.
fn listing_entries(body: &str) -> Vec<(&str, &str)> {
    let mut out = vec![];
    let mut skip = 0;
    let mut previous: Option<char> = None;
    for (i, c) in body.char_indices() {
        let line_start = previous.is_none_or(line_terminator);
        previous = Some(c);
        if i < skip || !line_start {
            continue;
        }
        let Some(rest) = body[i..].strip_prefix("- ") else {
            continue;
        };
        let run = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || "_.:-".contains(c)))
            .unwrap_or(rest.len());
        // The name may contain colons; backtracking ends it at the run's last one.
        let Some(colon) = rest[..run].rfind(':').filter(|&c| c > 0) else {
            continue;
        };
        let end = body[i..].find('\n').map_or(body.len(), |n| i + n);
        out.push((&rest[..colon], &body[i..end]));
        skip = end;
    }
    out
}

/// `/(?:^|\s)\$([a-zA-Z0-9_-]+(?::[a-zA-Z0-9_-]+)?)/g`: each match's UTF-16 index and name.
fn mentions(text: &str) -> Vec<(usize, &str)> {
    let name_length = |s: &str| {
        let word = |s: &str| {
            s.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
                .unwrap_or(s.len())
        };
        let first = word(s);
        if first == 0 {
            return None;
        }
        let second = s[first..].strip_prefix(':').map_or(0, word);
        Some(if second > 0 {
            first + 1 + second
        } else {
            first
        })
    };
    let mut out = vec![];
    let mut skip = 0;
    let mut offset = 0;
    for (i, c) in text.char_indices() {
        if i >= skip {
            let after = i + c.len_utf8();
            let start = if i == 0 && c == '$' {
                Some(after)
            } else if js_space(c) && text[after..].starts_with('$') {
                Some(after + 1)
            } else {
                None
            };
            if let Some(start) = start
                && let Some(length) = name_length(&text[start..])
            {
                out.push((offset, &text[start..start + length]));
                skip = start + length;
            }
        }
        offset += c.len_utf16();
    }
    out
}

/// Built-in commands, bundled skills, and their aliases reserve a synced skill's short name,
/// even when unavailable. Source checked 2026-11-08: <https://code.claude.com/docs/en/commands>.
fn claude_command(name: &str) -> bool {
    const COMMANDS: &[&str] = &[
        "add-dir",
        "advisor",
        "agents",
        "allowed-tools",
        "android",
        "app",
        "artifact-capabilities",
        "artifact-diagramming",
        "artifacts",
        "auto-mode-setup",
        "autocompact",
        "autofix-pr",
        "background",
        "bashes",
        "batch",
        "bg",
        "branch",
        "btw",
        "bug",
        "cd",
        "checkpoint",
        "checkup",
        "chrome",
        "claude-api",
        "claude-in-chrome",
        "clear",
        "code-review",
        "color",
        "compact",
        "config",
        "context",
        "continue",
        "copy",
        "cost",
        "dataviz",
        "debug",
        "deep-research",
        "design",
        "design-login",
        "design-sync",
        "desktop",
        "diff",
        "doctor",
        "effort",
        "exit",
        "export",
        "extra-usage",
        "fast",
        "feedback",
        "fewer-permission-prompts",
        "focus",
        "fork",
        "goal",
        "heapdump",
        "help",
        "hooks",
        "ide",
        "import",
        "init",
        "insights",
        "install-github-app",
        "install-slack-app",
        "ios",
        "keybindings",
        "list-agents",
        "login",
        "logout",
        "loop",
        "mcp",
        "memory",
        "mobile",
        "model",
        "new",
        "output-style",
        "passes",
        "peers",
        "permissions",
        "plan",
        "plugin",
        "plugin-authoring",
        "powerup",
        "pr-comments",
        "privacy-settings",
        "proactive",
        "quit",
        "radio",
        "rate-limit-options",
        "rc",
        "recap",
        "release-notes",
        "reload-plugins",
        "reload-skills",
        "remote-control",
        "remote-env",
        "rename",
        "reset",
        "restart",
        "resume",
        "review",
        "rewind",
        "routines",
        "run",
        "run-skill-generator",
        "sandbox",
        "schedule",
        "scroll-speed",
        "security-review",
        "settings",
        "setup-bedrock",
        "setup-vertex",
        "share",
        "simplify",
        "skill-doctor",
        "skills",
        "slides",
        "stats",
        "status",
        "statusline",
        "stickers",
        "stop",
        "subtask",
        "tasks",
        "team-onboarding",
        "teleport",
        "terminal-setup",
        "theme",
        "tp",
        "tui",
        "ultraplan",
        "ultrareview",
        "undo",
        "update",
        "update-config",
        "upgrade",
        "usage",
        "usage-credits",
        "verify",
        "vim",
        "voice",
        "web-setup",
        "workflow-authoring",
        "workflows",
    ];
    COMMANDS
        .iter()
        .any(|command| command.eq_ignore_ascii_case(name))
}

/// `/<command-name>\/?([a-zA-Z0-9_:-]+)<\/command-name>/`: the first match's name.
fn command(text: &str) -> Option<&str> {
    const OPEN: &str = "<command-name>";
    let mut from = 0;
    while let Some(found) = text[from..].find(OPEN) {
        let start = from + found + OPEN.len();
        let rest = &text[start..];
        let rest = rest.strip_prefix('/').unwrap_or(rest);
        let run = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || "_:-".contains(c)))
            .unwrap_or(rest.len());
        if run > 0 && rest[run..].starts_with("</command-name>") {
            return Some(&rest[..run]);
        }
        from += found + 1;
    }
    None
}

/// `/\b(mcp__[a-zA-Z0-9_]+)\s*\(/g`: each match's UTF-16 index and name.
fn script_references(code: &str) -> Vec<(usize, &str)> {
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut out = vec![];
    let mut skip = 0;
    let mut offset = 0;
    let mut previous: Option<char> = None;
    for (i, c) in code.char_indices() {
        let boundary = previous.is_none_or(|p| !word(p));
        previous = Some(c);
        if i >= skip && boundary && code[i..].starts_with("mcp__") {
            let rest = &code[i + 5..];
            let run = rest.find(|c: char| !word(c)).unwrap_or(rest.len());
            let after = &rest[run..];
            let spaces = after.find(|c: char| !js_space(c)).unwrap_or(after.len());
            if run > 0 && after[spaces..].starts_with('(') {
                out.push((offset, &code[i..i + 5 + run]));
                skip = i + 5 + run + spaces + 1;
            }
        }
        offset += c.len_utf16();
    }
    out
}

// ---------------------------------------------------------------------------
// JSON.stringify, byte for byte, into a string, a character count or a hash.

trait Sink {
    fn put(&mut self, text: &str);
}

impl Sink for String {
    fn put(&mut self, text: &str) {
        self.push_str(text);
    }
}

/// Counts code points, as iterating a JavaScript string does.
struct Chars(usize);

impl Sink for Chars {
    fn put(&mut self, text: &str) {
        self.0 += text.chars().count();
    }
}

/// Hashes and counts serialized text without materializing it.
#[derive(Default)]
struct Measure {
    hash: Sha256,
    characters: usize,
}

impl Sink for Measure {
    fn put(&mut self, text: &str) {
        self.hash.update(text.as_bytes());
        self.characters += text.chars().count();
    }
}

/// The lowercase hex digest, as `safe::identity` prints it.
fn hex(digest: impl IntoIterator<Item = u8>) -> String {
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push(char::from_digit((byte >> 4) as u32, 16).unwrap());
        out.push(char::from_digit((byte & 15) as u32, 16).unwrap());
    }
    out
}

fn json<S: Sink>(value: &Value, sink: &mut S) {
    match value {
        Value::Null => sink.put("null"),
        Value::Bool(b) => sink.put(if *b { "true" } else { "false" }),
        Value::Number(n) => sink.put(&number(n)),
        Value::String(s) => quote(s, sink),
        Value::Array(items) => {
            sink.put("[");
            for (i, v) in items.iter().enumerate() {
                if i > 0 {
                    sink.put(",");
                }
                json(v, sink);
            }
            sink.put("]");
        }
        Value::Object(map) => write_object(map, sink, None, |_, v, s| json(v, s)),
    }
}

/// Write an object in JavaScript property order, optionally without one key.
fn write_object<S: Sink>(
    map: &Map<String, Value>,
    sink: &mut S,
    skip: Option<&str>,
    mut value: impl FnMut(&str, &Value, &mut S),
) {
    sink.put("{");
    let mut first = true;
    let mut entry = |key: &str, v: &Value, sink: &mut S| {
        if Some(key) == skip {
            return;
        }
        if !first {
            sink.put(",");
        }
        first = false;
        quote(key, sink);
        sink.put(":");
        value(key, v, sink);
    };
    if map.keys().any(|k| array_index(k).is_some()) {
        for (k, v) in js_entries(map) {
            entry(k, v, sink);
        }
    } else {
        for (k, v) in map {
            entry(k, v, sink);
        }
    }
    sink.put("}");
}

/// Object entries in JavaScript order: array indices ascending, then insertion order.
pub(crate) fn js_entries(map: &Map<String, Value>) -> Vec<(&str, &Value)> {
    let mut entries: Vec<(&str, &Value)> = map.iter().map(|(k, v)| (k.as_str(), v)).collect();
    entries.sort_by_key(|(k, _)| array_index(k).map_or((1, 0), |n| (0, n)));
    entries
}

/// A canonical array index property name, which JavaScript enumerates first.
fn array_index(key: &str) -> Option<u32> {
    let b = key.as_bytes();
    if b.is_empty()
        || b.len() > 10
        || !b.iter().all(u8::is_ascii_digit)
        || (b[0] == b'0' && b.len() > 1)
    {
        return None;
    }
    let n: u64 = key.parse().ok()?;
    (n < u64::from(u32::MAX)).then_some(n as u32)
}

/// JSON.stringify's string escaping: quotes, backslashes and control characters only.
fn quote<S: Sink>(text: &str, sink: &mut S) {
    sink.put("\"");
    let mut start = 0;
    for (i, b) in text.bytes().enumerate() {
        let escape = match b {
            b'"' => "\\\"",
            b'\\' => "\\\\",
            b'\n' => "\\n",
            b'\r' => "\\r",
            b'\t' => "\\t",
            0x08 => "\\b",
            0x0c => "\\f",
            0..=0x1f => "",
            _ => continue,
        };
        sink.put(&text[start..i]);
        if escape.is_empty() {
            sink.put(&format!("\\u{b:04x}"));
        } else {
            sink.put(escape);
        }
        start = i + 1;
    }
    sink.put(&text[start..]);
    sink.put("\"");
}

/// JavaScript's `Number.prototype.toString` for a parsed JSON number.
pub(crate) fn number(n: &Number) -> String {
    const SAFE: u64 = 1 << 53;
    if let Some(u) = n.as_u64()
        && u <= SAFE
    {
        return u.to_string();
    }
    if let Some(i) = n.as_i64()
        && i.unsigned_abs() <= SAFE
    {
        return i.to_string();
    }
    let value = n.as_f64().expect("JSON numbers are finite");
    if value == 0.0 {
        return "0".into();
    }
    // Shortest round-trip digits, then ECMAScript's choice of plain or exponential form.
    let sign = if value < 0.0 { "-" } else { "" };
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific.split_once('e').expect("exponent");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i32;
    let n = exponent.parse::<i32>().expect("exponent") + 1;
    let body = if k <= n && n <= 21 {
        digits + &"0".repeat((n - k) as usize)
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat(-n as usize))
    } else {
        let e = n - 1;
        let fraction = if k > 1 {
            format!(".{}", &digits[1..])
        } else {
            String::new()
        };
        format!(
            "{}{fraction}e{}{}",
            &digits[..1],
            if e < 0 { '-' } else { '+' },
            e.abs()
        )
    };
    format!("{sign}{body}")
}

#[cfg(test)]
mod scan_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::make_request;
    use serde_json::json;

    fn at(second: u32) -> String {
        format!("2026-11-01T00:00:{second:02}.000Z")
    }

    fn millis(second: u32) -> Millis {
        safe::parse_instant(&at(second)).unwrap()
    }

    /// Feeds synthetic records from one file, numbering lines from 1.
    struct Feed {
        snapshot: Snapshot,
        file: Text,
        line: u64,
    }

    impl Feed {
        fn new(file: &str, harness: Harness, profiles: &[&str]) -> Self {
            Feed {
                snapshot: Snapshot {
                    file: file.into(),
                    harness,
                    profiles: profiles.iter().map(|p| p.to_string()).collect(),
                    bytes: 0,
                    device: 0,
                    inode: 0,
                    deferred_bytes: 0,
                    skipped_lines: 0,
                    lines: 0,
                },
                file: file.into(),
                line: 0,
            }
        }
        fn push(&mut self, c: &mut ExposureCollector, raw: Value) -> Result<(), ScanError> {
            self.line += 1;
            let location = Location {
                file: self.file.clone(),
                line: self.line,
            };
            c.observe(raw.as_object().unwrap(), &self.snapshot, &location)
        }
        fn all(&mut self, c: &mut ExposureCollector, records: Vec<Value>) {
            for raw in records {
                self.push(c, raw).unwrap();
            }
        }
        /// A request linked to the record on `line`, optionally by its wire reference.
        fn request(&self, id: &str, line: u64, reference: Option<&str>) -> Request {
            Request {
                id: id.into(),
                reference: reference.map(|r| identity(r).into()),
                locations: vec![Location {
                    file: self.file.clone(),
                    line,
                }],
                profiles: self.snapshot.profiles.clone().into(),
                ..make_request()
            }
        }
    }

    fn assistant(second: u32, content: Value) -> Value {
        json!({
            "type": "assistant",
            "timestamp": at(second),
            "sessionId": "session",
            "requestRef": format!("api-{second}"),
            "message": { "content": content },
        })
    }

    fn user(second: u32, content: Value) -> Value {
        json!({
            "type": "user",
            "timestamp": at(second),
            "sessionId": "session",
            "message": { "content": content },
        })
    }

    fn attachment(second: u32, value: Value) -> Value {
        json!({
            "type": "attachment",
            "timestamp": at(second),
            "sessionId": "session",
            "uuid": format!("attachment-{second}"),
            "attachment": value,
        })
    }

    fn tool_use(id: &str, name: &str, input: Value) -> Value {
        json!({ "type": "tool_use", "id": id, "name": name, "input": input })
    }

    fn category(scan: &ExposureScan, id: &str) -> Category {
        scan.components
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .category
    }

    #[test]
    fn shapes_lead_with_tools_then_system() {
        let tools = json!([
            { "name": "Bash", "description": "run", "input_schema": { "type": "object" } },
            { "name": "mcp__docs__read", "description": "read", "input_schema": {} },
        ]);
        let system = json!([{ "type": "text", "text": "system" }]);
        let message = json!({ "role": "user", "content": "hello" });
        let mut c = ExposureCollector::new(None);
        let mut feed = Feed::new("/p/main.jsonl", Harness::Claude, &["profile"]);
        feed.all(
            &mut c,
            vec![
                attachment(
                    0,
                    json!({
                        "type": "skill_listing",
                        "content": "- review: checks code",
                        "names": ["review"],
                    }),
                ),
                json!({ "type": "api-request-shape", "shapeHash": "shape",
                        "shape": { "system": system, "tools": tools } }),
                json!({ "type": "api-request-blob", "hash": "m1", "message": message }),
                json!({ "type": "api-request", "id": "api-1", "timestamp": at(1),
                        "shapeHash": "shape", "keep": 0, "tail": ["m1"], "requestDigest": "d1",
                        "params": { "model": "claude-opus-5-5" } }),
                assistant(1, json!([])),
            ],
        );
        let scan = c.finish(&[feed.request("r1", 5, Some("api-1"))], &[], None);
        let view = &scan.requests[0].view;
        assert_eq!(view.prefix, 3);
        assert_eq!(view.linkage, Linkage::Wire);
        assert_eq!(view.wire_model.as_deref(), Some("claude-opus-5-5"));
        let order: Vec<Category> = view
            .components
            .iter()
            .map(|id| category(&scan, id))
            .collect();
        assert_eq!(
            order,
            [
                Category::ToolSchema,
                Category::McpSchema,
                Category::Other,
                Category::SkillListing
            ]
        );
        let length = |v: &Value| serde_json::to_string(v).unwrap().len() as i64;
        let tools_length: i64 = tools.as_array().unwrap().iter().map(length).sum();
        assert_eq!(
            view.complete_characters,
            Some(length(&system) + tools_length + length(&message))
        );
        assert_eq!(view.coverage, TextCoverage::CompleteText);
        let references = view.references.as_ref().unwrap();
        assert_eq!(references.items.as_ref().unwrap().len(), 1);
        // The listing aggregate owns its entry span, which is reported but never exposed.
        let listing = scan
            .components
            .iter()
            .find(|c| c.scope == Scope::Aggregate && c.category == Category::SkillListing)
            .unwrap();
        assert_eq!(listing.owner, Owner::Skills);
        assert_eq!(listing.members.len(), 1);
        assert!(!view.components.iter().any(|id| **id == listing.members[0]));
        let member = scan
            .components
            .iter()
            .find(|c| c.id == listing.members[0])
            .unwrap();
        assert_eq!(
            member.size.characters,
            Some("- review: checks code".len() as i64)
        );
        let docs = scan
            .components
            .iter()
            .find(|c| c.name == "mcp__docs__read")
            .unwrap();
        assert_eq!(docs.group, Some(identity("mcp-server:docs")));
        assert_eq!(docs.targets, [target("mcp", "mcp__docs__read")]);
    }

    #[test]
    fn digest_chains_share_kept_prefixes() {
        let request = |n: u32, extra: Value| {
            let mut r = json!({ "type": "api-request", "id": format!("api-{n}"),
                                "timestamp": at(n), "params": {} });
            r.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            r
        };
        let mut c = ExposureCollector::new(None);
        let mut feed = Feed::new("/p/main.jsonl", Harness::Claude, &["profile"]);
        feed.all(
            &mut c,
            vec![
                request(
                    1,
                    json!({ "keep": 0, "tail": ["a", "b"], "requestDigest": "d1" }),
                ),
                request(
                    2,
                    json!({ "base": "d1", "keep": 1, "tail": ["c"], "requestDigest": "d2" }),
                ),
                request(3, json!({ "base": "d2", "keep": 2, "tail": ["d"] })),
                request(4, json!({ "base": "unknown", "keep": 9, "tail": [] })),
            ],
        );
        let requests: Vec<Request> = (1..=4)
            .map(|n| feed.request(&format!("r{n}"), n as u64, Some(&format!("api-{n}"))))
            .collect();
        let scan = c.finish(&requests, &[], None);
        let items = |n: usize| {
            let references = scan.requests[n].view.references.as_ref().unwrap();
            references
                .items
                .as_ref()
                .map(|items| items.iter().map(|t| t.to_string()).collect::<Vec<_>>())
        };
        let ids = |names: &[&str]| Some(names.iter().map(|n| identity(n)).collect::<Vec<_>>());
        assert_eq!(items(0), ids(&["a", "b"]));
        assert_eq!(items(1), ids(&["a", "c"]));
        assert_eq!(items(2), ids(&["a", "c", "d"]));
        // An unknown base proves nothing about the kept prefix.
        assert_eq!(items(3), None);
        assert_eq!(scan.requests[0].view.linkage, Linkage::Unknown);
        assert!(scan.requests[0].view.reasons[0].starts_with("request digest"));
        let serialized = serde_json::to_value(&scan.requests[2]).unwrap();
        assert_eq!(
            serialized["references"]["items"].as_array().unwrap().len(),
            3
        );

        let mut c = ExposureCollector::new(None);
        let mut feed = Feed::new("/p/main.jsonl", Harness::Claude, &["profile"]);
        let first = request(1, json!({ "keep": 0, "tail": [], "requestDigest": "d" }));
        feed.push(&mut c, first).unwrap();
        let second = request(2, json!({ "base": "d", "keep": 1, "tail": [] }));
        let error = feed.push(&mut c, second).unwrap_err();
        assert_eq!(error.message, "invalid request digest prefix length");
    }

    #[test]
    fn attachment_revisions_replace_by_path() {
        let instructions = |second: u32, content: &str| {
            attachment(
                second,
                json!({ "type": "instructions",
                        "files": [{ "path": "/repo/CLAUDE.md", "content": content }] }),
            )
        };
        let mut c = ExposureCollector::new(None);
        let mut feed = Feed::new("/p/main.jsonl", Harness::Claude, &["profile"]);
        feed.all(
            &mut c,
            vec![
                instructions(0, "first"),
                assistant(1, json!([])),
                instructions(2, "é\nsecond\n"),
                // Deferred definitions never displace or remove active revisions.
                attachment(
                    3,
                    json!({ "type": "deferred_tools_record", "entries": [
                        { "name": "mcp__x__y", "defer_loading": true, "input_schema": {} },
                    ] }),
                ),
                assistant(4, json!([])),
                assistant(5, json!([])),
            ],
        );
        let requests = [
            feed.request("r1", 2, None),
            feed.request("r2", 5, None),
            feed.request("r3", 6, None),
        ];
        let scan = c.finish(&requests, &[], Some(millis(4)));
        let [first, second, third] = [0, 1, 2].map(|i| &scan.requests[i].view);
        assert_eq!(first.linkage, Linkage::Chronological);
        assert_eq!(first.coverage, TextCoverage::Partial);
        assert_eq!(first.reasons, [CHRONOLOGICAL]);
        assert_eq!(second.candidates.len(), 1);
        assert_ne!(
            second.candidates.iter().next(),
            first.candidates.iter().next()
        );
        assert_eq!(second.removed, first.candidates);
        assert_eq!(second.deferred.len(), 1);
        assert_eq!(
            category(&scan, second.deferred.iter().next().unwrap()),
            Category::McpSchema
        );
        // A removal is reported once; later requests carry the same revision.
        assert!(third.removed.is_empty());
        assert_eq!(third.candidates, second.candidates);
        // Revisions first seen before the window are inherited.
        assert_eq!(second.inherited, second.candidates);
        assert_eq!(scan.components.len(), 3);
        let revision = scan
            .components
            .iter()
            .find(|c| c.id.as_str() == &**second.candidates.iter().next().unwrap())
            .unwrap();
        assert_eq!(
            revision.instruction_size,
            Some(crate::model::InstructionSize {
                bytes: 10,
                lines: 2
            })
        );
    }

    #[test]
    fn world_state_tracks_opaque_identities_and_compaction() {
        let token = |second: u32| json!({ "type": "token_usage_record", "timestamp": at(second) });
        let mut c = ExposureCollector::new(None);
        let mut feed = Feed::new("/p/session.jsonl", Harness::Codex, &["codex"]);
        feed.all(
            &mut c,
            vec![
                json!({ "type": "session_meta", "timestamp": at(0),
                        "payload": { "id": "s", "cli_version": "0.1", "originator": "cli" } }),
                json!({ "type": "world_state", "timestamp": at(0), "payload": { "full": true,
                    "state": {
                        "host_skills": { "body": "- one: x" },
                        "tools": { "mcp__a": "description" },
                        "top_level_tools": { "functions": "0123456789" },
                    } } }),
                token(1),
                json!({ "type": "world_state", "timestamp": at(2), "payload": {
                    "state": { "tools": { "mcp__a": null } } } }),
                token(3),
                json!({ "type": "compacted", "timestamp": at(4), "payload": {} }),
                token(5),
            ],
        );
        let requests = [3, 5, 7].map(|line| feed.request(&format!("r{line}"), line, None));
        let scan = c.finish(&requests, &[], None);
        assert_eq!(scan.requests[0].view.candidates.len(), 3);
        assert_eq!(scan.requests[1].view.removed.len(), 1);
        assert_eq!(scan.requests[1].view.candidates.len(), 2);
        assert_eq!(scan.requests[2].view.coverage, TextCoverage::Unknown);
        let opaque = scan
            .components
            .iter()
            .find(|c| c.provenance == "opaque-schema-identity")
            .unwrap();
        assert_eq!(opaque.size.characters, None);
        // The identity is of the serialized string, quotes included.
        assert_eq!(opaque.size.fingerprint, identity("\"0123456789\""));

        let mut c = ExposureCollector::new(None);
        let mut feed = Feed::new("/p/session.jsonl", Harness::Codex, &["codex"]);
        let invalid = json!({ "type": "world_state", "payload": { "state": { "tools": [] } } });
        let error = feed.push(&mut c, invalid).unwrap_err();
        assert_eq!(error.message, "invalid context object");
    }

    #[test]
    fn blob_characters_skip_signatures_and_opaque_media() {
        let thinking = json!({ "role": "assistant", "content": [
            { "type": "thinking", "thinking": "t", "signature": "zzzz" },
            // Tool input can hold fields named like opaque ones; they are billed text.
            { "type": "tool_use", "id": "c", "name": "x",
              "input": { "type": "thinking", "signature": "q", "data": "d" } },
            { "type": "tool_result", "tool_use_id": "c",
              "content": [{ "type": "thinking", "thinking": "u", "signature": "s" }] },
        ] });
        let expected = json!({ "role": "assistant", "content": [
            { "type": "thinking", "thinking": "t" },
            { "type": "tool_use", "id": "c", "name": "x",
              "input": { "type": "thinking", "signature": "q", "data": "d" } },
            { "type": "tool_result", "tool_use_id": "c",
              "content": [{ "type": "thinking", "thinking": "u" }] },
        ] });
        assert_eq!(
            blob_characters(Some(&thinking)).unwrap(),
            Some(serde_json::to_string(&expected).unwrap().chars().count() as i64)
        );
        for block in [
            json!({ "type": "image", "source": {} }),
            json!({ "type": "document", "source": {} }),
            json!({ "type": "redacted_thinking", "data": "x" }),
            json!({ "type": "tool_result", "content": [{ "type": "image" }] }),
        ] {
            let message = json!({ "role": "user", "content": [block] });
            assert_eq!(blob_characters(Some(&message)).unwrap(), None);
        }
        assert_eq!(blob_characters(Some(&json!("🌲"))).unwrap(), Some(3));
        assert!(blob_characters(None).is_err());
    }

    #[test]
    fn tool_calls_count_once_across_files() {
        let call = |id: &str, name: &str| json!([tool_use(id, name, json!({}))]);
        let mut c = ExposureCollector::new(Some(millis(1)));
        let mut a = Feed::new("/p/a.jsonl", Harness::Claude, &["work"]);
        let mut b = Feed::new("/q/b.jsonl", Harness::Claude, &["home"]);
        a.all(
            &mut c,
            vec![
                assistant(0, call("early", "Bash")),
                assistant(1, call("c1", "Bash")),
                assistant(2, call("c2", "Bash")),
                assistant(3, call("c3", "mcp__docs__read")),
            ],
        );
        b.all(
            &mut c,
            vec![
                assistant(1, call("c1", "Bash")),
                assistant(3, call("c3", "mcp__docs__read")),
            ],
        );
        let scan = c.finish(&[], &[], Some(millis(1)));
        let summary: Vec<(&str, Vec<&str>, u64, Millis)> = scan
            .tool_calls
            .iter()
            .map(|t| {
                let profiles = t.profiles.iter().map(String::as_str).collect();
                (t.name.as_str(), profiles, t.calls, t.last_used)
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("Bash", vec!["home", "work"], 1, millis(1)),
                ("Bash", vec!["work"], 1, millis(2)),
                ("mcp__docs__read", vec!["home", "work"], 1, millis(3)),
            ]
        );
        // The copied MCP call is one invocation, seen in both files.
        assert_eq!(scan.invocations.len(), 1);
        let call = &scan.invocations[0];
        assert_eq!(call.kind, InvocationKind::McpCall);
        assert_eq!(call.profiles, ["work", "home"]);
    }

    #[test]
    fn uses_follow_every_copying_sessions_owners() {
        let mut c = ExposureCollector::new(Some(millis(1)));
        let profiles = ["home", "native", "removed"];
        let call = |session: &str, second: u32| {
            json!({ "type": "assistant", "timestamp": at(second), "sessionId": session,
                    "message": { "content": [tool_use("c1", "Bash", json!({}))] } })
        };
        Feed::new("/p/a.jsonl", Harness::Claude, &profiles).all(&mut c, vec![call("s1", 2)]);
        Feed::new("/p/b.jsonl", Harness::Claude, &profiles).all(&mut c, vec![call("s2", 3)]);
        let owned = |session: &str, owner: &str| Request {
            session: identity(session).into(),
            profiles: vec![owner.to_string()].into(),
            ..make_request()
        };
        // s1's requests all precede the window; s2 copied its call under another owner.
        let scan = c.finish(
            &[owned("s2", "home")],
            &[owned("s1", "removed")],
            Some(millis(1)),
        );
        assert_eq!(scan.tool_calls[0].profiles, ["home", "removed"]);
    }

    #[test]
    fn tool_output_is_held_by_tool_until_compaction() {
        let result = |id: &str, content: Value| json!([{ "type": "tool_result", "tool_use_id": id, "content": content }]);
        let mut c = ExposureCollector::new(Some(millis(2)));
        let mut feed = Feed::new("/p/main.jsonl", Harness::Claude, &["profile"]);
        feed.all(
            &mut c,
            vec![
                assistant(0, json!([tool_use("c1", "Bash", json!({}))])),
                user(1, result("c1", json!("abcde"))),
                assistant(2, json!([tool_use("c2", "mcp__docs__read", json!({}))])),
                user(
                    3,
                    result(
                        "c2",
                        json!([{ "type": "text", "text": "xy" }, { "type": "image" }]),
                    ),
                ),
                assistant(4, json!([])),
                json!({ "type": "system", "subtype": "compact_boundary", "timestamp": at(5) }),
                assistant(6, json!([])),
            ],
        );
        let requests = [1, 3, 5, 7].map(|line| feed.request(&format!("r{line}"), line, None));
        let scan = c.finish(&requests, &[], Some(millis(2)));
        type Held<'a> = (u64, Vec<(&'a str, i64, bool)>);
        let held: Vec<Held> = scan
            .requests
            .iter()
            .map(|e| {
                let r = e.results.as_ref().unwrap();
                let tools = r.tools.iter().map(|(t, n, o)| (&**t, *n, *o)).collect();
                (r.epoch, tools)
            })
            .collect();
        let epoch = held[0].0;
        assert_eq!(held[1], (epoch, vec![("Bash", 5, false)]));
        assert_eq!(
            held[2],
            (epoch, vec![("Bash", 5, false), ("mcp__docs", 2, true)])
        );
        assert!(held[3].0 != epoch && held[3].1.is_empty());
        // Only output inside the window counts toward a tool's results.
        let results: Vec<(&str, u64, i64)> = scan
            .tool_results
            .iter()
            .map(|t| (t.name.as_str(), t.results, t.largest_characters))
            .collect();
        assert_eq!(results, [("mcp__docs", 1, 2)]);
    }

    #[test]
    fn codex_outputs_calls_and_provided_tools() {
        let item = |second: u32, payload: Value| json!({ "type": "response_item", "timestamp": at(second), "payload": payload });
        let mut c = ExposureCollector::new(None);
        let mut feed = Feed::new("/p/rollout.jsonl", Harness::Codex, &["codex"]);
        feed.all(
            &mut c,
            vec![
                json!({ "type": "session_meta", "timestamp": at(0), "payload": { "id": "s" } }),
                item(0, json!({ "type": "additional_tools",
                                "tools": [{ "name": "functions", "description": "run" }] })),
                // A full world state replaces its own context, not the provided tools.
                json!({ "type": "world_state", "timestamp": at(0), "payload": { "full": true,
                    "state": { "tools": { "mcp__t3_code": "threads" } } } }),
                item(1, json!({ "type": "custom_tool_call", "name": "exec", "call_id": "k1" })),
                json!({ "type": "event_msg", "timestamp": at(1), "payload": {
                    "type": "item_completed", "item": {
                        "type": "McpToolCall", "id": "m1", "server": "t3-code", "tool": "read" } } }),
                item(2, json!({ "type": "custom_tool_call_output", "call_id": "k1", "output": "12345" })),
                // Some outputs name their tool instead of a call.
                item(2, json!({ "type": "function_call_output", "id": "o1",
                                "name": "send", "namespace": "app", "output": "abc" })),
                json!({ "type": "token_usage_record", "timestamp": at(3) }),
            ],
        );
        let requests = [feed.request("r", 8, None)];
        let scan = c.finish(&requests, &[], None);
        let view = &scan.requests[0].view;
        let names: Vec<(&str, Category)> = view
            .candidates
            .iter()
            .map(|id| {
                let c = scan.components.iter().find(|c| c.id == **id).unwrap();
                (c.name.as_str(), c.category)
            })
            .collect();
        assert_eq!(
            names,
            [
                ("functions", Category::ToolSchema),
                ("mcp__t3_code", Category::McpDiscovery)
            ]
        );
        let held = scan.requests[0].results.as_ref().unwrap();
        let held: Vec<(&str, i64)> = held.tools.iter().map(|(t, n, _)| (&**t, *n)).collect();
        assert_eq!(held, [("exec", 5), ("app.send", 3)]);
        // Nested MCP calls use the server name the model sees.
        let calls: Vec<&str> = scan.tool_calls.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(calls, ["exec", "mcp__t3_code__read"]);
    }

    #[test]
    fn skill_calls_differ_from_requests_references_and_loads() {
        let result = |id: &str, error: bool| json!({ "type": "tool_result", "tool_use_id": id, "content": "body", "is_error": error });
        let mut c = ExposureCollector::new(None);
        let mut feed = Feed::new("/p/main.jsonl", Harness::Claude, &["profile"]);
        feed.all(
            &mut c,
            vec![
                attachment(
                    0,
                    json!({ "type": "skill_listing", "content": "x", "names": ["known"] }),
                ),
                user(
                    1,
                    json!("use $known and $HOME, costs $20 <command-name>/compact</command-name>"),
                ),
                assistant(
                    2,
                    json!([
                        tool_use("s", "Skill", json!({ "skill": "plugin:known" })),
                        tool_use("r", "Read", json!({ "file_path": "/x/SKILL.md" })),
                        tool_use("m", "mcp__docs__read", json!({})),
                        tool_use("o", "Read", json!({ "file_path": "/x/README.md" })),
                    ]),
                ),
                user(
                    3,
                    json!([result("s", false), result("r", false), result("m", true)]),
                ),
                assistant(4, json!([])),
            ],
        );
        let scan = c.finish(&[feed.request("r", 5, None)], &[], None);
        let summary: Vec<(InvocationKind, InvocationStatus, Via)> = scan
            .invocations
            .iter()
            .map(|i| (i.kind, i.status, i.via))
            .collect();
        use InvocationKind as K;
        use InvocationStatus as S;
        assert_eq!(
            summary,
            [
                (K::SkillRequest, S::Requested, Via::Mention),
                (K::SkillReference, S::Reference, Via::Mention),
                (K::SkillReference, S::Reference, Via::Mention),
                (K::SkillReference, S::Reference, Via::Wrapper),
                (K::SkillCall, S::Completed, Via::Tool),
                (K::SkillLoad, S::Completed, Via::Tool),
                (K::McpCall, S::Failed, Via::Tool),
            ]
        );
        assert!(
            scan.invocations
                .iter()
                .all(|i| i.identity_basis == IdentityBasis::Recorded)
        );
        let skill = &scan.invocations[4];
        assert_eq!(skill.target, target("skill", "plugin:known"));
        assert_eq!(skill.group, Some(identity("plugin:plugin")));
        // Only successful results become bodies, keyed by call and targeting what was called.
        let bodies: Vec<&Component> = scan
            .components
            .iter()
            .filter(|c| c.category == Category::SkillBody)
            .collect();
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[0].name, identity("s"));
        assert_eq!(bodies[0].targets, [target("skill", "plugin:known")]);
        assert_eq!(bodies[0].provenance, "observed-tool-result-not-wire-proof");
        assert_eq!(scan.requests[0].view.candidates.len(), 3);
    }

    #[test]
    fn a_short_name_reaches_a_synced_skill_unless_another_skill_owns_it() {
        let resolved = |names: Value| {
            let mut c = ExposureCollector::new(None);
            let mut feed = Feed::new("/p/main.jsonl", Harness::Claude, &["profile"]);
            feed.all(
                &mut c,
                vec![
                    attachment(
                        0,
                        json!({ "type": "skill_listing", "content": "x", "names": names }),
                    ),
                    user(1, json!("<command-name>/pdf</command-name>")),
                    assistant(
                        2,
                        json!([tool_use("s", "Skill", json!({ "skill": "pdf" }))]),
                    ),
                ],
            );
            let scan = c.finish(&[feed.request("r", 3, None)], &[], None);
            scan.invocations
                .iter()
                .map(|i| (i.kind, i.target.clone(), i.group.clone()))
                .collect::<Vec<_>>()
        };
        let both = |name: &str| {
            let namespace = name
                .starts_with("anthropic-skills:")
                .then(|| identity("plugin:anthropic-skills"));
            [
                (
                    InvocationKind::SkillRequest,
                    target("skill", name),
                    namespace.clone(),
                ),
                (InvocationKind::SkillCall, target("skill", name), namespace),
            ]
        };
        assert_eq!(
            resolved(json!(["anthropic-skills:pdf"])),
            both("anthropic-skills:pdf")
        );
        assert_eq!(
            resolved(json!(["pdf", "anthropic-skills:pdf"])),
            both("pdf")
        );
        assert_eq!(
            resolved(json!(["PDF", "anthropic-skills:pdf"])),
            both("PDF")
        );
        assert_eq!(
            resolved(json!(["anthropic-skills:PDF"])),
            both("anthropic-skills:PDF")
        );
    }

    #[test]
    fn codex_skill_names_do_not_gain_claude_aliases() {
        let mut c = ExposureCollector::new(None);
        let mut feed = Feed::new("/p/rollout.jsonl", Harness::Codex, &["codex"]);
        feed.all(
            &mut c,
            vec![
                json!({ "type": "session_meta", "timestamp": at(0), "payload": { "id": "s" } }),
                json!({ "type": "world_state", "timestamp": at(0), "payload": { "full": true,
                "state": { "host_skills": { "body": "- anthropic-skills:pdf: synced" } } } }),
                json!({ "type": "event_msg", "timestamp": at(1), "payload": {
                "type": "user_message", "message": "$pdf $anthropic-skills:pdf" } }),
                json!({ "type": "token_usage_record", "timestamp": at(2) }),
            ],
        );
        let scan = c.finish(&[feed.request("r", 4, None)], &[], None);
        let uses: Vec<_> = scan
            .invocations
            .iter()
            .map(|i| (i.kind, i.target.clone()))
            .collect();
        assert_eq!(
            uses,
            [
                (InvocationKind::SkillReference, target("skill", "pdf")),
                (
                    InvocationKind::SkillRequest,
                    target("skill-unresolved", "anthropic-skills:pdf")
                ),
            ]
        );
    }

    #[test]
    fn synced_aliases_do_not_override_claude_commands() {
        // Only whole command names are reserved, including aliases and bundled skills.
        for (name, reserved) in [
            ("compact", true),
            ("reset", true),
            ("doctor", true),
            ("Review", true),
            ("artifact-diagramming", true),
            ("deep-research", true),
            ("list-agents", true),
            ("peers", true),
            ("bashes", true),
            ("tp", true),
            ("extra-usage", true),
            ("diagramming", false),
            ("research", false),
        ] {
            let full = format!("anthropic-skills:{name}");
            let mut c = ExposureCollector::new(None);
            let mut feed = Feed::new("/p/main.jsonl", Harness::Claude, &["profile"]);
            feed.all(&mut c, vec![
                attachment(0, json!({ "type": "skill_listing", "content": "synced listing", "names": [full] })),
                user(1, json!(format!("<command-name>/{name}</command-name>"))),
                user(2, json!(format!("<command-name>/{full}</command-name>"))),
                assistant(3, json!([tool_use("s", "Skill", json!({ "skill": full }))])),
            ]);
            let scan = c.finish(&[feed.request("r", 4, None)], &[], None);
            let uses: Vec<_> = scan
                .invocations
                .iter()
                .map(|i| (i.kind, i.target.clone()))
                .collect();
            let short = if reserved {
                (InvocationKind::SkillReference, target("skill", name))
            } else {
                (InvocationKind::SkillRequest, target("skill", &full))
            };
            assert_eq!(
                uses,
                [
                    short,
                    (InvocationKind::SkillRequest, target("skill", &full)),
                    (InvocationKind::SkillCall, target("skill", &full)),
                ],
                "{name}"
            );
        }
    }

    #[test]
    fn replacing_a_catalog_refreshes_synced_aliases() {
        let mut c = ExposureCollector::new(None);
        let mut feed = Feed::new("/p/main.jsonl", Harness::Claude, &["profile"]);
        feed.all(
            &mut c,
            vec![
                attachment(0, json!({ "type": "skill_listing", "content": "- pdf: local\n- anthropic-skills:pdf: synced", "names": [] })),
                user(1, json!("<command-name>/pdf</command-name>")),
                attachment(2, json!({ "type": "skill_listing", "content": "synced listing", "names": ["anthropic-skills:pdf"] })),
                user(3, json!("<command-name>/pdf</command-name>")),
                assistant(4, json!([tool_use("s", "Skill", json!({ "skill": "pdf" }))])),
                user(5, json!([{ "type": "tool_result", "tool_use_id": "s", "content": "skill body" }])),
                attachment(6, json!({ "type": "skill_listing", "content": "", "names": [] })),
                user(7, json!("<command-name>/pdf</command-name>")),
                assistant(8, json!([])),
            ],
        );
        let scan = c.finish(&[feed.request("r", 9, None)], &[], None);
        let calls: Vec<_> = scan
            .invocations
            .iter()
            .map(|i| (i.kind, i.target.clone()))
            .collect();
        assert_eq!(
            calls,
            [
                (InvocationKind::SkillRequest, target("skill", "pdf")),
                (
                    InvocationKind::SkillRequest,
                    target("skill", "anthropic-skills:pdf")
                ),
                (
                    InvocationKind::SkillCall,
                    target("skill", "anthropic-skills:pdf")
                ),
                (InvocationKind::SkillReference, target("skill", "pdf")),
            ]
        );
        let body = scan
            .components
            .iter()
            .find(|c| c.category == Category::SkillBody)
            .unwrap();
        assert_eq!(body.targets, [target("skill", "anthropic-skills:pdf")]);
    }

    #[test]
    fn explicit_typed_names_are_evidence_separate_from_the_catalog() {
        let mut c = ExposureCollector::new(None);
        let mut feed = Feed::new("/p/main.jsonl", Harness::Claude, &["profile"]);
        feed.all(
            &mut c,
            vec![
                user(0, json!([{ "type": "skill", "name": "pdf" }, { "type": "skill", "name": "manual" }])),
                attachment(1, json!({ "type": "skill_listing", "content": "- anthropic-skills:pdf: synced", "names": [] })),
                user(2, json!("<command-name>/pdf</command-name>")),
                user(3, json!("<command-name>/manual</command-name>")),
                attachment(4, json!({ "type": "skill_listing", "content": "", "names": [] })),
                user(5, json!("<command-name>/pdf</command-name>")),
            ],
        );
        let scan = c.finish(&[feed.request("r", 6, None)], &[], None);
        assert!(
            scan.invocations
                .iter()
                .all(|i| i.kind == InvocationKind::SkillRequest)
        );
        let targets: Vec<_> = scan.invocations.iter().map(|i| i.target.clone()).collect();
        assert_eq!(
            targets,
            [
                target("skill", "pdf"),
                target("skill", "manual"),
                target("skill", "anthropic-skills:pdf"),
                target("skill", "manual"),
                target("skill", "pdf"),
            ]
        );
    }

    #[test]
    fn copies_agree_or_stay_unknown() {
        let records = |body: &str| {
            vec![
                json!({ "type": "session_meta", "timestamp": at(0), "payload": { "id": "s" } }),
                json!({ "type": "world_state", "timestamp": at(0),
                        "payload": { "full": true, "state": { "host_skills": { "body": body } } } }),
                json!({ "type": "token_usage_record", "timestamp": at(1) }),
            ]
        };
        let scan = |second: &str| {
            let mut c = ExposureCollector::new(None);
            let mut a = Feed::new("/s/a.jsonl", Harness::Codex, &["codex"]);
            let mut b = Feed::new("/s/b.jsonl", Harness::Codex, &["codex"]);
            a.all(&mut c, records("listing"));
            b.all(&mut c, records(second));
            let mut request = a.request("r", 3, None);
            request.locations.extend(b.request("r", 3, None).locations);
            c.finish(&[request], &[], None)
        };
        let same = scan("listing");
        assert_eq!(same.requests[0].view.linkage, Linkage::Chronological);
        assert_eq!(same.visibility.len(), 1);
        let different = scan("other listing");
        let view = &different.requests[0].view;
        assert_eq!(view.linkage, Linkage::Unknown);
        assert_eq!(view.candidates.len(), 2);
        assert_eq!(
            view.reasons,
            ["copied/inherited histories disagree on exposure"]
        );
        assert_eq!(different.visibility.len(), 2);
    }

    #[test]
    fn invalidated_context_is_disclosed() {
        let mut c = ExposureCollector::new(None);
        let mut feed = Feed::new("/p/main.jsonl", Harness::Claude, &["profile"]);
        let bad = attachment(0, json!({ "type": "skill_listing", "content": { "x": 1 } }));
        let error = feed.push(&mut c, bad).unwrap_err();
        assert_eq!(error.message, "invalid context text");
        c.invalidate();
        feed.push(&mut c, assistant(1, json!([]))).unwrap();
        let scan = c.finish(&[feed.request("r", 2, None)], &[], None);
        let view = &scan.requests[0].view;
        assert_eq!(view.coverage, TextCoverage::Unknown);
        assert!(view.reasons.iter().any(|r| r == INVALIDATED));
    }

    #[test]
    fn calibrate_pools_matching_cohorts() {
        let view = |characters: i64, model: &str| {
            Arc::new(ExposureView {
                components: Ids::from(vec![]),
                candidates: Ids::from(vec![]),
                removed: Ids::from(vec![]),
                inherited: Ids::from(vec![]),
                deferred: Ids::from(vec![]),
                linkage: Linkage::Wire,
                coverage: TextCoverage::CompleteText,
                references: None,
                complete_characters: Some(characters),
                prefix: 0,
                wire_model: Some(model.into()),
                reasons: vec![],
            })
        };
        let exposure = |request: &str, characters: i64, model: &str| Exposure {
            request: request.into(),
            view: view(characters, model),
            results: None,
        };
        let request = |id: &str, input: i64| {
            let mut r = Request {
                id: id.into(),
                ..make_request()
            };
            r.parts[0].usage.input = Some(input);
            r
        };
        let scan = ExposureScan {
            requests: vec![
                exposure("a", 100, "claude-opus-5-5"),
                exposure("b", 300, "claude-opus-5-5"),
                exposure("c", 100, "claude-haiku-5-5"),
                exposure("d", 100, "claude-opus-5-5"),
            ],
            ..ExposureScan::default()
        };
        let mut two_parts = request("d", 50);
        two_parts.parts.push(two_parts.parts[0].clone());
        let requests = [
            request("a", 50),
            request("b", 30),
            request("c", 10),
            two_parts,
        ];
        let calibrations = calibrate(&scan, &requests);
        assert_eq!(calibrations.len(), 1);
        let c = &calibrations[0];
        assert_eq!(
            c.cohort,
            identity(r#"["claude","claude-opus-5-5","anthropic"]"#)
        );
        assert_eq!(c.samples.len(), 2);
        assert_eq!(c.tokens_per_character, 80.0 / 400.0);
        assert_eq!(c.range, (0.1, 0.5));
    }

    #[test]
    fn message_sizes_follow_blob_changes() {
        let text = json!({ "role": "user", "content": "first" });
        let more = json!({ "role": "assistant", "content": "second" });
        let image = json!({ "role": "user", "content": [{ "type": "image" }] });
        let blob = |hash: &str, message: &Value| json!({ "type": "api-request-blob", "hash": hash, "message": message });
        let request = |n: u32, extra: Value| {
            let mut r = json!({ "type": "api-request", "id": format!("api-{n}"),
                                "timestamp": at(n), "shapeHash": "shape", "params": {} });
            r.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            r
        };
        let mut c = ExposureCollector::new(None);
        let mut feed = Feed::new("/p/main.jsonl", Harness::Claude, &["profile"]);
        feed.all(
            &mut c,
            vec![
                json!({ "type": "api-request-shape", "shapeHash": "shape",
                        "shape": { "system": "x", "tools": [] } }),
                blob("m1", &text),
                // The second message's blob has not been recorded yet.
                request(
                    1,
                    json!({ "keep": 0, "tail": ["m1", "m2"], "requestDigest": "d1" }),
                ),
                blob("m2", &more),
                request(
                    2,
                    json!({ "base": "d1", "keep": 2, "tail": [], "requestDigest": "d2" }),
                ),
                blob("m1", &image),
                request(3, json!({ "base": "d2", "keep": 2, "tail": [] })),
                request(4, json!({ "base": "d2", "keep": 1, "tail": ["m2"] })),
                blob("m1", &text),
                request(5, json!({ "base": "d2", "keep": 2, "tail": [] })),
            ],
        );
        let lines = [3, 5, 7, 8, 10];
        let requests: Vec<Request> = (1..=5)
            .map(|n| feed.request(&format!("r{n}"), lines[n - 1], Some(&format!("api-{n}"))))
            .collect();
        let scan = c.finish(&requests, &[], None);
        let sizes: Vec<Option<i64>> = scan
            .requests
            .iter()
            .map(|e| e.view.complete_characters)
            .collect();
        let length = |v: &Value| serde_json::to_string(v).unwrap().len() as i64;
        let both = Some(length(&text) + length(&more) + length(&json!("x")));
        assert_eq!(sizes, [None, both, None, None, both]);
    }

    #[test]
    fn json_matches_javascript() {
        let text = |v: &Value| {
            let mut out = String::new();
            json(v, &mut out);
            out
        };
        let parse = |s: &str| text(&serde_json::from_str(s).unwrap());
        assert_eq!(
            parse("[1.0, 1e2, 1e21, 1.5e-7, 0.000001, -0.0, 0.1, 12345678901234567890]"),
            "[1,100,1e+21,1.5e-7,0.000001,0,0.1,12345678901234567000]"
        );
        assert_eq!(
            parse(r#"{"b":1,"2":2,"a":3,"1":4,"01":5}"#),
            r#"{"1":4,"2":2,"b":1,"a":3,"01":5}"#
        );
        assert_eq!(
            text(&json!("a\u{1f}\"\n\u{2028}é")),
            "\"a\\u001f\\\"\\n\u{2028}é\""
        );
        assert_eq!(measure_size(Some("🌲木abc")).characters, Some(5));
        assert_eq!(measure_size(None).fingerprint, identity("opaque"));
        let value = json!({ "name": "x", "n": [1, 2.5] });
        assert_eq!(
            Content::Json(&value).measure().fingerprint,
            identity(r#"{"name":"x","n":[1,2.5]}"#)
        );
        assert_eq!(js_string(Some(&json!([1, null, "a"]))), "1,,a");
        assert_eq!(js_string(None), "undefined");
    }

    #[test]
    fn patterns_match_like_javascript() {
        assert_eq!(
            mentions("é $a:b-c $x$y\t$z: ok"),
            [(1, "a:b-c"), (8, "x"), (13, "z")]
        );
        assert_eq!(mentions("🌲 $a"), [(2, "a")]);
        assert_eq!(mentions("$start costs $20"), [(0, "start"), (12, "20")]);
        assert_eq!(
            listing_entries("Skills\r- a:b: one\n- bad\n- :x: y\n- c: two"),
            [("a:b", "- a:b: one"), (":x", "- :x: y"), ("c", "- c: two")]
        );
        assert_eq!(command("x <command-name>/a:b</command-name>"), Some("a:b"));
        assert_eq!(
            command("<command-name>/</command-name><command-name>ok</command-name>"),
            Some("ok")
        );
        assert_eq!(
            script_references("await mcp__a__b ( 1); xmcp__c(); mcp__d;mcp__e()"),
            [(6, "mcp__a__b"), (40, "mcp__e")]
        );
        assert_eq!(group("mcp__plugin:tool"), Some(identity("plugin:plugin")));
        assert_eq!(group("mcp__:x"), Some(identity("plugin:mcp__")));
        assert_eq!(
            group("mcp__server__tool"),
            Some(identity("mcp-server:server"))
        );
        assert_eq!(group("mcp___x"), Some(identity("mcp-server:_x")));
        assert_eq!(group("mcp__a\nb"), None);
        assert_eq!(group("Bash"), None);
        assert!(is_subagent("/p/x/subagents/agent.jsonl"));
        assert!(!is_subagent("/p/subagents.jsonl"));
        assert!(resource_tool("functions.list_mcp_resources"));
        assert!(!resource_tool("mcp.mcp.get_prompt"));
    }

    #[test]
    fn malformed_optional_codex_turn_ids_do_not_discard_exposure_records() {
        for turn_id in [json!(""), json!(42)] {
            let mut c = ExposureCollector::new(None);
            let mut feed = Feed::new("/p/rollout.jsonl", Harness::Codex, &["codex"]);
            feed.all(&mut c, vec![
                json!({"type":"session_meta", "timestamp":at(0), "payload":{"id":"s"}}),
                json!({"type":"turn_context", "timestamp":at(0), "payload":{"turn_id":turn_id}}),
                json!({"type":"event_msg", "timestamp":at(0), "payload":{"type":"task_started", "turn_id":turn_id}}),
                json!({"type":"world_state", "timestamp":at(0), "payload":{"full":true,
                    "state":{"host_skills":{"body":"- foo: x (file: /skills/foo/SKILL.md)"}}}}),
                json!({"type":"event_msg", "timestamp":at(1), "payload":{"type":"user_message", "message":"$foo"}}),
            ]);
            let scan = c.finish(&[feed.request("r", 2, None)], &[], None);
            assert_eq!(scan.invocations.len(), 1);
            assert_eq!(scan.invocations[0].kind, InvocationKind::SkillRequest);
        }
    }
}
