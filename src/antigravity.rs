//! Antigravity: where its CLI (`agy`) and its ACP server keep their history, the requests
//! their transcripts record, and the MCP servers, skills and rule files it offers.
//!
//! Both keep their state under `GEMINI_HOME` (`~/.gemini`): the CLI in `antigravity-cli/`,
//! the ACP server T3 Code runs in `antigravity-acp/`. Each conversation keeps a transcript of
//! its steps, `brain/<id>/.system_generated/logs/transcript.jsonl`, and every step that called
//! a model records its model and token counts there. Only each step's type, index, time, model,
//! counts and tool names, with the path of a `SKILL.md` it read, are read; its prompt, thinking,
//! reply and tool text are skipped unread. `conversation_summaries.db` names each
//! conversation's workspace and, for a subagent, the conversation that started it and its agent.

use std::io::{BufRead, Read};
use std::sync::Arc;

use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::collections::{HashMap, HashSet, IndexMap, IndexSet};
use crate::controls::Env;
use crate::discovery::{absent, error, join, resolve};
use crate::model::{
    Agent, AgentControls, Api, BillingPart, CompactionControls, ContextKind, Few, Harness,
    Location, Millis, PartKind, PerBucket, Profile, Request, RequestEvidence, RequestMeta,
    Snapshot, Subagent, Text, TtlControl, Usage, WindowControl,
};
use crate::safe::{ScanError, count, identity, label, parse_instant};
use crate::sessions::Native;

/// The CLI's directory under `GEMINI_HOME`.
pub(crate) const CLI: &str = "antigravity-cli";

/// The ACP server's directory under its `GEMINI_HOME`, which T3 Code gives each instance.
pub(crate) const ACP: &str = "antigravity-acp";

/// The models.dev provider every Antigravity model is priced under.
const PROVIDER: &str = "google";

/// The reasoning efforts a recorded model slug can end in, such as `gemini-4-neon-high`.
const EFFORTS: [&str; 5] = ["minimal", "low", "medium", "high", "xhigh"];

/// The database that names each conversation's workspace and parent, in the root.
const SUMMARIES: &str = "conversation_summaries.db";

/// What an Antigravity request's pricing assumes, since Antigravity records no tier.
const STANDARD_TIER: &str =
    "Antigravity records no service tier; priced at Google's standard catalog rate";

/// Why a request with no recorded model is priced at its conversation's.
const CACHED_MODEL: &str = "Antigravity recorded no model for this request; it read the conversation's cache, which Gemini keeps per model, and every earlier request in the conversation ran one model, so it is priced at that model";

/// `$GEMINI_HOME`, else `~/.gemini`.
pub(crate) fn gemini_home(home: &str, env: &Env) -> String {
    match env.get("GEMINI_HOME").filter(|d| !d.is_empty()) {
        Some(dir) => resolve(dir),
        None => join(&[home, ".gemini"]),
    }
}

/// The CLI's root, which holds its conversations.
pub(crate) fn cli_root(home: &str, env: &Env) -> String {
    join(&[&gemini_home(home, env), CLI])
}

/// Every conversation's transcript under `root`, in path order.
fn transcripts(root: &str) -> Result<Vec<String>, ScanError> {
    let brain = join(&[root, "brain"]);
    let entries = match std::fs::read_dir(&brain) {
        Ok(entries) => entries,
        Err(e) if absent(e.kind()) => return Ok(vec![]),
        Err(_) => return Err(error("cannot list Antigravity conversations", &brain, 0)),
    };
    let mut files = vec![];
    for entry in entries {
        let entry = entry.map_err(|_| error("cannot list Antigravity conversations", &brain, 0))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let file = join(&[
            &brain,
            &name,
            ".system_generated",
            "logs",
            "transcript.jsonl",
        ]);
        match std::fs::metadata(&file) {
            Ok(info) if info.is_file() => files.push(file),
            Ok(_) => {}
            Err(e) if absent(e.kind()) => {}
            Err(_) => return Err(error("cannot inspect Antigravity transcript", &file, 0)),
        }
    }
    files.sort();
    Ok(files)
}

/// One Antigravity root's profile: its transcripts, and no setting token-max changes, since
/// no record says what Antigravity sent besides the conversation.
pub(crate) fn profile(canonical: String, alias: String, key: &str) -> Result<Profile, ScanError> {
    let histories = transcripts(&canonical)?;
    let unset = |reason: &str| TtlControl {
        value: None,
        owner: String::new(),
        reason: reason.into(),
        varies: false,
    };
    Ok(Profile {
        id: identity(key),
        harness: Harness::Antigravity,
        settings_file: join(&[&canonical, "settings.json"]),
        root: canonical,
        aliases: vec![alias],
        histories,
        ttl: PerBucket {
            main: unset("Antigravity's cache lifetime is not replayed"),
            subagent: unset("Antigravity's cache lifetime is not replayed"),
        },
        provider: None,
        billing: None,
        settings: Default::default(),
        mcp_servers: vec![],
        compaction: CompactionControls {
            default: WindowControl {
                value: None,
                owner: String::new(),
                reason: "Antigravity's compaction is not replayed".into(),
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
        agents: AgentControls::default(),
        instruction_budget_file: None,
        instruction_budget_unresolved: None,
        outranking_files: vec![],
        ttl_directories: IndexMap::default(),
        claude_config: None,
        updater: None,
        codex_system_config: None,
        switched_off: vec![],
        pretended_on: vec![],
        policy_off: vec![],
        cache_stack: None,
    })
}

/// One root's requests, conversations and subagents, ready for the reader.
#[derive(Debug, Default)]
pub(crate) struct History {
    pub requests: Vec<Request>,
    /// Each conversation, with its transcript.
    pub sessions: Vec<(Native, Text)>,
    pub subagents: Vec<Subagent>,
    /// Each transcript as read, with the unfinished last line it left unread.
    pub snapshots: Vec<Snapshot>,
    /// Every tool call the transcripts record.
    pub calls: Vec<Call>,
}

/// One tool call a transcript records: its tool, and the `SKILL.md` it read, if any.
#[derive(Debug)]
pub(crate) struct Call {
    pub time: Millis,
    pub tool: String,
    pub skill: Option<String>,
}

/// What `conversation_summaries.db` says about one conversation.
#[derive(Debug, Default, PartialEq)]
struct Summary {
    cwd: Option<Text>,
    parent: Option<String>,
    agent: Option<String>,
}

/// The local path a `file://` URI names, when it is absolute and UTF-8.
fn file_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let bytes = rest.as_bytes();
    let mut path = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            path.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            path.push(bytes[i]);
            i += 1;
        }
    }
    let path = String::from_utf8(path).ok()?;
    // `file:///C:/work` names `C:/work` on Windows.
    let path = match path.strip_prefix('/') {
        Some(drive) if cfg!(windows) && drive.as_bytes().get(1) == Some(&b':') => drive.into(),
        _ => path,
    };
    crate::safe::is_absolute(&path).then_some(path)
}

