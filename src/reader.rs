//! Stream frozen transcript snapshots into deduplicated requests and structural events.
//!
//! Raw records live only while one line is normalized; retained data holds counts, opaque
//! identities, times and whitelisted structural metadata.

use crate::collections::{HashMap, HashSet, IndexMap, IndexSet};
use std::fs::File;
use std::hash::BuildHasher;
use std::io::{ErrorKind, Read};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::controls::js_string;
use crate::discovery::{
    Discovery, DiscoveryOptions, discover, discover_claude_controls, discover_codex_controls,
    structural_settings,
};
use crate::exposure::{ExposureCollector, js_entries};
use crate::model::{
    Accounting, Agent, Api, Attempt, BillingPart, CacheContext, EventKind, EventMeta, Harness, Id,
    IdleAttribution, Interner, Location, Millis, Origin, PartKind, Pricing, Profile, Profiles,
    Request, RequestEvidence, RequestMeta, RequestedSpeed, Scan, ScanCoverage, Settings, ShellCall,
    Snapshot, StructuralEvent, Subagent, Text, Usage,
};
use crate::safe::{ScanError, count, digest, id, identity, iso, label, object, timestamp};
use crate::sessions::{Record, Sessions};

/// Select a time window and local roots.
#[derive(Clone, Debug)]
pub struct ScanOptions {
    pub discovery: DiscoveryOptions,
    /// Records whose timestamp proves they precede this instant stay outside selected billing.
    pub since: Option<Millis>,
}

const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

/// Bytes read per system call; lines longer than this are assembled across chunks.
const CHUNK: usize = 1 << 20;

static NULL: Value = Value::Null;

/// A member of a JSON object, with absent members reading as null.
fn member<'a>(map: &'a Map<String, Value>, key: &str) -> &'a Value {
    map.get(key).unwrap_or(&NULL)
}

/// A nested object, with anything else reading as empty.
fn nested<'a>(map: &'a Map<String, Value>, key: &str) -> &'a Map<String, Value> {
    object(member(map, key))
}

/// JavaScript's `a ?? b`: a null or absent `a` falls through to `b`.
fn coalesce<'a>(a: Option<&'a Value>, b: Option<&'a Value>) -> Option<&'a Value> {
    a.filter(|v| !v.is_null()).or(b)
}

/// A member that is a string, like `typeof value === "string"`.
fn string<'a>(map: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    map.get(key).and_then(Value::as_str)
}

/// A member that is the string `expected`.
fn is(map: &Map<String, Value>, key: &str, expected: &str) -> bool {
    string(map, key) == Some(expected)
}

/// A member that is the boolean `true`.
fn is_true(map: &Map<String, Value>, key: &str) -> bool {
    map.get(key) == Some(&Value::Bool(true))
}

/// JavaScript truthiness of an optional JSON member.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

/// A recorded string ID, which must be non-empty, as its opaque identity.
fn opaque(map: &Map<String, Value>, key: &str) -> Result<Option<String>, ScanError> {
    match map.get(key) {
        Some(value @ Value::String(_)) => id(value).map(Some),
        _ => Ok(None),
    }
}

/// An internal lookup key; only equality matters, so raw digests save memory over hex.
type Key = [u8; 32];

/// A Claude record's lineage key within its conversation.
fn lineage(session: &str, uuid: &str) -> Key {
    Sha256::digest(format!("{session}:{}", identity(uuid)).as_bytes()).into()
}

fn sum(values: &[Option<i64>]) -> Result<Option<i64>, ScanError> {
    let mut total = 0;
    for value in values {
        let Some(value) = value else { return Ok(None) };
        total += value;
    }
    if total > MAX_SAFE_INTEGER {
        return Err(ScanError::new("token count sum exceeds safe integer range"));
    }
    Ok(Some(total))
}

/// Normalize usage without treating missing billing counters as zero.
pub fn usage(value: &Value, harness: Harness) -> Result<Usage, ScanError> {
    usage_of(object(value), harness)
}

fn usage_of(u: &Map<String, Value>, harness: Harness) -> Result<Usage, ScanError> {
    let claude = harness == Harness::Claude;
    let details = nested(u, "input_tokens_details");
    let creation = nested(u, "cache_creation");
    let writes = nested(details, "cache_write_tokens_details");
    let read = count(if claude {
        u.get("cache_read_input_tokens")
    } else {
        coalesce(u.get("cached_input_tokens"), details.get("cached_tokens"))
    })?;
    let write = count(if claude {
        u.get("cache_creation_input_tokens")
    } else {
        coalesce(
            u.get("cache_write_input_tokens"),
            details.get("cache_write_tokens"),
        )
    })?;
    let lifetime = |map: &Map<String, Value>, key: &str, wanted: bool| {
        if wanted {
            count(map.get(key))
        } else {
            Ok(None)
        }
    };
    let write5m = lifetime(creation, "ephemeral_5m_input_tokens", claude)?;
    let write1h = lifetime(creation, "ephemeral_1h_input_tokens", claude)?;
    let write30m = lifetime(writes, "ttl_30m_tokens", !claude)?;
    let write12h = lifetime(writes, "ttl_12h_tokens", !claude)?;
    let input = count(u.get("input_tokens"))?;
    let input = if claude {
        sum(&[input, read, write])?
    } else {
        input
    };
    let output = count(u.get("output_tokens"))?;
    let output_details = nested(u, "output_tokens_details");
    let reasoning = count(coalesce(
        coalesce(
            u.get("reasoning_output_tokens"),
            output_details.get("reasoning_tokens"),
        ),
        output_details.get("thinking_tokens"),
    ))?;
    if input.is_some_and(|input| read.unwrap_or(0) + write.unwrap_or(0) > input) {
        return Err(ScanError::new("cache subsets exceed total input"));
    }
    if output.is_some_and(|output| reasoning.unwrap_or(0) > output) {
        return Err(ScanError::new("reasoning subset exceeds output"));
    }
    let split = if claude {
        sum(&[write5m, write1h])?
    } else {
        sum(&[write30m, write12h])?
    };
    if let (Some(split), Some(write)) = (split, write)
        && split != write
    {
        return Err(ScanError::new(
            "cache write lifetime counts disagree with total",
        ));
    }
    Ok(Usage {
        input,
        output,
        read,
        write,
        write5m,
        write1h,
        write30m,
        write12h,
        reasoning,
    })
}

/// Keep known counters from an earlier snapshot of the same streamed part.
fn fill(usage: &mut Usage, old: &Usage) {
    let fields = [
        (&mut usage.input, old.input),
        (&mut usage.output, old.output),
        (&mut usage.read, old.read),
        (&mut usage.write, old.write),
        (&mut usage.write5m, old.write5m),
        (&mut usage.write1h, old.write1h),
        (&mut usage.write30m, old.write30m),
        (&mut usage.write12h, old.write12h),
        (&mut usage.reasoning, old.reasoning),
    ];
    for (value, old) in fields {
        if value.is_none() {
            *value = old;
        }
    }
}

/// What a Codex `session_meta` says about its thread, parsed once for the report and the
/// session export.
pub(crate) struct CodexThread<'a> {
    pub id: Option<&'a str>,
    /// The thread it was spawned from: a spawn's immediate parent, or the root that a native
    /// source such as `review` names as `session_id`.
    pub parent: Option<&'a str>,
    /// Set when a subagent source spawned it.
    pub spawn: Option<CodexSpawn>,
}

/// How a Codex subagent was spawned.
pub(crate) struct CodexSpawn {
    /// Its agent role (`default` when unnamed) or, when native, its source such as `review`.
    pub kind: Option<String>,
    /// A subagent source such as `review`, which no agent role defines.
    pub native: bool,
}

impl<'a> CodexThread<'a> {
    pub(crate) fn parse(meta: &'a Map<String, Value>) -> Self {
        let str = |v: Option<&'a Value>| v.and_then(Value::as_str).filter(|s| !s.is_empty());
        let id = str(meta.get("id")).or_else(|| str(meta.get("session_id")));
        let source = member(meta, "source");
        let subagent = object(source).get("subagent");
        let thread_spawn = subagent
            .and_then(Value::as_object)
            .and_then(|s| s.get("thread_spawn"));
        let spawn_map = object(thread_spawn.unwrap_or(&NULL));
        // `session_id` is the root thread, so a nested spawn's own parent comes first; older
        // spawns name it at the top level.
        let parent = str(spawn_map.get("parent_thread_id"))
            .or_else(|| str(meta.get("parent_thread_id")))
            .or_else(|| str(meta.get("session_id")).filter(|root| Some(*root) != id));
        let spawn = truthy(subagent).then(|| {
            let native = thread_spawn.is_none();
            let kind = if native {
                let subagent = subagent.unwrap_or(&NULL);
                label(subagent).or_else(|| label(member(object(subagent), "other")))
            } else {
                match coalesce(spawn_map.get("agent_role"), spawn_map.get("agent_type")) {
                    None | Some(Value::Null) => Some("default".into()),
                    Some(role) => label(role),
                }
            };
            CodexSpawn { kind, native }
        });
        Self { id, parent, spawn }
    }
}

/// A safe label as a shared string.
fn text(value: &Value) -> Option<Text> {
    label(value).map(Text::from)
}

fn part(
    u: &Map<String, Value>,
    model: Option<Text>,
    kind: PartKind,
) -> Result<BillingPart, ScanError> {
    Ok(BillingPart {
        kind,
        model: text(member(u, "model")).or(model),
        usage: usage_of(u, Harness::Claude)?,
        tier: text(member(u, "service_tier")),
        speed: text(member(u, "speed")),
        geo: text(member(u, "inference_geo")),
    })
}

fn claude_parts(raw: &Value, model: Option<Text>) -> Result<Vec<BillingPart>, ScanError> {
    let u = object(raw);
    let iterations = match u.get("iterations") {
        None | Some(Value::Null) => return Ok(vec![part(u, model, PartKind::Message)?]),
        Some(Value::Array(items)) if items.is_empty() => {
            return Ok(vec![part(u, model, PartKind::Message)?]);
        }
        Some(Value::Array(items)) => items,
        Some(_) => return Err(ScanError::new("invalid usage iterations")),
    };
    // Top-level totals overlap executor iterations and omit compaction/advisor usage.
    iterations
        .iter()
        .map(|value| {
            let iteration = object(value);
            let kind = match string(iteration, "type") {
                Some("compaction") => PartKind::Compaction,
                Some("advisor_message") => PartKind::Advisor,
                Some("message") => PartKind::Message,
                _ => PartKind::Unknown,
            };
            let mut p = part(iteration, model.clone(), kind)?;
            let advisor = kind == PartKind::Advisor;
            if p.tier.is_none() && !advisor {
                p.tier = text(member(u, "service_tier"));
            }
            if p.speed.is_none() && !advisor {
                p.speed = text(member(u, "speed"));
            }
            if p.geo.is_none() {
                p.geo = text(member(u, "inference_geo"));
            }
            Ok(p)
        })
        .collect()
}

fn numeric_tools(raw: Option<&Value>) -> Result<Option<IndexMap<String, i64>>, ScanError> {
    let map = match raw {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Object(map)) => map,
        Some(_) => return Err(ScanError::new("invalid server tool usage")),
    };
    let mut result = IndexMap::default();
    for (key, value) in js_entries(map) {
        if let Some(name) = label(&Value::String(key.to_string())) {
            let requests = count(Some(value))?
                .ok_or_else(|| ScanError::new("missing server-tool request count"))?;
            result.insert(name, requests);
        }
    }
    Ok(Some(result))
}

/// JavaScript's `\s` class, which the idle notice pattern also extends with NBSP.
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

/// The exact native idle notice text; any other informational notice is ignored.
fn idle_notice(text: &str) -> bool {
    const NOTICES: [&str; 2] = [
        "Compacted while idle, before the prompt cache expired",
        "Compacted using a summary written while you were away",
    ];
    const SUFFIX: &str = "turn off with \"idleCompaction\": false in settings.json";
    NOTICES.iter().any(|notice| {
        text.strip_prefix(notice)
            .and_then(|rest| rest.strip_prefix(" ·"))
            .is_some_and(|rest| rest.trim_start_matches(js_space) == SUFFIX)
    })
}

/// Whether a shell command sleeps inside a loop, judged from its words and never retained.
fn polling_loop(command: &str) -> bool {
    let words: Vec<&str> = command
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .collect();
    let has = |word: &str| words.contains(&word);
    has("sleep") && has("do") && (has("while") || has("until") || has("for"))
}

/// Nested Claude subagent transcripts live under a `subagents` directory.
fn is_subagent(file: &str) -> bool {
    let separator = |b: u8| b == b'/' || b == b'\\';
    file.as_bytes()
        .windows(11)
        .any(|w| separator(w[0]) && &w[1..10] == b"subagents" && separator(w[10]))
}

fn harness_name(harness: Harness) -> &'static str {
    match harness {
        Harness::Claude => "claude",
        Harness::Codex => "codex",
    }
}