/// Why the summaries could not be read, without their contents.
fn unreadable(file: &str, e: rusqlite::Error) -> ScanError {
    let reason = match e {
        rusqlite::Error::SqliteFailure(e, _)
            if matches!(
                e.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) =>
        {
            "Antigravity conversation summaries are locked"
        }
        rusqlite::Error::SqliteFailure(_, _) => "cannot read Antigravity conversation summaries",
        _ => "unexpected Antigravity conversation summaries shape",
    };
    error(reason, file, 0)
}

/// Each conversation's workspace, parent and agent, by conversation ID; empty when the root
/// keeps no summaries.
fn summaries(root: &str) -> Result<HashMap<String, Summary>, ScanError> {
    let file = join(&[root, SUMMARIES]);
    match std::fs::metadata(&file) {
        Ok(_) => {}
        Err(e) if absent(e.kind()) => return Ok(HashMap::default()),
        Err(_) => {
            return Err(error(
                "cannot inspect Antigravity conversation summaries",
                &file,
                0,
            ));
        }
    }
    let fail = |e| unreadable(&file, e);
    let mut db = Connection::open_with_flags(
        &file,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(fail)?;
    // Antigravity writes while it runs; a reader waits out a write briefly, then fails loud.
    db.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(fail)?;
    db.execute_batch("PRAGMA query_only=ON; PRAGMA trusted_schema=OFF;")
        .map_err(fail)?;
    let tx = db.transaction().map_err(fail)?;
    let mut found = HashMap::default();
    {
        // Titles, previews and summaries stay in SQLite.
        let mut statement = tx
            .prepare(
                "SELECT conversation_id, workspace_uris, agent_name, parent_conversation_id
                 FROM conversation_summaries",
            )
            .map_err(fail)?;
        let mut rows = statement.query([]).map_err(fail)?;
        while let Some(row) = rows.next().map_err(fail)? {
            let Some(id) = row.get::<_, Option<String>>(0).map_err(fail)? else {
                continue;
            };
            let uris: Option<String> = row.get(1).map_err(fail)?;
            let cwd = uris
                .and_then(|u| serde_json::from_str::<Vec<String>>(&u).ok())
                .and_then(|u| u.first().and_then(|u| file_path(u)))
                .map(Text::from);
            let name = |index| -> Result<Option<String>, ScanError> {
                let text: Option<String> = row.get(index).map_err(fail)?;
                Ok(text.and_then(|t| label(&Value::from(t))))
            };
            found.insert(
                id,
                Summary {
                    cwd,
                    agent: name(2)?,
                    parent: name(3)?,
                },
            );
        }
    }
    tx.rollback().map_err(fail)?;
    Ok(found)
}

/// One transcript step, by the fields token-max reads; its text is skipped unread.
#[derive(Deserialize)]
struct Step {
    #[serde(rename = "type")]
    kind: Option<String>,
    step_index: Option<u64>,
    created_at: Option<String>,
    model: Option<String>,
    input_tokens: Option<Value>,
    cache_read_tokens: Option<Value>,
    output_tokens: Option<Value>,
    tool_calls: Option<Vec<StepCall>>,
}

/// One tool call in a step, by its name and the file it reads; its other arguments are
/// skipped unread.
#[derive(Deserialize)]
struct StepCall {
    name: Option<Value>,
    args: Option<CallArgs>,
}

/// The one argument read: the file `read_file` reads.
#[derive(Deserialize)]
struct CallArgs {
    #[serde(rename = "Path")]
    path: Option<Value>,
}

/// The tool that reads a file, which is how Antigravity loads a skill's `SKILL.md`.
const READ_FILE: &str = "read_file";

/// A recorded model slug as its model and effort: `gemini-4-neon-high` is `gemini-4-neon` at
/// `high`.
fn split_model(slug: &str) -> (String, Option<String>) {
    match slug.rsplit_once('-') {
        Some((model, effort)) if !model.is_empty() && EFFORTS.contains(&effort) => {
            (model.into(), Some(effort.into()))
        }
        _ => (slug.into(), None),
    }
}

/// The usage Antigravity records, normalized: its input excludes cache reads, which are
/// added back, and its output already counts thinking. Gemini's implicit cache writes
/// nothing billed.
fn usage(input: Option<i64>, read: Option<i64>, output: Option<i64>) -> Usage {
    Usage {
        input: input.zip(read).map(|(input, read)| input + read),
        output,
        read,
        write: Some(0),
        ..Usage::default()
    }
}

/// The conversation a transcript belongs to: the `brain/<id>` directory it is under.
fn conversation(file: &str) -> Option<&str> {
    let mut parts = file.rsplit(['/', '\\']);
    let _ = (parts.next()?, parts.next()?, parts.next()?);
    parts.next().filter(|id| !id.is_empty())
}

/// Every request, conversation and subagent `root`'s transcripts record. `profiles` are the
/// profiles that read the root.
pub(crate) fn read(
    root: &str,
    files: &[String],
    profiles: &[String],
) -> Result<History, ScanError> {
    let summaries = summaries(root)?;
    let profiles: crate::model::Profiles = profiles.into();
    let mut history = History::default();
    for file in files {
        let Some(id) = conversation(file) else {
            return Err(error("unexpected Antigravity transcript path", file, 0));
        };
        let summary = summaries.get(id);
        let key: Text = identity(id).into();
        let parent = summary.and_then(|s| s.parent.clone());
        let agent = summary.and_then(|s| s.agent.clone());
        let cwd = summary.and_then(|s| s.cwd.clone());
        let shared = RequestMeta {
            harness: Harness::Antigravity,
            session: key.clone(),
            agent: if parent.is_some() {
                Agent::Subagent
            } else {
                Agent::Main
            },
            mode: agent.as_deref().map(Text::from),
            model: None,
            provider: Some(PROVIDER.into()),
            api: Api::Unknown,
            source: None,
            trigger: None,
            version: None,
            client: None,
            cwd: cwd.clone(),
            effort: None,
            server_tools: None,
            profiles: profiles.clone(),
            attempt: None,
            requested_speed: None,
            issues: vec![STANDARD_TIER.into()],
        };
        let (created, snapshot) = read_transcript(file, &shared, &mut history)?;
        history.snapshots.push(snapshot);
        if let Some(parent) = &parent {
            history.subagents.push(Subagent {
                harness: Harness::Antigravity,
                session: key.clone(),
                parent: Some(identity(parent).into()),
                kind: agent.unwrap_or_else(|| crate::reader::UNRECORDED.into()),
                built_in: None,
                fork: false,
                native: false,
                model: None,
                effort: None,
            });
        }
        let native = Native {
            key,
            id: id.into(),
            parent,
            cwd,
            version: None,
            created: created.unwrap_or(0),
        };
        history.sessions.push((native, file.as_str().into()));
    }
    Ok(history)
}

/// Add one transcript's requests to `history`, each sharing `shared` but for its model,
/// effort and source, and its tool calls, and return when the conversation began and what
/// was read.
fn read_transcript(
    file: &str,
    shared: &RequestMeta,
    history: &mut History,
) -> Result<(Option<Millis>, Snapshot), ScanError> {
    let requests = &mut history.requests;
    let handle = std::fs::File::open(file)
        .map_err(|_| error("cannot read Antigravity transcript", file, 0))?;
    let unreadable = |_| error("cannot read Antigravity transcript", file, 0);
    let (device, inode) = crate::reader::file_identity(&handle).map_err(unreadable)?;
    let bytes = handle.metadata().map_err(unreadable)?.len();
    // Read only the bytes the snapshot names, whatever Antigravity appends meanwhile.
    let mut reader = std::io::BufReader::new(handle).take(bytes);
    let mut snapshot = Snapshot {
        file: file.into(),
        harness: Harness::Antigravity,
        profiles: shared.profiles.iter().map(|p| p.to_string()).collect(),
        bytes,
        device,
        inode,
        deferred_bytes: 0,
        skipped_lines: 0,
        unreadable: Default::default(),
        lines: 0,
    };
    let location_file: Text = file.into();
    let mut created: Option<Millis> = None;
    // The one model every recorded request so far ran, or None once two differ.
    let mut only: Option<Option<String>> = None;
    let mut line = Vec::new();
    loop {
        line.clear();
        let line_number = snapshot.lines + 1;
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|_| error("cannot read Antigravity transcript", file, line_number))?;
        if read == 0 {
            break;
        }
        let finished = line.last() == Some(&b'\n');
        if finished {
            line.pop();
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            snapshot.lines = line_number;
            continue;
        }
        let step = match serde_json::from_slice::<Step>(&line) {
            Ok(step) => step,
            // A last line without its newline that doesn't parse is still being written: it
            // is left for a later read, and leaves the conversation's export incomplete. A
            // copied transcript's complete last record may lack the newline, so it parses.
            Err(_) if !finished => {
                snapshot.deferred_bytes = read as u64;
                break;
            }
            // A finished line that doesn't parse, such as a fragment a crash left that the
            // next write continued, is skipped as the transcript reader skips one, leaving
            // the conversation's export incomplete. It may have held a request on another
            // model, so it too ends the proof that one model filled the cache.
            Err(_) => {
                snapshot.lines = line_number;
                snapshot.skipped_lines += 1;
                only = Some(None);
                continue;
            }
        };
        snapshot.lines = line_number;
        let invalid = |_| error("invalid Antigravity token count", file, line_number);
        let time = step.created_at.as_deref().and_then(parse_instant);
        if let Some(time) = time {
            created = Some(created.map_or(time, |c| c.min(time)));
        }
        for call in step.tool_calls.iter().flatten() {
            // A name the model wrote that is no identifier names no feature.
            let Some(tool) = call.name.as_ref().and_then(label) else {
                continue;
            };
            let skill = call
                .args
                .as_ref()
                .filter(|_| tool == READ_FILE)
                .and_then(|a| a.path.as_ref()?.as_str())
                // Antigravity records each argument as JSON text, such as `"\"/a/SKILL.md\""`.
                .map(|p| serde_json::from_str::<String>(p).unwrap_or_else(|_| p.into()))
                .filter(|p| crate::skills::skill_file(p));
            history.calls.push(Call {
                time: time.or(created).unwrap_or(0),
                tool,
                skill,
            });
        }
        let input = count(step.input_tokens.as_ref()).map_err(invalid)?;
        let read = count(step.cache_read_tokens.as_ref()).map_err(invalid)?;
        let output = count(step.output_tokens.as_ref()).map_err(invalid)?;
        if input.is_none() && read.is_none() && output.is_none() {
            continue;
        }
        let recorded = step
            .model
            .as_deref()
            .and_then(|m| label(&Value::from(m)))
            .map(|m| split_model(&m));
        let mut meta = shared.clone();
        let model = match recorded {
            Some((model, effort)) => {
                only = match only {
                    None => Some(Some(model.clone())),
                    Some(Some(previous)) if previous == model => Some(Some(previous)),
                    Some(_) => Some(None),
                };
                meta.effort = effort.map(Text::from);
                Some(model)
            }
            // Gemini caches per model, so a read of this conversation's cache came from the
            // model every earlier request ran.
            None => match (&only, read) {
                (Some(Some(model)), Some(read)) if read > 0 => {
                    meta.issues.push(CACHED_MODEL.into());
                    Some(model.clone())
                }
                // A request on an unknown model could have filled the cache a later one reads.
                _ => {
                    only = Some(None);
                    None
                }
            },
        };
        meta.model = model.map(Text::from);
        let compaction = step.kind.as_deref() == Some("CHECKPOINT");
        if compaction {
            meta.source = Some("compaction".into());
        }
        let step_index = step.step_index.unwrap_or(line_number - 1);
        let time = time.or(created).unwrap_or(0);
        requests.push(Request {
            evidence: RequestEvidence::Measured,
            id: format!("antigravity:{}:{step_index}", shared.session).into(),
            time,
            end_time: time,
            reference: None,
            turn: None,
            parts: vec![BillingPart {
                kind: if compaction {
                    PartKind::Compaction
                } else {
                    PartKind::Message
                },
                model: meta.model.clone(),
                usage: usage(input, read, output),
                tier: Some("standard".into()),
                speed: None,
                geo: None,
            }],
            locations: Few::One(Location {
                file: location_file.clone(),
                line: line_number,
            }),
            t3: None,
            meta: Arc::new(meta),
        });
    }
    Ok((created, snapshot))
}