fn kind_name(kind: EventKind) -> &'static str {
    match kind {
        EventKind::Compaction => "compaction",
        EventKind::TurnStart => "turn-start",
        EventKind::TurnEnd => "turn-end",
        EventKind::ToolStart => "tool-start",
        EventKind::ToolEnd => "tool-end",
        EventKind::Settings => "settings",
        EventKind::Client => "client",
        EventKind::IdleNotice => "idle-notice",
    }
}

#[cfg(unix)]
fn file_identity(file: &File) -> std::io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let info = file.metadata()?;
    Ok((info.dev(), info.ino()))
}

/// The volume serial number and file index, which discovery records for Windows snapshots.
#[cfg(windows)]
fn file_identity(file: &File) -> std::io::Result<(u64, u64)> {
    let info = winapi_util::file::information(file)?;
    Ok((info.volume_serial_number(), info.file_index()))
}

/// Discovery records no device or inode without them.
#[cfg(not(any(unix, windows)))]
fn file_identity(_file: &File) -> std::io::Result<(u64, u64)> {
    Ok((0, 0))
}

/// Complete newline-terminated records of one snapshot, up to its frozen byte boundary.
///
/// Iterating counts lines and unreadable lines on the snapshot; reaching the boundary records
/// the unterminated final line's size as deferred bytes.
pub struct Records<'a> {
    snapshot: &'a mut Snapshot,
    /// One shared name for every location in this file.
    file: Text,
    handle: File,
    position: u64,
    buffer: Vec<u8>,
    start: usize,
    end: usize,
    /// The start of a line that continues in the next chunk.
    pending: Vec<u8>,
    done: bool,
}

/// Open a snapshot for reading only newline-terminated records within its frozen byte boundary.
pub fn read_snapshot(snapshot: &mut Snapshot) -> Result<Records<'_>, ScanError> {
    let file = Text::from(snapshot.file.as_str());
    let at = |message: &str| {
        ScanError::at(
            message,
            Location {
                file: file.clone(),
                line: 0,
            },
        )
    };
    let handle =
        File::open(&snapshot.file).map_err(|_| at("cannot open snapshotted transcript"))?;
    let info = handle
        .metadata()
        .map_err(|_| at("cannot inspect snapshotted transcript"))?;
    let identity =
        file_identity(&handle).map_err(|_| at("cannot inspect snapshotted transcript"))?;
    if identity != (snapshot.device, snapshot.inode) || info.len() < snapshot.bytes {
        return Err(at("transcript changed identity or shrank after snapshot"));
    }
    let size = snapshot.bytes.min(CHUNK as u64) as usize;
    Ok(Records {
        snapshot,
        file,
        handle,
        position: 0,
        buffer: vec![0; size],
        start: 0,
        end: 0,
        pending: Vec::new(),
        done: false,
    })
}

impl Records<'_> {
    fn fail(&mut self, message: &str) -> ScanError {
        self.done = true;
        ScanError::at(
            message,
            Location {
                file: self.file.clone(),
                line: self.snapshot.lines + 1,
            },
        )
    }
}

impl Iterator for Records<'_> {
    type Item = Result<(Map<String, Value>, Location), ScanError>;

    fn next(&mut self) -> Option<Self::Item> {
        while !self.done {
            if self.start < self.end {
                let rest = &self.buffer[self.start..self.end];
                let Some(index) = memchr::memchr(b'\n', rest) else {
                    self.pending.extend_from_slice(rest);
                    self.start = self.end;
                    continue;
                };
                let line = &rest[..index];
                let record = if self.pending.is_empty() {
                    parse(line)
                } else {
                    self.pending.extend_from_slice(line);
                    let record = parse(&self.pending);
                    self.pending.clear();
                    record
                };
                self.start += index + 1;
                self.snapshot.lines += 1;
                match record {
                    Some(raw) => {
                        let location = Location {
                            file: self.file.clone(),
                            line: self.snapshot.lines,
                        };
                        return Some(Ok((raw, location)));
                    }
                    // Harnesses leave truncated or interleaved lines after crashes and concurrent writes.
                    None => self.snapshot.skipped_lines += 1,
                }
            } else if self.position >= self.snapshot.bytes {
                self.snapshot.deferred_bytes = self.pending.len() as u64;
                self.done = true;
            } else {
                let wanted = (self.snapshot.bytes - self.position).min(self.buffer.len() as u64);
                match self.handle.read(&mut self.buffer[..wanted as usize]) {
                    Ok(0) => return Some(Err(self.fail("transcript shrank during scan"))),
                    Ok(read) => {
                        self.position += read as u64;
                        self.start = 0;
                        self.end = read;
                    }
                    Err(e) if e.kind() == ErrorKind::Interrupted => {}
                    Err(_) => return Some(Err(self.fail("cannot read snapshotted transcript"))),
                }
            }
        }
        None
    }
}

/// Parse one line like a fatal `TextDecoder` followed by `JSON.parse`; non-objects are unreadable.
fn parse(line: &[u8]) -> Option<Map<String, Value>> {
    // The decoder drops a leading byte order mark before JSON.parse sees the text.
    let line = line.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(line);
    let value = match serde_json::from_slice(line) {
        Ok(value) => value,
        Err(_) => {
            // JSON.parse accepts escaped lone surrogates, which JavaScript writers emit for
            // truncated text. U+FFFD is what hashing such a string in UTF-8 sees anyway.
            let text = std::str::from_utf8(line).ok()?;
            serde_json::from_str(&replace_lone_surrogates(text)?).ok()?
        }
    };
    match value {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

/// Replace `\uD800`-`\uDFFF` escapes that are not part of a pair, or None when there are none.
fn replace_lone_surrogates(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let unit = |at: usize| -> Option<u16> {
        let digits = bytes.get(at..at + 6)?;
        if digits[0] != b'\\' || digits[1] != b'u' {
            return None;
        }
        u16::from_str_radix(std::str::from_utf8(&digits[2..]).ok()?, 16).ok()
    };
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut replaced = false;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\\' {
            index += 1;
            continue;
        }
        let Some(code) = unit(index) else {
            // Skip the escaped character so `\\u` is not read as an escape.
            index += 2;
            continue;
        };
        let high = (0xd800..0xdc00).contains(&code);
        let low = (0xdc00..0xe000).contains(&code);
        if high && unit(index + 6).is_some_and(|next| (0xdc00..0xe000).contains(&next)) {
            index += 12;
        } else if high || low {
            out.push_str(&text[copied..index]);
            out.push_str("\\ufffd");
            index += 6;
            copied = index;
            replaced = true;
        } else {
            index += 6;
        }
    }
    replaced.then(|| {
        out.push_str(&text[copied..]);
        out
    })
}

/// The kind of a subagent no record names.
pub const UNRECORDED: &str = "unrecorded";

/// Metadata established by earlier records of the same file.
/// Fields every structural event copies are shared, since a history holds millions of events.
struct Context {
    session: Text,
    agent: Agent,
    model: Option<Text>,
    provider: Option<Text>,
    client: Option<Text>,
    version: Option<Text>,
    turn: Option<Text>,
    trigger: Option<Text>,
    tier: Option<Text>,
    source: Option<Text>,
    cwd: Option<Text>,
    effort: Option<Text>,
    pricing: Option<Arc<Pricing>>,
    profiles: Profiles,
}

/// Replace `slot` only when the value changes, so unchanged records keep sharing one copy.
fn set(slot: &mut Option<Text>, value: Option<&str>) {
    if slot.as_deref() != value {
        *slot = value.map(Text::from);
    }
}

/// Per-file reading state.
struct FileState {
    context: Context,
    subagent: bool,
    /// The raw Claude session ID that `context.session` was derived from.
    session_raw: Option<String>,
    previous_total: Option<Usage>,
    /// The model of the last pre-window Codex request kept; None before the first.
    earlier_model: Option<Option<Text>>,
    /// A Claude subagent transcript's spawn metadata, read from the file beside it.
    spawn: Option<Spawn>,
}

/// What a Claude subagent's `agent-*.meta.json` records about its spawn, never its task.
#[derive(Clone, Debug, Default)]
struct Spawn {
    kind: Option<String>,
    built_in: Option<bool>,
    fork: bool,
    model: Option<String>,
    effort: Option<String>,
}

impl Spawn {
    /// Read the metadata beside a subagent transcript; an absent or unreadable file records nothing.
    fn read(transcript: &str) -> Self {
        let Some(stem) = transcript.strip_suffix(".jsonl") else {
            return Self::default();
        };
        let Some(meta) = std::fs::read(format!("{stem}.meta.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        else {
            return Self::default();
        };
        let meta = object(&meta);
        Self {
            kind: label(member(meta, "agentType")),
            built_in: meta.get("isBuiltIn").and_then(Value::as_bool),
            fork: is_true(meta, "isFork"),
            model: label(member(meta, "model")),
            effort: label(member(meta, "effort")),
        }
    }
}

impl FileState {
    fn new(snapshot: &Snapshot, strings: &mut Interner) -> Self {
        let subagent = is_subagent(&snapshot.file);
        Self {
            context: Context {
                session: identity(&snapshot.file).into(),
                agent: match (snapshot.harness, subagent) {
                    (Harness::Claude, true) => Agent::Subagent,
                    (Harness::Claude, false) => Agent::Main,
                    (Harness::Codex, _) => Agent::Unknown,
                },
                model: None,
                provider: None,
                client: None,
                version: None,
                turn: None,
                trigger: None,
                tier: None,
                source: None,
                cwd: None,
                effort: None,
                pricing: None,
                profiles: strings.profiles(&snapshot.profiles),
            },
            spawn: (subagent && snapshot.harness == Harness::Claude)
                .then(|| Spawn::read(&snapshot.file)),
            subagent,
            earlier_model: None,
            session_raw: None,
            previous_total: None,
        }
    }
}

/// A structural event identified by its recorded string key, else by its file and line.
fn event(
    snapshot: &Snapshot,
    location: &Location,
    context: &Context,
    time: Millis,
    kind: EventKind,
    key: Option<&str>,
) -> StructuralEvent {
    let harness = harness_name(snapshot.harness);
    let kind_text = kind_name(kind);
    let id = match key {
        Some(key) => digest(&format!("{harness}:{kind_text}:{key}")),
        None => digest(&format!(
            "{harness}:{kind_text}:{}:{}",
            snapshot.file, location.line
        )),
    };
    StructuralEvent {
        id,
        origins: vec![Origin {
            session: context.session.clone(),
            location: location.clone(),
        }],
        time,
        kind,
        location: location.clone(),
        request: None,
        tool_id: None,
        settings: Settings::default(),
        idle_attribution: IdleAttribution::Unknown,
        shell: None,
        meta: EventMeta {
            harness: snapshot.harness,
            session: context.session.clone(),
            profiles: context.profiles.clone(),
            model: context.model.clone(),
            version: context.version.clone(),
            client: context.client.clone(),
            cwd: context.cwd.clone(),
            trigger: None,
            precomputed: None,
            pre_tokens: None,
            post_tokens: None,
            duration_ms: None,
            tool: None,
            t3: None,
            pricing: context.pricing.clone(),
        }
        .into(),
    }
}

/// A measured request named by an opaque identity.
fn request(
    snapshot: &Snapshot,
    location: &Location,
    context: &Context,
    time: Millis,
    identity: String,
) -> Request {
    Request {
        evidence: RequestEvidence::Measured,
        id: format!("{}:{identity}", harness_name(snapshot.harness)).into(),
        time,
        end_time: time,
        reference: None,
        turn: context.turn.clone(),
        parts: vec![],
        locations: vec![location.clone()],
        t3: None,
        meta: RequestMeta {
            harness: snapshot.harness,
            session: context.session.clone(),
            agent: context.agent,
            model: context.model.clone(),
            provider: context.provider.clone(),
            api: match snapshot.harness {
                Harness::Claude => Api::Messages,
                Harness::Codex => Api::Responses,
            },
            source: context.source.clone(),
            trigger: context.trigger.clone(),
            version: context.version.clone(),
            client: context.client.clone(),
            cwd: context.cwd.clone(),
            effort: context.effort.clone(),
            server_tools: None,
            profiles: context.profiles.clone(),
            accounting: Accounting::Individual,
            attempt: None,
            requested_speed: Some(RequestedSpeed {
                tier: context.tier.clone(),
                speed: None,
            }),
            issues: vec![],
        }
        .into(),
    }
}

/// `[...new Set([...target, ...items])]` for the few issues a record carries.
///
/// Targets are built duplicate-free, so appending unseen items gives the same order.
fn union(target: &mut Vec<Text>, items: Vec<Text>) {
    for item in items {
        if !target.contains(&item) {
            target.push(item);
        }
    }
}

/// A parsed file location, used when a record has no usable identity.
fn line_key(snapshot: &Snapshot, location: &Location) -> String {
    format!("{}:{}", snapshot.file, location.line)
}

/// A compaction lineage link; conflicting copies leave the record unlinked.
#[derive(PartialEq)]
struct Link {
    parent: Option<Key>,
    boundary: Option<Id>,
}

/// Requests in first-seen order, and each one's position by id, so the scan can take the
/// list without copying it.
#[derive(Default)]
struct Requests {
    list: Vec<Request>,
    ids: HashMap<Text, usize>,
}

/// Each event's position by id without a second copy of the id: a position is found by the
/// id's hash and confirmed against the event, and ids sharing a hash fall back to a map.
#[derive(Default)]
struct EventIndex {
    hasher: foldhash::fast::RandomState,
    by_hash: HashMap<u64, usize>,
    collisions: HashMap<Id, usize>,
}

impl EventIndex {
    fn get(&self, events: &[StructuralEvent], id: &Id) -> Option<usize> {
        match self.by_hash.get(&self.hasher.hash_one(id)) {
            Some(&index) if events[index].id == *id => Some(index),
            Some(_) => self.collisions.get(id).copied(),
            None => None,
        }
    }