/// One MCP server, skill or rule file an Antigravity profile offers, with the uses its
/// transcripts record over the window. What it cost is unmeasured: no transcript records the
/// prompt that carried it.
#[derive(Clone, Debug, PartialEq)]
pub struct Feature {
    /// [`ContextKind::McpServer`], [`ContextKind::Skill`] or [`ContextKind::Instructions`].
    pub kind: ContextKind,
    /// The server's or skill's name, or the rule file's path.
    pub name: String,
    pub profiles: Vec<String>,
    /// Where Antigravity finds it: the settings that declare a server, or a skill's
    /// `SKILL.md` files. Empty for a server only the client that started Antigravity supplies,
    /// such as T3 Code's, and for a rule file, which is its own source.
    pub sources: Vec<String>,
    /// Calls to a server's tools, reads of a skill's `SKILL.md`, or the conversations whose
    /// workspace the rule file applies to now.
    pub uses: u64,
    pub last_used: Option<Millis>,
}

/// Where an Antigravity root's features are read from, and for whom.
#[derive(Clone, Copy)]
pub(crate) struct Inventory<'a> {
    /// The root, as its profiles read it.
    pub root: &'a str,
    /// Each `GEMINI_HOME` the root was selected under: the parent of each path that names it,
    /// before any symlink is resolved.
    pub homes: &'a [String],
    pub profiles: &'a [String],
    /// Count only uses from here on.
    pub since: Option<Millis>,
    /// `--defaults`, which reads no `config.json`.
    pub defaults: bool,
}