    /// Record a new event's position; its id must not be indexed yet.
    fn insert(&mut self, id: &Id, index: usize) {
        match self.by_hash.entry(self.hasher.hash_one(id)) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(index);
            }
            std::collections::hash_map::Entry::Occupied(_) => {
                self.collisions.insert(id.clone(), index);
            }
        }
    }
}

/// What an API request record adds to the requests that answer it.
struct RefFacts {
    source: Option<Text>,
    effort: Option<Text>,
    attempt: Option<Attempt>,
    requested_speed: Option<RequestedSpeed>,
    time: Millis,
    profiles: Profiles,
}

impl RefFacts {
    fn of(r: &Request) -> Self {
        Self {
            source: r.source.clone(),
            effort: r.effort.clone(),
            attempt: r.attempt.clone(),
            requested_speed: r.requested_speed.clone(),
            time: r.time,
            profiles: r.profiles.clone(),
        }
    }
}

/// The last API request record read for one reference.
enum Ref {
    /// No request answers it yet, so it may become a request itself.
    Pending(Box<Request>),
    /// A request answers it, so only what the record adds to that request is kept.
    Linked(RefFacts),
}

impl Ref {
    fn profiles(&self) -> &Profiles {
        match self {
            Ref::Pending(r) => &r.profiles,
            Ref::Linked(facts) => &facts.profiles,
        }
    }
}

/// API request records by reference, and the references stored requests answer.
#[derive(Default)]
struct Refs {
    records: IndexMap<Text, Ref>,
    answered: HashSet<Text>,
}

impl Refs {
    /// Keep the latest record for `reference`, reduced once a request answers it.
    fn record(&mut self, reference: Text, r: Request) {
        let record = if self.answered.contains(&reference) {
            Ref::Linked(RefFacts::of(&r))
        } else {
            Ref::Pending(Box::new(r))
        };
        self.records.insert(reference, record);
    }

    /// Note that a stored request answers `reference`.
    fn answer(&mut self, reference: Option<Text>) {
        let Some(reference) = reference else { return };
        if let Some(record) = self.records.get_mut(&reference)
            && let Ref::Pending(r) = record
        {
            let facts = RefFacts::of(r);
            *record = Ref::Linked(facts);
        }
        self.answered.insert(reference);
    }
}

impl Requests {
    #[cfg(test)]
    fn get(&self, id: &str) -> Option<&Request> {
        self.ids.get(id).map(|&index| &self.list[index])
    }
}

/// State shared across every file of one scan.
struct Reader {
    since: Option<Millis>,
    exposure: ExposureCollector,
    strings: Interner,
    requests: Requests,
    // Events in arrival order, and each one's position by id.
    events: Vec<StructuralEvent>,
    event_ids: EventIndex,
    // Only explicit compaction lineage can connect a notice to its boundary.
    // Ordinary messages stop the walk; a missing parent is not a historical match.
    compact_parents: HashMap<Key, Option<Link>>,
    notice_parents: HashMap<Id, HashSet<Option<Key>>>,
    refs: Refs,
    issues: IndexSet<Text>,
    modern_totals: HashSet<String>,
    legacy: Vec<(Request, String)>,
    subagents: IndexMap<Text, Subagent>,
    earlier: Vec<Request>,
    sessions: Sessions,
}

impl Reader {
    /// Keep a pre-window Codex request, without usage, when it starts its transcript or changes
    /// its model: subagent attribution needs a run's start and its parent's model at spawn.
    fn earlier(
        &mut self,
        snapshot: &Snapshot,
        file: &mut FileState,
        location: &Location,
        time: Option<Millis>,
    ) {
        let Some(time) = time else { return };
        if file.earlier_model.as_ref() == Some(&file.context.model) {
            return;
        }
        file.earlier_model = Some(file.context.model.clone());
        let mut r = request(snapshot, location, &file.context, time, String::new());
        r.requested_speed = None;
        self.strings.request(&mut r);
        self.earlier.push(r);
    }

    /// Record a subagent session once; a later copy can only name a type the first left unrecorded.
    fn spawned(&mut self, mut subagent: Subagent) {
        subagent.session = self.strings.text(&subagent.session);
        self.strings.share(&mut subagent.parent);
        match self.subagents.get_mut(&subagent.session) {
            Some(known) if known.kind == UNRECORDED => {
                known.kind = subagent.kind;
                known.native = subagent.native;
            }
            Some(_) => {}
            None => {
                self.subagents.insert(subagent.session.clone(), subagent);
            }
        }
    }

    fn add(&mut self, mut r: Request) {
        self.strings.request(&mut r);
        let answered = add(&mut self.requests, &mut self.strings, r);
        self.refs.answer(answered);
    }

    fn add_event(&mut self, mut e: StructuralEvent) {
        match self.event_ids.get(&self.events, &e.id) {
            Some(index) => {
                let previous = &mut self.events[index];
                let profiles = self.strings.union(&previous.profiles, &e.profiles);
                if !Arc::ptr_eq(&profiles, &previous.profiles) {
                    previous.profiles = profiles;
                    self.strings.event_meta(&mut previous.meta);
                }
                for origin in &mut e.origins {
                    origin.session = self.strings.text(&origin.session);
                }
                previous.origins.extend(e.origins);
            }
            None => {
                self.strings.event(&mut e);
                self.event_ids.insert(&e.id, self.events.len());
                self.events.push(e);
            }
        }
    }

    fn consume(
        &mut self,
        snapshot: &Snapshot,
        file: &mut FileState,
        raw: &Map<String, Value>,
        location: &Location,
    ) -> Result<(), ScanError> {
        let time = timestamp(member(raw, "timestamp"));
        let selected = match (time, self.since) {
            (Some(time), Some(since)) => time >= since,
            _ => true,
        };
        if let Err(error) = self.exposure.observe(raw, snapshot, location) {
            if selected {
                return Err(error);
            }
            self.exposure.invalidate();
            self.issues.insert(
                "invalid pre-window component metadata; subsequent exposure is incomplete".into(),
            );
        }
        match snapshot.harness {
            Harness::Claude => self.claude(snapshot, file, raw, location, time, selected)?,
            Harness::Codex => self.codex(snapshot, file, raw, location, time, selected)?,
        }
        self.sessions.observe(Record {
            harness: snapshot.harness,
            session: &file.context.session,
            subagent: file.subagent,
            file: &location.file,
            cwd: file.context.cwd.as_ref(),
            raw,
            time,
            selected,
        });
        Ok(())
    }