/// The names Antigravity gives the rule files it loads.
const RULES: [&str; 2] = ["GEMINI.md", "AGENTS.md"];

/// The object a JSON config file holds; empty when the file is missing or blank, as
/// Antigravity leaves `mcp_config.json` until a server is added.
fn config(file: &str) -> Result<Map<String, Value>, ScanError> {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(e) if absent(e.kind()) => return Ok(Map::new()),
        Err(_) => return Err(error("cannot read Antigravity config", file, 0)),
    };
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Ok(map),
        _ => Err(error("invalid Antigravity config", file, 0)),
    }
}

/// The names of the entries in `dir` that are directories, or else files, in order; none
/// when it is missing.
fn entries(dir: &str, directories: bool) -> Result<Vec<String>, ScanError> {
    let listed = match std::fs::read_dir(dir) {
        Ok(listed) => listed,
        Err(e) if absent(e.kind()) => return Ok(vec![]),
        Err(_) => return Err(error("cannot list Antigravity directory", dir, 0)),
    };
    let mut names = vec![];
    for entry in listed {
        let entry = entry.map_err(|_| error("cannot list Antigravity directory", dir, 0))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        // Followed through a symlink, as Antigravity follows it.
        match std::fs::metadata(entry.path()) {
            Ok(info) if info.is_dir() == directories => names.push(name),
            Ok(_) => {}
            Err(e) if absent(e.kind()) => {}
            Err(_) => {
                return Err(error(
                    "cannot inspect Antigravity file",
                    &join(&[dir, &name]),
                    0,
                ));
            }
        }
    }
    names.sort();
    Ok(names)
}

/// Whether `path` is a file.
fn is_file(path: &str) -> Result<bool, ScanError> {
    match std::fs::metadata(path) {
        Ok(info) => Ok(info.is_file()),
        Err(e) if absent(e.kind()) => Ok(false),
        Err(_) => Err(error("cannot inspect Antigravity file", path, 0)),
    }
}

/// The enabled plugins' directories: those in the config's `plugins` and the root's built-in
/// ones, each on unless `config.json` turns it off, or, with no entry there, its own
/// `plugin.json` does. `--defaults` reads no `config.json`.
fn plugins(home: &str, root: &str, defaults: bool) -> Result<IndexSet<String>, ScanError> {
    // `config.json` holds credentials too; only `plugins` is kept.
    let choices = match defaults {
        true => Map::new(),
        false => match config(&join(&[home, "config", "config.json"]))?.remove("plugins") {
            Some(Value::Object(choices)) => choices,
            _ => Map::new(),
        },
    };
    let mut enabled = IndexSet::default();
    for parent in [
        join(&[home, "config", "plugins"]),
        join(&[root, "builtin", "plugins"]),
    ] {
        for name in entries(&parent, true)? {
            let dir = join(&[&parent, &name]);
            let manifest = join(&[&dir, "plugin.json"]);
            if !is_file(&manifest)? {
                continue;
            }
            let chosen = choices
                .get(&name)
                .and_then(|c| c.get("enabled"))
                .and_then(Value::as_bool);
            let on = match chosen {
                Some(on) => on,
                None => config(&manifest)?.get("disabled").and_then(Value::as_bool) != Some(true),
            };
            if on {
                enabled.insert(dir);
            }
        }
    }
    Ok(enabled)
}

/// Add the servers an `mcp_config.json` declares, each with the setting that declares it.
fn servers_in(file: &str, into: &mut IndexMap<String, IndexSet<String>>) -> Result<(), ScanError> {
    if let Some(Value::Object(servers)) = config(file)?.remove("mcpServers") {
        for name in servers.keys() {
            into.entry(name.clone())
                .or_default()
                .insert(format!("{file}:mcpServers.{name}"));
        }
    }
    Ok(())
}

/// Add the skills a skills directory holds: each subdirectory with a `SKILL.md`, by name.
fn skills_in(dir: &str, into: &mut IndexMap<String, IndexSet<String>>) -> Result<(), ScanError> {
    for name in entries(dir, true)? {
        let file = join(&[dir, &name, "SKILL.md"]);
        if is_file(&file)? {
            into.entry(name).or_default().insert(file);
        }
    }
    Ok(())
}

/// The skill a `SKILL.md` in a project's `.agents/skills` is, by its directory's name.
fn project_skill(path: &str) -> Option<&str> {
    let mut parts = path.rsplit(['/', '\\']);
    let (_, name, skills, agents) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    (skills == "skills" && agents == ".agents" && !name.is_empty()).then_some(name)
}

/// The server whose tool `tool` is: Antigravity names one `mcp_<server>_<tool>`. A name two
/// servers could own goes to the one that cached such a tool, else to neither.
fn server_of<'a>(
    tool: &str,
    servers: &'a IndexMap<String, IndexSet<String>>,
    cached: &HashMap<String, HashSet<String>>,
) -> Option<&'a str> {
    let rest = tool.strip_prefix("mcp_")?;
    let owners: Vec<(&str, &str)> = servers
        .keys()
        .filter_map(|s| {
            let own = rest.strip_prefix(s.as_str())?.strip_prefix('_')?;
            (!own.is_empty()).then_some((s.as_str(), own))
        })
        .collect();
    match owners.as_slice() {
        [(server, _)] => Some(server),
        _ => {
            let mut sure = owners
                .iter()
                .filter(|(s, own)| cached.get(*s).is_some_and(|t| t.contains(*own)));
            match (sure.next(), sure.next()) {
                (Some((server, _)), None) => Some(server),
                _ => None,
            }
        }
    }
}