    fn claude(
        &mut self,
        snapshot: &Snapshot,
        file: &mut FileState,
        raw: &Map<String, Value>,
        location: &Location,
        time: Option<Millis>,
        selected: bool,
    ) -> Result<(), ScanError> {
        let subagent = file.subagent;
        let context = &mut file.context;
        if let Some(cwd) = string(raw, "cwd")
            && crate::safe::is_absolute(cwd)
        {
            set(&mut context.cwd, Some(cwd));
        }
        if let Some(value) = raw.get("sessionId") {
            let session = value
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| ScanError::new("missing request or session identity"))?;
            // Hashing every record's session would dominate reading; it rarely changes.
            if file.session_raw.as_deref() != Some(session) {
                let opaque = identity(session);
                context.session = if subagent {
                    let name = snapshot.file.rsplit(['/', '\\']).next().unwrap_or_default();
                    identity(&format!("{opaque}:{name}"))
                } else {
                    opaque.clone()
                }
                .into();
                file.session_raw = Some(session.to_string());
                if let Some(spawn) = file.spawn.clone() {
                    self.spawned(Subagent {
                        harness: Harness::Claude,
                        session: context.session.clone(),
                        parent: Some(opaque.into()),
                        kind: spawn.kind.unwrap_or_else(|| UNRECORDED.into()),
                        built_in: spawn.built_in,
                        fork: spawn.fork,
                        native: false,
                        model: spawn.model,
                        effort: spawn.effort,
                    });
                }
            }
        }
        // Transcripts without a metadata file still name their agent type on records.
        if file.spawn.as_ref().is_some_and(|s| s.kind.is_none())
            && let Some(known) = self.subagents.get_mut(&context.session)
            && known.kind == UNRECORDED
            && let Some(kind) = label(member(raw, "attributionAgent")).or_else(|| {
                string(raw, "querySource")
                    .and_then(|s| s.strip_prefix("agent:builtin:"))
                    .and_then(|kind| label(&Value::from(kind)))
            })
        {
            known.kind = kind;
        }
        if let Some(version) = label(member(raw, "version")) {
            set(&mut context.version, Some(&version));
        }
        if let Some(client) = label(member(raw, "entrypoint")) {
            set(&mut context.client, Some(&client));
        }
        let kind = string(raw, "type");
        let uuid = string(raw, "uuid");
        let parent = string(raw, "parentUuid");
        if uuid == Some("") || parent == Some("") {
            return Err(ScanError::new("missing request or session identity"));
        }
        let compact_boundary = kind == Some("system") && is(raw, "subtype", "compact_boundary");
        if let Some(uuid) = uuid
            && (is_true(raw, "isCompactSummary") || kind == Some("attachment") || compact_boundary)
        {
            let record = lineage(&context.session, uuid);
            let link = Link {
                parent: parent.map(|parent| lineage(&context.session, parent)),
                boundary: (kind == Some("system"))
                    .then(|| digest(&format!("claude:compaction:{uuid}"))),
            };
            match self.compact_parents.get_mut(&record) {
                Some(existing) if existing.as_ref() != Some(&link) => *existing = None,
                _ => {
                    self.compact_parents.insert(record, Some(link));
                }
            }
        }
        if kind == Some("api-request") {
            let time = time.ok_or_else(|| ScanError::new("missing request timestamp"))?;
            let recorded = string(raw, "id").filter(|s| !s.is_empty());
            let reference = match recorded {
                None if !selected => identity(&line_key(snapshot, location)),
                _ => id(member(raw, "id"))?,
            };
            let mut pending = request(snapshot, location, context, time, reference.clone());
            let reference = Text::from(reference);
            pending.reference = recorded.map(|_| reference.clone());
            pending.agent = if is_true(raw, "isSidechain") || subagent {
                Agent::Subagent
            } else {
                Agent::Main
            };
            pending.source = text(member(raw, "querySource"));
            let params = nested(raw, "params");
            pending.model = text(member(params, "model"));
            pending.effort = text(member(nested(params, "output_config"), "effort"));
            pending.attempt = Some(Attempt {
                max_output_tokens: match count(params.get("max_tokens")) {
                    Ok(budget) => budget,
                    Err(error) if selected => return Err(error),
                    Err(_) => {
                        pending.issues.push(
                            "pre-window output budget is invalid; no budget estimate retained"
                                .into(),
                        );
                        None
                    }
                },
            });
            pending.requested_speed = Some(RequestedSpeed {
                tier: text(member(params, "service_tier")),
                speed: text(member(params, "speed")),
            });
            // Attempt parameters are requested, not proof of the billed tier/speed.
            pending.parts = vec![BillingPart {
                kind: PartKind::Message,
                model: pending.model.clone(),
                usage: Usage::default(),
                tier: None,
                speed: None,
                geo: text(member(params, "inference_geo")),
            }];
            pending.evidence = RequestEvidence::Unknown;
            pending.issues.push(
                "api request has no linked assistant usage; failed/in-flight/hidden request billing is unknown"
                    .into(),
            );
            if let Some(prior) = self.refs.records.get(&reference) {
                pending.profiles = self.strings.union(prior.profiles(), &pending.profiles);
            }
            self.strings.request(&mut pending);
            self.refs.record(reference, pending);
            return Ok(());
        }
        let assistant = kind == Some("assistant") && !is_true(raw, "isApiErrorMessage");
        // Structural pricing context is needed for later boundaries even outside --since.
        if assistant {
            let m = nested(raw, "message");
            let u = nested(m, "usage");
            if string(m, "model") != Some("<synthetic>")
                && let Some(model) = label(member(m, "model"))
            {
                set(&mut context.model, Some(&model));
                let pricing = Pricing {
                    model: context.model.clone(),
                    provider: context.provider.clone(),
                    tier: text(member(u, "service_tier")),
                    speed: text(member(u, "speed")),
                    geo: text(member(u, "inference_geo")),
                };
                if context.pricing.as_deref() != Some(&pricing) {
                    context.pricing = Some(Arc::new(pricing));
                }
            }
        }
        let notice = kind == Some("system")
            && is(raw, "subtype", "informational")
            && is(raw, "level", "notice")
            && string(raw, "content").is_some_and(idle_notice);
        // Earlier user turns still count toward thresholds such as a recap's three turns, and an
        // earlier idle notice still marks its boundary as no window trigger.
        if !selected && !matches!(kind, Some("assistant" | "user")) && !compact_boundary && !notice
        {
            return Ok(());
        }
        let context = &file.context;
        if kind == Some("assistant") {
            let m = nested(raw, "message");
            if !assistant || is(m, "model", "<synthetic>") {
                return Ok(());
            }
            let time = time.ok_or_else(|| ScanError::new("missing request timestamp"))?;
            if selected && m.is_empty() {
                return Err(ScanError::new("missing assistant message"));
            }
            let recorded = coalesce(
                coalesce(m.get("id"), raw.get("requestId")),
                raw.get("requestRef"),
            );
            let name = match recorded {
                Some(Value::String(s)) if !s.is_empty() => identity(s),
                _ if !selected => identity(&line_key(snapshot, location)),
                _ => id(recorded.unwrap_or(&NULL))?,
            };
            let mut r = request(snapshot, location, context, time, name);
            r.model = text(member(m, "model"));
            r.effort = text(member(raw, "effort"));
            r.reference = opaque(raw, "requestRef")?.map(Text::from);
            r.source = text(member(raw, "querySource"));
            r.agent = if is_true(raw, "isSidechain") || subagent {
                Agent::Subagent
            } else {
                Agent::Main
            };
            let usage = member(m, "usage");
            match claude_parts(usage, r.model.clone()) {
                Ok(parts) => r.parts = parts,
                Err(error) if selected => return Err(error),
                Err(_) => {
                    r.parts = vec![part(&Map::new(), None, PartKind::Unknown)?];
                    r.evidence = RequestEvidence::Unknown;
                    r.issues.push(
                        "pre-window usage is invalid; cache predecessor remains unknown".into(),
                    );
                }
            }
            r.server_tools = if selected {
                numeric_tools(object(usage).get("server_tool_use"))?
                    .map(|tools| tools.into_iter().collect())
            } else {
                None
            };
            if !truthy(m.get("usage")) {
                r.evidence = RequestEvidence::Unknown;
                r.issues.push("request usage is absent".into());
            }
            let request_id = r.id.clone();
            self.add(r);
            if selected && let Some(Value::Array(content)) = m.get("content") {
                for block in content {
                    let b = object(block);
                    if is(b, "type", "tool_use") {
                        let mut e = event(
                            snapshot,
                            location,
                            context,
                            time,
                            EventKind::ToolStart,
                            string(b, "id"),
                        );
                        e.tool = text(member(b, "name"));
                        e.tool_id = opaque(b, "id")?.map(Id::from);
                        e.request = Some(request_id.clone());
                        if is(b, "name", "Bash") {
                            let input = nested(b, "input");
                            e.shell = Some(ShellCall {
                                background: is_true(input, "run_in_background"),
                                polls: string(input, "command").is_some_and(polling_loop),
                                timed_out: false,
                            });
                        }
                        self.add_event(e);
                    }
                }
            }
        } else if compact_boundary {
            let time = time.ok_or_else(|| ScanError::new("missing compaction timestamp"))?;
            let meta = nested(raw, "compactMetadata");
            let mut e = event(
                snapshot,
                location,
                context,
                time,
                EventKind::Compaction,
                uuid,
            );
            e.trigger = string(meta, "trigger")
                .filter(|t| *t == "auto" || *t == "manual")
                .map(str::to_string);
            e.precomputed = meta.get("precomputed").and_then(Value::as_bool);
            if selected {
                e.pre_tokens = count(meta.get("preTokens"))?;
                e.post_tokens = count(meta.get("postTokens"))?;
                e.duration_ms = count(meta.get("durationMs"))?;
            } else {
                // A pre-window trigger still shows the current window; its other counts are never read.
                e.pre_tokens = count(meta.get("preTokens")).ok().flatten();
            }
            e.request = if let Some(reference) = opaque(raw, "requestRef")? {
                Some(format!("ref:{reference}").into())
            } else {
                opaque(meta, "requestId")?.map(|r| format!("claude:{r}").into())
            };
            self.add_event(e);
        } else if notice {
            let time = time.ok_or_else(|| ScanError::new("missing idle notice timestamp"))?;
            let mut e = event(
                snapshot,
                location,
                context,
                time,
                EventKind::IdleNotice,
                uuid,
            );
            e.idle_attribution = IdleAttribution::Notice;
            let notice = e.id.clone();
            self.add_event(e);
            self.notice_parents
                .entry(notice)
                .or_default()
                .insert(parent.map(|parent| lineage(&context.session, parent)));
        } else if kind == Some("user") {
            let Some(time) = time else { return Ok(()) };
            let m = nested(raw, "message");
            let mut tool_result = false;
            if let Some(Value::Array(content)) = m.get("content") {
                // The message-level result belongs to its block only when there is one.
                let results = content
                    .iter()
                    .filter(|block| is(object(block), "type", "tool_result"))
                    .count();
                let timed_out = results == 1
                    && nested(raw, "toolUseResult")
                        .get("timedOutAfterMs")
                        .is_some_and(Value::is_number);
                for block in content {
                    let b = object(block);
                    if is(b, "type", "tool_result") {
                        tool_result = true;
                        // Earlier tool results are not retained, so their metadata cannot fail the scan.
                        if !selected {
                            continue;
                        }
                        let mut e = event(
                            snapshot,
                            location,
                            context,
                            time,
                            EventKind::ToolEnd,
                            string(b, "tool_use_id"),
                        );
                        e.tool_id = opaque(b, "tool_use_id")?.map(Id::from);
                        if timed_out {
                            e.shell = Some(ShellCall {
                                timed_out,
                                ..ShellCall::default()
                            });
                        }
                        self.add_event(e);
                    }
                }
            }
            if !tool_result && !is_true(raw, "isMeta") {
                let e = event(
                    snapshot,
                    location,
                    context,
                    time,
                    EventKind::TurnStart,
                    uuid,
                );
                self.add_event(e);
            }
        } else if kind == Some("system")
            && is(raw, "subtype", "turn_duration")
            && let Some(time) = time
        {
            let mut e = event(snapshot, location, context, time, EventKind::TurnEnd, uuid);
            e.duration_ms = count(raw.get("durationMs"))?;
            self.add_event(e);
        }
        Ok(())
    }

    fn codex(
        &mut self,
        snapshot: &Snapshot,
        file: &mut FileState,
        raw: &Map<String, Value>,
        location: &Location,
        time: Option<Millis>,
        selected: bool,
    ) -> Result<(), ScanError> {
        let p = nested(raw, "payload");
        let kind = string(raw, "type");
        let payload_kind = string(p, "type");
        let context = &mut file.context;
        let absolute_cwd = string(p, "cwd").filter(|cwd| crate::safe::is_absolute(cwd));
        if kind == Some("session_meta") {
            if let Some(cwd) = absolute_cwd {
                set(&mut context.cwd, Some(cwd));
            }
            context.session =
                id(coalesce(p.get("id"), p.get("session_id")).unwrap_or(&NULL))?.into();
            context.provider = text(member(p, "model_provider"));
            let source = member(p, "source");
            set(
                &mut context.client,
                // The source is not a client: T3 runs the app-server, whose default is `vscode`.
                crate::safe::originator(member(p, "originator")).as_deref(),
            );
            let spawned = truthy(object(source).get("subagent"));
            context.source = text(source).or_else(|| spawned.then(|| "subagent".into()));
            set(
                &mut context.version,
                label(member(p, "cli_version")).as_deref(),
            );
            context.agent = match source {
                Value::Object(_) if spawned => Agent::Subagent,
                Value::String(_) => Agent::Main,
                _ => Agent::Unknown,
            };
            let thread = CodexThread::parse(p);
            if let Some(spawn) = thread.spawn {
                let CodexSpawn { kind, native } = spawn;
                let parent = match thread.parent {
                    Some(parent) => Some(id(&Value::from(parent))?.into()),
                    None => None,
                };
                let session = context.session.clone();
                self.spawned(Subagent {
                    harness: Harness::Codex,
                    session,
                    parent,
                    kind: kind.unwrap_or_else(|| UNRECORDED.into()),
                    built_in: None,
                    fork: false,
                    native,
                    model: None,
                    effort: None,
                });
            }
            if let Some(time) = time
                && selected
            {
                let session = context.session.clone();
                let e = event(
                    snapshot,
                    location,
                    context,
                    time,
                    EventKind::Client,
                    Some(&session),
                );
                self.add_event(e);
            }
        } else if kind == Some("turn_context") {
            if let Some(cwd) = absolute_cwd {
                set(&mut context.cwd, Some(cwd));
            }
            set(&mut context.model, label(member(p, "model")).as_deref());
            let mode = nested(nested(p, "collaboration_mode"), "settings");
            set(
                &mut context.effort,
                label(coalesce(p.get("effort"), mode.get("reasoning_effort")).unwrap_or(&NULL))
                    .as_deref(),
            );
            context.turn = opaque(p, "turn_id")?.map(Text::from);
            context.tier = text(member(p, "service_tier"));
            if let Some(time) = time
                && selected
            {
                let mut e = event(
                    snapshot,
                    location,
                    context,
                    time,
                    EventKind::Settings,
                    string(p, "turn_id"),
                );
                e.settings = structural_settings(p).into();
                self.add_event(e);
            }
        } else if kind == Some("token_usage_record") {
            if !selected {
                self.earlier(snapshot, file, location, time);
                return Ok(());
            }
            let time = time.ok_or_else(|| ScanError::new("missing request timestamp"))?;
            let mut r = request(
                snapshot,
                location,
                context,
                time,
                id(member(p, "response_id"))?,
            );
            r.turn = match opaque(p, "turn_id")? {
                Some(turn) => Some(turn.into()),
                None => context.turn.clone(),
            };
            r.parts = vec![BillingPart {
                kind: PartKind::Message,
                model: r.model.clone(),
                usage: usage(member(p, "usage"), Harness::Codex)?,
                // Native TokenUsageRecord has no billed service-tier field.
                tier: None,
                speed: None,
                geo: None,
            }];
            let total = nested(p, "thread_token_usage");
            if !total.is_empty() {
                let total = usage_of(total, Harness::Codex)?;
                self.modern_totals.insert(format!(
                    "{}:{}",
                    context.session,
                    serde_json::to_string(&total).expect("usage serializes")
                ));
            }
            self.add(r);
        } else if kind == Some("event_msg") && payload_kind == Some("token_count") {
            let info = nested(p, "info");
            if info.is_empty() {
                return Ok(());
            }
            let total = match usage(member(info, "total_token_usage"), Harness::Codex) {
                Ok(total) => total,
                Err(_) if !selected => {
                    file.previous_total = None;
                    self.earlier(snapshot, file, location, time);
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
            let total_json = serde_json::to_string(&total).expect("usage serializes");
            let total_key = format!("{}:{total_json}", context.session);
            let repeated = file.previous_total == Some(total);
            if !selected {
                file.previous_total = Some(total);
                self.earlier(snapshot, file, location, time);
                return Ok(());
            }
            let mut u = if !nested(info, "last_token_usage").is_empty() {
                Some(usage(member(info, "last_token_usage"), Harness::Codex)?)
            } else if let Some(previous) = file.previous_total {
                let delta = |a: Option<i64>, b: Option<i64>| Some(a? - b?);
                let difference = Usage {
                    input: delta(total.input, previous.input),
                    output: delta(total.output, previous.output),
                    read: delta(total.read, previous.read),
                    write: delta(total.write, previous.write),
                    write5m: delta(total.write5m, previous.write5m),
                    write1h: delta(total.write1h, previous.write1h),
                    write30m: delta(total.write30m, previous.write30m),
                    write12h: delta(total.write12h, previous.write12h),
                    reasoning: delta(total.reasoning, previous.reasoning),
                };
                let fields = [
                    difference.input,
                    difference.output,
                    difference.read,
                    difference.write,
                    difference.write5m,
                    difference.write1h,
                    difference.write30m,
                    difference.write12h,
                    difference.reasoning,
                ];
                (!fields.iter().any(|v| v.is_some_and(|v| v < 0))).then_some(difference)
            } else {
                None
            };
            file.previous_total = Some(total);
            if repeated {
                return Ok(());
            }
            let time = time.ok_or_else(|| ScanError::new("missing usage timestamp"))?;
            let scope = context.turn.as_deref().unwrap_or(&context.session);
            let mut r = request(
                snapshot,
                location,
                context,
                time,
                identity(&format!("{scope}:{time}:{total_json}")),
            );
            let last = truthy(info.get("last_token_usage"));
            r.accounting = if last {
                Accounting::LegacyLast
            } else {
                Accounting::LegacyDelta
            };
            r.evidence = if last {
                RequestEvidence::Measured
            } else {
                RequestEvidence::Estimated
            };
            if u.is_none() {
                u = Some(Usage::default());
                r.evidence = RequestEvidence::Unknown;
                r.issues.push(
                    "cumulative usage has no verified request delta after start/reset".into(),
                );
            }
            r.parts = vec![BillingPart {
                kind: PartKind::Message,
                model: r.model.clone(),
                usage: u.unwrap_or_default(),
                tier: None,
                speed: None,
                geo: None,
            }];
            self.strings.request(&mut r);
            self.legacy.push((r, total_key));
        } else if kind == Some("compacted") && selected {
            let time = time.ok_or_else(|| ScanError::new("missing compaction timestamp"))?;
            let fallback = format!("{}:{time}", context.session);
            let key = match coalesce(p.get("compaction_response_id"), p.get("window_id")) {
                Some(Value::Null) | None => Some(fallback.as_str()),
                Some(value) => value.as_str(),
            };
            let mut e = event(
                snapshot,
                location,
                context,
                time,
                EventKind::Compaction,
                key,
            );
            e.request = opaque(p, "compaction_response_id")?.map(|r| format!("codex:{r}").into());
            self.add_event(e);
        } else if kind == Some("event_msg")
            && matches!(
                js_string(p.get("type")).as_str(),
                "task_started" | "task_complete" | "thread_settings_applied"
            )
        {
            if payload_kind == Some("task_started") {
                if let Some(turn) = opaque(p, "turn_id")? {
                    context.turn = Some(turn.into());
                }
                context.trigger = text(member(p, "turn_trigger"))
                    .or_else(|| text(member(nested(p, "turn_attribution"), "turn_trigger")));
            }
            let applied = nested(p, "thread_settings");
            let settings_applied = payload_kind == Some("thread_settings_applied");
            if settings_applied {
                if let Some(tier) = label(member(applied, "service_tier"))
                    .or_else(|| label(member(p, "service_tier")))
                {
                    context.tier = Some(tier.into());
                }
                if let Some(model) = label(member(applied, "model")) {
                    set(&mut context.model, Some(&model));
                }
                if let Some(effort) = label(member(applied, "reasoning_effort")) {
                    set(&mut context.effort, Some(&effort));
                }
                if let Some(provider) = label(member(applied, "model_provider_id")) {
                    context.provider = Some(provider.into());
                }
            }
            if let Some(time) = time
                && selected
            {
                let fallback = format!("{}:{time}", context.session);
                let key = match p.get("turn_id") {
                    Some(Value::Null) | None => Some(fallback.as_str()),
                    Some(value) => value.as_str(),
                };
                let kind = match payload_kind {
                    Some("task_started") => EventKind::TurnStart,
                    Some("task_complete") => EventKind::TurnEnd,
                    _ => EventKind::Settings,
                };
                let mut e = event(snapshot, location, context, time, kind, key);
                e.settings = structural_settings(if settings_applied { applied } else { p }).into();
                self.add_event(e);
            }
        } else if kind == Some("response_item")
            && selected
            && let Some(time) = time
        {
            let item = js_string(p.get("type"));
            if matches!(
                item.as_str(),
                "function_call"
                    | "custom_tool_call"
                    | "function_call_output"
                    | "custom_tool_call_output"
            ) {
                let kind = if item.ends_with("output") {
                    EventKind::ToolEnd
                } else {
                    EventKind::ToolStart
                };
                let mut e = event(
                    snapshot,
                    location,
                    context,
                    time,
                    kind,
                    string(p, "call_id"),
                );
                e.tool = text(member(p, "name"));
                e.tool_id = opaque(p, "call_id")?.map(Id::from);
                self.add_event(e);
            }
        }
        Ok(())
    }
}

/// Merge a request into its earlier copy; `r` shares the interner's strings. Returns the
/// reference the stored request newly answers.
fn add(requests: &mut Requests, strings: &mut Interner, r: Request) -> Option<Text> {
    let Some(&index) = requests.ids.get(&r.id) else {
        let answered = r.reference.clone();
        requests.ids.insert(r.id.clone(), requests.list.len());
        requests.list.push(r);
        return answered;
    };
    let Request {
        evidence,
        time,
        end_time,
        reference,
        mut parts,
        mut locations,
        meta: incoming,
        ..
    } = r;
    let previous = &mut requests.list[index];
    // The shared fields are copied once here and shared again below.
    let meta = Arc::make_mut(&mut previous.meta);
    meta.profiles = strings.union(&meta.profiles, &incoming.profiles);
    previous.locations.append(&mut locations);
    previous.time = previous.time.min(time);
    previous.end_time = previous.end_time.max(end_time);
    if meta.harness == Harness::Codex {
        let conflict = |a: &Option<Text>, b: &Option<Text>| {
            a.as_ref()
                .zip(b.as_ref())
                .is_some_and(|(a, b)| crate::prices::tier_name(a) != crate::prices::tier_name(b))
        };
        if previous
            .parts
            .iter()
            .zip(&parts)
            .any(|(a, b)| a.kind == b.kind && a.model == b.model && conflict(&a.tier, &b.tier))
        {
            union(
                &mut meta.issues,
                vec![strings.text("conflicting effective service tiers")],
            );
        }
        if let (Some(a), Some(b)) = (&meta.requested_speed, &incoming.requested_speed)
            && (conflict(&a.tier, &b.tier) || conflict(&a.speed, &b.speed))
        {
            union(
                &mut meta.issues,
                vec![strings.text("conflicting requested service tiers")],
            );
        }
        if meta.requested_speed.is_none() {
            meta.requested_speed = incoming.requested_speed.clone();
        } else if let (Some(old), Some(new)) =
            (&mut meta.requested_speed, &incoming.requested_speed)
        {
            if old.tier.is_none() {
                old.tier = new.tier.clone();
            }
            if old.speed.is_none() {
                old.speed = new.speed.clone();
            }
        }
        for (old, new) in previous.parts.iter_mut().zip(&parts) {
            if old.kind == new.kind && old.model == new.model && old.tier.is_none() {
                old.tier = new.tier.clone();
            }
        }
    }
    // A streamed message's usage is a running snapshot, never an increment.
    let output =
        |parts: &[BillingPart]| -> i64 { parts.iter().map(|p| p.usage.output.unwrap_or(0)).sum() };
    let old_output = output(&previous.parts);
    let new_output = output(&parts);
    if parts.len() > previous.parts.len()
        || (parts.len() == previous.parts.len()
            && new_output >= old_output
            && end_time >= previous.end_time)
    {
        for (index, p) in parts.iter_mut().enumerate() {
            if let Some(old) = previous.parts.get(index)
                && old.kind == p.kind
                && old.model == p.model
            {
                fill(&mut p.usage, &old.usage);
                if p.tier.is_none() {
                    p.tier = old.tier.clone();
                }
            }
        }
        previous.parts = parts;
        previous.evidence = evidence;
        if incoming.server_tools.is_some() {
            meta.server_tools = incoming.server_tools.clone();
        }
    }
    let mut answered = None;
    if previous.reference.is_none() {
        previous.reference = reference;
        answered = previous.reference.clone();
    }
    if meta.source.is_none() {
        meta.source = incoming.source.clone();
    }
    if meta.effort.is_none() {
        meta.effort = incoming.effort.clone();
    }
    if meta.provider.is_none() {
        meta.provider = incoming.provider.clone();
    }
    union(&mut meta.issues, incoming.issues.clone());
    strings.meta(&mut previous.meta);
    answered
}

/// The profiles among `all` that a history is shared by.
fn sharing<'a>(all: &'a [Profile], ids: &[String]) -> Vec<&'a Profile> {
    all.iter().filter(|p| ids.contains(&p.id)).collect()
}

/// The single provider these profiles agree on, if they agree.
fn agreed(profiles: &[&Profile]) -> Option<Option<String>> {
    let first = profiles.first()?;
    profiles
        .iter()
        .all(|p| p.provider == first.provider)
        .then(|| first.provider.clone())
}

pub(crate) fn now() -> Millis {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970")
        .as_millis() as Millis
}

/// Parse request and structural metadata, deduplicating streams and inherited history.
pub fn scan(options: &ScanOptions) -> Result<Scan, ScanError> {
    let since = options.since;
    let snapshot_started_at = iso(now());
    let Discovery {
        roots,
        mut profiles,
        mut snapshots,
        mut t3,
    } = discover(&options.discovery)?;
    let snapshot_finished_at = iso(now());
    let mut native_ids = crate::t3::NativeIds::default();
    let has_t3 = profiles.iter().any(|p| p.t3.is_some());
    let mut reader = Reader {
        since,
        exposure: ExposureCollector::new(since),
        strings: Interner::default(),
        requests: Requests::default(),
        events: vec![],
        event_ids: EventIndex::default(),
        compact_parents: HashMap::default(),
        notice_parents: HashMap::default(),
        refs: Refs::default(),
        issues: IndexSet::default(),
        modern_totals: HashSet::default(),
        legacy: vec![],
        subagents: IndexMap::default(),
        earlier: vec![],
        sessions: Sessions::default(),
    };
    reader.exposure.roots(&profiles);
    for snapshot in &mut snapshots {
        // Records update the snapshot's counters while records are consumed against its metadata.
        let view = snapshot.clone();
        let mut file = FileState::new(&view, &mut reader.strings);
        for record in read_snapshot(snapshot)? {
            let (raw, location) = record?;
            reader
                .consume(&view, &mut file, &raw, &location)
                .map_err(|error| ScanError::at(error.message, location))?;
            if has_t3 {
                native_ids.observe(&raw, view.harness, &file.context.session);
            }
        }
    }
    let Reader {
        exposure,
        mut strings,
        mut requests,
        mut events,
        event_ids,
        compact_parents,
        notice_parents,
        mut refs,
        mut issues,
        modern_totals,
        legacy,
        subagents,
        earlier,
        sessions,
        ..
    } = reader;
    for (r, total_key) in legacy {
        if !modern_totals.contains(&total_key) {
            let answered = add(&mut requests, &mut strings, r);
            refs.answer(answered);
        }
    }
    // Records no request answers become requests themselves.
    let Refs {
        records: mut refs,
        answered,
    } = refs;
    drop(answered);
    let mut unanswered = vec![];
    for record in refs.values_mut() {
        if let Ref::Pending(r) = record {
            let facts = RefFacts::of(r);
            let Ref::Pending(r) = std::mem::replace(record, Ref::Linked(facts)) else {
                unreachable!("matched as pending")
            };
            unanswered.push(*r);
        }
    }
    for r in unanswered {
        add(&mut requests, &mut strings, r);
    }
    let Requests {
        list: mut requests,
        ids,
    } = requests;
    drop(ids);
    let mut ref_to_request: HashMap<Text, Text> = HashMap::default();
    for r in &mut requests {
        if let Some(reference) = &r.reference {
            ref_to_request.insert(reference.clone(), r.id.clone());
            if let Some(Ref::Linked(found)) = refs.get(reference) {
                if r.source.is_none() {
                    r.source = found.source.clone();
                }
                r.attempt = found.attempt.clone();
                if r.effort.is_none() {
                    r.effort = found.effort.clone();
                }
                if found.requested_speed.is_some() {
                    r.requested_speed = found.requested_speed.clone();
                }
                r.time = r.time.min(found.time);
                r.profiles = strings.union(&r.profiles, &found.profiles);
            }
        }
        if r.source.as_deref().is_some_and(|s| s.starts_with("agent:")) {
            r.agent = Agent::Subagent;
        }
        if matches!(r.source.as_deref(), Some("compact" | "compaction")) {
            for p in &mut r.parts {
                p.kind = PartKind::Compaction;
            }
        }
        if r.provider.is_none()
            && let Some(Some(provider)) = agreed(&sharing(&profiles, &r.profiles))
        {
            r.provider = Some(strings.text(&provider));
            r.issues.push(
                strings.text("provider inferred from current profile settings applied to history"),
            );
        }
        if r.harness == Harness::Claude && r.provider.is_none() {
            if sharing(&profiles, &r.profiles)
                .iter()
                .all(|p| p.provider.is_none())
            {
                r.provider = Some(strings.text("anthropic"));
                r.issues.push(
                    strings
                        .text("public Anthropic API equivalent; historical endpoint not recorded"),
                );
            } else {
                r.issues.push(strings.text(
                    "shared history has conflicting current provider conditions; historical endpoint unknown",
                ));
            }
        }
        r.locations.shrink_to_fit();
        strings.meta(&mut r.meta);
    }
    // The provider an event's pricing is filled with depends only on its shared fields, so
    // fill each shared copy once. Each entry keeps its source alive, so no other copy can
    // reuse the address it is keyed by.
    let mut filled: HashMap<usize, (Arc<EventMeta>, Arc<EventMeta>)> = HashMap::default();
    for e in &mut events {
        if let Some(reference) = e.request.as_deref().and_then(|r| r.strip_prefix("ref:")) {
            e.request = ref_to_request.get(reference).cloned();
        }
        if e.harness == Harness::Claude
            && let Some(pricing) = &e.pricing
            && pricing.provider.is_none()
            && let Some(provider) = agreed(&sharing(&profiles, &e.profiles))
        {
            let (_, shared) = filled
                .entry(Arc::as_ptr(&e.meta) as usize)
                .or_insert_with(|| {
                    let provider = provider.unwrap_or_else(|| "anthropic".into());
                    let mut meta = (*e.meta).clone();
                    meta.pricing = Some(Arc::new(Pricing {
                        provider: Some(strings.text(&provider)),
                        ..(**pricing).clone()
                    }));
                    let mut shared = Arc::new(meta);
                    strings.event_meta(&mut shared);
                    (e.meta.clone(), shared)
                });
            e.meta = shared.clone();
        }
    }
    drop(filled);
    attribute_idle(&mut events, &event_ids, &compact_parents, &notice_parents);
    drop((
        event_ids,
        compact_parents,
        notice_parents,
        refs,
        ref_to_request,
    ));
    // Every request and event stays in memory, so sort in place: a stable slice sort would
    // buffer half of them, and splitting at the window keeps the selected part where it is.
    // Request IDs are a harness name and a hex digest, where byte order is locale order.
    requests.sort_by_cached_key(|r| (r.time, r.id.clone()));
    events.sort_by_cached_key(|e| e.time);
    discover_claude_controls(&options.discovery, &mut profiles, &requests)?;
    discover_codex_controls(&options.discovery, &mut profiles, &requests)?;
    crate::subagents::discover(&options.discovery, &mut profiles, &requests, &earlier)?;
    crate::t3::enrich(
        &mut t3,
        &mut profiles,
        &mut requests,
        &mut events,
        &native_ids,
        &snapshot_finished_at,
        since,
        &mut strings,
    );
    let before = |time: Millis| since.is_some_and(|since| time < since);
    let split = requests.partition_point(|r| before(r.time));
    let context_requests: Vec<Request> = requests.drain(..split).collect();
    requests.shrink_to_fit();
    let split = events.partition_point(|e| before(e.time));
    let context_events: Vec<StructuralEvent> = events
        .drain(..split)
        .filter(|e| matches!(e.kind, EventKind::Compaction | EventKind::TurnStart))
        .collect();
    events.shrink_to_fit();
    for r in &requests {
        for issue in &r.issues {
            if !issues.contains(issue) {
                issues.insert(issue.clone());
            }
        }
    }
    if events.iter().any(|e| e.kind == EventKind::Compaction) {
        issues.insert(
            "only native informational idle notices attribute applied idle compaction; precomputed alone and final missing boundaries do not establish summary generation or absence"
                .into(),
        );
    }
    let coverage = ScanCoverage {
        requests: requests.len() as u64,
        source_known: requests
            .iter()
            .filter(|r| r.source.is_some() || r.trigger.is_some())
            .count() as u64,
        tier_known: requests
            .iter()
            .filter(|r| {
                r.parts
                    .iter()
                    .all(|p| p.tier.is_some() && !crate::prices::conditional_tier(r, p))
            })
            .count() as u64,
        missing_usage: requests
            .iter()
            .filter(|r| {
                r.parts
                    .iter()
                    .any(|p| p.usage.input.is_none() || p.usage.output.is_none())
            })
            .count() as u64,
        deferred_files: snapshots.iter().filter(|s| s.deferred_bytes > 0).count() as u64,
        skipped_lines: snapshots.iter().map(|s| s.skipped_lines).sum(),
        skipped_files: snapshots.iter().filter(|s| s.skipped_lines > 0).count() as u64,
        issues: issues.into_iter().map(|issue| issue.to_string()).collect(),
    };
    Ok(Scan {
        snapshot_started_at,
        snapshot_finished_at,
        since,
        roots,
        profiles,
        snapshots,
        exposure: exposure.finish(&requests, &context_requests, since),
        requests,
        events,
        cache_context: CacheContext {
            requests: context_requests,
            codex: earlier,
            prompts: context_events
                .iter()
                .filter(|e| e.kind == EventKind::TurnStart)
                .cloned()
                .collect(),
            boundaries: context_events
                .into_iter()
                .filter(|e| e.kind == EventKind::Compaction)
                .collect(),
        },
        coverage,
        subagents: subagents.into_values().collect(),
        t3,
        sessions,
    })
}