/// Each MCP server, skill and rule file `at` names offers, with the uses `history` records
/// from its `since` on.
///
/// Servers are those the config's `mcp_config.json` and each enabled plugin's declare, and
/// those whose tools Antigravity cached under the root's `mcp/`, as the server T3 Code supplies
/// is. Skills are those in the config's `skills`, the CLI's `skills`, the root's built-in ones,
/// each enabled plugin's, and each conversation's workspace's `.agents/skills`. Rule files are
/// `GEMINI.md` and `AGENTS.md` in `GEMINI_HOME`, each enabled plugin's `rules`, and each
/// directory from a conversation's workspace up to its repository's root.
pub(crate) fn features(at: &Inventory<'_>, history: &History) -> Result<Vec<Feature>, ScanError> {
    let Inventory {
        root,
        homes,
        profiles,
        since,
        defaults,
    } = *at;
    let windowed = |time: Millis| since.is_none_or(|since| time >= since);
    let mut plugins: IndexSet<String> = IndexSet::default();
    for home in homes {
        plugins.extend(self::plugins(home, root, defaults)?);
    }

    let mut servers: IndexMap<String, IndexSet<String>> = IndexMap::default();
    for home in homes {
        servers_in(&join(&[home, "config", "mcp_config.json"]), &mut servers)?;
    }
    for plugin in &plugins {
        servers_in(&join(&[plugin, "mcp_config.json"]), &mut servers)?;
    }
    let cache = join(&[root, "mcp"]);
    let mut cached: HashMap<String, HashSet<String>> = HashMap::default();
    for server in entries(&cache, true)? {
        let tools = entries(&join(&[&cache, &server]), false)?
            .into_iter()
            .filter_map(|file| file.strip_suffix(".json").map(String::from))
            .collect();
        cached.insert(server.clone(), tools);
        servers.entry(server).or_default();
    }

    let mut skills: IndexMap<String, IndexSet<String>> = IndexMap::default();
    for home in homes {
        skills_in(&join(&[home, "config", "skills"]), &mut skills)?;
        skills_in(&join(&[home, CLI, "skills"]), &mut skills)?;
    }
    skills_in(&join(&[root, "builtin", "skills"]), &mut skills)?;
    for plugin in &plugins {
        skills_in(&join(&[plugin, "skills"]), &mut skills)?;
    }
    let mut workspaces: IndexSet<&str> = IndexSet::default();
    for (session, _) in &history.sessions {
        if let Some(cwd) = session.cwd.as_deref()
            && workspaces.insert(cwd)
            && crate::discovery::existing_directory(cwd)?
        {
            skills_in(&join(&[cwd, ".agents", "skills"]), &mut skills)?;
        }
    }

    // Each feature's uses and when it was last used, by kind and name.
    let mut used: HashMap<(ContextKind, String), (u64, Millis)> = HashMap::default();
    let mut credit = |kind, name: &str, time: Millis| {
        let entry = used.entry((kind, name.to_string())).or_insert((0, time));
        entry.0 += 1;
        entry.1 = entry.1.max(time);
    };
    // Each skill's file by its path as found and as resolved, since a transcript may name it
    // through a symlink or its target.
    let mut files: HashMap<String, &str> = HashMap::default();
    for (name, found) in &skills {
        for file in found {
            files.insert(file.clone(), name);
            if let Ok(real) = crate::discovery::realpath(file) {
                files.insert(real, name);
            }
        }
    }
    // A project skill read from a workspace that no longer exists is still its skill.
    let mut read: Vec<(String, String)> = vec![];
    for call in history.calls.iter().filter(|c| windowed(c.time)) {
        if let Some(server) = server_of(&call.tool, &servers, &cached) {
            credit(ContextKind::McpServer, server, call.time);
        }
        let Some(path) = call.skill.as_deref() else {
            continue;
        };
        let known = files.get(path).copied().or_else(|| {
            let real = crate::discovery::realpath(path).ok()?;
            files.get(&real).copied()
        });
        let name = match known {
            Some(name) => name,
            None => match project_skill(path) {
                Some(name) => {
                    read.push((name.into(), path.into()));
                    name
                }
                None => continue,
            },
        };
        credit(ContextKind::Skill, name, call.time);
    }
    for (name, path) in read {
        skills.entry(name).or_default().insert(path);
    }

    // The window's conversations, and the rule files each loads.
    let active: IndexSet<&str> = history
        .requests
        .iter()
        .filter(|r| windowed(r.time))
        .map(|r| &*r.session)
        .collect();
    let mut rules: IndexMap<String, IndexSet<&str>> = IndexMap::default();
    let mut global = vec![];
    for dir in homes
        .iter()
        .cloned()
        .chain(plugins.iter().map(|p| join(&[p, "rules"])))
    {
        for name in RULES {
            let file = join(&[&dir, name]);
            if is_file(&file)? {
                global.push(file);
            }
        }
    }
    for file in global {
        rules
            .entry(file)
            .or_default()
            .extend(active.iter().copied());
    }
    let mut loaded: HashMap<&str, Vec<String>> = HashMap::default();
    for (session, _) in &history.sessions {
        let Some(cwd) = session.cwd.as_deref() else {
            continue;
        };
        if !loaded.contains_key(cwd) {
            let mut found = vec![];
            if crate::discovery::existing_directory(cwd)? {
                for dir in crate::discovery::project_directories(cwd, &[".git".into()])? {
                    for name in RULES {
                        let file = join(&[&dir, name]);
                        if is_file(&file)? {
                            found.push(file);
                        }
                    }
                }
            }
            loaded.insert(cwd, found);
        }
        for file in &loaded[cwd] {
            let sessions = rules.entry(file.clone()).or_default();
            if active.contains(&*session.key) {
                sessions.insert(&*session.key);
            }
        }
    }

    let feature = |kind, name: String, sources: IndexSet<String>| {
        let (uses, last) = used.get(&(kind, name.clone())).copied().unwrap_or_default();
        Feature {
            kind,
            name,
            profiles: profiles.to_vec(),
            sources: sources.into_iter().collect(),
            uses,
            last_used: (uses > 0).then_some(last),
        }
    };
    let mut found: Vec<Feature> = servers
        .into_iter()
        .map(|(name, sources)| feature(ContextKind::McpServer, name, sources))
        .collect();
    found.extend(
        skills
            .into_iter()
            .map(|(name, sources)| feature(ContextKind::Skill, name, sources)),
    );
    found.extend(rules.into_iter().map(|(file, sessions)| Feature {
        kind: ContextKind::Instructions,
        name: file,
        profiles: profiles.to_vec(),
        sources: vec![],
        uses: sessions.len() as u64,
        // Only the file as it is now is known, not when it was added.
        last_used: None,
    }));
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slug_splits_off_only_a_known_effort() {
        assert_eq!(
            split_model("gemini-4-neon-high"),
            ("gemini-4-neon".into(), Some("high".into()))
        );
        assert_eq!(
            split_model("gemini-4-carbon-xhigh"),
            ("gemini-4-carbon".into(), Some("xhigh".into()))
        );
        assert_eq!(split_model("gemini-4-neon"), ("gemini-4-neon".into(), None));
        assert_eq!(
            split_model("gemini-2.5-pro"),
            ("gemini-2.5-pro".into(), None)
        );
        assert_eq!(split_model("-high"), ("-high".into(), None));
    }

    #[test]
    fn workspace_uris_name_absolute_decoded_paths() {
        if cfg!(windows) {
            assert_eq!(
                file_path("file:///C:/my%20work").as_deref(),
                Some("C:/my work")
            );
        } else {
            assert_eq!(
                file_path("file:///my%20work/repo").as_deref(),
                Some("/my work/repo")
            );
        }
        assert_eq!(file_path("https://example.com/x"), None);
        assert_eq!(file_path("file://relative"), None);
        assert_eq!(file_path("file:///bad%zz"), None);
        assert_eq!(file_path("file:///bad%ff"), None);
    }

    #[test]
    fn a_transcript_names_its_conversation() {
        let file =
            "/home/u/.gemini/antigravity-cli/brain/abc/.system_generated/logs/transcript.jsonl";
        assert_eq!(conversation(file), Some("abc"));
        assert_eq!(conversation("transcript.jsonl"), None);
    }

    /// The synthetic root's history, read as one profile.
    fn synthetic(w: &crate::test_support::Workspace) -> (String, History) {
        let root = w.home.join(".gemini").join(CLI);
        crate::test_support::antigravity_root(&root);
        let root = root.to_string_lossy().into_owned();
        let profile = profile(root.clone(), root.clone(), "key").unwrap();
        assert_eq!(profile.histories.len(), 2);
        let history = read(&root, &profile.histories, std::slice::from_ref(&profile.id)).unwrap();
        (root, history)
    }

    #[test]
    fn transcripts_become_priced_requests_conversations_and_subagents_without_text() {
        let w = crate::test_support::Workspace::new();
        let (_, history) = synthetic(&w);
        assert!(!format!("{history:?}").contains("SYNTHETIC_PRIVATE"));
        let catalog = crate::test_support::prices();
        let cost = |r: &Request| crate::prices::price_request(&catalog, r).total_usd;
        let main = identity("main-id");
        let requests: Vec<&Request> = history
            .requests
            .iter()
            .filter(|r| *r.session == *main)
            .collect();
        // The user step records no usage.
        assert_eq!(requests.len(), 3);
        let first = requests[0];
        assert_eq!(first.model.as_deref(), Some("gemini-4-neon"));
        assert_eq!(first.effort.as_deref(), Some("high"));
        assert_eq!(first.agent, Agent::Main);
        assert_eq!(first.provider.as_deref(), Some(PROVIDER));
        // Recorded input is uncached; the normalized input adds the cache reads back.
        let second = &requests[1].parts[0].usage;
        assert_eq!(
            (second.input, second.read, second.write, second.output),
            (Some(1_500), Some(1_000), Some(0), Some(50))
        );
        // 500 uncached at $3, 1,000 cached at $0.15 and 50 output at $15 per million.
        assert!((cost(requests[1]).unwrap().to_f64() - 0.0024).abs() < 1e-12);
        // The checkpoint read the cache only gemini-4-neon held.
        let checkpoint = requests[2];
        assert_eq!(checkpoint.parts[0].kind, PartKind::Compaction);
        assert_eq!(checkpoint.source.as_deref(), Some("compaction"));
        assert_eq!(checkpoint.model.as_deref(), Some("gemini-4-neon"));
        assert_eq!(checkpoint.effort, None);
        assert!(checkpoint.issues.iter().any(|i| &**i == CACHED_MODEL));
        assert!(cost(checkpoint).is_some());
        let sub = history
            .requests
            .iter()
            .find(|r| *r.session == *identity("sub-id"))
            .unwrap();
        assert_eq!(sub.agent, Agent::Subagent);
        assert_eq!(sub.mode.as_deref(), Some("research"));
        assert_eq!(sub.effort.as_deref(), Some("medium"));
        assert_eq!(history.subagents.len(), 1);
        let subagent = &history.subagents[0];
        assert_eq!(subagent.kind, "research");
        assert_eq!(subagent.parent.as_deref(), Some(&*main));
        let (session, file) = history
            .sessions
            .iter()
            .find(|(s, _)| s.id == "sub-id")
            .unwrap();
        assert_eq!(session.parent.as_deref(), Some("main-id"));
        assert!(file.ends_with("transcript.jsonl"));
        if cfg!(unix) {
            assert_eq!(session.cwd.as_deref(), Some("/work/repo"));
        }
    }

    #[test]
    fn a_request_without_a_model_is_priced_only_when_the_conversations_cache_proves_it() {
        let w = crate::test_support::Workspace::new();
        let (root, _) = synthetic(&w);
        let file = |id: &str| {
            join(&[
                &root,
                "brain",
                id,
                ".system_generated",
                "logs",
                "transcript.jsonl",
            ])
        };
        let step = |model: Option<&str>, read: u64| {
            serde_json::json!({"type": "PLANNER_RESPONSE", "created_at": "2026-01-01T00:00:00Z",
                "model": model, "input_tokens": 10, "cache_read_tokens": read,
                "output_tokens": 1})
            .to_string()
        };
        // A first request with no model has no cache to prove one.
        let cold = [step(None, 0)].join("\n") + "\n";
        // Two models ran, so a cache read proves neither.
        let mixed = [
            step(Some("gemini-4-neon-high"), 0),
            step(Some("gemini-4-carbon-high"), 0),
            step(None, 100),
        ]
        .join("\n")
            + "\n";
        // A request on an unknown model could have filled the cache the next one reads.
        let gap = [
            step(Some("gemini-4-neon-high"), 0),
            step(None, 0),
            step(None, 100),
        ]
        .join("\n")
            + "\n";
        // So could a line that doesn't parse.
        let skipped = [
            step(Some("gemini-4-neon-high"), 0),
            "{\"type\":".into(),
            step(None, 100),
        ]
        .join("\n")
            + "\n";
        let ids = ["cold", "mixed", "gap", "skipped"];
        for (id, text) in ids.into_iter().zip([cold, mixed, gap, skipped]) {
            std::fs::create_dir_all(crate::discovery::dirname(&file(id))).unwrap();
            std::fs::write(file(id), text).unwrap();
        }
        let files: Vec<String> = ids.into_iter().map(file).collect();
        let history = read(&root, &files, &["p".into()]).unwrap();
        let unknown: Vec<&Request> = history
            .requests
            .iter()
            .filter(|r| r.model.is_none())
            .collect();
        assert_eq!(unknown.len(), 5);
        let catalog = crate::test_support::prices();
        for r in unknown {
            assert_eq!(crate::prices::price_request(&catalog, r).total_usd, None);
        }
    }

    /// The synthetic root, plus config, plugins, cached MCP tools, skills, rule files and a
    /// conversation `tools-id` in a checkout's `repo/sub` that calls tools at noon on January
    /// 2 and 3; returns its root, home and workspace.
    fn customized(w: &crate::test_support::Workspace) -> (String, String, String) {
        use serde_json::json;
        let home = w.home.join(".gemini").to_string_lossy().into_owned();
        let root = join(&[&home, CLI]);
        crate::test_support::antigravity_root(std::path::Path::new(&root));
        let write = |parts: &[&str], text: &str| {
            let file = join(parts);
            std::fs::create_dir_all(crate::discovery::dirname(&file)).unwrap();
            std::fs::write(&file, text).unwrap();
        };
        let servers = |names: &[&str]| {
            let servers: Map<String, Value> = names
                .iter()
                .map(|n| (n.to_string(), json!({"command": "x"})))
                .collect();
            json!({ "mcpServers": servers }).to_string()
        };
        write(
            &[&home, "config", "mcp_config.json"],
            &servers(&["docs", "a", "a_b"]),
        );
        // Credentials sit beside the plugin choices; `kit` is turned off here.
        write(
            &[&home, "config", "config.json"],
            &json!({"auth": "SYNTHETIC_PRIVATE", "plugins": {"kit": {"enabled": false}}})
                .to_string(),
        );
        write(&[&home, "config", "plugins", "kit", "plugin.json"], "{}");
        write(
            &[&home, "config", "plugins", "kit", "mcp_config.json"],
            &servers(&["kit-server"]),
        );
        write(
            &[
                &home,
                "config",
                "plugins",
                "kit",
                "skills",
                "kit-skill",
                "SKILL.md",
            ],
            "x",
        );
        write(
            &[&home, "config", "plugins", "kit", "rules", "AGENTS.md"],
            "x",
        );
        write(
            &[&root, "builtin", "plugins", "shipped-off", "plugin.json"],
            r#"{"disabled": true}"#,
        );
        write(
            &[
                &root,
                "builtin",
                "plugins",
                "shipped-off",
                "mcp_config.json",
            ],
            &servers(&["hidden"]),
        );
        write(&[&root, "mcp", "t3-code", "thread_list.json"], "{}");
        write(&[&root, "mcp", "a_b", "y.json"], "{}");
        write(&[&home, "config", "skills", "notes", "SKILL.md"], "x");
        write(&[&root, "builtin", "skills", "guide", "SKILL.md"], "x");
        write(&[&home, "GEMINI.md"], "x");
        let repo = join(&[&w.home.to_string_lossy(), "repo"]);
        std::fs::create_dir_all(join(&[&repo, ".git"])).unwrap();
        write(&[&repo, "AGENTS.md"], "x");
        let sub = join(&[&repo, "sub"]);
        write(&[&sub, "GEMINI.md"], "x");
        write(&[&sub, ".agents", "skills", "lint", "SKILL.md"], "x");
        let uri = match cfg!(windows) {
            true => format!("file:///{}", sub.replace('\\', "/")),
            false => format!("file://{sub}"),
        };
        let db = rusqlite::Connection::open(join(&[&root, SUMMARIES])).unwrap();
        db.execute(
            "INSERT INTO conversation_summaries VALUES ('tools-id', '', ?1, '', '')",
            [json!([uri]).to_string()],
        )
        .unwrap();
        let guide = join(&[&root, "builtin", "skills", "guide", "SKILL.md"]);
        let calls = |day: u32, calls: Value| {
            json!({"type": "PLANNER_RESPONSE", "created_at": format!("2026-01-0{day}T12:00:00Z"),
                   "model": "gemini-4-neon-high", "input_tokens": 10, "cache_read_tokens": 0,
                   "output_tokens": 1, "tool_calls": calls})
            .to_string()
        };
        let path = |p: &str| json!({ "Path": json!(p).to_string() });
        let steps = [
            calls(2, json!([{"name": "mcp_docs_search", "args": {}}])),
            calls(
                3,
                json!([
                    {"name": "mcp_docs_search", "args": {"Query": "SYNTHETIC_PRIVATE"}},
                    {"name": "mcp_t3-code_thread_list", "args": {}},
                    // `a` and `a_b` could both own these; only `a_b` cached `y`.
                    {"name": "mcp_a_b_x", "args": {}},
                    {"name": "mcp_a_b_y", "args": {}},
                    {"name": "read_file", "args": path(&guide)},
                    // A project skill whose workspace is gone is still its skill.
                    {"name": "read_file", "args": path("/gone/.agents/skills/deploy/SKILL.md")},
                    {"name": "read_file", "args": path("/gone/notes.md")},
                    {"name": "SYNTHETIC PRIVATE", "args": {}},
                ]),
            ),
        ];
        write(
            &[
                &root,
                "brain",
                "tools-id",
                ".system_generated",
                "logs",
                "transcript.jsonl",
            ],
            &(steps.join("\n") + "\n"),
        );
        let sub = match cfg!(windows) {
            true => sub.replace('\\', "/"),
            false => sub,
        };
        (root, home, sub)
    }

    #[test]
    fn features_count_their_recorded_uses_in_the_window() {
        let w = crate::test_support::Workspace::new();
        let (root, home, sub) = customized(&w);
        let ids = vec!["p".to_string()];
        let files = transcripts(&root).unwrap();
        let history = read(&root, &files, &ids).unwrap();
        assert!(!format!("{history:?}").contains("SYNTHETIC"));
        let found = |since, defaults| {
            let at = Inventory {
                root: &root,
                homes: std::slice::from_ref(&home),
                profiles: &ids,
                since,
                defaults,
            };
            features(&at, &history)
                .unwrap()
                .into_iter()
                .map(|f| ((f.kind, f.name), (f.uses, f.sources)))
                .collect::<IndexMap<_, _>>()
        };
        let all = found(None, false);
        let get = |kind, name: &str| all[&(kind, name.to_string())].clone();
        let config = join(&[&home, "config", "mcp_config.json"]);
        assert_eq!(
            get(ContextKind::McpServer, "docs"),
            (2, vec![format!("{config}:mcpServers.docs")])
        );
        // A call two servers could own goes to the one that cached the tool, else neither.
        assert_eq!(get(ContextKind::McpServer, "a").0, 0);
        assert_eq!(get(ContextKind::McpServer, "a_b").0, 1);
        // T3's server is known only by the tools Antigravity cached.
        assert_eq!(get(ContextKind::McpServer, "t3-code"), (1, vec![]));
        assert_eq!(get(ContextKind::Skill, "guide").0, 1);
        assert_eq!(get(ContextKind::Skill, "notes").0, 0);
        assert_eq!(
            get(ContextKind::Skill, "lint"),
            (
                0,
                vec![join(&[&sub, ".agents", "skills", "lint", "SKILL.md"])]
            )
        );
        assert_eq!(
            get(ContextKind::Skill, "deploy"),
            (1, vec!["/gone/.agents/skills/deploy/SKILL.md".to_string()])
        );
        // A rule file applies to every conversation under its directory.
        assert_eq!(
            get(ContextKind::Instructions, &join(&[&home, "GEMINI.md"])).0,
            3
        );
        assert_eq!(
            get(ContextKind::Instructions, &join(&[&sub, "GEMINI.md"])).0,
            1
        );
        let repo = crate::discovery::dirname(&sub);
        assert_eq!(
            get(ContextKind::Instructions, &join(&[repo, "AGENTS.md"])).0,
            1
        );
        // `config.json` turns `kit` off, and `shipped-off` ships off.
        let names: Vec<&str> = all.keys().map(|(_, n)| n.as_str()).collect();
        for absent in ["kit-server", "kit-skill", "hidden"] {
            assert!(!names.contains(&absent), "{absent}");
        }
        // `--defaults` reads no `config.json`, so `kit` is on as its manifest ships it.
        let defaults = found(None, true);
        assert!(defaults.contains_key(&(ContextKind::McpServer, "kit-server".into())));
        assert!(defaults.contains_key(&(ContextKind::Skill, "kit-skill".into())));
        let kit_rules = join(&[&home, "config", "plugins", "kit", "rules", "AGENTS.md"]);
        assert_eq!(defaults[&(ContextKind::Instructions, kit_rules)].0, 3);
        assert!(!defaults.contains_key(&(ContextKind::McpServer, "hidden".into())));
        // Only the window's calls and conversations count.
        let since = parse_instant("2026-01-03T00:00:00Z");
        let window = found(since, false);
        assert_eq!(window[&(ContextKind::McpServer, "docs".into())].0, 1);
        let global = (ContextKind::Instructions, join(&[&home, "GEMINI.md"]));
        assert_eq!(window[&global].0, 1);
        // The scan carries them for the report.
        let scan = crate::reader::scan(&w.scan_options()).unwrap();
        assert_eq!(scan.exposure.antigravity.len(), all.len());
    }

    /// A root selected through a symlink reads the settings of the home it was selected in,
    /// and a skill read by its resolved path is still the skill its home lists.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_root_reads_the_home_it_was_selected_in() {
        use serde_json::json;
        let w = crate::test_support::Workspace::new();
        let (root, _, _) = customized(&w);
        let linked = w.home.join("linked");
        std::fs::create_dir_all(linked.join("config")).unwrap();
        std::fs::write(
            linked.join("config/mcp_config.json"),
            r#"{"mcpServers": {"linked": {}}}"#,
        )
        .unwrap();
        std::os::unix::fs::symlink(&root, linked.join(CLI)).unwrap();
        let real = w.home.join("real-skills");
        std::fs::create_dir_all(real.join("x")).unwrap();
        std::fs::write(real.join("x/SKILL.md"), "x").unwrap();
        std::os::unix::fs::symlink(&real, linked.join("config/skills")).unwrap();
        let step = json!({"type": "PLANNER_RESPONSE", "created_at": "2026-01-04T00:00:00Z",
            "model": "gemini-4-neon-high", "input_tokens": 10, "cache_read_tokens": 0,
            "output_tokens": 1, "tool_calls": [{"name": "read_file", "args": {
                "Path": json!(real.join("x/SKILL.md")).to_string()}}]});
        let file = join(&[
            &root,
            "brain",
            "tools-id",
            ".system_generated",
            "logs",
            "transcript.jsonl",
        ]);
        let text = std::fs::read_to_string(&file).unwrap();
        std::fs::write(&file, format!("{text}{step}\n")).unwrap();
        let mut options = w.scan_options();
        options.discovery.antigravity_dirs = vec![linked.join(CLI).to_string_lossy().into_owned()];
        let scan = crate::reader::scan(&options).unwrap();
        let uses = |kind, name: &str| {
            scan.exposure
                .antigravity
                .iter()
                .find(|f| f.kind == kind && f.name == name)
                .map(|f| f.uses)
        };
        assert_eq!(uses(ContextKind::McpServer, "linked"), Some(0));
        // `~/.gemini`'s config is not the home this root was selected in.
        assert_eq!(uses(ContextKind::McpServer, "docs"), None);
        assert_eq!(uses(ContextKind::Skill, "x"), Some(1));
    }

    #[test]
    fn a_malformed_config_fails_loud_and_a_blank_one_declares_nothing() {
        let w = crate::test_support::Workspace::new();
        let (root, home, _) = customized(&w);
        let file = join(&[&home, "config", "mcp_config.json"]);
        std::fs::write(&file, "\n").unwrap();
        let history = History::default();
        let at = Inventory {
            root: &root,
            homes: std::slice::from_ref(&home),
            profiles: &[],
            since: None,
            defaults: false,
        };
        let servers: Vec<String> = features(&at, &history)
            .unwrap()
            .into_iter()
            .filter(|f| f.kind == ContextKind::McpServer)
            .map(|f| f.name)
            .collect();
        assert_eq!(servers, ["a_b", "t3-code"]);
        std::fs::write(&file, "{").unwrap();
        let error = features(&at, &history).unwrap_err();
        assert!(
            error.to_string().contains("invalid Antigravity config"),
            "{error}"
        );
        // A session export reads no settings, so the config can't stop it.
        let export = crate::reader::ScanOptions {
            reach: crate::reader::Reach::Window,
            ..w.scan_options()
        };
        let scan = crate::reader::scan(&export).unwrap();
        assert!(!scan.requests.is_empty());
        assert!(scan.exposure.antigravity.is_empty());
    }

    #[test]
    fn gemini_home_moves_the_cli_and_a_missing_one_is_skipped() {
        let w = crate::test_support::Workspace::new();
        let moved = w.home.join("gemini");
        crate::test_support::antigravity_root(&moved.join(CLI));
        let antigravity = |options: &crate::discovery::DiscoveryOptions| -> Vec<String> {
            crate::discovery::discover(options)
                .unwrap()
                .profiles
                .into_iter()
                .filter(|p| p.harness == Harness::Antigravity)
                .map(|p| p.root)
                .collect()
        };
        let mut options = w.scan_options().discovery;
        options
            .env
            .insert("GEMINI_HOME".into(), moved.to_string_lossy().into_owned());
        assert_eq!(
            antigravity(&options),
            [moved.join(CLI).to_string_lossy().into_owned()]
        );
        // Gemini's other tools set it too, so a home without the CLI fails nothing.
        options.env.insert(
            "GEMINI_HOME".into(),
            w.home.join("absent").to_string_lossy().into_owned(),
        );
        assert!(antigravity(&options).is_empty());
        // `--defaults` still finds the history the moved home holds.
        options
            .env
            .insert("GEMINI_HOME".into(), moved.to_string_lossy().into_owned());
        let mut defaults = options.clone();
        defaults.pretend_defaults();
        assert_eq!(antigravity(&defaults), antigravity(&options));
        // The audit reads no Antigravity history.
        options.unreplayed = false;
        assert!(antigravity(&options).is_empty());
    }

    #[test]
    fn a_malformed_line_is_skipped_and_only_an_unfinished_last_line_waits() {
        let w = crate::test_support::Workspace::new();
        let (root, _) = synthetic(&w);
        let file = join(&[
            &root,
            "brain",
            "main-id",
            ".system_generated",
            "logs",
            "transcript.jsonl",
        ]);
        let text = std::fs::read_to_string(&file).unwrap();
        std::fs::write(&file, format!("{text}{{\"type\":\"PLANNER_RES")).unwrap();
        let history = read(&root, std::slice::from_ref(&file), &["p".into()]).unwrap();
        assert_eq!(history.requests.len(), 3);
        // Left for a later read, it leaves the conversation's export incomplete.
        let deferred = "{\"type\":\"PLANNER_RES".len() as u64;
        assert_eq!(history.snapshots[0].deferred_bytes, deferred);
        // A copied transcript's complete last record may lack its newline.
        std::fs::write(&file, text.trim_end()).unwrap();
        let copied = read(&root, std::slice::from_ref(&file), &["p".into()]).unwrap();
        assert_eq!(copied.requests.len(), 3);
        let s = &copied.snapshots[0];
        assert_eq!((s.deferred_bytes, s.skipped_lines), (0, 0));
        // A finished line that doesn't parse, last or not, is skipped and the rest kept.
        for text in [format!("{text}{{\"type\":\n"), format!("{{\"type\":{text}")] {
            std::fs::write(&file, text).unwrap();
            let history = read(&root, std::slice::from_ref(&file), &["p".into()]).unwrap();
            assert_eq!(history.requests.len(), 3);
            let s = &history.snapshots[0];
            assert_eq!((s.deferred_bytes, s.skipped_lines), (0, 1));
        }
    }
}