/// Mark automatic boundaries that an idle notice reaches through explicit compaction lineage.
///
/// Attribution only ever sets `notice`, so the order notices are visited in does not matter.
fn attribute_idle(
    events: &mut [StructuralEvent],
    ids: &EventIndex,
    compact_parents: &HashMap<Key, Option<Link>>,
    notice_parents: &HashMap<Id, HashSet<Option<Key>>>,
) {
    let notices: Vec<(Millis, Vec<String>, Option<Key>)> = events
        .iter()
        .filter(|e| e.kind == EventKind::IdleNotice)
        .map(|e| {
            let parents = notice_parents.get(&e.id);
            let parent = match parents {
                Some(parents) if parents.len() == 1 => *parents.iter().next().unwrap(),
                _ => None,
            };
            let sessions = e.origins.iter().map(|o| o.session.to_string()).collect();
            (e.time, sessions, parent)
        })
        .collect();
    for (time, sessions, mut parent) in notices {
        let mut visited = HashSet::default();
        while let Some(current) = parent
            && visited.insert(current)
        {
            let Some(Some(link)) = compact_parents.get(&current) else {
                break;
            };
            if let Some(boundary) = &link.boundary {
                if let Some(boundary) = ids.get(events, boundary)
                    && let boundary = &mut events[boundary]
                    && boundary.trigger.as_deref() == Some("auto")
                    && boundary.time <= time
                    && boundary.idle_attribution != IdleAttribution::Notice
                    && boundary
                        .origins
                        .iter()
                        .any(|o| sessions.iter().any(|s| **s == *o.session))
                {
                    boundary.idle_attribution = IdleAttribution::Notice;
                }
                break;
            }
            parent = link.parent;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Workspace;
    use serde_json::json;
    use std::path::Path;

    fn text(path: &Path) -> String {
        // Windows paths joined from "a/b" keep the slash; the code under test normalizes it.
        let path: std::path::PathBuf = if cfg!(windows) {
            path.components().collect()
        } else {
            path.to_path_buf()
        };
        path.to_string_lossy().into_owned()
    }

    fn options(w: &Workspace) -> ScanOptions {
        w.scan_options()
    }

    #[test]
    fn copied_codex_tier_evidence_is_preserved_or_marked_conflicting() {
        let mut r = crate::test_support::make_request();
        r.harness = Harness::Codex;
        r.parts[0].tier = Some("priority".into());
        let mut missing = r.clone();
        missing.parts[0].tier = None;
        let s = &mut Interner::default();
        let mut requests = Requests::default();
        add(&mut requests, s, r.clone());
        add(&mut requests, s, missing.clone());
        let id = r.id.clone();
        let get = |requests: &Requests| requests.get(&id).unwrap().clone();
        assert_eq!(get(&requests).parts[0].tier.as_deref(), Some("priority"));
        requests = Requests::default();
        add(&mut requests, s, missing);
        add(&mut requests, s, r.clone());
        assert_eq!(get(&requests).parts[0].tier.as_deref(), Some("priority"));
        r.parts[0].tier = Some("default".into());
        add(&mut requests, s, r.clone());
        add(&mut requests, s, r.clone());
        assert_eq!(
            get(&requests)
                .issues
                .iter()
                .filter(|s| &***s == "conflicting effective service tiers")
                .count(),
            1
        );
        assert!(
            get(&requests)
                .issues
                .iter()
                .any(|s| &**s == "conflicting effective service tiers")
        );
        requests = Requests::default();
        r.parts[0].tier = None;
        r.requested_speed = Some(RequestedSpeed {
            tier: Some("fast".into()),
            speed: None,
        });
        add(&mut requests, s, r.clone());
        r.requested_speed.as_mut().unwrap().tier = Some("priority".into());
        add(&mut requests, s, r.clone());
        assert!(get(&requests).issues.is_empty());
        r.requested_speed.as_mut().unwrap().tier = Some("default".into());
        add(&mut requests, s, r.clone());
        assert!(
            get(&requests)
                .issues
                .iter()
                .any(|s| &**s == "conflicting requested service tiers")
        );
        requests = Requests::default();
        r.requested_speed = None;
        r.parts[0].tier = Some("priority".into());
        add(&mut requests, s, r.clone());
        let mut summary = r.clone();
        summary.parts[0].kind = PartKind::Compaction;
        summary.parts[0].tier = Some("default".into());
        add(&mut requests, s, summary);
        assert!(get(&requests).issues.is_empty());
    }

    fn since(options: ScanOptions, instant: &str) -> ScanOptions {
        let instant: jiff::Timestamp = instant.parse().unwrap();
        ScanOptions {
            since: Some(instant.as_millisecond()),
            ..options
        }
    }

    fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) {
        let path = path.as_ref();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn append(path: impl AsRef<Path>, contents: &str) {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(contents.as_bytes()).unwrap();
    }

    fn jsonl(records: &[Value]) -> String {
        records.iter().map(|r| format!("{r}\n")).collect()
    }

    fn private(scan: &Scan) -> bool {
        serde_json::to_string(scan)
            .unwrap()
            .contains("SYNTHETIC_PRIVATE")
    }

    fn assistant(session: &str, message: &str) -> Value {
        json!({
            "type": "assistant",
            "sessionId": session,
            "timestamp": "2026-11-01T00:00:00Z",
            "cwd": "/synthetic/project",
            "message": {
                "id": message,
                "model": "claude-opus-5-5",
                "usage": {
                    "input_tokens": 1,
                    "output_tokens": 2,
                    "cache_read_input_tokens": 0,
                    "cache_creation_input_tokens": 0,
                },
            },
        })
    }

    #[test]
    fn claude_streams_deduplicate_link_source_and_keep_only_structural_metadata() {
        let w = Workspace::new();
        w.fixture(w.claude.join("projects/project/main.jsonl"), "claude.jsonl");
        let data = scan(&options(&w)).unwrap();
        assert_eq!(data.requests.len(), 2);
        assert_eq!(data.requests[0].parts[0].usage.output, Some(20));
        assert_eq!(data.requests[0].source.as_deref(), Some("sdk"));
        assert_eq!(data.requests[0].locations.len(), 2);
        assert_eq!(data.events[0].precomputed, Some(true));
        assert_eq!(data.events[0].idle_attribution, IdleAttribution::Unknown);
        assert_eq!(data.events[0].pre_tokens, Some(1100));
        assert_eq!(data.events[0].post_tokens, Some(100));
        assert!(!private(&data));
    }

    #[test]
    fn a_later_copy_of_a_request_fills_its_unrecorded_effort() {
        let w = Workspace::new();
        let mut later = assistant("session", "message");
        later["effort"] = json!("high");
        write(
            w.claude.join("projects/project/main.jsonl"),
            jsonl(&[assistant("session", "message"), later]),
        );
        let data = scan(&options(&w)).unwrap();
        assert_eq!(data.requests.len(), 1);
        assert_eq!(data.requests[0].effort.as_deref(), Some("high"));
    }

    #[test]
    fn codex_response_usage_replaces_overlapping_totals_and_duplicate_response_ids() {
        let w = Workspace::new();
        w.fixture(w.codex.join("sessions/main.jsonl"), "codex.jsonl");
        let data = scan(&options(&w)).unwrap();
        assert_eq!(data.requests.len(), 1);
        assert_eq!(data.requests[0].parts[0].usage.input, Some(1000));
        assert_eq!(data.requests[0].parts[0].usage.reasoning, Some(20));
        assert_eq!(data.requests[0].accounting, Accounting::Individual);
        assert_eq!(data.events.last().unwrap().kind, EventKind::TurnEnd);
        assert!(!private(&data));
    }

    // Creating the shared directory needs a symlink, which Windows reserves for privileged users.
    #[cfg(unix)]
    #[test]
    fn shared_directories_are_read_once_keep_profile_aliases_and_include_nested_subagents() {
        use crate::model::{RootState, Ttl};
        let w = Workspace::new();
        let alternate = w.home.join(".claude-work");
        std::fs::create_dir(&alternate).unwrap();
        std::os::unix::fs::symlink(w.claude.join("projects"), alternate.join("projects")).unwrap();
        write(
            alternate.join("settings.json"),
            json!({ "promptCacheTtl": "1h" }).to_string(),
        );
        w.fixture(w.claude.join("projects/project/main.jsonl"), "claude.jsonl");
        write(
            w.claude.join("projects/project/main/subagents/agent.jsonl"),
            jsonl(&[assistant("child", "child-message")]),
        );
        let data = scan(&options(&w)).unwrap();
        assert_eq!(data.snapshots.len(), 2);
        assert_eq!(data.requests.len(), 3);
        assert!(data.requests.iter().all(|r| r.profiles.len() == 2));
        let subagents = data.requests.iter().filter(|r| r.agent == Agent::Subagent);
        assert_eq!(subagents.count(), 1);
        let mut selected = options(&w);
        selected.discovery.claude_dirs = vec![text(&alternate)];
        let selected = scan(&selected).unwrap();
        assert_eq!(selected.profiles.len(), 1);
        assert!(
            selected
                .roots
                .iter()
                .any(|r| r.path == text(&w.claude) && r.status == RootState::Excluded)
        );
        assert_eq!(selected.profiles[0].ttl.main.value, Some(Ttl::OneHour));
    }

    #[test]
    fn subagent_types_come_from_metadata_or_records_and_never_keep_the_task() {
        let w = Workspace::new();
        let dir = w.claude.join("projects/project/parent/subagents");
        let mut explore = assistant("parent", "explore-message");
        explore["effort"] = json!("medium");
        write(dir.join("agent-a.jsonl"), jsonl(&[explore]));
        write(
            dir.join("agent-a.meta.json"),
            json!({
                "agentType": "Explore",
                "isBuiltIn": true,
                "model": "haiku",
                "effort": "low",
                "description": "SYNTHETIC_PRIVATE task",
            })
            .to_string(),
        );
        // No metadata file: the records name the type.
        let mut older = assistant("parent", "older-message");
        older["attributionAgent"] = json!("reviewer");
        write(dir.join("agent-b.jsonl"), jsonl(&[older]));
        // Unreadable metadata records nothing rather than failing the scan.
        write(
            dir.join("agent-c.jsonl"),
            jsonl(&[assistant("parent", "c")]),
        );
        write(dir.join("agent-c.meta.json"), "{");
        let data = scan(&options(&w)).unwrap();
        let kinds: Vec<(&str, Option<&str>, Option<&str>)> = data
            .subagents
            .iter()
            .map(|s| (s.kind.as_str(), s.model.as_deref(), s.effort.as_deref()))
            .collect();
        assert_eq!(
            kinds,
            [
                ("Explore", Some("haiku"), Some("low")),
                ("reviewer", None, None),
                (UNRECORDED, None, None),
            ]
        );
        let first = &data.subagents[0];
        assert_eq!(first.built_in, Some(true));
        assert_eq!(first.parent.as_deref(), Some(identity("parent").as_str()));
        let explore = data
            .requests
            .iter()
            .find(|r| r.session == first.session)
            .unwrap();
        assert_eq!(explore.effort.as_deref(), Some("medium"));
        assert!(!private(&data));
    }

    #[test]
    fn codex_subagents_are_named_by_role_or_source_with_their_effort() {
        let w = Workspace::new();
        let thread = |id: &str, source: Value| {
            let mut meta = json!({
                "type": "session_meta",
                "timestamp": "2026-11-01T00:00:00Z",
                "payload": { "id": id, "source": source, "model_provider": "openai" },
            });
            // Every thread names its root; a review names no other parent.
            meta["payload"]["session_id"] = json!("thread-root");
            meta
        };
        let spawn = |role: Value| {
            json!({ "subagent": { "thread_spawn": {
            "parent_thread_id": "thread-a", "depth": 1, "agent_role": role,
        } } })
        };
        let turn = json!({
            "type": "turn_context",
            "timestamp": "2026-11-01T00:00:00Z",
            "payload": { "turn_id": "turn-x", "model": "gpt-6.1-sol", "effort": "low" },
        });
        for (name, source) in [
            ("default", spawn(Value::Null)),
            ("explorer", spawn(json!("explorer"))),
            ("review", json!({ "subagent": "review" })),
        ] {
            let usage = json!({
                "type": "token_usage_record",
                "timestamp": "2026-11-01T00:00:01Z",
                "payload": { "response_id": format!("response-{name}"), "turn_id": "turn-x",
                    "usage": { "input_tokens": 10, "output_tokens": 1 } },
            });
            write(
                w.codex.join(format!("sessions/{name}.jsonl")),
                jsonl(&[
                    thread(&format!("thread-{name}"), source),
                    turn.clone(),
                    usage,
                ]),
            );
        }
        let data = scan(&options(&w)).unwrap();
        let mut kinds: Vec<(&str, bool)> = data
            .subagents
            .iter()
            .map(|s| (s.kind.as_str(), s.native))
            .collect();
        kinds.sort();
        assert_eq!(
            kinds,
            [("default", false), ("explorer", false), ("review", true)]
        );
        // Spawns keep their immediate parent; only the review falls back to the root.
        let parent = |thread: &str| Some(Text::from(id(&json!(thread)).unwrap()));
        for s in &data.subagents {
            let expected = if s.native { "thread-root" } else { "thread-a" };
            assert_eq!(s.parent, parent(expected), "{}", s.kind);
        }
        assert!(
            data.requests
                .iter()
                .all(|r| r.agent == Agent::Subagent && r.effort.as_deref() == Some("low"))
        );
    }

    #[test]
    fn codex_keeps_pre_window_starts_and_model_changes_without_usage() {
        let w = Workspace::new();
        let line = |kind: &str, time: &str, payload: Value| json!({ "type": kind, "timestamp": time, "payload": payload });
        let usage = |id: &str, time: &str| {
            line(
                "token_usage_record",
                time,
                json!({ "response_id": id, "usage": { "input_tokens": 10, "output_tokens": 1 } }),
            )
        };
        write(
            w.codex.join("sessions/parent.jsonl"),
            jsonl(&[
                line(
                    "session_meta",
                    "2026-10-01T00:00:00Z",
                    json!({ "id": "thread-a", "source": "cli" }),
                ),
                line(
                    "turn_context",
                    "2026-10-01T00:00:00Z",
                    json!({ "model": "gpt-6-luna" }),
                ),
                usage("a1", "2026-10-01T00:00:01Z"),
                usage("a2", "2026-10-01T00:00:02Z"),
                line(
                    "turn_context",
                    "2026-10-02T00:00:00Z",
                    json!({ "model": "gpt-6.1-sol" }),
                ),
                usage("a3", "2026-10-02T00:00:01Z"),
                usage("a4", "2026-11-02T00:00:01Z"),
            ]),
        );
        let data = scan(&since(options(&w), "2026-11-01T00:00:00Z")).unwrap();
        assert_eq!(data.requests.len(), 1);
        // The transcript's first request and its model change, but not their cost.
        let earlier: Vec<(Millis, Option<&str>, usize)> = data
            .cache_context
            .codex
            .iter()
            .map(|r| (r.time, r.model.as_deref(), r.parts.len()))
            .collect();
        let at = |t: &str| crate::safe::parse_instant(t).unwrap();
        assert_eq!(
            earlier,
            [
                (at("2026-10-01T00:00:01Z"), Some("gpt-6-luna"), 0),
                (at("2026-10-02T00:00:01Z"), Some("gpt-6.1-sol"), 0),
            ]
        );
        assert!(data.cache_context.requests.is_empty());
    }

    #[test]
    fn lengths_freeze_before_reading_and_unterminated_records_and_appends_are_deferred() {
        let w = Workspace::new();
        let file = w.claude.join("projects/project/active.jsonl");
        write(
            &file,
            "{\"type\":\"custom-title\"}\n{\"private\":\"SYNTHETIC_PRIVATE",
        );
        let mut discovery = discover(&options(&w).discovery).unwrap();
        let snapshot = &mut discovery.snapshots[0];
        append(&file, "_REPLY\"}\n{\"type\":\"new\"}\n");
        let records = read_snapshot(snapshot).unwrap().map(Result::unwrap).count();
        assert_eq!(records, 1);
        assert!(snapshot.deferred_bytes > 0);
        assert_eq!(snapshot.lines, 1);
        assert!(std::fs::metadata(&file).unwrap().len() > snapshot.bytes);
    }

    #[test]
    fn invalid_selected_usage_fails_safely_while_unreadable_lines_are_counted_and_skipped() {
        let w = Workspace::new();
        let file = w.claude.join("projects/project/bad.jsonl");
        write(
            &file,
            "{\"timestamp\":\"2020-01-01T00:00:00Z\",\"type\":\"assistant\",\"message\":{\"usage\":{\"input_tokens\":\"bad\"}}}\n",
        );
        let windowed = since(options(&w), "2026-01-01T00:00:00Z");
        assert_eq!(scan(&windowed).unwrap().requests.len(), 0);
        append(
            &file,
            "{\"type\":\"assistant\",\"private\":\"SYNTHETIC_PRIVATE_REPLY\", broken}\n[1]\n",
        );
        let error = scan(&options(&w)).unwrap_err();
        assert_eq!(error.location.as_ref().map(|l| l.line), Some(1));
        assert!(!error.message.contains("SYNTHETIC_PRIVATE"));
        let skipped = scan(&windowed).unwrap();
        assert_eq!(skipped.coverage.skipped_lines, 2);
        assert_eq!(skipped.coverage.skipped_files, 1);
    }

    #[test]
    fn earlier_prompts_are_context_and_earlier_tool_results_never_fail() {
        let w = Workspace::new();
        let user = |time: &str, content: Value| {
            json!({
                "type": "user",
                "sessionId": "s",
                "uuid": format!("u-{time}"),
                "timestamp": time,
                "message": { "role": "user", "content": content },
            })
        };
        write(
            w.claude.join("projects/project/main.jsonl"),
            jsonl(&[
                user("2025-12-31T00:00:00Z", json!("SYNTHETIC_PRIVATE_PROMPT")),
                user(
                    "2025-12-31T00:01:00Z",
                    json!([{ "type": "tool_result", "tool_use_id": 7 }]),
                ),
                user("2026-01-02T00:00:00Z", json!("SYNTHETIC_PRIVATE_PROMPT")),
            ]),
        );
        let data = scan(&since(options(&w), "2026-01-01T00:00:00Z")).unwrap();
        assert_eq!(data.cache_context.prompts.len(), 1);
        assert_eq!(data.cache_context.prompts[0].kind, EventKind::TurnStart);
        assert_eq!(data.events.len(), 1);
        assert!(!private(&data));
    }

    #[test]
    fn legacy_counters_use_last_usage_ignore_repeated_totals_and_mark_resets_unknown() {
        let w = Workspace::new();
        let mut records = vec![
            json!({
                "type": "session_meta",
                "timestamp": "2026-11-01T00:00:00Z",
                "payload": { "id": "legacy", "source": "cli", "model_provider": "openai" },
            }),
            json!({
                "type": "turn_context",
                "timestamp": "2026-11-01T00:00:00Z",
                "payload": { "model": "gpt-5.4", "service_tier": "standard" },
            }),
        ];
        let u = |n: i64| {
            json!({
                "input_tokens": n,
                "cached_input_tokens": 0,
                "output_tokens": n,
                "reasoning_output_tokens": 0,
            })
        };
        for (minute, total, last) in [
            (1, 100, Some(100)),
            (2, 100, Some(100)),
            (3, 300, Some(200)),
            (4, 10, None),
            (5, 30, None),
        ] {
            let mut info = json!({ "total_token_usage": u(total) });
            if let Some(last) = last {
                info["last_token_usage"] = u(last);
            }
            records.push(json!({
                "type": "event_msg",
                "timestamp": format!("2026-11-01T00:0{minute}:00Z"),
                "payload": { "type": "token_count", "info": info },
            }));
        }
        write(w.codex.join("sessions/legacy.jsonl"), jsonl(&records));
        let data = scan(&options(&w)).unwrap();
        assert_eq!(data.requests.len(), 4);
        let inputs: Vec<_> = data
            .requests
            .iter()
            .map(|r| r.parts[0].usage.input)
            .collect();
        assert_eq!(inputs, [Some(100), Some(200), None, Some(20)]);
        assert!(!data.requests[2].issues.is_empty());
    }

    #[test]
    fn usage_validates_subset_invariants_and_keeps_missing_write_counters() {
        let codex = |v: Value| usage(&v, Harness::Codex).map_err(|e| e.message);
        let u = codex(json!({
            "input_tokens": 100,
            "output_tokens": 20,
            "cached_input_tokens": 30,
            "reasoning_output_tokens": 10,
        }))
        .unwrap();
        assert_eq!((u.write, u.output), (None, Some(20)));
        assert_eq!(
            codex(json!({ "input_tokens": 10, "cached_input_tokens": 11, "output_tokens": 1 })),
            Err("cache subsets exceed total input".into())
        );
        let negative = json!({ "input_tokens": 10, "output_tokens": -1 });
        assert!(usage(&negative, Harness::Claude).is_err());
        assert_eq!(
            codex(json!({ "input_tokens": 10, "output_tokens": 5, "reasoning_output_tokens": 6 })),
            Err("reasoning subset exceeds output".into())
        );
        let split = json!({
            "cache_creation_input_tokens": 3,
            "cache_creation": { "ephemeral_5m_input_tokens": 1, "ephemeral_1h_input_tokens": 1 },
        });
        assert_eq!(
            usage(&split, Harness::Claude).map_err(|e| e.message),
            Err("cache write lifetime counts disagree with total".into())
        );
    }

    #[test]
    fn polling_loops_need_a_loop_and_a_sleep() {
        for command in [
            "while ! test -f done; do sleep 30; done",
            "until gh pr checks 1; do sleep 60; done",
            "for i in $(seq 10); do curl -s x && break; sleep 5; done",
        ] {
            assert!(polling_loop(command), "{command}");
        }
        for command in [
            "sleep 300",
            "cargo build && sleep 1",
            "for f in *.rs; do wc -l $f; done",
            "./sleeper --while",
        ] {
            assert!(!polling_loop(command), "{command}");
        }
    }

    #[test]
    fn separate_subagent_files_do_not_share_a_session_when_parent_ids_match() {
        let w = Workspace::new();
        for name in ["agent-a", "agent-b"] {
            write(
                w.claude
                    .join(format!("projects/project/main/subagents/{name}.jsonl")),
                jsonl(&[assistant("same-parent", name)]),
            );
        }
        let data = scan(&options(&w)).unwrap();
        assert_eq!(data.requests.len(), 2);
        assert_ne!(data.requests[0].session, data.requests[1].session);
        assert_eq!(data.requests[0].cwd.as_deref(), Some("/synthetic/project"));
    }

    #[test]
    fn unlinked_api_attempts_expose_missing_usage_instead_of_disappearing() {
        let w = Workspace::new();
        write(
            w.claude.join("projects/project/main.jsonl"),
            jsonl(&[json!({
                "type": "api-request",
                "id": "orphan",
                "isSidechain": true,
                "sessionId": "session",
                "timestamp": "2026-11-01T00:00:00Z",
                "querySource": "sdk",
                "params": {
                    "model": "claude-opus-5-5",
                    "max_tokens": 1234,
                    "service_tier": "standard",
                    "messages": [{ "content": "SYNTHETIC_PRIVATE_PROMPT" }],
                },
            })]),
        );
        let data = scan(&options(&w)).unwrap();
        assert_eq!(data.requests.len(), 1);
        assert_eq!(data.coverage.missing_usage, 1);
        let r = &data.requests[0];
        assert_eq!(r.attempt.as_ref().unwrap().max_output_tokens, Some(1234));
        assert_eq!(r.agent, Agent::Subagent);
        assert_eq!(r.parts[0].tier, None);
        let requested = r.requested_speed.as_ref().unwrap();
        assert_eq!(requested.tier.as_deref(), Some("standard"));
        assert!(!private(&data));
        assert_eq!(r.evidence, RequestEvidence::Unknown);
        assert!(r.issues.join(" ").contains("no linked assistant usage"));
    }

    #[test]
    fn nested_codex_settings_and_trigger_attribution_keep_only_whitelisted_metadata() {
        let w = Workspace::new();
        let file = w.codex.join("sessions/main.jsonl");
        w.fixture(&file, "codex.jsonl");
        let raw = std::fs::read_to_string(&file).unwrap();
        let mut lines: Vec<String> = raw.trim_end().split('\n').map(str::to_string).collect();
        let applied = json!({
            "type": "event_msg",
            "timestamp": "2026-11-01T00:00:00Z",
            "payload": {
                "type": "thread_settings_applied",
                "thread_settings": {
                    "service_tier": "fast",
                    "model_provider_id": "openai",
                    "model": "gpt-6.1-sol",
                    "developer_instructions": "SYNTHETIC_PRIVATE",
                },
            },
        });
        lines.insert(2, applied.to_string());
        write(&file, lines.join("\n") + "\n");
        let data = scan(&options(&w)).unwrap();
        let r = &data.requests[0];
        assert_eq!(r.parts[0].tier, None);
        let requested = r.requested_speed.as_ref().unwrap();
        assert_eq!(requested.tier.as_deref(), Some("fast"));
        assert_eq!(r.source.as_deref(), Some("cli"));
        assert!(!private(&data));
    }

    #[test]
    fn large_utf8_records_span_chunks_and_a_shrunk_snapshot_fails() {
        let w = Workspace::new();
        let file = w.claude.join("projects/project/large.jsonl");
        // Longer than one read, so the record is assembled across chunks.
        let title = "😀".repeat(CHUNK / 4 + 40_000);
        let record = json!({ "type": "custom-title", "title": title });
        write(&file, record.to_string() + "\n");
        let mut discovery = discover(&options(&w).discovery).unwrap();
        let snapshot = &mut discovery.snapshots[0];
        let parsed = read_snapshot(snapshot).unwrap().map(Result::unwrap).count();
        assert_eq!((parsed, snapshot.skipped_lines), (1, 0));
        let mut original = discover(&options(&w).discovery).unwrap();
        write(&file, "{}\n");
        let error = read_snapshot(&mut original.snapshots[0]).err().unwrap();
        assert_eq!(
            error.message,
            "transcript changed identity or shrank after snapshot"
        );
        let mut absent = options(&w).discovery;
        absent.claude_dirs = vec![text(&w.home.join("absent"))];
        assert!(discover(&absent).is_err());
    }

    #[test]
    fn copied_claude_boundaries_deduplicate_across_changed_session_ids() {
        let w = Workspace::new();
        let file = w.claude.join("projects/project/main.jsonl");
        w.fixture(&file, "claude.jsonl");
        let copy = std::fs::read_to_string(&file).unwrap();
        write(
            w.claude.join("projects/project/copy.jsonl"),
            copy.replace("session-a", "session-copy"),
        );
        let result = scan(&options(&w)).unwrap();
        assert_eq!(result.requests.len(), 2);
        let compactions: Vec<_> = result
            .events
            .iter()
            .filter(|e| e.kind == EventKind::Compaction)
            .collect();
        assert_eq!(compactions.len(), 1);
        let sessions: HashSet<_> = compactions[0].origins.iter().map(|o| &o.session).collect();
        assert_eq!(sessions.len(), 2);
    }

    #[test]
    fn legacy_codex_copied_turn_identities_deduplicate_across_fork_session_metadata() {
        let w = Workspace::new();
        let raw = jsonl(&[
            json!({
                "type": "session_meta",
                "timestamp": "2026-11-01T00:00:00Z",
                "payload": { "id": "parent", "source": "cli", "model_provider": "openai" },
            }),
            json!({
                "type": "turn_context",
                "timestamp": "2026-11-01T00:00:00Z",
                "payload": { "turn_id": "shared-turn", "model": "gpt-5.4", "service_tier": "standard" },
            }),
            json!({
                "type": "event_msg",
                "timestamp": "2026-11-01T00:01:00Z",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": { "input_tokens": 100, "output_tokens": 10 },
                        "last_token_usage": { "input_tokens": 100, "output_tokens": 10 },
                    },
                },
            }),
        ]);
        write(w.codex.join("sessions/parent.jsonl"), &raw);
        let fork = raw.replacen(
            "\"id\":\"parent\"",
            "\"id\":\"fork\",\"forked_from_id\":\"parent\"",
            1,
        );
        write(w.codex.join("sessions/fork.jsonl"), fork);
        let result = scan(&options(&w)).unwrap();
        assert_eq!(result.requests.len(), 1);
        assert_eq!(result.requests[0].locations.len(), 2);
    }

    #[test]
    fn empty_claude_iterations_keep_top_level_usage() {
        let w = Workspace::new();
        w.fixture(
            w.claude.join("projects/project/main.jsonl"),
            "claude-empty-iterations.jsonl",
        );
        let result = scan(&options(&w)).unwrap();
        assert_eq!(result.requests.len(), 1);
        let part = &result.requests[0].parts[0];
        assert_eq!(part.usage.write5m, Some(1000));
        assert_eq!(part.usage.output, Some(10));
        assert_eq!(part.geo.as_deref(), Some("global"));
    }

    #[test]
    fn unexpected_normalization_failures_stay_internal_errors_not_malformed_records() {
        let w = Workspace::new();
        write(w.claude.join("projects/project/main.jsonl"), "{}\n");
        let mut discovery = discover(&options(&w).discovery).unwrap();
        let snapshot = &mut discovery.snapshots[0];
        // Consumers normalize outside the reader, so their failures, panics included, propagate
        // from the consumer and a record that reaches one is never counted as malformed.
        let mut records = read_snapshot(snapshot).unwrap();
        let (raw, location) = records.next().unwrap().unwrap();
        assert!(raw.is_empty());
        assert_eq!(location.line, 1);
        assert!(records.next().is_none());
        assert_eq!((snapshot.lines, snapshot.skipped_lines), (1, 0));
    }

    #[test]
    fn a_custom_base_url_proxy_is_priced_as_the_public_anthropic_api() {
        let w = Workspace::new();
        let settings = json!({ "env": { "ANTHROPIC_BASE_URL": "https://proxy.invalid" } });
        write(w.claude.join("settings.json"), settings.to_string());
        w.fixture(w.claude.join("projects/project/main.jsonl"), "claude.jsonl");
        let data = scan(&options(&w)).unwrap();
        assert_eq!(data.profiles[0].provider, None);
        assert!(!data.requests.is_empty());
        let anthropic = |r: &Request| r.provider.as_deref() == Some("anthropic");
        assert!(data.requests.iter().all(anthropic));
    }

    #[test]
    fn lines_decode_like_javascript() {
        let w = Workspace::new();
        let mut bytes = b"\xEF\xBB\xBF{\"type\":\"custom-title\"}\n".to_vec();
        bytes.extend_from_slice(br#"{"title":"\ud83d \\ud800 \udc00"}"#);
        bytes.extend_from_slice(b"\n");
        // A valid escaped pair, split so it stays escaped in this source.
        bytes.extend_from_slice([r#"{"title":"\ud83d"#, r#"\ude00"}"#].concat().as_bytes());
        bytes.extend_from_slice(b"\n{\"title\":\"\xff\"}\n\n42\n");
        write(w.claude.join("projects/project/main.jsonl"), bytes);
        let mut discovery = discover(&options(&w).discovery).unwrap();
        let snapshot = &mut discovery.snapshots[0];
        let titles: Vec<Option<String>> = read_snapshot(snapshot)
            .unwrap()
            .map(|r| string(&r.unwrap().0, "title").map(str::to_string))
            .collect();
        let expected = [
            None,
            Some("\u{fffd} \\ud800 \u{fffd}".to_string()),
            Some("😀".to_string()),
        ];
        assert_eq!(titles, expected);
        assert_eq!((snapshot.lines, snapshot.skipped_lines), (6, 3));
    }

    #[test]
    fn idle_notices_attribute_only_auto_boundaries_on_their_lineage() {
        let w = Workspace::new();
        let record = |mut value: Value| {
            value["sessionId"] = json!("native");
            value
        };
        let boundary = |uuid: &str, trigger: &str, second: u32| {
            record(json!({
                "type": "system",
                "subtype": "compact_boundary",
                "uuid": uuid,
                "timestamp": format!("2026-11-01T00:00:{second:02}Z"),
                "compactMetadata": { "trigger": trigger },
            }))
        };
        let notice = |uuid: &str, parent: &str| {
            record(json!({
                "type": "system",
                "subtype": "informational",
                "level": "notice",
                "uuid": uuid,
                "parentUuid": parent,
                "timestamp": "2026-11-01T00:00:30Z",
                "content": "Compacted using a summary written while you were away ·\u{a0} turn off with \"idleCompaction\": false in settings.json",
            }))
        };
        let user = |uuid: &str, parent: &str, summary: bool| {
            record(json!({
                "type": "user",
                "isCompactSummary": summary,
                "uuid": uuid,
                "parentUuid": parent,
                "timestamp": "2026-11-01T00:00:10Z",
                "message": { "content": "SYNTHETIC_PRIVATE_SUMMARY" },
            }))
        };
        write(
            w.claude.join("projects/project/main.jsonl"),
            jsonl(&[
                boundary("auto", "auto", 1),
                user("summary", "auto", true),
                notice("first", "summary"),
                boundary("manual", "manual", 2),
                notice("second", "manual"),
                // An ordinary message stops the walk before the boundary.
                boundary("orphan", "auto", 3),
                user("message", "orphan", false),
                notice("third", "message"),
            ]),
        );
        fn attribution(events: &[StructuralEvent]) -> Vec<(Option<&str>, IdleAttribution)> {
            events
                .iter()
                .filter(|e| e.kind == EventKind::Compaction)
                .map(|e| (e.trigger.as_deref(), e.idle_attribution))
                .collect()
        }
        let expected = [
            (Some("auto"), IdleAttribution::Notice),
            (Some("manual"), IdleAttribution::Unknown),
            (Some("auto"), IdleAttribution::Unknown),
        ];
        let data = scan(&options(&w)).unwrap();
        assert_eq!(attribution(&data.events), expected);
        assert!(!private(&data));
        // Notices before --since still mark their boundaries, which stay as cache context.
        let mut before = options(&w);
        before.since = Some(crate::safe::parse_instant("2026-11-01T00:01:00Z").unwrap());
        let data = scan(&before).unwrap();
        assert_eq!(attribution(&data.cache_context.boundaries), expected);
    }
}
